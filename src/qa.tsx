// Q&A and retrieval (design 9.5, 9.7): scope and mode, the question thread,
// structured answers with citations and per-sentence grounding status, the
// execution process, and the two-step egress approval — an in-app preview
// that can drop excerpts, then the native confirmation built by the core.
import React, { useEffect, useMemo, useRef, useState } from 'react';
import { api, type AnswerPreview, type ClaimRow, type EvidenceItem, type ProcessStep, type RetrievalResult, type TaskState, type Warning } from './api';
import { EvidenceList, locatorLabel } from './evidence';
import { ExportButton } from './export';
import { Icon } from './icons';
import { sourceOf, useSession, type EvidencePanel } from './session';
import { Busy, Callout, Chip, ErrorLine, Modal, Segmented, useDismiss, useLoad, useNotice } from './ui';

export type QaTurn = {
  id: string;
  question: string;
  matters: string[];
  mode: 'answer' | 'evidence';
  state: 'preparing' | 'running' | 'done' | 'failed' | 'cancelled';
  started: number;
  finished?: number;
  task?: string;
  status?: string;
  warnings?: Warning[];
  resultRef?: string | null;
  sent?: number;
  error?: string;
};

type Update = (change: (turns: QaTurn[]) => QaTurn[]) => void;

/** Resolve running turns from finished tasks (called by the shell). */
export function watchQaTasks(tasks: TaskState[], update: Update) {
  const finished = new Map(tasks.filter((t) => t.status !== 'running').map((t) => [t.task_id, t]));
  if (!finished.size) return;
  update((turns) => {
    let changed = false;
    const next = turns.map((turn) => {
      const task = turn.state === 'running' && turn.task ? finished.get(turn.task) : undefined;
      if (!task) return turn;
      changed = true;
      const envelope = task.envelope;
      const state: QaTurn['state'] = task.status === 'cancelled' ? 'cancelled' : envelope?.data_ref ? 'done' : 'failed';
      return { ...turn, state, status: task.status, warnings: envelope?.warnings ?? [], resultRef: envelope?.data_ref ?? null, finished: Date.now(), error: state === 'failed' ? envelope?.warnings.map((w) => w.message).join('；') || '回答未完成' : undefined };
    });
    return changed ? next : turns;
  });
}

