// A read-only view of saved encrypted results, rebuilt after each unlock.
import React, { useEffect, useState } from 'react';
import { api, type HistoryEntry, type HistoryQuery } from './api';
import { useSession } from './session';
import { KIND_LABEL } from './turns';
import { Busy, ErrorLine, Field, Modal } from './ui';

export function QaHistory({ initialMatter, onOpen, onClose }: { initialMatter?: string; onOpen: (entry: HistoryEntry) => void; onClose: () => void }) {
  const { matters } = useSession();
  const [search, setSearch] = useState('');
  const [matter, setMatter] = useState(initialMatter ?? '');
  const [from, setFrom] = useState('');
  const [through, setThrough] = useState('');
  const [query, setQuery] = useState<HistoryQuery>({ matter: initialMatter ?? null });
  const [cursor, setCursor] = useState<number | null>(null);
  const [next, setNext] = useState<number | null>(null);
  const [items, setItems] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  useEffect(() => {
    let alive = true;
    setLoading(true); setError('');
    api.qaHistory({ ...query, before: cursor, limit: 40 }).then((page) => {
      if (!alive) return;
      setItems((previous) => cursor === null ? page.items : [...previous, ...page.items.filter((item) => !previous.some((saved) => saved.id === item.id))]);
      setNext(page.next_cursor);
    }).catch((e: unknown) => { if (alive) setError(String(e)); }).finally(() => { if (alive) setLoading(false); });
    return () => { alive = false; };
  }, [query, cursor]);
  return <Modal title="历史问答与分析" wide onClose={onClose} footer={<><span className="t-caption text-secondary">已显示 {items.length} 条</span><button className="btn" disabled={loading || next === null || !!error} onClick={() => setCursor(next)}>加载更多</button><button className="btn" onClick={onClose}>关闭</button></>}>
    <p className="text-secondary">打开已保存的回答、原文、核查与反馈。此处只读取当前私密空间，不调用生成接口。</p>
    <form onSubmit={(e) => { e.preventDefault(); setItems([]); setNext(null); setCursor(null); setQuery({ search: search.trim(), matter: matter || null, from: from || null, through: through || null }); }}>
      <Field label="搜索问题"><input maxLength={2000} value={search} onChange={(e) => setSearch(e.target.value)} placeholder="输入问题中的文字" /></Field>
      <div className="inline-form">
        <Field label="事务"><select value={matter} onChange={(e) => setMatter(e.target.value)}><option value="">全部事务</option>{matters.map((m) => <option key={m.id} value={m.id}>{m.title}</option>)}</select></Field>
        <Field label="开始日期（北京时间）"><input type="date" value={from} onChange={(e) => setFrom(e.target.value)} /></Field>
        <Field label="结束日期（含当天）"><input type="date" value={through} min={from || undefined} onChange={(e) => setThrough(e.target.value)} /></Field>
        <button className="btn" type="submit" disabled={loading || (!!from && !!through && from > through)}>筛选记录</button>
      </div>
    </form>
    <ErrorLine text={error} />
    {loading && <Busy label="正在读取历史记录…" />}
    {!loading && !error && !items.length && <p className="empty">没有符合条件的已保存记录。</p>}
    {items.length > 0 && <div className="table-wrap"><table className="table"><thead><tr><th>问题</th><th>类型与时间</th><th>操作</th></tr></thead><tbody>{items.map((item) => <tr key={item.id}>
      <td className="reading">{item.question}</td><td><div>{KIND_LABEL[item.kind]}</div><time className="t-caption text-secondary" dateTime={item.created_at}>{new Date(item.created_at).toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai' })}</time></td>
      <td><button className="btn btn-sm" onClick={() => onOpen(item)}>打开记录</button></td>
    </tr>)}</tbody></table></div>}
  </Modal>;
}
