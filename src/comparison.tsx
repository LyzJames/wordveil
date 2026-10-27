// Document rows and question columns. The core owns cell states, version
// bindings, sequential execution and budgets; polling never starts work.
import React, { useEffect, useState } from 'react';
import { api, snapshot, stageName, type Comparison, type ComparisonCell, type ComparisonPreview, type DocumentRow, type RetrievalResult } from './api';
import { EvidenceList } from './evidence';
import { ExcerptText } from './egress';
import { Icon } from './icons';
import { sourceOf, useSession } from './session';
import { Busy, Callout, Chip, ErrorLine, Field, Modal, useAction, useLoad } from './ui';
import './comparison.css';

const STATE: Record<string, string> = {
  draft: '尚未准备', pending: '待准备', preparing: '正在本机准备', prepared: '等待确认', queued: '排队中', running: '正在处理',
  done: '已完成', completed: '已完成', no_evidence: '未找到依据', failed: '未完成', interrupted: '已中断', budget: '额度已用完',
  paused: '已暂停', partial: '部分完成', stale: '资料已变化',stopping:'正在停止',
};
const label = (state: string) => STATE[state] ?? state;
const finished = (cell: ComparisonCell) => cell.state === 'done' || cell.state === 'no_evidence';

export function ComparisonPage({ initialId }: { initialId?: string }) {
  const { connection, go, refresh } = useSession();
  const history = useLoad(api.comparisonList, []);
  const [id, setId] = useState(initialId ?? '');
  const [batch, setBatch] = useState<Comparison>();
  const [error, setError] = useState('');
  const [creating, setCreating] = useState(false);
  const [preview, setPreview] = useState<ComparisonPreview | null>(null);
  const [opened, setOpened] = useState<ComparisonCell | null>(null);
  const [budget, setBudget] = useState<number | null>(null);
  const { busy, run } = useAction();
  useEffect(() => { if (batch?.state) history.reload(); }, [batch?.state, history.reload]);
  useEffect(() => {
    setBatch(undefined); setError('');
    if (!id) return;
    let alive = true; let timer: ReturnType<typeof setTimeout>;
    async function load() {
      try { const next = await api.comparisonRead(id); if (alive) { setBatch(next); setError(''); timer = setTimeout(load, next.state === 'running' || next.state === 'preparing' ? 1000 : 3000); } }
      catch (e) { if (alive) { setError(String(e)); timer = setTimeout(load, 3000); } }
    }
    load(); return () => { alive = false; clearTimeout(timer); };
  }, [id]);
  const active = batch?.state === 'running' || batch?.state === 'preparing';
  const complete = batch?.cells.filter(finished).length ?? 0;
  async function showPreview(selected: string[] = []) {
    if (!id) return;
    const data = await run(() => api.comparisonPreview(id, selected)); if (data) setPreview(data);
  }
  return (
    <div className="page comparison-page">
      <header className="page-head"><div><h1>资料对照表</h1><p className="text-secondary">把同样的问题逐份对照。每格固定一份资料版本，回答和原文引用分别保留。</p></div><button className="btn btn-primary" onClick={() => setCreating(true)} disabled={!connection}><Icon name="add" />新建对照表</button></header>
      {!connection && <Callout title="先连接生成接口" action={<button className="btn btn-sm" onClick={() => go({ name: 'connections' })}>配置接口</button>}>整张表会先在本机准备，再预览资料和整批预算；系统确认后才开始顺序生成。</Callout>}
      <ErrorLine text={error || history.error} />
      <Field label="打开已保存的对照表"><select value={id} onChange={(e) => { setId(e.target.value); setOpened(null); }}><option value="">选择对照表…</option>{history.data?.map((item) => <option key={item.id} value={item.id}>{item.title} · {label(item.state)}</option>)}</select></Field>
      {!id && <div className="empty"><Icon name="list" size="xl" /><p>最多选择 8 份资料、4 个问题。逐格进度和中断位置会保存在当前私密空间。</p></div>}
      {id && !batch && !error && <Busy label="正在读取对照表…" />}
      {batch && <>
        <div className="matrix-toolbar"><div><h2>{batch.title}</h2><div role="status" aria-live="polite">{label(batch.state)} · 已完成 {complete}/{batch.cells.length} 格 · 请求已使用 {batch.used_requests}/{batch.max_requests} 次</div></div><div className="actions">
          {active ? <button className="btn" disabled={busy} onClick={() => run(() => api.comparisonPause(id), '已请求暂停')}>暂停</button> : <>
            <button className="btn" disabled={busy || batch.state === 'stale' || complete === batch.cells.length} onClick={() => run(() => api.comparisonPrepare(id), '开始本机准备')}>准备未开始的格子</button>
            <button className="btn btn-primary" disabled={busy || batch.remaining_requests <= 0 || complete === batch.cells.length || batch.state === 'stale'} onClick={() => showPreview()}>预览并继续未完成格</button>
            <button className="btn" disabled={busy || batch.state === 'stale' || complete === batch.cells.length} onClick={() => setBudget(batch.max_requests)}>调整请求预算</button>
          </>}
        </div></div>
        {batch.state === 'stale' && <Callout title="资料版本已变化">本表已停止调用。已完成的结果仍须结合原资料核对；请用需要的资料版本新建对照表。</Callout>}
        {batch.remaining_requests <= 0 && <Callout title="整批请求预算已用完">已完成的格子已保留。可调整整批上限后再确认继续；每格累计仍最多 7 次，重试不会清零。</Callout>}
        <div className="matrix-scroll" role="region" aria-label={`${batch.title}，可横向滚动`} tabIndex={0}>
          <table className="matrix"><caption>{batch.title}：文档为行，问题为列；每格单独显示进度、回答和引用。</caption><thead><tr><th scope="col">固定资料版本</th>{batch.spec.questions.map((q, i) => <th scope="col" key={i}><span className="text-tertiary">问题 {i + 1}</span><div>{q}</div></th>)}</tr></thead>
            <tbody>{batch.spec.documents.map((doc, row) => <tr key={doc.version_id}><th scope="row"><div>{doc.display_name}</div><div className="t-caption text-secondary">第 {doc.version_no} 版</div><button className="btn btn-xs" onClick={() => go({ name: 'matter', id: doc.matter_id, tab: 'documents' })}>所属事务</button></th>{batch.spec.questions.map((_, col) => {
              const cell = batch.cells.find((c) => c.row === row && c.col === col);
              return <td key={col}>{cell && <MatrixCell cell={cell} disabled={busy || !!active || batch.remaining_requests <= 0} onOpen={() => setOpened(cell)} onResume={() => cell.attempts > 0 ? showPreview([cell.id]) : run(() => api.comparisonPrepare(id), '重新准备未开始的格子')} />}</td>;
            })}</tr>)}</tbody>
          </table>
        </div>
        <p className="t-caption text-secondary matrix-note">每格只使用对应行的固定版本；未找到依据会明确留空。失败、格式修复和重试均计入已用请求，完成格不会随批次恢复重复生成。</p>
      </>}
      {creating && <NewComparison onClose={() => setCreating(false)} onCreated={(next) => { setCreating(false); setId(next); history.reload(); refresh(); }} />}
      {preview && <BatchPreview preview={preview} onCancel={() => setPreview(null)} onConfirm={async (cells) => { const result = await run(() => api.comparisonStart(preview.id, cells, preview.binding), '已开始顺序处理所选格子'); if (result !== undefined) { setPreview(null); history.reload(); } }} />}
      {opened && batch && <CellDetail cell={opened} document={batch.spec.documents[opened.row].display_name} question={batch.spec.questions[opened.col]} onClose={() => setOpened(null)} />}
      {budget !== null && batch && <Modal title="调整整批请求预算" onClose={() => setBudget(null)} footer={<><button className="btn" onClick={() => setBudget(null)}>取消</button><button className="btn btn-primary" disabled={busy || budget <= batch.used_requests || budget > batch.cells.length * 7} onClick={async () => { const result = await run(() => api.comparisonBudget(id, budget), '预算已更新，继续前仍需系统确认'); if (result !== undefined) setBudget(null); }}>保存预算</button></>}><Field label={`累计请求上限（已用 ${batch.used_requests} 次，不会清零）`}><input type="number" min={batch.used_requests + 1} max={batch.cells.length * 7} value={budget} onChange={(e) => setBudget(Number(e.target.value))} /></Field><p>每格累计上限为 7 次。此处只调整预算，保存后不会自动发送。</p></Modal>}
    </div>
  );
}

