// Analysis entries (contract reading, loan reconciliation, budget review).
// They run the local evidence and calculation steps now; the written
// analysis by the generation connection is a later M2 segment, and the
// result says so plainly.
import React, { useRef, useState } from 'react';
import { api, money, type EvidenceItem, type Snapshot } from './api';
import { EvidenceList } from './evidence';
import { Icon } from './icons';
import { sourceOf, useSession } from './session';
import { Busy, Callout, ErrorLine, Field, Modal, useAction, useDismiss } from './ui';

type Workflow = 'contract-review' | 'loan-analysis' | 'budget-analysis';
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
  const { open } = useSession();
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

  return (
    <Modal
      title={TITLES[workflow]}
      icon="list"
      onClose={onClose}
      wide
      footer={
        <>
          <button className="btn" onClick={onClose}>关闭</button>
          <button className="btn btn-primary" disabled={busy || (workflow !== 'loan-analysis' && versions.size === 0 && mediaRefs.size === 0)} onClick={start}>
            {busy ? '正在本机检索与计算…' : '开始'}
          </button>
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
          <Callout title="书面分析将在后续版本由生成接口提供">
            {outcome.message ? `${outcome.message}。` : ''}以下是本机找到的原文依据{outcome.calculation ? '与确定性计算结果' : ''}，可以逐条回到原文核对；也可以在“问答与检索”中就这些资料提问。
          </Callout>
          {outcome.calculation && <CalculationSummary result={outcome.calculation} />}
          <h3 className="t-subtitle">原文依据（{outcome.items.length}）</h3>
          <EvidenceList items={outcome.items} onOpen={(item) => { onClose(); open(sourceOf(item)); }} />
        </div>
      )}
    </Modal>
  );
}

const LABELS: Record<string, string> = {
  total_lent: '借出合计', total_repaid: '已还合计', outstanding: '尚欠', total_received: '收到', total_paid: '付出', finance_cost: '资金成本',
};

function CalculationSummary({ result }: { result: Record<string, unknown> }) {
  const figures = Object.entries(LABELS).filter(([key]) => typeof result[key] === 'string');
  if (!figures.length) {
    return (
      <details>
        <summary>计算结果</summary>
        <pre className="preview">{JSON.stringify(result, null, 2)}</pre>
      </details>
    );
  }
  return (
    <dl className="figures">
      {figures.map(([key, label]) => (
        <div key={key} className="figure">
          <dt>{label}</dt>
          <dd>{money(result[key] as string)}<small>元</small></dd>
        </div>
      ))}
    </dl>
  );
}
