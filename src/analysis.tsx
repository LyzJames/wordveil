// Analysis entries (contract reading, loan reconciliation, budget review).
// Local evidence and deterministic calculations always run on this computer.
// With a generation connection, the written analysis by fixed narrow roles
// is approved in the egress preview and the native confirmation, and its
// result appears in the Q&A thread.
import React, { useRef, useState } from 'react';
import { api, money, type AnswerPreview, type EvidenceItem, type Snapshot } from './api';
import { EgressDialog } from './egress';
import { EvidenceList } from './evidence';
import { Icon } from './icons';
import { loanFigures } from './money';
import { sourceOf, useSession } from './session';
import { newTurnId, type TurnKind } from './turns';
import { Busy, Callout, ErrorLine, Field, Modal, useAction, useDismiss } from './ui';

type Workflow = 'contract-review' | 'loan-analysis' | 'budget-analysis';
const TURN: Record<Workflow, TurnKind> = { 'contract-review': 'contract', 'loan-analysis': 'loan', 'budget-analysis': 'budget' };
const TITLES: Record<Workflow, string> = { 'contract-review': '合同阅读', 'loan-analysis': '借款核对', 'budget-analysis': '预算分析' };
const QUESTIONS: Record<Workflow, string> = {
  'contract-review': '这份合同的押金、租金、期限和违约责任是怎样约定的？',
  'loan-analysis': '借款金额、已还款和剩余欠款是多少？还款期限是什么时候？',
  'budget-analysis': '这段时间的收支情况如何？有哪些异常或固定支出？',
};

export function AnalysisButton({ matter, snapshot }: { matter: string; snapshot: Snapshot }) {
  const [workflow, setWorkflow] = useState<Workflow | null>(null);
  const [menu, setMenu] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(ref, menu, () => setMenu(false), '.analysis-menu > button');
  const suggested: Workflow = snapshot.matter.matter_type === 'loan' ? 'loan-analysis' : snapshot.matter.matter_type === 'household' ? 'budget-analysis' : 'contract-review';
  return (
    <div className="analysis-menu">
      <button className="btn" aria-haspopup="menu" aria-expanded={menu} onClick={() => setMenu(!menu)}>
        分析
        <Icon name="chevronDown" />
      </button>
      {menu && (
        <div className="popover menu fade-in" role="menu" ref={ref}>
          {(['contract-review', 'loan-analysis', 'budget-analysis'] as Workflow[]).map((w) => (
            <button key={w} role="menuitem" className="menu-item" onClick={() => { setMenu(false); setWorkflow(w); }}>
              {TITLES[w]}
              {w === suggested && <span className="hint">推荐</span>}
            </button>
          ))}
        </div>
      )}
      {workflow && <AnalysisModal workflow={workflow} matter={matter} snapshot={snapshot} onClose={() => setWorkflow(null)} />}
    </div>
  );
}

type Outcome = { status: string; message: string; items: EvidenceItem[]; calculation: Record<string, unknown> | null };