function NewComparison({ onClose, onCreated }: { onClose: () => void; onCreated: (id: string) => void }) {
  const { matters, connection } = useSession();
  const [matter, setMatter] = useState(matters[0]?.id ?? '');
  const docs = useLoad(() => matter ? snapshot(matter).then((s) => s.documents) : Promise.resolve([]), [matter]);
  const [selected, setSelected] = useState<DocumentRow[]>([]);
  const [questions, setQuestions] = useState(['约定的金额和履行期限是什么？']);
  const [title, setTitle] = useState('资料对照');
  const [ceiling, setCeiling] = useState<number | null>(null);
  const [started, setStarted] = useState(false);
  const { busy, run } = useAction();
  const validQuestions = questions.map((q) => q.trim()).filter(Boolean);
  const maximum = selected.length * validQuestions.length * 7;
  const budget = ceiling ?? maximum;
  function toggle(doc: DocumentRow) { setSelected((items) => items.some((d) => d.version_id === doc.version_id) ? items.filter((d) => d.version_id !== doc.version_id) : items.length < 8 ? [...items, doc] : items); setCeiling(null); }
  return <Modal title="新建资料对照表" wide onClose={busy ? () => {} : onClose} footer={<><button className="btn" disabled={busy} onClick={onClose}>取消</button><button className="btn btn-primary" disabled={busy || started || !title.trim() || !selected.length || !validQuestions.length || budget < 1 || budget > maximum || !connection} onClick={async () => {
    const id = await run(() => api.comparisonCreate(title, selected.map((d) => d.version_id), validQuestions, connection?.id ?? null, budget));
    if (id) { setStarted(true); await run(() => api.comparisonPrepare(id)); onCreated(id); }
  }}>{busy ? '正在保存…' : '保存并在本机准备'}</button></>}>
    <Field label="对照表名称"><input maxLength={120} value={title} onChange={(e) => setTitle(e.target.value)} /></Field>
    <Field label="从事务中选择资料（最多 8 份）"><select value={matter} onChange={(e) => setMatter(e.target.value)}>{matters.map((m) => <option value={m.id} key={m.id}>{m.title}</option>)}</select></Field>
    <ErrorLine text={docs.error} />{docs.loading ? <Busy label="正在读取资料…" /> : <div className="matrix-picker">{docs.data?.map((doc) => <label key={doc.version_id} className="matrix-pick"><input type="checkbox" checked={selected.some((d) => d.version_id === doc.version_id)} disabled={doc.status !== 'ready' && doc.status !== 'partial'} onChange={() => toggle(doc)} /><span>{doc.display_name} · 第 {doc.version_no} 版 <span className="text-tertiary">{doc.status === 'ready' ? '' : '（未完全就绪）'}</span></span></label>)}{!docs.data?.length && <p>该事务暂无资料，请先导入并完成索引。</p>}</div>}
    {selected.length > 0 && <div className="matrix-selected">{selected.map((d) => <button key={d.version_id} className="btn btn-xs" onClick={() => toggle(d)} aria-label={`移除 ${d.display_name}`}>{d.display_name} · v{d.version_no} ×</button>)}</div>}
    <div className="matrix-questions">{questions.map((q, i) => <div key={i}><Field label={`问题 ${i + 1}`}><textarea rows={2} maxLength={2000} value={q} onChange={(e) => { setQuestions((items) => items.map((text, j) => j === i ? e.target.value : text)); setCeiling(null); }} /></Field>{questions.length > 1 && <button className="btn btn-xs" onClick={() => { setQuestions((items) => items.filter((_, j) => j !== i)); setCeiling(null); }}>移除此问题</button>}</div>)}<button className="btn btn-sm" disabled={questions.length >= 4} onClick={() => setQuestions((items) => [...items, ''])}>添加问题（最多 4 个）</button></div>
    <Field label={`整批累计请求预算（${selected.length} 份 × ${validQuestions.length} 个问题，每格最多 7 次）`}><input type="number" min={1} max={maximum || 1} value={budget} onChange={(e) => setCeiling(Number(e.target.value))} /></Field>
    <Callout title="此步只在本机准备">随后可逐格查看将发送的原文摘录、接收方和预算；系统确认后才会调用接口。所有结果保存在当前私密空间。</Callout>
  </Modal>;
}

