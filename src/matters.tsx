// Matters: list, overview tabs, documents and candidate facts.
import React, { useEffect, useMemo, useRef, useState } from 'react';
import {
  api, applyChange, bytes, commitChange, day, ensure, FACT_TYPES, MATTER_TYPES, prepareChange, RELATION_TYPES, snapshot, statusLabel, versionStatus,
  type DocumentRow, type Fact, type Snapshot,
} from './api';
import { AnalysisButton } from './analysis';
import { ExportButton } from './export';
import { useSession } from './main';
import { CalcTab, CashflowTab } from './money';
import { Busy, Chip, Empty, ErrorLine, Field, Modal, Tabs, useAction, useLoad } from './ui';
import { MatterCard, NewMatter } from './workbench';

export function MatterList() {
  const { info, go, revision, refresh } = useSession();
  const root = useLoad(() => snapshot(info.root_matter), [info.root_matter, revision]);
  const [creating, setCreating] = useState(false);
  const matters = root.data?.matters ?? [];
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <p className="eyebrow">我的事务</p>
          <h1>全部事务</h1>
        </div>
        <button className="primary" onClick={() => setCreating(true)}>新建事务</button>
      </header>
      {root.loading && !root.data && <Busy />}
      <ErrorLine text={root.error} />
      {matters.length === 0 && !root.loading ? (
        <Empty title="还没有事务">事务用来归集同一件事的资料、事实、金额记录与待办。</Empty>
      ) : (
        <div className="cards">
          {matters.map((m) => (
            <MatterCard key={m.id} matter={m} onOpen={() => go({ name: 'matter', id: m.id })} />
          ))}
        </div>
      )}
      {creating && <NewMatter root={info.root_matter} onClose={() => setCreating(false)} onCreated={(id) => { setCreating(false); refresh(); go({ name: 'matter', id }); }} />}
    </div>
  );
}

type Tab = 'documents' | 'facts' | 'cashflows' | 'calc' | 'timeline' | 'relations';

export function MatterPage({ id, initialTab }: { id: string; initialTab?: string }) {
  const { revision, go } = useSession();
  const [tab, setTab] = useState<Tab>((initialTab as Tab) ?? 'documents');
  const data = useLoad(() => snapshot(id), [id, revision]);
  const s = data.data;
  if (!s) return <div className="page">{data.error ? <ErrorLine text={data.error} /> : <Busy />}</div>;
  const candidates = s.facts.filter((f) => f.status === 'candidate').length;
  const pendingCash = s.cashflows.filter((c) => c.status === 'candidate').length;
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <p className="eyebrow">
            <button className="link" onClick={() => go({ name: 'matters' })}>我的事务</button> / {MATTER_TYPES[s.matter.matter_type] ?? s.matter.matter_type}
          </p>
          <h1>{s.matter.title}</h1>
        </div>
        <div className="actions">
          <button onClick={() => go({ name: 'search', matters: [id] })}>检索本事务</button>
          <AnalysisButton matter={id} snapshot={s} />
          <ExportButton resultRefs={async () => [ensure(await api.run('matter-manage', { matter_id: id, operation: 'read' })).data_ref!]} name={s.matter.title} label="导出事务" />
        </div>
      </header>
      <Tabs<Tab>
        value={tab}
        onChange={setTab}
        items={[
          ['documents', '资料', s.documents.length],
          ['facts', '候选与事实', candidates || undefined],
          ['cashflows', '金额记录', pendingCash || undefined],
          ['calc', '计算'],
          ['timeline', '时间线与待办', s.todos.filter((t) => t.status === 'open').length || undefined],
          ['relations', '关系', s.relations.length || undefined],
        ]}
      />
      <div className="tab-body">
        {tab === 'documents' && <DocumentsTab matter={id} snapshot={s} />}
        {tab === 'facts' && <FactsTab matter={id} snapshot={s} />}
        {tab === 'cashflows' && <CashflowTab matter={id} snapshot={s} />}
        {tab === 'calc' && <CalcTab matter={id} snapshot={s} />}
        {tab === 'timeline' && <TimelineTab matter={id} snapshot={s} />}
        {tab === 'relations' && <RelationsTab matter={id} snapshot={s} />}
      </div>
    </div>
  );
}

