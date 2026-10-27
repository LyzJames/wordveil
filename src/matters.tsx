// Matters (design 9.3): list; matter page with overview, documents, the
// keyboard review queue for candidate facts, money, calculation, timeline
// and relations.
import React, { useEffect, useMemo, useRef, useState } from 'react';
import {
  api, applyChange, bytes, commitChange, ensure, FACT_TYPES, MATTER_TYPES, money, prepareChange, relative, RELATION_TYPES, snapshot, statusLabel, versionStatus,
  type DocumentRow, type Fact, type Snapshot,
} from './api';
import { AnalysisButton } from './analysis';
import { MediaAnalysisButton } from './media';
import { ExportButton } from './export';
import { Icon, type IconName } from './icons';
import { REGIONS } from './regions';
import { CalcTab, CashflowTab } from './money';
import { useSession, type ViewerTarget } from './session';
import { Chip, Empty, ErrorLine, Field, Modal, Skeleton, Tabs, useAction, useLoad } from './ui';
import { MatterCard, MattersEmpty } from './workbench';

export function MatterList() {
  const { go, matters, mattersLoaded, newMatter } = useSession();
  const sorted = [...matters].sort((a, b) => b.updated_at.localeCompare(a.updated_at));
  return (
    <div className="page fade-in">
      <header className="page-head">
        <div className="titles">
          <h1 className="t-page">我的事务</h1>
          <p className="text-secondary">{mattersLoaded ? `${matters.length} 件事务` : ''}</p>
        </div>
        <div className="actions">
          <button className="btn btn-primary" onClick={newMatter}>
            <Icon name="add" />
            新建事务
          </button>
        </div>
      </header>
      {!mattersLoaded && <Skeleton rows={4} />}
      {mattersLoaded && matters.length === 0 ? (
        <MattersEmpty />
      ) : (
        <div className="grid-cards">
          {sorted.map((m) => (
            <MatterCard key={m.id} matter={m} onOpen={() => go({ name: 'matter', id: m.id })} />
          ))}
        </div>
      )}
    </div>
  );
}

type Tab = 'overview' | 'documents' | 'facts' | 'cashflows' | 'calc' | 'timeline' | 'relations' | 'qa';

export function MatterPage({ id, initialTab }: { id: string; initialTab?: string }) {
  const { revision, go, refresh } = useSession();
  const [tab, setTab] = useState<Tab>((initialTab as Tab) ?? 'overview');
  const data = useLoad(() => snapshot(id), [id, revision]);
  const { busy, run } = useAction();
  const s = data.data;

  async function importFiles() {
    const started = await run(() => api.importFiles(id, 'copy'));
    if (started?.length) refresh();
  }
  // Ctrl+I imports into this matter (design §11).
  const importRef = useRef(importFiles);
  importRef.current = importFiles;
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (e.ctrlKey && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'i' && !document.querySelector('.overlay')) {
        e.preventDefault();
        importRef.current();
      }
    };
    window.addEventListener('keydown', key);
    return () => window.removeEventListener('keydown', key);
  }, []);

  if (!s) return <div className="page">{data.error ? <ErrorLine text={data.error} /> : <Skeleton rows={6} />}</div>;
  const candidates = s.facts.filter((f) => f.status === 'candidate').length;
  const pendingCash = s.cashflows.filter((c) => c.status === 'candidate').length;
  const openTodos = s.todos.filter((t) => t.status === 'open').length;
  return (
    <div className="page fade-in">
      <header className="page-head">
        <div className="titles">
          <nav className="breadcrumb" aria-label="位置">
            <button onClick={() => go({ name: 'matters' })}>我的事务</button>
            <Icon name="chevronRight" />
            <span>{MATTER_TYPES[s.matter.matter_type] ?? s.matter.matter_type}</span>
          </nav>
          <div className="title-row">
            <h1 className="t-page">{s.matter.title}</h1>
            <Chip tone={s.matter.status === 'active' ? 'accent' : 'neutral'}>{s.matter.status === 'active' ? '进行中' : statusLabel(s.matter.status)}</Chip>
          </div>
        </div>
        <div className="actions">
          <button className="btn btn-primary" disabled={busy} onClick={importFiles} title="Ctrl+I">
            <Icon name="import" />
            导入资料
          </button>
          <button className="btn" onClick={() => go({ name: 'qa', matters: [id] })}>
            <Icon name="chat" />
            提问
          </button>
          <AnalysisButton matter={id} snapshot={s} />
          <ExportButton resultRefs={async () => [ensure(await api.run('matter-manage', { matter_id: id, operation: 'read' })).data_ref!]} name={s.matter.title} label="导出事务" />
        </div>
      </header>
      <Tabs<Tab>
        label="事务内容"
        value={tab}
        onChange={(next) => (next === 'qa' ? go({ name: 'qa', matters: [id] }) : setTab(next))}
        items={[
          ['overview', '概览'],
          ['documents', '资料', s.documents.length, 'neutral'],
          ['facts', '事实', candidates],
          ['cashflows', '金额', pendingCash],
          ['calc', '计算'],
          ['timeline', '时间线', openTodos, 'neutral'],
          ['relations', '关系'],
          ['qa', '问答'],
        ]}
      />
      <div className="tab-body">
        {tab === 'overview' && <OverviewTab snapshot={s} onTab={setTab} />}
        {tab === 'documents' && <DocumentsTab matter={id} snapshot={s} onImport={importFiles} importing={busy} />}
        {tab === 'facts' && <FactsTab matter={id} snapshot={s} />}
        {tab === 'cashflows' && <CashflowTab matter={id} snapshot={s} />}
        {tab === 'calc' && <CalcTab matter={id} snapshot={s} />}
        {tab === 'timeline' && <TimelineTab matter={id} snapshot={s} />}
        {tab === 'relations' && <RelationsTab matter={id} snapshot={s} />}
      </div>
    </div>
  );
}