function MatrixCell({ cell, disabled, onOpen, onResume }: { cell: ComparisonCell; disabled: boolean; onOpen: () => void; onResume: () => unknown }) {
  const data = useLoad(() => cell.result_ref ? api.result<RetrievalResult>(cell.result_ref) : Promise.resolve(null), [cell.result_ref]);
  const claims = data.data?.answer?.sections?.conclusion ?? [];
  const text = claims.map((c) => c.text).join(' ');
  const citations = data.data?.items.length ?? 0;
  const recoverable = ['failed', 'interrupted', 'budget'].includes(cell.state);
  return <div className="matrix-cell"><div className="matrix-cell-state"><Chip tone={cell.state === 'done' ? 'ok' : recoverable ? 'outline' : 'info'}>{label(cell.state)}</Chip><span className="t-caption text-tertiary">{cell.used_attempts}/7 次额度</span></div>
    {(cell.state === 'running' || cell.state === 'preparing') && <div role="status"><span className="spinner" /> {cell.progress ? stageName(cell.progress.stage) : cell.state === 'preparing' ? '本机检索与原文定位' : '等待处理进度'}{cell.progress && cell.progress.total > 0 ? ` ${cell.progress.done}/${cell.progress.total}` : ''}</div>}
    {text && <p className="matrix-answer">{text.slice(0, 260)}{text.length > 260 ? '…' : ''}</p>}
    {cell.state === 'no_evidence' && <p>这份资料未找到回答该问题的依据。</p>}
    {recoverable && <p className="t-caption text-secondary">{cell.reason_message || '任务未完成，请查看处理过程。'}</p>}
    {recoverable && <p className="t-caption text-secondary">已保留本格进度和请求计数。恢复只处理本格。</p>}
    <ErrorLine text={data.error} />
    <div className="actions">{cell.result_ref && <button className="btn btn-xs" onClick={onOpen}>{text ? '查看回答与引用' : '查看已找到的原文'}（{citations}）</button>}{recoverable && <button className="btn btn-xs" disabled={disabled || cell.remaining_requests <= 0} onClick={onResume}>{cell.attempts ? '恢复此格' : '重新准备'}</button>}</div>
    {cell.remaining_requests <= 0 && recoverable && <p className="t-caption text-secondary">已达到此格的累计请求上限。</p>}
  </div>;
}