let counter = 0;
const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function QaPage({ initialMatters, initialQuestion }: { initialMatters?: string[]; initialQuestion?: string }) {
  const { matters, connection, qa, go } = useSession();
  const [scope, setScope] = useState<string[]>(initialMatters ?? []);
  const [mode, setMode] = useState<'answer' | 'evidence'>(connection ? 'answer' : 'evidence');
  const modeTouched = useRef(false);
  const [text, setText] = useState(initialQuestion ?? '');
  const [preview, setPreview] = useState<{ turn: string; data: AnswerPreview } | null>(null);
  const [picking, setPicking] = useState(false);
  const thread = useRef<HTMLDivElement>(null);
  const composer = useRef<HTMLTextAreaElement>(null);
  const notice = useNotice();
  const chosen = scope.length ? scope : matters.map((m) => m.id);
  const effectiveMode = connection ? mode : 'evidence';
  const running = qa.turns.find((t) => t.state === 'running' || t.state === 'preparing');

  useEffect(() => {
    if (connection && !modeTouched.current) setMode('answer');
  }, [connection]);
  // Results load after the thread renders; keep the newest turn in view
  // while earlier ones grow, until the user scrolls or clicks in the thread.
  useEffect(() => {
    const el = thread.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    let follow = true;
    const stop = () => {
      follow = false;
    };
    const observer = new ResizeObserver(() => {
      if (follow) el.querySelector<HTMLElement>('.turn:last-of-type')?.scrollIntoView({ block: 'start' });
    });
    for (const child of el.querySelectorAll('.turn')) observer.observe(child);
    el.addEventListener('wheel', stop, { passive: true });
    el.addEventListener('pointerdown', stop);
    el.addEventListener('keydown', stop);
    return () => {
      observer.disconnect();
      el.removeEventListener('wheel', stop);
      el.removeEventListener('pointerdown', stop);
      el.removeEventListener('keydown', stop);
    };
  }, [qa.turns.length]);
  useEffect(() => {
    if (initialQuestion?.trim()) submit(initialQuestion);
    else composer.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const patch = (id: string, change: Partial<QaTurn>) => qa.update((turns) => turns.map((t) => (t.id === id ? { ...t, ...change } : t)));

  // Leaving the page while a preview is pending discards it: nothing was
  // approved, so nothing is sent, and the thread does not stay blocked.
  const mounted = useRef(true);
  const pending = useRef<{ turn: string; grant: string | null } | null>(null);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      const open = pending.current;
      if (open) {
        if (open.grant) api.answerDiscard(open.grant).catch(() => {});
        qa.update((turns) => turns.map((t) => (t.id === open.turn ? { ...t, state: 'cancelled', error: '已离开问答页，未发送任何内容' } : t)));
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function submit(value = text, asMode: 'answer' | 'evidence' = effectiveMode, within = chosen) {
    const question = value.trim();
    if (!question || !within.length || running) return;
    const turn: QaTurn = { id: `turn-${++counter}`, question, matters: within, mode: asMode, state: 'preparing', started: Date.now() };
    qa.update((turns) => [...turns, turn]);
    setText('');
    try {
      if (asMode === 'evidence') {
        const questionId = await api.object('question', { text: question });
        const scopeId = await api.object('scope', { matters: within, versions: [] });
        const task = await api.start('lvjin-retrieval', { question_id: questionId, scope_id: scopeId, known_constraint_ids: [], requested_output: 'evidence_only' });
        patch(turn.id, { state: 'running', task });
      } else {
        // Local retrieval only; nothing leaves the computer before approval.
        pending.current = { turn: turn.id, grant: null };
        const data = await api.answerPreview(question, within, connection?.id ?? null);
        if (!mounted.current) {
          api.answerDiscard(data.grant_id).catch(() => {});
          return;
        }
        pending.current = { turn: turn.id, grant: data.grant_id };
        setPreview({ turn: turn.id, data });
      }
    } catch (e) {
      if (pending.current?.turn === turn.id) pending.current = null;
      patch(turn.id, { state: 'failed', error: message(e) });
    }
  }

  async function approve(excluded: string[]) {
    if (!preview) return;
    const { turn, data } = preview;
    pending.current = null;
    try {
      const task = await api.answerStart(data.grant_id, excluded);
      setPreview(null);
      patch(turn, { state: 'running', task, sent: data.excerpts.length - excluded.length });
    } catch (e) {
      // Declined in the native dialog (or the grant no longer applies).
      setPreview(null);
      patch(turn, { state: 'cancelled', error: message(e) });
    }
  }

  function discard() {
    if (!preview) return;
    pending.current = null;
    api.answerDiscard(preview.data.grant_id).catch(() => {});
    patch(preview.turn, { state: 'cancelled', error: '已取消，未发送任何内容' });
    setPreview(null);
  }

  function stop() {
    if (running?.task) api.cancelTask(running.task).then(() => notice('info', '已请求停止')).catch(() => {});
  }

  return (
    <div className="qa">
      <div className="qa-scope">
        <span className="text-secondary">范围</span>
        {scope.length === 0 ? (
          <Chip tone="accent">全部事务（{matters.length}）</Chip>
        ) : (
          scope.map((id) => (
            <span key={id} className="chip accent">
              {matters.find((m) => m.id === id)?.title ?? '事务'}
              <button className="btn btn-link chip-remove" aria-label="移出范围" onClick={() => setScope(scope.filter((x) => x !== id))}>
                <Icon name="close" />
              </button>
            </span>
          ))
        )}
        <div className="scope-picker">
          <button className="btn btn-sm btn-subtle" onClick={() => setPicking(!picking)} aria-expanded={picking}>
            <Icon name="add" />
            选择事务
          </button>
          {picking && <ScopePicker value={scope} onChange={setScope} onClose={() => setPicking(false)} />}
        </div>
        <span style={{ flex: 1 }} />
        {!connection && (
          <button className="btn btn-link t-caption" onClick={() => go({ name: 'connections' })}>
            连接生成接口后可生成回答
          </button>
        )}
        <Segmented
          label="模式"
          value={effectiveMode}
          onChange={(v) => {
            modeTouched.current = true;
            setMode(v);
          }}
          items={[['answer', '生成回答', !connection], ['evidence', '只找原文']]}
        />
      </div>
      <div className="qa-thread" ref={thread}>
        {qa.turns.length === 0 && <Intro hasConnection={!!connection} />}
        {qa.turns.map((turn, index) => (
          <TurnView key={turn.id} turn={turn} latest={index === qa.turns.length - 1} onStop={stop} onRetry={(asMode) => submit(turn.question, asMode, turn.matters)} />
        ))}
      </div>
      <div className="composer">
        <div className="composer-box">
          <textarea
            ref={composer}
            rows={2}
            value={text}
            maxLength={2000}
            placeholder={effectiveMode === 'answer' ? '输入问题…（Ctrl+Enter 发送）' : '输入要查找的内容…（Ctrl+Enter 检索）'}
            aria-label="问题"
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && e.ctrlKey) {
                e.preventDefault();
                submit();
              } else if (e.key === 'Escape' && running) {
                e.preventDefault();
                stop();
              }
            }}
          />
          {running ? (
            <button className="btn btn-icon" aria-label="停止" title="停止（Esc）" onClick={stop} disabled={!running.task}>
              <Icon name="stop" />
            </button>
          ) : (
            <button className="btn btn-primary btn-icon" aria-label={effectiveMode === 'answer' ? '发送' : '检索'} title="Ctrl+Enter" onClick={() => submit()} disabled={!text.trim() || !chosen.length}>
              <Icon name={effectiveMode === 'answer' ? 'send' : 'search'} />
            </button>
          )}
        </div>
        <div className="composer-meta">
          {effectiveMode === 'answer' && connection ? (
            <>
              <Icon name="globe" className="cap-outbound" />
              <span>发送前会显示将发往 {connection.host} 的原文，并请你确认</span>
            </>
          ) : (
            <>
              <Icon name="shield" className="cap-supported" />
              <span>只在本机检索原文，不发送任何内容</span>
            </>
          )}
        </div>
      </div>
      {preview && <EgressDialog preview={preview.data} onCancel={discard} onConfirm={approve} />}
    </div>
  );
}