// ------------------------------------------------------------------ helpers

const KIND_LABEL: Record<string, string> = { document: '文档', audio: '录音', video: '视频' };
const KIND_ICON: Record<string, IconName> = { document: 'document', audio: 'mic', video: 'video' };

function factValue(f: Fact): string {
  if (f.fact_type === 'amount') return `${money(f.value.amount ?? '')} ${f.value.currency && f.value.currency !== 'CNY' ? f.value.currency : '元'}`;
  if (f.fact_type === 'date') return f.value.date ?? '';
  return f.value.text ?? '';
}

function factTarget(f: Fact, documents: Map<string, DocumentRow>): ViewerTarget | null {
  const doc = f.source_version ? documents.get(f.source_version) : undefined;
  if (!doc) return null;
  const locator = f.source?.block_locator;
  return { version: doc.version_id, title: doc.display_name, page: f.source?.page_id ?? undefined, highlight: locator ? [locator] : undefined };
}

function SourceTag({ fact, documents, short }: { fact: Fact; documents: Map<string, DocumentRow>; short?: boolean }) {
  const { open } = useSession();
  const target = factTarget(fact, documents);
  if (!target) return <span className="t-caption text-tertiary">{fact.origin === 'user' ? '手动添加' : '—'}</span>;
  const page = fact.source?.block_locator?.page_no;
  const where = page && page < 100000 ? `第 ${page} 页` : '';
  return (
    <button className="source-tag" title={`${target.title}${where ? ` · ${where}` : ''}：在右侧打开原文`} onClick={(e) => { e.stopPropagation(); open(target); }}>
      <Icon name="document" />
      <span className="ellipsis">{short ? where || target.title : [target.title, where].filter(Boolean).join(' · ')}</span>
    </button>
  );
}

// ----------------------------------------------------------------- overview