function BatchPreview({ preview, onCancel, onConfirm }: { preview: ComparisonPreview; onCancel: () => void; onConfirm: (cells: string[]) => Promise<void> }) {
  const [chosen, setChosen] = useState(new Set(preview.cells.map((c) => c.id)));
  const [busy, setBusy] = useState(false);
  return <Modal title="对照表整批发送预览" wide onClose={busy ? () => {} : onCancel} footer={<><span className="t-caption text-secondary">已选 {chosen.size} 格 · 剩余 {preview.remaining_requests} 次</span><button className="btn" disabled={busy} onClick={onCancel}>取消</button><button className="btn btn-primary" disabled={busy || !chosen.size} onClick={async () => { setBusy(true); try { await onConfirm([...chosen]); } finally { setBusy(false); } }}>{busy ? '等待系统确认…' : '继续，打开系统确认'}</button></>}>
    <Callout title="一次确认整批预算，顺序生成所选格子">已完成格不会重复生成。暂停、锁定或失败后，可单独恢复未完成格；保留原计数。</Callout>
    {preview.cells.some((c)=>c.unknown_requests>0)&&<Callout tone="warning" title="有上次请求的结果尚不明确">服务商可能已经处理这些请求，已计入原预算。本次确认会允许在剩余额度内继续所选格，可能重新发送未完成步骤。</Callout>}
    <dl className="kv"><dt>接收方</dt><dd>{preview.connection.name}（{preview.connection.host}）· {preview.connection.model_id}</dd><dt>累计请求上限</dt><dd>{preview.max_requests} 次；已用 {preview.used_requests} 次，剩余 {preview.remaining_requests} 次；每格累计最多 {preview.per_cell_max_requests} 次</dd><dt>输出上限</dt><dd>每次最多 {preview.max_output_tokens.toLocaleString('zh-CN')} 令牌；失败、格式修复和重试均消耗请求额度</dd><dt>资料范围</dt><dd>每格仅使用对应行的固定版本，单次生成最多 10 段原文；不联网搜索。下列为本机准备的摘录，后续仍只在该固定版本内检索，实际发送材料可在请求记录中核对。</dd></dl>
    <div className="batch-preview-cells">{preview.cells.map((cell) => <section key={cell.id}><label className="matrix-pick"><input type="checkbox" checked={chosen.has(cell.id)} onChange={() => setChosen((items) => { const next = new Set(items); if (next.has(cell.id)) next.delete(cell.id); else next.add(cell.id); return next; })} /><span><strong>{preview.spec.documents[cell.row].display_name} · 第 {preview.spec.documents[cell.row].version_no} 版</strong><br />问题 {cell.col + 1}：{preview.spec.questions[cell.col]}</span></label><p className="t-caption text-secondary">{cell.continuation ? '恢复此格' : '尚未调用生成接口'} · 已用 {cell.used_requests} 次，单格剩余 {cell.remaining_requests} 次</p><details><summary>查看本机准备的原文摘录（{cell.preview.excerpts.length} 段）</summary>{cell.preview.excerpts.map((item) => <blockquote key={item.evidence_id}><span className="t-caption text-secondary">{item.display_name} · {item.unit_title}</span><ExcerptText excerpt={item} /></blockquote>)}</details></section>)}</div>
  </Modal>;
}