function Intro({ hasConnection }: { hasConnection: boolean }) {
  return (
    <div className="qa-intro">
      <Icon name="chat" size="xl" />
      <h2 className="t-title">在你的资料里提问</h2>
      <p className="text-secondary">
        {hasConnection
          ? '回答由你连接的生成接口起草，只依据所选事务中检索到的原文；每一句都会在本机核对依据，发送前先请你确认。'
          : '目前只在本机检索原文并按相关度排序。连接生成接口后，可以生成带引用、逐句核查的回答。'}
      </p>
      <ul className="text-secondary t-caption">
        <li>例：押金什么时候退还？</li>
        <li>例：借款还剩多少没还？</li>
        <li>例：提前退租要提前多久通知？</li>
      </ul>
    </div>
  );
}

function ScopePicker({ value, onChange, onClose }: { value: string[]; onChange: (v: string[]) => void; onClose: () => void }) {
  const { matters } = useSession();
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(ref, true, onClose, '.scope-picker > button');
  return (
    <div className="popover scope-popover fade-in" ref={ref} role="dialog" aria-label="选择事务">
      <p className="t-caption text-tertiary">{value.length ? `已选 ${value.length} 件事务` : '未选择时在全部事务中查找'}</p>
      <div className="pick">
        {matters.map((m) => (
          <label key={m.id} className="check">
            <input type="checkbox" checked={value.includes(m.id)} onChange={() => onChange(value.includes(m.id) ? value.filter((x) => x !== m.id) : [...value, m.id])} />
            <span className="ellipsis">{m.title}</span>
          </label>
        ))}
      </div>
      {value.length > 0 && <button className="btn btn-sm btn-subtle" onClick={() => onChange([])}>清除选择</button>}
    </div>
  );
}