function AnalysisModal({ workflow, matter, snapshot, onClose }: { workflow: Workflow; matter: string; snapshot: Snapshot; onClose: () => void }) {
  const { open, connection, qa, go } = useSession();
  const [preview, setPreview] = useState<AnswerPreview | null>(null);
  const documents = snapshot.documents.filter((d) => d.media_kind === 'document');
  const media = snapshot.documents.filter((d) => d.media_id);
  const [question, setQuestion] = useState(QUESTIONS[workflow]);
  const [versions, setVersions] = useState<Set<string>>(new Set(documents.slice(0, 3).map((d) => d.version_id)));
  const [mediaRefs, setMediaRefs] = useState<Set<string>>(new Set());
  const [period, setPeriod] = useState({ start: '', end: '' });
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [error, setError] = useState('');
  const { busy, run } = useAction();
  const flip = (set: Set<string>, id: string) => {
    const next = new Set(set);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    return next;
  };

  /** Written analysis: preview what may leave, then the native confirmation. */
  async function written() {
    setError('');
    if (workflow === 'budget-analysis' && (!period.start || !period.end)) {
      setError('请选择分析期间');
      return;
    }
    const prepared = await run(() => api.analysisPreview(workflow, {
      question, matter_id: matter, document_versions: [...versions], period: workflow === 'budget-analysis' ? period : undefined, account_ids: [], connection: connection?.id ?? null,
    }));
    if (prepared) setPreview(prepared);
  }

  async function approve(excluded: string[], excludedMedia: string[]) {
    if (!preview) return;
    try {
      const task = await api.answerStart(preview.grant_id, excluded, excludedMedia);
      qa.update((turns) => [...turns, { id: newTurnId(), question, matters: [matter], kind: TURN[workflow], state: 'running', started: Date.now(), task, source: snapshot.matter.title }]);
      onClose();
      go({ name: 'qa' });
    } catch (e) {
      setPreview(null);
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  async function start() {
    setError('');
    setOutcome(null);
    const result = await run(async () => {
      const questionId = await api.object('question', { text: question });
      const input: Record<string, unknown> = { question_id: questionId };
      if (workflow === 'contract-review') Object.assign(input, { document_versions: [...versions], known_constraint_ids: [], media_refs: [...mediaRefs] });
      if (workflow === 'loan-analysis') Object.assign(input, { matter_id: matter, document_versions: [...versions], media_refs: [...mediaRefs] });
      if (workflow === 'budget-analysis') {
        if (!period.start || !period.end) throw new Error('请选择分析期间');
        Object.assign(input, { document_versions: [...versions], period, account_ids: [] });
      }
      const envelope = await api.run(workflow, input);
      if (!envelope.data_ref) throw new Error(envelope.warnings.map((w) => w.message).join('；') || '分析未完成');
      const payload = await api.result<{ retrieval: string | null; calculation: string | null }>(envelope.data_ref);
      const retrieval = payload.retrieval ? await api.result<{ items: EvidenceItem[] }>(payload.retrieval) : { items: [] };
      const calculation = payload.calculation ? (await api.result<{ result: Record<string, unknown> }>(payload.calculation)).result : null;
      return { status: envelope.status, message: envelope.warnings.map((w) => w.message).join('；'), items: retrieval.items, calculation };
    });
    if (result) setOutcome(result);
  }

  if (preview) {
    return <EgressDialog preview={preview} onCancel={() => { api.answerDiscard(preview.grant_id).catch(() => {}); setPreview(null); }} onConfirm={approve} />;
  }
  const empty = workflow !== 'loan-analysis' && versions.size === 0 && mediaRefs.size === 0;
  return (
    <Modal
      title={TITLES[workflow]}
      icon="list"
      onClose={onClose}
      wide
      footer={
        <>
          <button className="btn" onClick={onClose}>关闭</button>
          <button className={connection ? 'btn' : 'btn btn-primary'} disabled={busy || empty} onClick={start}>
            {busy ? '正在本机检索与计算…' : '只看本机证据与计算'}
          </button>
          {connection && (
            <button className="btn btn-primary" disabled={busy || empty || mediaRefs.size > 0} onClick={written} title={mediaRefs.size > 0 ? '书面分析暂不含录音/视频；可在资料列表中单独分析录音或视频' : undefined}>
              生成书面分析
            </button>
          )}
        </>
      }
    >
      <Field label="你想了解什么">
        <textarea rows={2} value={question} onChange={(e) => setQuestion(e.target.value)} maxLength={2000} />
      </Field>
      {(workflow !== 'loan-analysis' || documents.length > 0) && (
        <fieldset className="pick">
          <legend>使用的资料</legend>
          {documents.length === 0 && <p className="t-caption text-tertiary">本事务还没有文档资料。</p>}
          {documents.map((d) => (
            <label key={d.version_id} className="check">
              <input type="checkbox" checked={versions.has(d.version_id)} onChange={() => setVersions((s) => flip(s, d.version_id))} />
              {d.display_name}（第 {d.version_no} 版）
            </label>
          ))}
        </fieldset>
      )}
      {workflow !== 'budget-analysis' && media.length > 0 && (
        <fieldset className="pick">
          <legend>同时转写并检索的录音/视频（本机 ASR 与抽帧 OCR）</legend>
          {media.map((d) => (
            <label key={d.media_id!} className="check">
              <input type="checkbox" checked={mediaRefs.has(d.media_id!)} onChange={() => setMediaRefs((s) => flip(s, d.media_id!))} />
              {d.display_name}
            </label>
          ))}
        </fieldset>
      )}
      {workflow === 'budget-analysis' && (
        <div className="grid-2">
          <Field label="期间开始"><input type="date" value={period.start} onChange={(e) => setPeriod({ ...period, start: e.target.value })} /></Field>
          <Field label="期间结束"><input type="date" value={period.end} onChange={(e) => setPeriod({ ...period, end: e.target.value })} /></Field>
        </div>
      )}
      <ErrorLine text={error} />
      {busy && <Busy label="正在本机检索原文、重排并核对计算（首次加载模型可能需要一分钟）…" />}
      {outcome && (
        <div className="stack">
          <Callout title={connection ? '本机证据与计算（未发送任何内容）' : '书面分析需要连接生成接口'}>
            {outcome.message ? `${outcome.message}。` : ''}以下是本机找到的原文依据{outcome.calculation ? '与确定性计算结果' : ''}，可以逐条回到原文核对{connection ? '；“生成书面分析”会在发送前请你确认' : ''}。
          </Callout>
          {outcome.calculation && <CalculationSummary result={outcome.calculation} />}
          <h3 className="t-subtitle">原文依据（{outcome.items.length}）</h3>
          <EvidenceList items={outcome.items} onOpen={(item) => { onClose(); open(sourceOf(item)); }} />
        </div>
      )}
    </Modal>
  );
}

const LABELS: Record<string, string> = { total_received: '收到', total_paid: '付出', finance_cost: '资金成本' };

export function CalculationSummary({ result }: { result: Record<string, unknown> }) {
  const figures: [string, string][] = result.mode === 'reconcile'
    ? loanFigures(result)
    : Object.entries(LABELS).filter(([key]) => typeof result[key] === 'string').map(([key, label]) => [label, result[key] as string]);
  if (!figures.length) {
    return (
      <details>
        <summary>计算结果</summary>
        <pre className="preview">{JSON.stringify(result, null, 2)}</pre>
      </details>
    );
  }
  const unit = typeof result.currency === 'string' && result.currency !== 'CNY' ? result.currency : '元';
  return (
    <>
      <dl className="figures">
        {figures.map(([label, value]) => (
          <div key={label} className="figure">
            <dt>{label}</dt>
            <dd>{money(value)}<small>{unit}</small></dd>
          </div>
        ))}
      </dl>
      {typeof result.note === 'string' && <p className="t-caption text-tertiary">{result.note}</p>}
    </>
  );
}