function CellDetail({ cell, document, question, onClose }: { cell: ComparisonCell; document: string; question: string; onClose: () => void }) {
  const { open } = useSession();
  const result = useLoad(() => cell.result_ref ? api.result<RetrievalResult>(cell.result_ref) : Promise.resolve(null), [cell.result_ref]);
  const data = result.data;
  return <Modal title={`${document} · 回答与引用`} wide onClose={onClose} footer={<button className="btn" onClick={onClose}>关闭</button>}><p className="t-strong">{question}</p><ErrorLine text={result.error} />{result.loading && <Busy label="正在读取结果…" />}{data && <>
    {data.answer ? <div className="matrix-detail-answer">{(['conclusion', 'basis', 'calculations', 'next_steps'] as const).map((section) => <section key={section}>{data.answer?.sections[section]?.map((claim) => <p key={claim.claim_id}>{claim.text} {claim.evidence.map((cite) => <span className="citation" key={cite.evidence_id}>[{cite.label}]</span>)} {claim.status !== 'supported' && <Chip tone="outline">{claim.status === 'unsupported' ? '依据未支持' : '未核实'}</Chip>}</p>)}</section>)}<p className="t-caption text-secondary">{data.answer.verification_note}</p></div> : <Callout title="尚未生成可交付的回答">以下保留已找到的原文，不将它们视为已核实的结论。</Callout>}
    <h3>本格原文依据</h3><EvidenceList items={data.items} labels={Object.fromEntries(data.answer?.labels.map((l) => [l.evidence_id, l.label]) ?? [])} onOpen={(item) => { onClose(); open(sourceOf(item)); }} />
  </>}</Modal>;
}
