// Retrieval: q0 hybrid search (BM25 + Qwen vectors, RRF) with Qwen rerank in
// the chosen matters; every result opens the exact source location.
import React, { useState } from 'react';
import { api, snapshot, type EvidenceItem, type Locator } from './api';
import { ExportButton } from './export';
import { useSession } from './main';
import { Busy, Chip, Empty, ErrorLine, Field, useAction, useLoad } from './ui';

export function SearchPage({ initial }: { initial?: string[] }) {
  const { info, open, revision } = useSession();
  const root = useLoad(() => snapshot(info.root_matter), [info.root_matter, revision]);
  const matters = root.data?.matters ?? [];
  const [scope, setScope] = useState<Set<string>>(new Set(initial ?? []));
  const [question, setQuestion] = useState('');
  const [result, setResult] = useState<{ id: string; items: EvidenceItem[]; status: string; notes: string[]; answer: boolean } | null>(null);
  const [error, setError] = useState('');
  const { busy, run } = useAction();
  const chosen = scope.size ? [...scope] : matters.map((m) => m.id);

  async function search(answer: boolean) {
    setError('');
    setResult(null);
    const outcome = await run(async () => {
      const questionId = await api.object('question', { text: question });
      const scopeId = await api.object('scope', { matters: chosen, versions: [] });
      const envelope = await api.run('lvjin-retrieval', { question_id: questionId, scope_id: scopeId, known_constraint_ids: [], requested_output: answer ? 'answer' : 'evidence_only' });
      if (!envelope.data_ref) throw new Error(envelope.warnings.map((w) => w.message).join('；') || '检索失败');
      const payload = await api.result<{ items: EvidenceItem[] }>(envelope.data_ref);
      return { id: envelope.data_ref, items: payload.items, status: envelope.status, notes: envelope.warnings.map((w) => w.message).concat(envelope.coverage.limitations), answer };
    });
    if (outcome) setResult(outcome);
  }

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <p className="eyebrow">资料检索</p>
          <h1>在私密资料中查找原文</h1>
        </div>
      </header>
      <section className="panel">
        <form className="stack" onSubmit={(e) => { e.preventDefault(); search(false); }}>
          <Field label="问题或关键词" hint="例如：押金什么时候退还？借款还剩多少？">
            <textarea rows={2} value={question} onChange={(e) => setQuestion(e.target.value)} maxLength={2000} required />
          </Field>
          <fieldset className="pick inline">
            <legend>检索范围{scope.size ? '' : '（未选择时为全部事务）'}</legend>
            {matters.map((m) => (
              <label key={m.id} className="check"><input type="checkbox" checked={scope.has(m.id)} onChange={() => setScope((s) => { const n = new Set(s); n.has(m.id) ? n.delete(m.id) : n.add(m.id); return n; })} />{m.title}</label>
            ))}
          </fieldset>
          <div className="actions">
            <button className="primary" disabled={busy || !question.trim() || chosen.length === 0}>{busy ? '正在检索…' : '检索原文'}</button>
            <button type="button" disabled={busy || !question.trim() || chosen.length === 0} onClick={() => search(true)} title="生成回答需要配置生成接口（M2）；本版本会交付找到的原文依据">生成回答</button>
          </div>
        </form>
        <p className="muted small">检索在本机完成：原始问题同时做关键词（BM25）与语义向量检索，两路结果用 RRF 融合，再由本机 Qwen 重排模型排序。查询改写需要生成接口，当前只用原始问题。</p>
      </section>
      <ErrorLine text={error} />
      {busy && <Busy label="正在检索与重排（首次加载模型可能需要一分钟）…" />}
      {result && (
        <section className="panel">
          <div className="row-between">
            <h2>原文依据（{result.items.length}）</h2>
            {result.items.length > 0 && <ExportButton resultRefs={async () => [result.id]} name={question.slice(0, 30)} label="导出依据" />}
          </div>
          {result.answer && (
            <div className="callout"><strong>生成回答需要生成接口</strong><p>当前版本（M1）未连接生成接口，不生成答案。下面是本机找到的原文依据，请对照原文阅读。</p></div>
          )}
          {result.notes.length > 0 && <p className="muted small">{result.notes.filter((n) => !n.includes('生成答案需要')).join('；')}</p>}
          {result.items.length === 0 ? <Empty title="依据不足">所选范围内没有检索到相关原文。可以换个说法，或扩大检索范围。</Empty> : (
            <EvidenceList items={result.items} onOpen={(item) => open({ version: item.version_id, title: item.document.display_name, page: item.pages[0], highlight: item.highlight_indexes.map((i) => item.locators[i]).filter(Boolean), media: item.document.media_kind !== 'document' })} />
          )}
        </section>
      )}
    </div>
  );
}

export function locatorLabel(locators: Locator[]): string {
  const pages = [...new Set(locators.map((l) => l.page_no).filter((p): p is number => typeof p === 'number' && p < 100000))];
  if (pages.length) return `第 ${pages.join('、')} 页`;
  const time = locators.find((l) => l.type === 'time');
  if (time && time.start_s !== undefined) return `${fmt(time.start_s)}–${fmt(time.end_s ?? time.start_s)}`;
  const frame = locators.find((l) => (l.page_no ?? 0) >= 100000);
  if (frame) return '视频画面';
  const row = locators.find((l) => l.row !== undefined);
  if (row) return `${row.sheet ? row.sheet + ' ' : ''}第 ${row.row} 行`;
  const line = locators.find((l) => l.line !== undefined || l.paragraph !== undefined);
  if (line) return line.line !== undefined ? `第 ${line.line} 行` : `第 ${line.paragraph} 段`;
  return '';
}

const fmt = (s: number) => `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(Math.floor(s % 60)).padStart(2, '0')}`;

export function EvidenceList({ items, onOpen }: { items: EvidenceItem[]; onOpen: (item: EvidenceItem) => void }) {
  return (
    <ol className="evidence">
      {items.map((item, index) => (
        <li key={item.evidence_id}>
          <div className="evidence-head">
            <span className="rank">{index + 1}</span>
            <strong title={item.document.display_name}>{item.document.display_name}</strong>
            <span className="muted small">第 {item.document.version_no} 版 · {locatorLabel(item.locators)}</span>
            {item.locators.some((l) => l.low_confidence) && <Chip tone="warn" title="其中有识别置信度较低的文字，金额、日期和姓名请对照原图">含低置信度文字</Chip>}
            {item.document.media_kind !== 'document' && <Chip tone="info">{item.document.media_kind === 'audio' ? '录音转写' : '视频文字'}</Chip>}
          </div>
          {item.unit_title && <p className="muted small">{item.unit_title}</p>}
          <blockquote>{item.exact_text}</blockquote>
          <button className="small" onClick={() => onOpen(item)}>查看原文位置</button>
        </li>
      ))}
    </ol>
  );
}