// ------------------------------------------------------------------- turns

function TurnView({ turn, latest, onStop, onRetry }: { turn: QaTurn; latest: boolean; onStop: () => void; onRetry: (mode: 'answer' | 'evidence') => void }) {
  const { matters } = useSession();
  const ref = useRef<HTMLElement>(null);
  const names = turn.matters.length === matters.length && matters.length > 1 ? '全部事务' : turn.matters.map((id) => matters.find((m) => m.id === id)?.title ?? '事务').join('、');
  // The newest question and the start of its result stay in view.
  useEffect(() => {
    if (latest) ref.current?.scrollIntoView({ block: 'start' });
  }, [latest, turn.state]);
  return (
    <section className="turn" aria-label="一次问答" ref={ref}>
      <div className="question">
        <div className="bubble">
          {turn.question}
          <div className="t-caption text-tertiary">{turn.mode === 'answer' ? '生成回答' : '只找原文'} · {names}</div>
        </div>
      </div>
      {turn.state === 'preparing' && (
        <div className="answer running">
          <div className="answer-empty">
            <Busy label={turn.mode === 'answer' ? '正在本机检索可以发送的原文…（首次加载模型约需一分钟）' : '正在本机检索与重排…'} />
          </div>
        </div>
      )}
      {turn.state === 'running' && (
        <div className="answer running">
          <div className="answer-empty row-between">
            <Busy label={turn.mode === 'answer' ? `正在生成：理解问题 → 检索 → 起草 → 逐句核查${turn.sent !== undefined ? `（已批准发送 ${turn.sent} 段原文）` : ''}` : '正在本机检索与重排…'} />
            <button className="btn btn-sm" onClick={onStop}>
              <Icon name="stop" />
              停止
            </button>
          </div>
        </div>
      )}
      {turn.state === 'failed' && (
        <Callout tone="danger" title="没有完成" action={<button className="btn btn-sm" onClick={() => onRetry(turn.mode)}>重试</button>}>
          {turn.error}
        </Callout>
      )}
      {turn.state === 'cancelled' && (
        <Callout
          title={turn.error ?? '已停止'}
          action={turn.mode === 'answer' ? <button className="btn btn-sm" onClick={() => onRetry('evidence')}>只看本机原文</button> : undefined}
        >
          {turn.mode === 'answer' ? '可以调整问题或范围后重新提问；只找原文不会发送任何内容。' : undefined}
        </Callout>
      )}
      {turn.state === 'done' && turn.resultRef && <TurnResult turn={turn} latest={latest} />}
    </section>
  );
}

function TurnResult({ turn, latest }: { turn: QaTurn; latest: boolean }) {
  const result = useLoad(() => api.result<RetrievalResult>(turn.resultRef!), [turn.resultRef]);
  if (result.error) return <ErrorLine text={result.error} />;
  if (!result.data) return <Busy label="正在打开结果…" />;
  return result.data.answer ? <AnswerCard turn={turn} result={result.data} latest={latest} /> : <EvidenceCard turn={turn} result={result.data} />;
}

const seconds = (turn: QaTurn) => (turn.finished ? Math.max(1, Math.round((turn.finished - turn.started) / 1000)) : null);