function OverviewTab({ snapshot: s, onTab }: { snapshot: Snapshot; onTab: (tab: Tab) => void }) {
  const { open } = useSession();
  const documents = useMemo(() => new Map(s.documents.map((d) => [d.version_id, d])), [s.documents]);
  const confirmed = s.facts.filter((f) => f.status === 'confirmed');
  const candidates = s.facts.filter((f) => f.status === 'candidate');
  const todos = s.todos.filter((t) => t.status === 'open').sort((a, b) => (a.due_on ?? '9999').localeCompare(b.due_on ?? '9999'));
  return (
    <>
      <div className="two-col">
        <section aria-labelledby="facts-head">
          <div className="section-head">
            <h2 id="facts-head">关键事实</h2>
            <span className="t-caption text-tertiary">已确认 {confirmed.length} 条</span>
          </div>
          {confirmed.length === 0 ? (
            <p className="text-secondary">确认候选或手动添加后，关键事实会显示在这里。</p>
          ) : (
            <div className="table-wrap">
              <table className="table">
                <thead>
                  <tr><th>事实</th><th className="num">内容</th><th>来源</th></tr>
                </thead>
                <tbody>
                  {confirmed.slice(0, 8).map((f) => (
                    <tr key={f.id}>
                      <td className="label">{f.label && f.origin !== 'rule_extracted' ? f.label : FACT_TYPES[f.fact_type] ?? f.fact_type}</td>
                      <td className={`num${f.fact_type === 'amount' ? ' amount' : ''}`}>{factValue(f)}</td>
                      <td><SourceTag fact={f} documents={documents} /></td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </section>
        <section aria-labelledby="pending-head">
          <div className="section-head">
            <h2 id="pending-head">待你核对</h2>
            {candidates.length > 0 && (
              <button className="btn btn-sm" onClick={() => onTab('facts')}>
                <Icon name="highlight" />
                进入核对（{candidates.length}）
              </button>
            )}
          </div>
          {candidates.length === 0 ? (
            <p className="text-secondary">没有待校对的候选。</p>
          ) : (
            <div className="queue">
              {candidates.slice(0, 3).map((f) => (
                <div key={f.id} className="queue-item">
                  <span className="chip attention">{FACT_TYPES[f.fact_type] ?? f.fact_type}</span>
                  <div style={{ minWidth: 0 }}>
                    <div className={`value${f.fact_type === 'amount' ? ' amount' : ' num'}`}>{factValue(f)}</div>
                    <div className="excerpt">原文：<mark>{f.label}</mark></div>
                  </div>
                  <SourceTag fact={f} documents={documents} short />
                </div>
              ))}
            </div>
          )}
          {todos.length > 0 && (
            <>
              <div className="section-head" style={{ marginTop: 20 }}>
                <h2>未完成待办</h2>
                <button className="btn btn-link" onClick={() => onTab('timeline')}>全部</button>
              </div>
              <ul className="plain">
                {todos.slice(0, 4).map((t) => (
                  <li key={t.id} className="row-between">
                    <span>{t.title}</span>
                    <span className="t-caption text-tertiary">{t.due_on ?? '无截止日期'}</span>
                  </li>
                ))}
              </ul>
            </>
          )}
        </section>
      </div>
      <section className="section" aria-labelledby="docs-head">
        <div className="section-head">
          <h2 id="docs-head">资料</h2>
          <button className="btn btn-link" onClick={() => onTab('documents')}>全部资料</button>
        </div>
        {s.documents.length === 0 ? (
          <p className="text-secondary">还没有资料。用“导入资料”加入合同、借条、账单、截图、录音或视频。</p>
        ) : (
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr><th>名称</th><th>类型</th><th>状态</th><th>导入时间</th><th aria-label="操作" /></tr>
              </thead>
              <tbody>
                {s.documents.slice(0, 5).map((doc) => (
                  <tr key={doc.version_id}>
                    <td className="name-cell"><DocName doc={doc} /></td>
                    <td>{KIND_LABEL[doc.media_kind] ?? doc.media_kind}{doc.pages ? ` · ${doc.pages} 页` : ''}</td>
                    <td><VersionChip status={doc.status} /></td>
                    <td className="num text-secondary" title={doc.created_at}>{relative(doc.created_at)}</td>
                    <td className="row-actions">
                      <button className="btn btn-xs" onClick={() => open({ version: doc.version_id, title: doc.display_name, media: doc.media_kind !== 'document' })}>查看</button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </>
  );
}

function DocName({ doc }: { doc: DocumentRow }) {
  return (
    <span className="doc-name" title={`${doc.display_name}（第 ${doc.version_no} 版，导入于 ${doc.created_at.slice(0, 16).replace('T', ' ')}）`}>
      <Icon name={KIND_ICON[doc.media_kind] ?? 'document'} />
      <span className="ellipsis">{doc.display_name}</span>
      <span className="t-caption text-tertiary nowrap">第 {doc.version_no} 版</span>
    </span>
  );
}

function VersionChip({ status }: { status: string }) {
  const tone = status === 'ready' ? 'ok' : status === 'partial' || status === 'interrupted' ? 'warning' : status === 'failed' ? 'danger' : 'neutral';
  return <Chip tone={tone} icon={status === 'ready' ? 'check' : undefined}>{versionStatus(status)}</Chip>;
}

// ---------------------------------------------------------------- documents

function DocumentsTab({ matter, snapshot: s, onImport, importing }: { matter: string; snapshot: Snapshot; onImport: () => void; importing: boolean }) {
  const { open, refresh } = useSession();
  const { busy, run } = useAction();
  const [deleting, setDeleting] = useState<{ doc: DocumentRow; ref: string; impact: Record<string, unknown> } | null>(null);

  async function transcribe(doc: DocumentRow) {
    await run(async () => {
      const range = await api.object('range', { media: doc.media_id, start_s: 0 });
      const question = await api.object('question', { text: `识别《${doc.display_name}》中的语音与画面文字` });
      const skill = doc.media_kind === 'video' ? 'video-review' : 'audio-review';
      await api.start(skill, { source_ref: doc.media_id, range_ref: range, question_id: question, analysis_goal: 'text_only' });
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
        <p className="t-caption text-secondary">原件、页面图、识别文字与索引都加密保存在本机；导入后自动解析、识别并建立检索索引。</p>
        <div className="actions">
          <button className="btn" disabled={busy || importing} onClick={() => run(() => api.importFiles(matter, 'new_version'), '已开始导入新版本')} title="选择与已有资料同名的文件，作为该资料的新版本">
            导入新版本
          </button>
        </div>
      </div>
      {s.documents.length === 0 ? (
        <Empty icon="import" title="还没有资料" action={<button className="btn btn-primary" onClick={onImport} disabled={importing}>导入资料</button>}>
          支持 PDF、Word（docx）、Excel（xlsx）、CSV/文本、图片与截图、录音和视频。
        </Empty>
      ) : (
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr><th>名称</th><th>类型</th><th>状态</th><th className="num">页数</th><th className="num">大小</th><th>导入时间</th><th aria-label="操作" /></tr>
            </thead>
            <tbody>
              {s.documents.map((doc) => (
                <tr key={doc.version_id}>
                  <td className="name-cell"><DocName doc={doc} /></td>
                  <td>{KIND_LABEL[doc.media_kind] ?? doc.media_kind}</td>
                  <td><VersionChip status={doc.status} /></td>
                  <td className="num">{doc.pages || '—'}</td>
                  <td className="num">{bytes(doc.bytes)}</td>
                  <td className="num text-secondary" title={doc.created_at}>{relative(doc.created_at)}</td>
                  <td className="row-actions">
                    <button className="btn btn-xs" onClick={() => open({ version: doc.version_id, title: doc.display_name, media: doc.media_kind !== 'document' })}>查看</button>
                    {doc.media_id && <button className="btn btn-xs" disabled={busy || doc.status === 'processing_media'} onClick={() => transcribe(doc)}>{doc.media_kind === 'video' ? '识别画面与语音' : '转写'}</button>}
                    <MediaAnalysisButton doc={doc} matter={matter} />
                    {doc.media_kind === 'audio' && <button className="btn btn-xs" disabled={busy || doc.status === 'processing_media'} onClick={() => run(() => api.start('knowledge-index', { document_versions: [doc.version_id], operation: 'build' }), '已开始建立索引')} title="把转写文字加入检索索引">建立索引</button>}
                    {doc.media_kind === 'document' && doc.pages > 0 && <button className="btn btn-xs" disabled={busy} onClick={() => correct(doc)} title="对扫描页或照片做弯曲矫正后重新识别">矫正重识别</button>}
                    <button className="btn btn-xs btn-danger" disabled={busy} onClick={() => askDelete(doc)}>删除…</button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {deleting && (
        <Modal
          title="删除这份资料及其识别结果？"
          icon="delete"
          onClose={() => setDeleting(null)}
          footer={
            <>
              <button className="btn" onClick={() => setDeleting(null)}>取消</button>
              <button className="btn btn-danger" disabled={busy} onClick={async () => { const done = await run(() => commitChange(matter, deleting.ref)); if (done !== undefined) { setDeleting(null); if (done) refresh(); } }}>继续删除…</button>
            </>
          }
        >
          <p>将删除《{deleting.doc.display_name}》第 {deleting.doc.version_no} 版及其全部派生内容：</p>
          <dl className="kv">
            {(['pages', 'blocks', 'chunks', 'facts', 'confirmed_facts', 'cashflows', 'media'] as const).map((key) => (
              <React.Fragment key={key}>
                <dt>{({ pages: '页面', blocks: '文字块', chunks: '检索片段', facts: '事实', confirmed_facts: '其中已确认事实', cashflows: '金额记录', media: '音视频记录' })[key]}</dt>
                <dd className="num">{String(deleting.impact[key] ?? 0)}</dd>
              </React.Fragment>
            ))}
          </dl>
          <p className="t-caption text-tertiary">{String(deleting.impact.note ?? '')}继续后会弹出系统确认框。</p>
        </Modal>
      )}
    </section>
  );
}

// ------------------------------------------------------- facts: review queue

function typing(target: EventTarget | null) {
  const el = target as HTMLElement | null;
  return !!el && (el.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT', 'BUTTON', 'A'].includes(el.tagName));
}

function FactsTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { refresh, open } = useSession();
  const { busy, run } = useAction();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [index, setIndex] = useState(0);
  const [editing, setEditing] = useState<Fact | null>(null);
  const [adding, setAdding] = useState(false);
  const documents = useMemo(() => new Map(s.documents.map((d) => [d.version_id, d])), [s.documents]);
  const candidates = s.facts.filter((f) => f.status === 'candidate');
  const confirmed = s.facts.filter((f) => f.status === 'confirmed');
  const rejected = s.facts.filter((f) => f.status === 'rejected');
  const current = candidates[Math.min(index, candidates.length - 1)];
  const queue = useRef<HTMLDivElement>(null);

  async function decide(type: 'confirm_facts' | 'reject_facts', ids: string[]) {
    if (!ids.length) return;
    const done = await run(() => applyChange(matter, { type, fact_ids: ids }), type === 'confirm_facts' ? '已确认' : '已排除');
    if (done) {
      setSelected((set) => new Set([...set].filter((id) => !ids.includes(id))));
      refresh();
    }
  }
  const toggle = (id: string) => setSelected((set) => {
    const next = new Set(set);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    return next;
  });

  // J/K move, Enter confirms, E corrects, X rejects, Space selects (design 9.3).
  const keys = useRef<(e: KeyboardEvent) => void>(() => {});
  keys.current = (e: KeyboardEvent) => {
    if (e.ctrlKey || e.altKey || e.metaKey || typing(e.target) || document.querySelector('.overlay') || !candidates.length || busy) return;
    const k = e.key.toLowerCase();
    if (k === 'j' || e.key === 'ArrowDown') setIndex((i) => Math.min(i + 1, candidates.length - 1));
    else if (k === 'k' || e.key === 'ArrowUp') setIndex((i) => Math.max(i - 1, 0));
    else if (e.key === 'Enter' && current) decide('confirm_facts', [current.id]);
    else if (k === 'e' && current && (current.fact_type === 'amount' || current.fact_type === 'date')) setEditing(current);
    else if (k === 'x' && current) decide('reject_facts', [current.id]);
    else if (e.key === ' ' && current) toggle(current.id);
    else if (k === 'o' && current) {
      const target = factTarget(current, documents);
      if (target) open(target);
    } else return;
    e.preventDefault();
  };
  useEffect(() => {
    const handler = (e: KeyboardEvent) => keys.current(e);
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, []);
  useEffect(() => {
    queue.current?.querySelector('[aria-current="true"]')?.scrollIntoView({ block: 'nearest' });
  }, [index, candidates.length]);

  return (
    <section>
      <div className="callout attention reading" style={{ marginBottom: 16 }}>
        <Icon name="highlight" />
        <div className="body">
          <strong>候选值由规则从原文提取，确认前不会用于计算</strong>
          <span className="text-secondary">逐条对照原文；可以更正数值或排除误识别。确认与排除都会再弹出系统确认框。</span>
        </div>
        <button className="btn btn-sm btn-subtle" onClick={() => setAdding(true)}>手动添加事实</button>
      </div>
      <div className="section-head">
        <h2>待校对 {candidates.length > 0 && <span className="count">{candidates.length}</span>}</h2>
        {candidates.length > 0 && (
          <div className="actions">
            <label className="check t-caption">
              <input type="checkbox" aria-label="全选" checked={candidates.every((f) => selected.has(f.id))} onChange={(e) => setSelected(e.target.checked ? new Set(candidates.map((f) => f.id)) : new Set())} />
              全选
            </label>
            <button className="btn btn-sm" disabled={busy || !selected.size} onClick={() => decide('reject_facts', [...selected])}>排除所选</button>
            <button className="btn btn-sm btn-primary" disabled={busy || !selected.size} onClick={() => decide('confirm_facts', [...selected])}>
              <Icon name="check" />
              确认所选（{selected.size}）
            </button>
          </div>
        )}
      </div>
      {candidates.length === 0 ? (
        <p className="text-secondary">没有待校对的候选。</p>
      ) : (
        <>
          <div className="queue" ref={queue} role="list" aria-label="待校对候选">
            {candidates.map((f, i) => (
              <div key={f.id} role="listitem" className="queue-item" aria-current={f === current ? 'true' : undefined} onClick={() => setIndex(i)}>
                <label className="check" onClick={(e) => e.stopPropagation()}>
                  <input type="checkbox" aria-label={`选择 ${factValue(f)}`} checked={selected.has(f.id)} onChange={() => toggle(f.id)} />
                  <span className="chip attention">{FACT_TYPES[f.fact_type] ?? f.fact_type}</span>
                </label>
                <div style={{ minWidth: 0 }}>
                  <div className={`value${f.fact_type === 'amount' ? ' amount' : ' num'}`}>{factValue(f)}</div>
                  <div className="excerpt">原文：<mark>{f.label}</mark></div>
                </div>
                <div className="actions">
                  <SourceTag fact={f} documents={documents} short />
                  {(f.fact_type === 'amount' || f.fact_type === 'date') && <button className="btn btn-xs" onClick={(e) => { e.stopPropagation(); setEditing(f); }}>更正</button>}
                  <button className="btn btn-xs btn-subtle" disabled={busy} onClick={(e) => { e.stopPropagation(); decide('reject_facts', [f.id]); }}>排除</button>
                  <button className="btn btn-xs" disabled={busy} onClick={(e) => { e.stopPropagation(); decide('confirm_facts', [f.id]); }}>确认</button>
                </div>
              </div>
            ))}
          </div>
          <p className="t-caption text-tertiary" style={{ marginTop: 10 }}>
            键盘：<span className="kbd">J</span>/<span className="kbd">K</span> 上下，<span className="kbd">Enter</span> 确认当前，<span className="kbd">E</span> 更正，<span className="kbd">X</span> 排除，<span className="kbd">Space</span> 选择，<span className="kbd">O</span> 打开原文。
          </p>
        </>
      )}
      <details className="section" open={confirmed.length > 0 && candidates.length === 0}>
        <summary className="text-secondary">已确认（{confirmed.length}）· 已排除（{rejected.length}）</summary>
        {confirmed.length > 0 && <FactTable rows={confirmed} documents={documents} onEdit={setEditing} />}
        {rejected.length > 0 && (
          <>
            <h3 className="t-caption text-secondary" style={{ margin: '12px 0 6px' }}>已排除</h3>
            <FactTable rows={rejected} documents={documents} />
          </>
        )}
      </details>
      {editing && <EditFact fact={editing} matter={matter} onClose={() => setEditing(null)} onSaved={() => { setEditing(null); refresh(); }} />}
      {adding && <AddFact matter={matter} onClose={() => setAdding(false)} onSaved={() => { setAdding(false); refresh(); }} />}
    </section>
  );
}

function FactTable({ rows, documents, onEdit }: { rows: Fact[]; documents: Map<string, DocumentRow>; onEdit?: (f: Fact) => void }) {
  return (
    <div className="table-wrap">
      <table className="table">
        <thead>
          <tr><th>类型</th><th>名称</th><th className="num">内容</th><th>来源</th><th>状态</th><th aria-label="操作" /></tr>
        </thead>
        <tbody>
          {rows.map((f) => (
            <tr key={f.id} className={f.status === 'rejected' ? 'muted' : undefined}>
              <td>{FACT_TYPES[f.fact_type] ?? f.fact_type}</td>
              <td>{f.label && f.origin !== 'rule_extracted' ? f.label : <span className="text-tertiary">{f.label}</span>}</td>
              <td className={`num${f.fact_type === 'amount' ? ' amount' : ''}`}>{factValue(f)}</td>
              <td><SourceTag fact={f} documents={documents} /></td>
              <td>
                <Chip tone={f.status === 'confirmed' ? 'ok' : 'neutral'} icon={f.status === 'confirmed' ? 'check' : undefined}>
                  {f.status === 'confirmed' ? (f.origin === 'rule_corrected' ? '已更正确认' : '已确认') : '已排除'}
                </Chip>
              </td>
              <td className="row-actions">{onEdit && f.status !== 'rejected' && (f.fact_type === 'amount' || f.fact_type === 'date') && <button className="btn btn-xs" onClick={() => onEdit(f)}>更正</button>}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function EditFact({ fact, matter, onClose, onSaved }: { fact: Fact; matter: string; onClose: () => void; onSaved: () => void }) {
  const [value, setValue] = useState(fact.fact_type === 'amount' ? fact.value.amount ?? '' : fact.value.date ?? '');
  const [label, setLabel] = useState(fact.origin === 'rule_extracted' ? '' : fact.label ?? '');
  const { busy, run } = useAction();
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const next = fact.fact_type === 'amount' ? { amount: value.trim(), currency: fact.value.currency ?? 'CNY' } : { date: value.trim() };
    const done = await run(() => applyChange(matter, { type: 'edit_fact', fact_id: fact.id, value: next, label: label.trim() || null }), '已更正并确认');
    if (done) onSaved();
  }
  return (
    <Modal title="更正并确认" icon="edit" onClose={onClose}>
      <form className="stack" onSubmit={submit}>
        <p className="t-caption text-secondary">原文：{fact.label}</p>
        <Field label={fact.fact_type === 'amount' ? '金额（元，最多 6 位小数）' : '日期（YYYY-MM-DD）'}>
          <input value={value} onChange={(e) => setValue(e.target.value)} inputMode={fact.fact_type === 'amount' ? 'decimal' : 'text'} pattern={fact.fact_type === 'amount' ? '\\d{1,15}(\\.\\d{1,6})?' : '\\d{4}-\\d{2}-\\d{2}'} required />
        </Field>
        <Field label="名称（可选）" hint="例如：押金、月租金、起租日">
          <input value={label} onChange={(e) => setLabel(e.target.value)} maxLength={60} />
        </Field>
        <div className="actions end">
          <button type="button" className="btn" onClick={onClose}>取消</button>
          <button className="btn btn-primary" disabled={busy}>保存</button>
        </div>
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
    <Modal title="手动添加事实" icon="add" onClose={onClose}>
      <form className="stack" onSubmit={submit}>
        <Field label="类型">
          <select value={type} onChange={(e) => setType(e.target.value)}>
            {Object.entries(FACT_TYPES).map(([id, l]) => <option key={id} value={id}>{l}</option>)}
          </select>
        </Field>
        <Field label={type === 'amount' ? '金额（元）' : type === 'date' ? '日期（YYYY-MM-DD）' : '内容'}>
          {type === 'note' || type === 'event' ? <textarea value={value} onChange={(e) => setValue(e.target.value)} rows={3} maxLength={2000} required /> : <input value={value} onChange={(e) => setValue(e.target.value)} required />}
        </Field>
        <Field label="名称（可选）">
          <input value={label} onChange={(e) => setLabel(e.target.value)} maxLength={60} />
        </Field>
        <p className="t-caption text-tertiary">手动添加的事实直接记为“已确认”，来源标记为你本人。</p>
        <div className="actions end">
          <button type="button" className="btn" onClick={onClose}>取消</button>
          <button className="btn btn-primary" disabled={busy}>添加</button>
        </div>
      </form>
    </Modal>
  );
}

// ----------------------------------------------------------------- timeline

const TL_STATUS: Record<string, string> = { confirmed: '已确认', candidate: '待校对', open: '未完成', done: '已完成', rejected: '已排除' };

function TimelineTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { refresh } = useSession();
  const { busy, run } = useAction();
  const [title, setTitle] = useState('');
  const [due, setDue] = useState('');
  async function add(e: React.FormEvent) {
    e.preventDefault();
    const done = await run(() => applyChange(matter, { type: 'add_todo', title: title.trim(), due_on: due || null }), '已添加待办');
    if (done) {
      setTitle('');
      setDue('');
      refresh();
    }
  }
  return (
    <section className="two-col">
      <div>
        <div className="section-head"><h2>时间线</h2></div>
        {s.timeline.length === 0 ? (
          <p className="text-secondary">确认日期事实、金额记录或添加待办后，会按日期排列在这里。</p>
        ) : (
          <ol className="timeline">
            {s.timeline.map((t) => (
              <li key={`${t.kind}-${t.ref}`} className={`tl-${t.kind}`}>
                <time>{t.date}</time>
                <span>{t.label}</span>
                <Chip tone={t.status === 'confirmed' || t.status === 'done' ? 'ok' : t.status === 'candidate' ? 'warn' : 'neutral'}>
                  {({ fact: '事实', cashflow: '金额', todo: '待办' } as Record<string, string>)[t.kind]} · {TL_STATUS[t.status] ?? statusLabel(t.status)}
                </Chip>
              </li>
            ))}
          </ol>
        )}
      </div>
      <div>
        <div className="section-head"><h2>待办</h2></div>
        <ul className="plain">
          {s.todos.map((t) => (
            <li key={t.id} className="row-between">
              <span className={t.status === 'done' ? 'done' : ''}>
                {t.title}
                {t.due_on && <span className="t-caption text-tertiary"> · {t.due_on}</span>}
              </span>
              {t.status === 'open' && <button className="btn btn-xs" disabled={busy} onClick={() => run(async () => { const d = await applyChange(matter, { type: 'complete_todo', todo_id: t.id }); if (d) refresh(); }, '已完成')}>完成</button>}
            </li>
          ))}
        </ul>
        <form className="stack compact" onSubmit={add} style={{ marginTop: 12, maxWidth: 'var(--form-w)' }}>
          <Field label="新待办"><input value={title} onChange={(e) => setTitle(e.target.value)} maxLength={120} required /></Field>
          <Field label="截止日期（可选）"><input type="date" value={due} onChange={(e) => setDue(e.target.value)} /></Field>
          <div><button className="btn" disabled={busy || !title.trim()}>添加待办</button></div>
        </form>
        <p className="t-caption text-tertiary" style={{ marginTop: 8 }}>两周内到期和已逾期的未完成待办会显示在工作台的“需要你处理”；系统通知式的到期提醒将在后续版本提供。</p>
      </div>
    </section>
  );
}

// ---------------------------------------------------------------- relations

function RelationsTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { refresh,open } = useSession();
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
  const [proof,setProof]=useState('');
  const [validFrom,setValidFrom]=useState('');
  const [validTo,setValidTo]=useState('');
  const [region,setRegion]=useState('');
  const sourceVersion=s.documents.find((d)=>d.version_id===src)?.version_id??s.facts.find((f)=>f.id===src)?.source_version??'';
  const sources=useLoad(()=>sourceVersion?api.view<{evidence_id:string;unit_title:string;excerpt:string}[]>('relation_sources',sourceVersion):Promise.resolve([]),[sourceVersion]);
  useEffect(()=>setProof(''),[src]);
  const invalidDates=!!(validFrom&&validTo&&validFrom>=validTo);
  async function add(e: React.FormEvent) {
    e.preventDefault();
    const done = await run(() => applyChange(matter, { type: 'add_relation', relation_type: type, src, dst,evidence:proof?{evidence_id:proof}:null,valid_from:validFrom||null,valid_to:validTo||null,jurisdiction:region||null }), '已写入关系');
    if (done) refresh();
  }
  return (
    <section>
      <p className="t-caption text-secondary reading" style={{ marginBottom: 12 }}>这里只保存你明确确认的关系。选择原文依据后，可在对应范围的关系检索中用于查找资料；未附原文的关系只保留为个人记录。确认候选事实时会自动保留其来源。</p>
      {s.relations.length === 0 ? (
        <p className="text-secondary">还没有关系。</p>
      ) : (
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr><th>起点</th><th>关系</th><th>终点</th><th>来源</th><th aria-label="操作" /></tr>
            </thead>
            <tbody>
              {s.relations.map((r) => (
                <tr key={r.id}>
                  <td>{name(r.src)}</td>
                  <td><Chip tone="info">{RELATION_TYPES[r.relation_type] ?? r.relation_type} →</Chip></td>
                  <td>{name(r.dst)}</td>
                  <td className="t-caption text-tertiary">{r.evidence?'含原文依据':'个人记录，未用于图检索'}{r.valid_from&&<div>{r.valid_from} 起</div>}{r.valid_to&&<div>{r.valid_to} 起失效</div>}{r.source_version&&<button className="btn btn-xs" onClick={()=>open({version:r.source_version!,title:'关系的原文来源'})}>查看来源</button>}</td>
                  <td className="row-actions"><button className="btn btn-xs" disabled={busy} onClick={() => run(async () => { const d = await applyChange(matter, { type: 'remove_relation', relation_id: r.id }); if (d) refresh(); }, '已移除')}>移除</button></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <form className="inline-form" onSubmit={add}>
        <Field label="起点"><select value={src} onChange={(e) => setSrc(e.target.value)} required><option value="">选择…</option>{nodes.map((n) => <option key={n.id} value={n.id}>{n.label}</option>)}</select></Field>
        <Field label="关系"><select value={type} onChange={(e) => setType(e.target.value)}>{Object.entries(RELATION_TYPES).map(([id, l]) => <option key={id} value={id}>{l}</option>)}</select></Field>
        <Field label="终点"><select value={dst} onChange={(e) => setDst(e.target.value)} required><option value="">选择…</option>{nodes.map((n) => <option key={n.id} value={n.id}>{n.label}</option>)}</select></Field>
        <Field label="起点资料中的原文依据（选填）"><select value={proof} onChange={(e)=>setProof(e.target.value)} disabled={!sourceVersion||sources.loading}><option value="">只保留为个人记录</option>{sources.data?.map((source)=><option key={source.evidence_id} value={source.evidence_id}>{source.unit_title||'原文'}：{source.excerpt.slice(0,100)}</option>)}</select></Field>
        <Field label="生效日期（选填）"><input type="date" value={validFrom} onChange={(e)=>setValidFrom(e.target.value)} /></Field>
        <Field label="失效日期（当天起不适用）"><input type="date" value={validTo} onChange={(e)=>setValidTo(e.target.value)} /></Field>
        <Field label="关系的适用地域"><select value={region} onChange={(e)=>setRegion(e.target.value)}><option value="">未注明</option>{REGIONS.map(([id,name])=><option key={id} value={id}>{name}</option>)}</select></Field>
        <button className="btn" disabled={busy || !src || !dst || src === dst||invalidDates}>写入关系</button>
      </form>
      <ErrorLine text={invalidDates?'失效日期必须晚于生效日期。':sources.error} />
    </section>
  );
}