const KIND_LABEL: Record<string, string> = { document: '文档', audio: '音频', video: '视频' };

function DocumentsTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { open, refresh, tasks } = useSession();
  const { busy, run } = useAction();
  const [deleting, setDeleting] = useState<{ doc: DocumentRow; ref: string; impact: Record<string, unknown> } | null>(null);
  const pendingIndex = useRef(new Map<string, string>());

  // A finished transcription is followed by indexing so the text becomes searchable.
  useEffect(() => {
    for (const [task, version] of [...pendingIndex.current]) {
      const state = tasks.find((t) => t.task_id === task);
      if (state && state.status !== 'running') {
        pendingIndex.current.delete(task);
        if (state.status === 'completed' || state.status === 'partial') {
          run(() => api.start('knowledge-index', { document_versions: [version], operation: 'build' }), '转写完成，正在建立索引');
        }
      }
    }
  }, [tasks, run]);

  async function transcribe(doc: DocumentRow) {
    await run(async () => {
      const range = await api.object('range', { media: doc.media_id, start_s: 0 });
      const question = await api.object('question', { text: `识别《${doc.display_name}》中的语音与画面文字` });
      const skill = doc.media_kind === 'video' ? 'video-review' : 'audio-review';
      const task = await api.start(skill, { source_ref: doc.media_id, range_ref: range, question_id: question, analysis_goal: 'text_only' });
      if (skill === 'audio-review') pendingIndex.current.set(task, doc.version_id);
    }, doc.media_kind === 'video' ? '已开始抽帧识别与转写' : '已开始转写');
  }

  async function correct(doc: DocumentRow) {
    await run(async () => {
      const pages = await api.view<{ pages: { page_id: string; source: string }[] }>('version', doc.version_id);
      const ids = pages.pages.filter((p) => p.source === 'scan' || p.source === 'image').map((p) => p.page_id);
      if (!ids.length) throw new Error('该资料没有扫描页或图片页');
      await api.start('document-ocr', { document_version: doc.version_id, page_ids: ids, purpose: 'correct' });
      await api.start('knowledge-index', { document_versions: [doc.version_id], operation: 'build' });
    }, '已开始矫正弯曲并重新识别');
  }

  async function askDelete(doc: DocumentRow) {
    const prepared = await run(() => prepareChange(matter, { type: 'delete_document_version', version_id: doc.version_id }));
    if (prepared) setDeleting({ doc, ...prepared });
  }

  return (
    <section>
      <div className="row-between toolbar">
        <p className="muted small">原件、页面图、识别文字与索引都加密保存在本机。导入后自动解析、识别并建立检索索引。</p>
        <div className="actions">
          <button className="primary" disabled={busy} onClick={() => run(async () => { const started = await api.importFiles(matter, 'copy'); if (started.length) refresh(); return started; }, '已开始导入')}>导入资料</button>
          <button disabled={busy} onClick={() => run(() => api.importFiles(matter, 'new_version'), '已开始导入新版本')} title="选择与已有资料同名的文件，作为该资料的新版本">导入新版本</button>
        </div>
      </div>
      {s.documents.length === 0 ? (
        <Empty title="还没有资料">支持 PDF、Word（docx）、Excel（xlsx）、CSV/文本、图片与截图、录音和视频。</Empty>
      ) : (
        <table className="table">
          <thead>
            <tr><th>资料</th><th>版本</th><th>类型</th><th>状态</th><th className="num">页数</th><th className="num">大小</th><th>导入日期</th><th aria-label="操作" /></tr>
          </thead>
          <tbody>
            {s.documents.map((doc) => (
              <tr key={doc.version_id}>
                <td className="name-cell" title={doc.display_name}>{doc.display_name}</td>
                <td>第 {doc.version_no} 版</td>
                <td>{KIND_LABEL[doc.media_kind] ?? doc.media_kind}</td>
                <td><Chip tone={doc.status === 'ready' ? 'ok' : doc.status === 'partial' || doc.status === 'interrupted' ? 'warn' : doc.status === 'failed' ? 'danger' : 'neutral'}>{versionStatus(doc.status)}</Chip></td>
                <td className="num">{doc.pages || '—'}</td>
                <td className="num">{bytes(doc.bytes)}</td>
                <td>{day(doc.created_at)}</td>
                <td className="row-actions">
                  <button className="small" onClick={() => open({ version: doc.version_id, title: doc.display_name, media: doc.media_kind !== 'document' })}>查看</button>
                  {doc.media_id && <button className="small" disabled={busy} onClick={() => transcribe(doc)}>{doc.media_kind === 'video' ? '识别画面与语音' : '转写'}</button>}
                  {doc.media_kind === 'audio' && <button className="small" disabled={busy} onClick={() => run(() => api.start('knowledge-index', { document_versions: [doc.version_id], operation: 'build' }), '已开始建立索引')} title="把转写文字加入检索索引">建立索引</button>}
                  {doc.media_kind === 'document' && doc.pages > 0 && <button className="small" disabled={busy} onClick={() => correct(doc)} title="对扫描页或照片做弯曲矫正后重新识别">矫正重识别</button>}
                  <button className="small danger" disabled={busy} onClick={() => askDelete(doc)}>删除</button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {deleting && (
        <Modal title="删除资料版本" onClose={() => setDeleting(null)}>
          <p>将删除《{deleting.doc.display_name}》第 {deleting.doc.version_no} 版及其全部派生内容：</p>
          <ul className="plain">
            {(['pages', 'blocks', 'chunks', 'facts', 'confirmed_facts', 'cashflows', 'media'] as const).map((key) => (
              <li key={key} className="row-between"><span>{({ pages: '页面', blocks: '文字块', chunks: '检索片段', facts: '事实', confirmed_facts: '其中已确认事实', cashflows: '金额记录', media: '音视频记录' })[key]}</span><strong>{String(deleting.impact[key] ?? 0)}</strong></li>
            ))}
          </ul>
          <p className="muted small">{String(deleting.impact.note ?? '')}</p>
          <div className="actions end">
            <button onClick={() => setDeleting(null)}>取消</button>
            <button className="danger" disabled={busy} onClick={async () => { const done = await run(() => commitChange(matter, deleting.ref)); if (done !== undefined) { setDeleting(null); if (done) refresh(); } }}>继续删除…</button>
          </div>
        </Modal>
      )}
    </section>
  );
}

function factValue(f: Fact): string {
  if (f.fact_type === 'amount') return `${f.value.amount ?? ''} ${f.value.currency && f.value.currency !== 'CNY' ? f.value.currency : '元'}`;
  if (f.fact_type === 'date') return f.value.date ?? '';
  return f.value.text ?? '';
}

function FactsTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { open, refresh } = useSession();
  const { busy, run } = useAction();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [editing, setEditing] = useState<Fact | null>(null);
  const [adding, setAdding] = useState(false);
  const documents = useMemo(() => new Map(s.documents.map((d) => [d.version_id, d])), [s.documents]);
  const groups = { candidate: s.facts.filter((f) => f.status === 'candidate'), confirmed: s.facts.filter((f) => f.status === 'confirmed'), rejected: s.facts.filter((f) => f.status === 'rejected') };
  const toggle = (id: string) => setSelected((set) => { const next = new Set(set); next.has(id) ? next.delete(id) : next.add(id); return next; });
  async function batch(type: 'confirm_facts' | 'reject_facts') {
    const done = await run(() => applyChange(matter, { type, fact_ids: [...selected] }), type === 'confirm_facts' ? '已确认' : '已排除');
    if (done) { setSelected(new Set()); refresh(); }
  }
  const source = (f: Fact) => {
    const doc = f.source_version ? documents.get(f.source_version) : undefined;
    if (!doc) return <span className="muted">{f.origin === 'user' ? '手动添加' : '—'}</span>;
    const locator = f.source?.block_locator;
    return (
      <button className="link" onClick={() => open({ version: doc.version_id, title: doc.display_name, page: f.source?.page_id ?? undefined, highlight: locator ? [locator] : undefined })}>
        {doc.display_name}{locator?.page_no ? ` · 第 ${locator.page_no} 页` : ''}
      </button>
    );
  };
  const table = (rows: Fact[], selectable: boolean) => (
    <table className="table">
      <thead><tr>{selectable && <th className="check-cell"><input type="checkbox" aria-label="全选" checked={rows.length > 0 && rows.every((r) => selected.has(r.id))} onChange={(e) => setSelected(e.target.checked ? new Set(rows.map((r) => r.id)) : new Set())} /></th>}<th>类型</th><th>名称</th><th>内容</th><th>来源</th><th>状态</th><th aria-label="操作" /></tr></thead>
      <tbody>
        {rows.map((f) => (
          <tr key={f.id}>
            {selectable && <td className="check-cell"><input type="checkbox" aria-label="选择" checked={selected.has(f.id)} onChange={() => toggle(f.id)} /></td>}
            <td>{FACT_TYPES[f.fact_type] ?? f.fact_type}</td>
            <td>{f.label && f.origin !== 'rule_extracted' ? f.label : <span className="muted">{f.label}</span>}</td>
            <td className={f.fact_type === 'amount' ? 'num' : ''}>{factValue(f)}</td>
            <td>{source(f)}</td>
            <td><Chip tone={f.status === 'confirmed' ? 'ok' : f.status === 'rejected' ? 'neutral' : 'warn'}>{f.status === 'candidate' ? '待校对' : f.status === 'confirmed' ? (f.origin === 'rule_corrected' ? '已更正确认' : '已确认') : '已排除'}</Chip></td>
            <td className="row-actions">{f.status !== 'rejected' && (f.fact_type === 'amount' || f.fact_type === 'date') && <button className="small" onClick={() => setEditing(f)}>更正</button>}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
  return (
    <section>
      <div className="row-between toolbar">
        <p className="muted small">候选金额与日期由规则从原文提取，<strong>未经你确认不会被采用</strong>。请对照原文核对后确认或排除。</p>
        <button onClick={() => setAdding(true)}>手动添加事实</button>
      </div>
      <h3>待校对（{groups.candidate.length}）</h3>
      {groups.candidate.length ? (
        <>
          <div className="actions">
            <button className="primary" disabled={busy || !selected.size} onClick={() => batch('confirm_facts')}>确认所选（{selected.size}）</button>
            <button disabled={busy || !selected.size} onClick={() => batch('reject_facts')}>排除所选</button>
          </div>
          {table(groups.candidate, true)}
        </>
      ) : <p className="muted">没有待校对的候选。</p>}
      <h3>已确认（{groups.confirmed.length}）</h3>
      {groups.confirmed.length ? table(groups.confirmed, false) : <p className="muted">尚无已确认的事实。</p>}
      {groups.rejected.length > 0 && (<details><summary>已排除（{groups.rejected.length}）</summary>{table(groups.rejected, false)}</details>)}
      {editing && <EditFact fact={editing} matter={matter} onClose={() => setEditing(null)} onSaved={() => { setEditing(null); refresh(); }} />}
      {adding && <AddFact matter={matter} onClose={() => setAdding(false)} onSaved={() => { setAdding(false); refresh(); }} />}
    </section>
  );
}

function EditFact({ fact, matter, onClose, onSaved }: { fact: Fact; matter: string; onClose: () => void; onSaved: () => void }) {
  const [value, setValue] = useState(fact.fact_type === 'amount' ? fact.value.amount ?? '' : fact.value.date ?? '');
  const [label, setLabel] = useState(fact.label ?? '');
  const { busy, run } = useAction();
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const next = fact.fact_type === 'amount' ? { amount: value.trim(), currency: fact.value.currency ?? 'CNY' } : { date: value.trim() };
    const done = await run(() => applyChange(matter, { type: 'edit_fact', fact_id: fact.id, value: next, label: label.trim() || null }), '已更正并确认');
    if (done) onSaved();
  }
  return (
    <Modal title="更正并确认" onClose={onClose}>
      <form className="stack" onSubmit={submit}>
        <p className="muted small">原文：{fact.label}</p>
        <Field label={fact.fact_type === 'amount' ? '金额（元，最多 6 位小数）' : '日期（YYYY-MM-DD）'}>
          <input value={value} onChange={(e) => setValue(e.target.value)} inputMode={fact.fact_type === 'amount' ? 'decimal' : 'text'} pattern={fact.fact_type === 'amount' ? '\\d{1,15}(\\.\\d{1,6})?' : '\\d{4}-\\d{2}-\\d{2}'} required />
        </Field>
        <Field label="名称（可选）" hint="例如：押金、月租金、起租日"><input value={label} onChange={(e) => setLabel(e.target.value)} maxLength={60} /></Field>
        <div className="actions end"><button type="button" onClick={onClose}>取消</button><button className="primary" disabled={busy}>保存</button></div>
      </form>
    </Modal>
  );
}

function AddFact({ matter, onClose, onSaved }: { matter: string; onClose: () => void; onSaved: () => void }) {
  const [type, setType] = useState('note');
  const [value, setValue] = useState('');
  const [label, setLabel] = useState('');
  const { busy, run } = useAction();
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const v = type === 'amount' ? { amount: value.trim(), currency: 'CNY' } : type === 'date' ? { date: value.trim() } : { text: value.trim() };
    const done = await run(() => applyChange(matter, { type: 'add_fact', fact_type: type, value: v, label: label.trim() || null }), '已添加');
    if (done) onSaved();
  }
  return (
    <Modal title="手动添加事实" onClose={onClose}>
      <form className="stack" onSubmit={submit}>
        <Field label="类型"><select value={type} onChange={(e) => setType(e.target.value)}>{Object.entries(FACT_TYPES).map(([id, l]) => <option key={id} value={id}>{l}</option>)}</select></Field>
        <Field label={type === 'amount' ? '金额（元）' : type === 'date' ? '日期（YYYY-MM-DD）' : '内容'}>
          {type === 'note' || type === 'event' ? <textarea value={value} onChange={(e) => setValue(e.target.value)} rows={3} maxLength={2000} required /> : <input value={value} onChange={(e) => setValue(e.target.value)} required />}
        </Field>
        <Field label="名称（可选）"><input value={label} onChange={(e) => setLabel(e.target.value)} maxLength={60} /></Field>
        <p className="muted small">手动添加的事实直接记为“已确认”，来源标记为你本人。</p>
        <div className="actions end"><button type="button" onClick={onClose}>取消</button><button className="primary" disabled={busy}>添加</button></div>
      </form>
    </Modal>
  );
}

function TimelineTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { refresh } = useSession();
  const { busy, run } = useAction();
  const [title, setTitle] = useState('');
  const [due, setDue] = useState('');
  async function add(e: React.FormEvent) {
    e.preventDefault();
    const done = await run(() => applyChange(matter, { type: 'add_todo', title: title.trim(), due_on: due || null }), '已添加待办');
    if (done) { setTitle(''); setDue(''); refresh(); }
  }
  return (
    <section className="split">
      <div>
        <h3>时间线</h3>
        {s.timeline.length === 0 ? <p className="muted">确认日期事实、金额记录或添加待办后，会按日期排列在这里。</p> : (
          <ol className="timeline">
            {s.timeline.map((t) => (
              <li key={`${t.kind}-${t.ref}`} className={`tl-${t.kind}`}>
                <time>{t.date}</time>
                <span>{t.label}</span>
                <Chip tone={t.status === 'confirmed' || t.status === 'done' ? 'ok' : t.status === 'candidate' ? 'warn' : 'neutral'}>{({ fact: '事实', cashflow: '金额', todo: '待办' } as Record<string, string>)[t.kind]} · {statusLabel(t.status) === t.status ? ({ confirmed: '已确认', candidate: '待校对', open: '未完成', done: '已完成', rejected: '已排除' } as Record<string, string>)[t.status] ?? t.status : statusLabel(t.status)}</Chip>
              </li>
            ))}
          </ol>
        )}
      </div>
      <div>
        <h3>待办</h3>
        <ul className="plain">
          {s.todos.map((t) => (
            <li key={t.id} className="row-between">
              <span className={t.status === 'done' ? 'done' : ''}>{t.title}{t.due_on && <span className="muted small"> · {t.due_on}</span>}</span>
              {t.status === 'open' && <button className="small" disabled={busy} onClick={() => run(async () => { const d = await applyChange(matter, { type: 'complete_todo', todo_id: t.id }); if (d) refresh(); }, '已完成')}>完成</button>}
            </li>
          ))}
        </ul>
        <form className="stack compact" onSubmit={add}>
          <Field label="新待办"><input value={title} onChange={(e) => setTitle(e.target.value)} maxLength={120} required /></Field>
          <Field label="截止日期（可选）"><input type="date" value={due} onChange={(e) => setDue(e.target.value)} /></Field>
          <button disabled={busy || !title.trim()}>添加待办</button>
        </form>
        <p className="muted small">本版本记录待办与日期；到期提醒需要应用保持运行的本机提醒功能，将在后续版本提供。</p>
      </div>
    </section>
  );
}

function RelationsTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { refresh } = useSession();
  const { busy, run } = useAction();
  const nodes = useMemo(() => {
    const list: { id: string; label: string }[] = s.documents.map((d) => ({ id: d.version_id, label: `资料：${d.display_name}（第 ${d.version_no} 版）` }));
    for (const f of s.facts.filter((x) => x.status === 'confirmed')) list.push({ id: f.id, label: `事实：${f.label ? f.label + ' ' : ''}${factValue(f)}` });
    return list;
  }, [s]);
  const name = (id: string) => nodes.find((n) => n.id === id)?.label ?? '（已删除或不在本事务）';
  const [src, setSrc] = useState('');
  const [dst, setDst] = useState('');
  const [type, setType] = useState('cites');
  async function add(e: React.FormEvent) {
    e.preventDefault();
    const done = await run(() => applyChange(matter, { type: 'add_relation', relation_type: type, src, dst }), '已写入关系');
    if (done) refresh();
  }
  return (
    <section>
      <p className="muted small">这里只保存你明确确认的关系（例如“新版本取代旧版本”“该资料是某事实的依据”）。确认候选事实时会自动记录“依据”关系。</p>
      {s.relations.length === 0 ? <p className="muted">还没有关系。</p> : (
        <table className="table">
          <thead><tr><th>起点</th><th>关系</th><th>终点</th><th>来源</th><th aria-label="操作" /></tr></thead>
          <tbody>
            {s.relations.map((r) => (
              <tr key={r.id}>
                <td>{name(r.src)}</td>
                <td><Chip tone="info">{RELATION_TYPES[r.relation_type] ?? r.relation_type} →</Chip></td>
                <td>{name(r.dst)}</td>
                <td className="muted small">{r.origin === 'user_confirmed_candidate' ? '确认候选时记录' : '手动'}</td>
                <td><button className="small" disabled={busy} onClick={() => run(async () => { const d = await applyChange(matter, { type: 'remove_relation', relation_id: r.id }); if (d) refresh(); }, '已移除')}>移除</button></td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <form className="inline-form" onSubmit={add}>
        <Field label="起点"><select value={src} onChange={(e) => setSrc(e.target.value)} required><option value="">选择…</option>{nodes.map((n) => <option key={n.id} value={n.id}>{n.label}</option>)}</select></Field>
        <Field label="关系"><select value={type} onChange={(e) => setType(e.target.value)}>{Object.entries(RELATION_TYPES).map(([id, l]) => <option key={id} value={id}>{l}</option>)}</select></Field>
        <Field label="终点"><select value={dst} onChange={(e) => setDst(e.target.value)} required><option value="">选择…</option>{nodes.map((n) => <option key={n.id} value={n.id}>{n.label}</option>)}</select></Field>
        <button disabled={busy || !src || !dst || src === dst}>写入关系</button>
      </form>
    </section>
  );
}