function EvidenceCard({ turn, result }: { turn: QaTurn; result: RetrievalResult }) {
  const { open, go } = useSession();
  const warnings = turn.warnings ?? [];
  const needsGeneration = turn.status === 'requires_generation';
  const other = warnings.filter((w) => w.code !== 'requires_generation' && w.code !== 'insufficient_evidence');
  return (
    <article className="answer" aria-label="检索结果">
      <div className="answer-head">
        <Icon name="search" />
        <span>本机检索</span>
        <span>·</span>
        <span>找到 {result.items.length} 段原文</span>
        {seconds(turn) && (
          <>
            <span>·</span>
            <span>{seconds(turn)} 秒</span>
          </>
        )}
        <span className="spacer" />
        <Chip icon="shield">未发送任何内容</Chip>
      </div>
      <div className="answer-body">
        {needsGeneration && (
          <Callout title="生成回答需要连接并授权生成接口" action={<button className="btn btn-sm" onClick={() => go({ name: 'connections' })}>连接生成接口</button>}>
            下面是本机找到的原文依据，请对照原文阅读。
          </Callout>
        )}
        {other.length > 0 && <Callout tone="warning" title="部分完成">{other.map((w) => w.message).join('；')}</Callout>}
        {result.items.length === 0 ? (
          <Callout tone="attention" title="依据不足">所选范围内没有检索到相关原文。可以换个说法、扩大范围，或导入补充资料。</Callout>
        ) : (
          <EvidenceList items={result.items} onOpen={(item) => open(sourceOf(item))} />
        )}
      </div>
      {result.items.length > 0 && (
        <div className="answer-foot">
          <ExportButton resultRefs={async () => [turn.resultRef!]} name={turn.question.slice(0, 30)} label="导出依据" small />
          <span style={{ flex: 1 }} />
          <span className="t-caption text-tertiary">检索与重排在本机完成：关键词＋语义检索，RRF 融合后由本机模型重排</span>
        </div>
      )}
    </article>
  );
}

// ------------------------------------------------------------------ answer

function numberOf(label: string) {
  return label.replace(/^E/, '');
}

