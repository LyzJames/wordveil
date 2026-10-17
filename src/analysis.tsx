// Analysis entries stay available without a generation connection: they run
// the local evidence and calculation steps and say plainly that the written
// analysis needs a generation endpoint (M2).
import React, { useState } from 'react';
import { api, type EvidenceItem, type Snapshot } from './api';
import { useSession } from './main';
import { EvidenceList } from './search';
import { Busy, ErrorLine, Field, Modal, useAction } from './ui';

type Workflow = 'contract-review' | 'loan-analysis' | 'budget-analysis';
const TITLES: Record<Workflow, string> = { 'contract-review': '合同阅读', 'loan-analysis': '借款核对', 'budget-analysis': '预算分析' };
const QUESTIONS: Record<Workflow, string> = {
  'contract-review': '这份合同的押金、租金、期限和违约责任是怎样约定的？',
  'loan-analysis': '借款金额、已还款和剩余欠款是多少？还款期限是什么时候？',
  'budget-analysis': '这段时间的收支情况如何？有哪些异常或固定支出？',
};

export function AnalysisButton({ matter, snapshot }: { matter: string; snapshot: Snapshot }) {
  const [workflow, setWorkflow] = useState<Workflow | null>(null);
  const suggested: Workflow = snapshot.matter.matter_type === 'loan' ? 'loan-analysis' : snapshot.matter.matter_type === 'household' ? 'budget-analysis' : 'contract-review';
  return (
    <>
      <select aria-label="分析" value="" onChange={(e) => e.target.value && setWorkflow(e.target.value as Workflow)} className="select-button">
        <option value="">分析…</option>
        {(['contract-review', 'loan-analysis', 'budget-analysis'] as Workflow[]).map((w) => (
          <option key={w} value={w}>{TITLES[w]}{w === suggested ? '（推荐）' : ''}</option>
        ))}
      </select>
      {workflow && <AnalysisModal workflow={workflow} matter={matter} snapshot={snapshot} onClose={() => setWorkflow(null)} />}
    </>
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
    <Modal title={TITLES[workflow]} onClose={onClose} wide>
      <div className="stack">
        <Field label="你想了解什么"><textarea rows={2} value={question} onChange={(e) => setQuestion(e.target.value)} maxLength={2000} /></Field>
        {workflow !== 'loan-analysis' || documents.length > 0 ? (
          <fieldset className="pick">
            <legend>使用的资料</legend>
            {documents.length === 0 && <p className="muted small">本事务还没有文档资料。</p>}
            {documents.map((d) => (
              <label key={d.version_id} className="check"><input type="checkbox" checked={versions.has(d.version_id)} onChange={() => setVersions((s) => { const n = new Set(s); n.has(d.version_id) ? n.delete(d.version_id) : n.add(d.version_id); return n; })} />{d.display_name}（第 {d.version_no} 版）</label>
            ))}
          </fieldset>
        ) : null}
        {workflow !== 'budget-analysis' && media.length > 0 && (
          <fieldset className="pick">
            <legend>同时转写并检索的录音/视频（本机 ASR 与抽帧 OCR）</legend>
            {media.map((d) => (
              <label key={d.media_id!} className="check"><input type="checkbox" checked={mediaRefs.has(d.media_id!)} onChange={() => setMediaRefs((s) => { const n = new Set(s); n.has(d.media_id!) ? n.delete(d.media_id!) : n.add(d.media_id!); return n; })} />{d.display_name}</label>
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
        <div className="actions end">
          <button onClick={onClose}>关闭</button>
          <button className="primary" disabled={busy || (workflow !== 'loan-analysis' && versions.size === 0 && mediaRefs.size === 0)} onClick={start}>{busy ? '正在本机检索与计算…' : '开始'}</button>
        </div>
        {busy && <Busy label="正在本机检索原文、重排并核对计算（首次加载模型可能需要一分钟）…" />}
        {outcome && (
          <div className="analysis-result">
            <div className="callout">
              <strong>分析结论需要生成接口</strong>
              <p>{outcome.message || '当前版本未连接生成接口，不生成分析答案。'}以下是本机找到的原文依据{outcome.calculation ? '与确定性计算结果' : ''}，可逐条回到原文核对。</p>
            </div>
            {outcome.calculation && (
              <details open>
                <summary>计算结果</summary>
                <pre className="preview small">{JSON.stringify(outcome.calculation, null, 2)}</pre>
              </details>
            )}
            <h3>原文依据（{outcome.items.length}）</h3>
            <EvidenceList items={outcome.items} onOpen={(item) => open({ version: item.version_id, title: item.document.display_name, page: item.pages[0], highlight: item.highlight_indexes.map((i) => item.locators[i]).filter(Boolean) })} />
          </div>
        )}
      </div>
    </Modal>
  );
}