function AnswerCard({ turn, result, latest }: { turn: QaTurn; result: RetrievalResult; latest: boolean }) {
  const { inspect, inspector } = useSession();
  const notice = useNotice();
  const answer = result.answer!;
  const s = answer.sections;
  const labels = useMemo(() => Object.fromEntries(answer.labels.map((l) => [l.evidence_id, numberOf(l.label)])), [answer.labels]);
  const byLabel = useMemo(() => new Map(answer.labels.map((l) => [l.label, l.evidence_id])), [answer.labels]);
  const sent = useMemo(() => {
    const byId = new Map(result.items.map((i) => [i.evidence_id, i]));
    return answer.labels.map((l) => byId.get(l.evidence_id)).filter((i): i is EvidenceItem => !!i);
  }, [answer.labels, result.items]);
  const claims = [...s.conclusion, ...s.basis, ...s.calculations];
  const state = claims.some((c) => c.status === 'unsupported') ? 'unsupported' : claims.some((c) => c.status === 'partial') ? 'partial' : claims.some((c) => c.status === 'unchecked') ? 'unchecked' : 'supported';

  function panel(current: string | null): EvidencePanel {
    const unsent = result.items.length - sent.length;
    return { title: `依据（${sent.length}）`, note: `本次回答发送并可引用的原文${unsent > 0 ? `；另有 ${unsent} 段只在本机检索到，未发送` : ''}`, items: sent, labels, current };
  }
  function cite(evidence: string) {
    const item = sent.find((i) => i.evidence_id === evidence);
    inspect({ kind: 'evidence', evidence: panel(evidence), source: item ? sourceOf(item) : undefined });
  }
  // The newest answer opens its evidence beside it once (design 9.5).
  const opened = useRef(false);
  useEffect(() => {
    if (latest && !opened.current && !inspector && sent.length) {
      opened.current = true;
      inspect({ kind: 'evidence', evidence: panel(null) });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [latest]);

  function copy() {
    const line = (c: ClaimRow) => `${c.text.replace(/E(\d+)/g, (m, n) => (byLabel.has(m) ? `[${n}]` : m))}${c.evidence.map((e) => `[${numberOf(e.label)}]`).join('')}`;
    const parts: string[] = [];
    const add = (title: string, rows: string[]) => rows.length && parts.push(`${title}：\n${rows.join('\n')}`);
    add('结论', s.conclusion.map(line));
    add('适用前提', s.assumptions);
    add('依据', s.basis.map(line));
    add('计算', s.calculations.map(line));
    add('缺失信息', s.missing_information);
    add('资料之间的矛盾', s.conflicts);
    add('下一步', s.next_steps.map(line));
    const sources = sent.map((i) => `[${labels[i.evidence_id]}] ${i.document.display_name} ${locatorLabel(i.locators)}`);
    add('引用', sources);
    navigator.clipboard.writeText(`问题：${turn.question}\n\n${parts.join('\n\n')}`).then(() => notice('ok', '已复制回答与引用'), () => notice('error', '无法写入剪贴板'));
  }

  const other = (turn.warnings ?? []).filter((w) => !['unsupported_claims', 'insufficient_evidence'].includes(w.code));
  return (
    <article className="answer" aria-label="回答">
      <div className="answer-head">
        <Icon name="robot" />
        <span>{result.connection ? `${result.connection.name} · ${result.connection.model_id}` : '生成接口'}</span>
        <span>·</span>
        <button className="btn btn-link t-caption" onClick={() => inspect({ kind: 'evidence', evidence: panel(null) })}>发送原文 {sent.length} 段</button>
        {seconds(turn) && (
          <>
            <span>·</span>
            <span>{seconds(turn)} 秒</span>
          </>
        )}
        {answer.revisions > 0 && (
          <>
            <span>·</span>
            <span>已修订 {answer.revisions} 次</span>
          </>
        )}
        <span className="spacer" />
        {state === 'supported' && <Chip tone="ok" icon="check">已核查</Chip>}
        {state === 'partial' && <Chip tone="warn" icon="highlight">部分有依据</Chip>}
        {state === 'unsupported' && <Chip tone="danger" icon="warning">有内容未找到依据</Chip>}
        {state === 'unchecked' && <Chip icon="unknown">部分未核查</Chip>}
      </div>
      <div className="answer-body">
        {answer.insufficient_evidence && <Callout tone="attention" title="依据不足">资料中没有足够的原文支持完整结论；下面说明了缺少哪些信息。可以导入补充资料，或只看找到的原文。</Callout>}
        {state === 'unsupported' && <Callout tone="danger" title="有内容未找到依据，请谨慎使用">红色虚线标出的句子在所引原文中找不到支持。</Callout>}
        {other.length > 0 && <Callout tone="warning" title="部分完成">{other.map((w) => w.message).join('；')}</Callout>}
        <Part title="结论" claims={s.conclusion} onCite={cite} byLabel={byLabel} className="conclusion" />
        <Part title="适用前提" items={s.assumptions} />
        <Part title="依据" claims={s.basis} onCite={cite} byLabel={byLabel} />
        <Part title="计算" claims={s.calculations} onCite={cite} byLabel={byLabel} />
        <Part title="缺失信息" items={s.missing_information} />
        <Part title="资料之间的矛盾" items={s.conflicts} />
        <Part title="下一步" claims={s.next_steps} onCite={cite} byLabel={byLabel} />
      </div>
      <Process steps={result.process} running={false} />
      <div className="answer-foot">
        <button className="btn btn-xs btn-subtle" onClick={copy}>
          <Icon name="copy" />
          复制
        </button>
        <ExportButton resultRefs={async () => [turn.resultRef!]} name={turn.question.slice(0, 30)} label="导出依据" small />
        <span style={{ flex: 1 }} />
        <span className="t-caption text-tertiary" title={answer.verification_note}>结论只基于你的资料，不构成法律意见</span>
      </div>
    </article>
  );
}

function Part({ title, claims, items, onCite, byLabel, className }: { title: string; claims?: ClaimRow[]; items?: string[]; onCite?: (evidence: string) => void; byLabel?: Map<string, string>; className?: string }) {
  const rows = claims ?? [];
  const texts = items ?? [];
  if (!rows.length && !texts.length) return null;
  const body = claims
    ? rows.map((c) => <Claim key={c.claim_id} claim={c} onCite={onCite!} byLabel={byLabel ?? new Map()} />)
    : texts.map((t, i) => <React.Fragment key={i}>{t}</React.Fragment>);
  return (
    <section className={`answer-part${className ? ` ${className}` : ''}`}>
      <h3>{title}</h3>
      {body.length === 1 ? <p>{body[0]}</p> : <ul>{body.map((b, i) => <li key={i}>{b}</li>)}</ul>}
    </section>
  );
}

/** Evidence labels the model wrote into the sentence itself ("依据E4")
 *  become citation markers; unknown labels stay as plain text. */
function inlineCites(text: string, byLabel: Map<string, string>, onCite: (evidence: string) => void): React.ReactNode {
  const out: React.ReactNode[] = [];
  let at = 0;
  for (const m of text.matchAll(/E(\d+)/g)) {
    const evidence = byLabel.get(m[0]);
    if (!evidence || m.index === undefined) continue;
    out.push(text.slice(at, m.index));
    out.push(<Cite key={m.index} label={m[1]} onOpen={() => onCite(evidence)} />);
    at = m.index + m[0].length;
  }
  if (!out.length) return text;
  out.push(text.slice(at));
  return out;
}

function Claim({ claim, onCite, byLabel }: { claim: ClaimRow; onCite: (evidence: string) => void; byLabel: Map<string, string> }) {
  let text: React.ReactNode = inlineCites(claim.text, byLabel, onCite);
  if (claim.status === 'unsupported') {
    text = <span className="claim-unsupported" title={`未找到依据：${claim.message || '所引原文不支持这句话'}`}>{claim.text}</span>;
  } else if (claim.status === 'partial' && claim.flagged?.length) {
    const pieces: React.ReactNode[] = [];
    let at = 0;
    for (const [i, f] of claim.flagged.entries()) {
      if (f.utf16_start > at) pieces.push(claim.text.slice(at, f.utf16_start));
      pieces.push(<span key={i} className="claim-partial" title={`部分有依据：${claim.message || '这部分在所引原文中没有找到'}`}>{claim.text.slice(f.utf16_start, f.utf16_end)}</span>);
      at = Math.max(at, f.utf16_end);
    }
    if (at < claim.text.length) pieces.push(claim.text.slice(at));
    text = pieces;
  } else if (claim.status === 'partial') {
    text = <span className="claim-partial" title={`部分有依据：${claim.message ?? ''}`}>{claim.text}</span>;
  }
  return (
    <span className="claim">
      {text}
      {claim.evidence.map((e) => (
        <Cite key={e.evidence_id} label={numberOf(e.label)} onOpen={() => onCite(e.evidence_id)} />
      ))}
      {claim.status === 'unsupported' && <span className="claim-status"><Chip tone="danger">未找到依据</Chip></span>}
      {claim.status === 'unchecked' && <span className="claim-status"><Chip title="超出核查预算或输入过长，这句话没有核查">未核查</Chip></span>}
    </span>
  );
}

function Cite({ label, onOpen }: { label: string; onOpen: () => void }) {
  return (
    <button className="cite" aria-label={`依据 ${label}，在右侧打开`} title={`依据 ${label}：在右侧打开原文`} onClick={onOpen}>
      {label}
    </button>
  );
}

const STEP_TEXT: Record<string, (s: ProcessStep) => string> = {
  plan: (s) => (s.status === 'completed' ? `理解问题并改写检索词（${s.rewrites ?? 0} 条）` : '查询规划未完成，改用原问题检索'),
  search: (s) => `关键词＋语义检索：${s.queries ?? 1} 组查询，融合 ${s.candidates ?? 0} 段`,
  rerank: (s) => `本机重排，取前 ${s.evidence ?? 0} 段`,
  draft: () => '起草回答（只依据已批准发送的原文）',
  verify: (s) => `逐句核查：${s.supported ?? 0} 句有依据${s.problems ? `，${s.problems} 处需要修订` : ''}`,
  revise: () => '按核查结果修订回答',
};

function Process({ steps, running }: { steps: ProcessStep[]; running: boolean }) {
  if (!steps.length) return null;
  const total = steps.reduce((sum, s) => sum + (s.ms ?? 0), 0) / 1000;
  const search = steps.find((s) => s.step === 'search');
  const verify = [...steps].reverse().find((s) => s.step === 'verify');
  return (
    <details className="process" open={running}>
      <summary>
        <Icon name="list" />
        执行过程：检索 {search?.candidates ?? 0} 段 → 重排 → 起草 → 核查{verify ? `（${verify.problems ? `${verify.problems} 处待改` : '通过'}）` : ''} · 共 {total.toFixed(1)} 秒
      </summary>
      <ol className="steps">
        {steps.map((s, i) => (
          <li key={i} className={s.status === 'completed' ? 'done' : s.status === 'failed' ? 'failed' : 'waiting'}>
            <span className="state" aria-hidden="true">{s.status === 'completed' ? String.fromCodePoint(0xe73e) : s.status === 'failed' ? String.fromCodePoint(0xe711) : ''}</span>
            <span>{(STEP_TEXT[s.step] ?? (() => s.step))(s)}</span>
            <span className="t-caption text-tertiary">{s.ms !== undefined ? `${(s.ms / 1000).toFixed(1)} 秒` : ''}</span>
          </li>
        ))}
      </ol>
    </details>
  );
}

// ------------------------------------------------------------- egress (9.7)

function EgressDialog({ preview, onCancel, onConfirm }: { preview: AnswerPreview; onCancel: () => void; onConfirm: (excluded: string[]) => Promise<void> }) {
  const [excluded, setExcluded] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const sending = preview.excerpts.length - excluded.size;
  const toggle = (id: string) => setExcluded((set) => {
    const next = new Set(set);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    return next;
  });
  return (
    <Modal
      title="发送前预览"
      icon="globe"
      wide
      onClose={busy ? () => {} : onCancel}
      footer={
        <>
          <span className="t-caption text-tertiary" style={{ marginRight: 'auto' }}>将发送至 {preview.connection.host}</span>
          <button className="btn" onClick={onCancel} disabled={busy}>取消</button>
          <button
            className="btn btn-primary"
            disabled={busy || sending === 0}
            onClick={async () => {
              setBusy(true);
              await onConfirm([...excluded]);
              setBusy(false);
            }}
          >
            {busy ? '等待系统确认…' : '继续，打开系统确认'}
          </button>
        </>
      }
    >
      <Callout title="以下内容将发送到你配置的生成接口，用于生成这一次回答">可以取消勾选不想发送的段落。继续后会弹出系统确认框，确认后才会发送。</Callout>
      <dl className="kv">
        <dt>接收方</dt>
        <dd>{preview.connection.name}（{preview.connection.host}）· 模型 {preview.connection.model_id}</dd>
        <dt>用途</dt>
        <dd>生成这一次回答：改写检索词、起草，并按本机核查结果修订（不包含联网搜索）</dd>
        <dt>有效期</dt>
        <dd>仅本次回答；之后再发送需要重新确认</dd>
        <dt>上限</dt>
        <dd>最多 {preview.max_requests} 次请求；每次输出不超过 {preview.max_output_tokens.toLocaleString('zh-CN')} 个令牌；原文最多 {preview.max_items} 段</dd>
      </dl>
      <div>
        <div className="t-subtitle" style={{ marginBottom: 4 }}>将发送的内容（{sending + 1} 项）</div>
        <div className="send-item">
          <Icon name="chat" />
          <div>
            <div className="t-strong">你的问题</div>
            <div className="text-secondary">{preview.question}</div>
          </div>
          <Chip>必需</Chip>
        </div>
        {preview.excerpts.length === 0 && <p className="text-secondary">所选范围内没有检索到相关原文；继续时只会发送问题，回答会说明依据不足。</p>}
        {preview.excerpts.map((e) => (
          <div key={e.evidence_id} className={`send-item${excluded.has(e.evidence_id) ? ' excluded' : ''}`}>
            <Icon name="document" />
            <div style={{ minWidth: 0 }}>
              <div className="t-strong ellipsis">{[e.display_name, locatorLabel(e.locators ?? [])].filter(Boolean).join(' · ')}</div>
              <div className="text-secondary excerpt-text">{e.excerpt}</div>
            </div>
            <label className="check t-caption">
              <input type="checkbox" checked={!excluded.has(e.evidence_id)} onChange={() => toggle(e.evidence_id)} />
              发送
            </label>
          </div>
        ))}
      </div>
      <p className="t-caption text-tertiary">
        不会发送：原件与页面图、其他页面、事实与金额记录、你的姓名与空间名称。改写检索词后可能在同一范围内换用其他段落（同样不超过 {preview.max_items} 段，取消勾选的段落不会发送）。已发送的内容无法从服务方撤回。
      </p>
    </Modal>
  );
}
