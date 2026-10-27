// Q&A and retrieval (design 9.5, 9.7): scope and mode, the question thread,
// structured answers with citations and per-sentence grounding status, the
// narrow roles' contributions and the checker's challenges, native media
// observations with the local comparison, the execution process, the two-step
// egress approval, and interrupted tasks that can continue after a new
// confirmation.
import React, { useContext, useEffect, useId, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import {
  api, type AnswerPreview, type ChallengeSummary, type ClaimRow, type EvidenceItem, type Feedback, type InterruptedFlow, type ObservationSummary, type ProcessStep,
  type RetrievalResult, type RoleSummary, type TaskPlanSummary, type TaskState,
} from './api';
import { CalculationSummary } from './analysis';
import { QaHistory } from './qa-history';
import { EgressDialog } from './egress';
import { EvidenceList, locatorLabel } from './evidence';
import { ExportButton } from './export';
import { REGIONS } from './regions';
import { Icon } from './icons';
import { sourceOf, useSession, type EvidencePanel } from './session';
import { generates, KIND_LABEL, kindOfRun, newTurnId, ROLE_TITLE, type QaTurn, type TurnKind } from './turns';
import { Busy, Callout, Chip, ErrorLine, Field, FieldGroup, Segmented, useAction, useDismiss, useLoad, useNotice } from './ui';

export type { QaTurn } from './turns';

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
      if (task.status === 'interrupted') {
        return { ...turn, state: 'interrupted' as const, status: task.status, run: envelope?.flow_run, warnings: envelope?.warnings ?? [], finished: Date.now() };
      }
      const state: QaTurn['state'] = task.status === 'cancelled' ? 'cancelled' : envelope?.data_ref ? 'done' : 'failed';
      return { ...turn, state, status: task.status, warnings: envelope?.warnings ?? [], resultRef: envelope?.data_ref ?? null, finished: Date.now(), error: state === 'failed' ? envelope?.warnings.map((w) => w.message).join('；') || '没有完成' : undefined };
    });
    return changed ? next : turns;
  });
}

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));
type Mode = 'answer' | 'deep' | 'evidence';

export function QaPage({ initialMatters, initialQuestion }: { initialMatters?: string[]; initialQuestion?: string }) {
  const { matters, connection, qa, go } = useSession();
  const [scope, setScope] = useState<string[]>(initialMatters ?? []);
  const [mode, setMode] = useState<Mode>(connection ? 'answer' : 'evidence');
  const modeTouched = useRef(false);
  const [text, setText] = useState(initialQuestion ?? '');
  const [preview, setPreview] = useState<{ turn: string; data: AnswerPreview } | null>(null);
  const [picking, setPicking] = useState(false);
  const [history, setHistory] = useState(false);
  const [graphEnabled,setGraphEnabled]=useState(false);
  const [asOf,setAsOf]=useState('');
  const [region,setRegion]=useState('CN-mainland');
  const policy={graph_enabled:graphEnabled,as_of:asOf||null,region:region||null};
  const thread = useRef<HTMLDivElement>(null);
  const composer = useRef<HTMLTextAreaElement>(null);
  const notice = useNotice();
  const chosen = scope.length ? scope : matters.map((m) => m.id);
  const effectiveMode: Mode = connection ? mode : 'evidence';
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

  async function submit(value = text, asMode: Mode = effectiveMode, within = chosen) {
    const question = value.trim();
    if (!question || !within.length || running) return;
    const turn: QaTurn = { id: newTurnId(), question, matters: within, kind: asMode, state: 'preparing', started: Date.now() };
    qa.update((turns) => [...turns, turn]);
    setText('');
    try {
      if (asMode === 'evidence') {
        const questionId = await api.object('question', { text: question });
        const scopeId = await api.object('scope', { matters: within, versions: [],policy });
        const task = await api.start('lvjin-retrieval', { question_id: questionId, scope_id: scopeId, known_constraint_ids: [], requested_output: 'evidence_only' });
        patch(turn.id, { state: 'running', task });
      } else {
        // Local retrieval only; nothing leaves the computer before approval.
        pending.current = { turn: turn.id, grant: null };
        const data = await api.answerPreview(question, within, connection?.id ?? null, asMode === 'deep',policy);
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

  async function approve(excluded: string[], excludedMedia: string[]) {
    if (!preview) return;
    const { turn, data } = preview;
    pending.current = null;
    try {
      const task = await api.answerStart(data.grant_id, excluded, excludedMedia);
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

  /** A suggested follow-up goes into the composer; sending still previews first. */
  function ask(question: string) {
    setText(question);
    window.requestAnimationFrame(() => composer.current?.focus());
  }

  /** Continue an interrupted run (the core opens a native confirmation). */
  async function resume(run: string, question: string | null, kind: TurnKind) {
    try {
      const task = await api.flowContinue(run);
      const existing = qa.turns.find((t) => t.run === run);
      if (existing) patch(existing.id, { state: 'running', task, error: undefined });
      else qa.update((turns) => [...turns, { id: newTurnId(), question: question ?? '（中断的任务）', matters: [], kind, state: 'running', started: Date.now(), task, run, source: '继续中断的任务' }]);
      return true;
    } catch (e) {
      notice('error', message(e));
      return false;
    }
  }

  async function abandon(run: string) {
    try {
      await api.flowDiscard(run);
      qa.update((turns) => turns.map((t) => (t.run === run ? { ...t, state: 'cancelled', error: '已放弃这个中断的任务' } : t)));
    } catch (e) {
      notice('error', message(e));
    }
  }

  return (
    <div className="qa">
      {history && <QaHistory initialMatter={scope.length === 1 ? scope[0] : undefined} onClose={() => setHistory(false)} onOpen={(item) => {
        qa.update((turns) => [...turns.filter((turn) => turn.resultRef !== item.id), turns.find((turn) => turn.resultRef === item.id) ?? {
          id: `history-${item.id}`, question: item.question, matters: item.matters, kind: item.kind, state: 'done',
          started: Date.parse(item.created_at), resultRef: item.id, source: `历史记录 · ${new Date(item.created_at).toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai' })}`,
        }]);
        setHistory(false);
        requestAnimationFrame(() => document.querySelector('.qa-thread .turn:last-of-type')?.scrollIntoView({ block: 'start' }));
      }} />}
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
        <button className="btn btn-sm" onClick={() => setHistory(true)}><Icon name="history" />历史记录</button>
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
          items={[['answer', '生成回答', !connection], ['deep', '深入分析', !connection], ['evidence', '只找原文']]}
        />
      </div>
      <details className="qa-retrieval-options"><summary>检索条件{graphEnabled?' · 已开启关系扩展':''}{asOf?` · 截至 ${asOf}`:''}</summary><div className="inline-form">
        <Field label="适用日期（留空按今天）"><input type="date" value={asOf} disabled={!!running} onChange={(e)=>setAsOf(e.target.value)} /></Field>
        <Field label="公共资料地域"><select value={region} disabled={!!running} onChange={(e)=>setRegion(e.target.value)}><option value="">不限定地域，逐项核对</option>{REGIONS.map(([id,name])=><option value={id} key={id}>{name}</option>)}</select></Field>
        <label className="check"><input type="checkbox" checked={graphEnabled} disabled={!!running} onChange={(e)=>setGraphEnabled(e.target.checked)} />沿有来源的关系补充原文（最多两跳）</label>
      </div><p className="t-caption text-secondary">关系扩展默认关闭。启用后仍共用每轮候选与核查额度；日期和地域只筛选已标注的资料，不自动认定个案适用性。缺少历史版本时会保留信息缺口。</p></details>
      <div className="qa-thread" ref={thread}>
        <InterruptedRuns turns={qa.turns} onResume={resume} onDiscard={abandon} />
        {qa.turns.length === 0 && <Intro hasConnection={!!connection} />}
        {qa.turns.map((turn, index) => (
          <TurnView
            key={turn.id}
            turn={turn}
            latest={index === qa.turns.length - 1}
            onStop={stop}
            onRetry={(asMode) => submit(turn.question, asMode, turn.matters)}
            onAsk={ask}
            onResume={() => turn.run && resume(turn.run, turn.question, turn.kind)}
            onDiscard={() => turn.run && abandon(turn.run)}
          />
        ))}
      </div>
      <div className="composer">
        <div className="composer-box">
          <textarea
            ref={composer}
            rows={2}
            value={text}
            maxLength={2000}
            placeholder={effectiveMode === 'evidence' ? '输入要查找的内容…（Ctrl+Enter 检索）' : '输入问题…（Ctrl+Enter 发送）'}
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
            <button className="btn btn-primary btn-icon" aria-label={effectiveMode === 'evidence' ? '检索' : '发送'} title="Ctrl+Enter" onClick={() => submit()} disabled={!text.trim() || !chosen.length}>
              <Icon name={effectiveMode === 'evidence' ? 'search' : 'send'} />
            </button>
          )}
        </div>
        <div className="composer-meta">
          {effectiveMode !== 'evidence' && connection ? (
            <>
              <Icon name="globe" className="cap-outbound" />
              <span>
                {effectiveMode === 'deep' ? '深入分析会安排至多 3 个专业角色并由检查者复核，最多 7 次请求；' : ''}发送前会显示将发往 {connection.host} 的原文，并请你确认
              </span>
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
          ? '回答由你连接的生成接口起草，只依据所选事务中检索到的原文；每一句都会在本机核对依据，发送前先请你确认。复杂问题可以选“深入分析”，由几个专业角色分别分析后再综合。'
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

/** Runs stopped by a lock or an exit, listed by the core after unlocking. */
function InterruptedRuns({ turns, onResume, onDiscard }: { turns: QaTurn[]; onResume: (run: string, question: string | null, kind: TurnKind) => Promise<boolean>; onDiscard: (run: string) => Promise<void> }) {
  const [rows, setRows] = useState<InterruptedFlow[]>([]);
  const shown = new Set(turns.filter((t) => t.state === 'interrupted' && t.run).map((t) => t.run));
  const signature = turns.map((t) => `${t.id}:${t.state}`).join('|');
  const load = () => api.flowsInterrupted().then(setRows).catch(() => setRows([]));
  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [signature]);
  const hidden = rows.filter((r) => !shown.has(r.run_id));
  if (!hidden.length) return null;
  return (
    <Callout tone="attention" title={`有 ${hidden.length} 个任务停在中断处`}>
      <ul className="interrupted">
        {hidden.map((row) => (
          <li key={row.run_id}>
            <span className="ellipsis">{KIND_LABEL[kindOfRun(row.flow, row.skill)]}：{row.question ?? '（没有问题文本）'}</span>
            <span className="t-caption text-tertiary">
              {row.stale ? '资料已变化，不能从中断处继续' : row.unknown_requests ? `${row.unknown_requests} 次请求结果未知` : '已完成的步骤不会重复'}
            </span>
            {!row.stale && <button className="btn btn-xs" onClick={async () => { if (await onResume(row.run_id, row.question, kindOfRun(row.flow, row.skill))) load(); }}>继续</button>}
            <button className="btn btn-xs btn-subtle" onClick={async () => { await onDiscard(row.run_id); load(); }}>放弃</button>
          </li>
        ))}
      </ul>
    </Callout>
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

const RUNNING_TEXT: Record<TurnKind, string> = {
  answer: '正在生成：理解问题 → 检索 → 起草 → 逐句核查',
  deep: '正在深入分析：安排专业角色 → 分别分析 → 综合起草 → 逐句核查 → 证据检查',
  evidence: '正在本机检索与重排…',
  contract: '正在书面分析：本机检索 → 法律与金融角色分析 → 综合起草 → 核查与检查',
  loan: '正在书面分析：本机对账与检索 → 金融与法律角色分析 → 综合起草 → 核查与检查',
  budget: '正在书面分析：本机预算计算 → 金融角色分析 → 综合起草 → 核查与检查',
  image: '正在分析图片：本机识别 → 直接读取图片 → 与识别文字核对 → 起草与核查',
  audio: '正在分析录音：本机转写 → 直接听取所选时段 → 与转写核对 → 起草与核查',
  video: '正在分析视频：抽帧识别与转写 → 直接读取画面与音轨 → 核对 → 起草与核查',
};

function TurnView({ turn, latest, onStop, onRetry, onAsk, onResume, onDiscard }: {
  turn: QaTurn; latest: boolean; onStop: () => void; onRetry: (mode: Mode) => void; onAsk: (question: string) => void; onResume: () => void; onDiscard: () => void;
}) {
  const { matters } = useSession();
  const ref = useRef<HTMLElement>(null);
  const names = turn.source ?? (turn.matters.length === matters.length && matters.length > 1 ? '全部事务' : turn.matters.map((id) => matters.find((m) => m.id === id)?.title ?? '事务').join('、'));
  const retryable = turn.kind === 'answer' || turn.kind === 'deep' || turn.kind === 'evidence';
  // The newest question and the start of its result stay in view.
  useEffect(() => {
    if (latest) ref.current?.scrollIntoView({ block: 'start' });
  }, [latest, turn.state]);
  return (
    <section className="turn" aria-label="一次问答" ref={ref}>
      <div className="question">
        <div className="bubble">
          {turn.question}
          <div className="t-caption text-tertiary">{KIND_LABEL[turn.kind]}{names ? ` · ${names}` : ''}</div>
        </div>
      </div>
      {turn.state === 'preparing' && (
        <div className="answer running">
          <div className="answer-empty">
            <Busy label={turn.kind === 'evidence' ? '正在本机检索与重排…' : '正在本机检索可以发送的原文…（首次加载模型可能需要几分钟）'} />
          </div>
        </div>
      )}
      {turn.state === 'running' && (
        <div className="answer running">
          <div className="answer-empty row-between">
            <Busy label={`${RUNNING_TEXT[turn.kind]}${turn.sent !== undefined && generates(turn.kind) ? `（已批准发送 ${turn.sent} 段原文）` : ''}`} />
            <button className="btn btn-sm" onClick={onStop}>
              <Icon name="stop" />
              停止
            </button>
          </div>
        </div>
      )}
      {turn.state === 'interrupted' && (
        <Callout
          tone="attention"
          title="任务已中断"
          action={
            <>
              <button className="btn btn-sm" onClick={onResume} disabled={!turn.run}>继续</button>
              <button className="btn btn-sm btn-subtle" onClick={onDiscard} disabled={!turn.run}>放弃</button>
            </>
          }
        >
          空间锁定或程序退出打断了这个任务。继续时已完成的步骤不会重复，并会再次请你确认发送。
        </Callout>
      )}
      {turn.state === 'failed' && (
        <Callout tone="danger" title="没有完成" action={retryable ? <button className="btn btn-sm" onClick={() => onRetry(turn.kind as Mode)}>重试</button> : undefined}>
          {turn.error}
        </Callout>
      )}
      {turn.state === 'cancelled' && (
        <Callout
          title={turn.error ?? '已停止'}
          action={turn.kind === 'answer' || turn.kind === 'deep' ? <button className="btn btn-sm" onClick={() => onRetry('evidence')}>只看本机原文</button> : undefined}
        >
          {turn.kind === 'answer' || turn.kind === 'deep' ? '可以调整问题或范围后重新提问；只找原文不会发送任何内容。' : undefined}
        </Callout>
      )}
      {turn.state === 'done' && turn.resultRef && <TurnResult turn={turn} latest={latest} onAsk={onAsk} />}
    </section>
  );
}

type ReviewPayload = {
  answer?: RetrievalResult['answer']; analysis?: string | null; retrieval?: string | null; evidence_read?: string | null; calculation?: string | null;
  roles?: RoleSummary[]; challenge?: ChallengeSummary | null; process?: ProcessStep[]; observations?: ObservationSummary[]; visual_check?: string; direct_audio_check?: string;
  flow_run?: string | null; usage?: RetrievalResult['usage']; connection?: RetrievalResult['connection']; created_at?: string;
};

/** Plain Q&A results are retrieval results; analyses and media reviews are
 *  review results whose answer, evidence and calculation are gathered here. */
async function loadResult(turn: QaTurn): Promise<RetrievalResult> {
  if (turn.kind === 'answer' || turn.kind === 'deep' || turn.kind === 'evidence') return api.result<RetrievalResult>(turn.resultRef!);
  const review = await api.result<ReviewPayload>(turn.resultRef!);
  const items = review.retrieval ? (await api.result<RetrievalResult>(review.retrieval)).items : review.evidence_read ? (await api.result<{ items: EvidenceItem[] }>(review.evidence_read)).items : [];
  const child = review.analysis ? await api.result<RetrievalResult>(review.analysis) : null;
  const calculation = review.calculation ? (await api.result<{ result: Record<string, unknown> }>(review.calculation)).result : null;
  return {
    question: turn.question, items, answer: review.answer ?? null, process: review.process ?? [], query_rewrites: [], plan: { fallback: true, rewrites: [], missing_facts: [] },
    usage: review.usage ?? child?.usage ?? { requests: 0, input_tokens: 0, output_tokens: 0 }, connection: review.connection ?? child?.connection ?? null, requested_output: 'answer', evidence_bundle: null,
    roles: review.roles ?? [], challenge: review.challenge ?? null, task_plan: null, observations: review.observations ?? [], calculation,
    visual_check: review.visual_check, direct_audio_check: review.direct_audio_check, flow_run: review.flow_run, created_at: review.created_at,
  };
}

function TurnResult({ turn, latest, onAsk }: { turn: QaTurn; latest: boolean; onAsk: (question: string) => void }) {
  const result = useLoad(() => loadResult(turn), [turn.resultRef]);
  if (result.error) return <ErrorLine text={result.error} />;
  if (!result.data) return <Busy label="正在打开结果…" />;
  return result.data.answer ? <AnswerCard turn={turn} result={result.data} latest={latest} onAsk={onAsk} /> : <EvidenceCard turn={turn} result={result.data} />;
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
        {turn.kind === 'evidence' && <Chip icon="shield">未发送任何内容</Chip>}
      </div>
      <div className="answer-body">
        {needsGeneration && (
          <Callout title="生成回答需要连接并授权生成接口" action={<button className="btn btn-sm" onClick={() => go({ name: 'connections' })}>连接生成接口</button>}>
            下面是本机找到的原文依据，请对照原文阅读。
          </Callout>
        )}
        {other.length > 0 && <Callout tone="warning" title="部分完成">{other.map((w) => w.message).join('；')}</Callout>}
        {result.observations && result.observations.length > 0 && <Observations rows={result.observations} visual={result.visual_check} audio={result.direct_audio_check} />}
        {result.calculation && <CalculationSummary result={result.calculation} />}
        {result.items.length === 0 ? (
          result.process.some((p) => p.status !== 'completed') ? <Callout tone="warning" title="检索未完成">请查看处理过程和上方失败原因；当前结果不能用于判断已导入资料是否缺少正文。</Callout> : <Callout tone="attention" title="依据不足">所选范围内没有检索到相关原文。可以换个说法、扩大范围，或导入补充资料。</Callout>
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

const ISSUE_LABEL: Record<Feedback['issue'], string> = { unsupported: '引用不支持这句话', wrong_source: '引用的位置不对', contradiction: '与原文矛盾', other: '其他问题' };
const clip = (text: string, n: number) => (text.length > n ? `${text.slice(0, n)}…` : text);

/** The local check of one sentence, as plain text for copies. */
const checkMark = (c: ClaimRow) => (c.status === 'unsupported' ? '（未找到依据）' : c.status === 'partial' ? '（部分有依据）' : c.status === 'unchecked' ? '（未核查）' : '');

/** Explicit label for generated text that leaves the app (GB 45438-2025):
 *  generated by AI, by which service and model, when, and how it was checked. */
function aiLabel(connection: RetrievalResult['connection'], at: string | number | undefined): string {
  const by = connection ? `${connection.name}（${connection.host}）的模型 ${connection.model_id}` : '你连接的生成接口';
  const when = at ? new Date(at).toLocaleString('zh-CN', { hour12: false }) : '';
  return `【AI 生成】以下内容由人工智能生成：${by}${when ? `，生成于 ${when}` : ''}。只依据所选资料中的原文，并已在本机逐句核对依据；标有“未找到依据”“部分有依据”“未核查”的句子请谨慎使用。不构成法律或财务意见。`;
}

/** Evidence of the answer being read, for the citation previews. */
const CiteItems = React.createContext<Map<string, EvidenceItem>>(new Map());

/** Where a citation preview goes: below the marker, or above it near the bottom. */
function previewPlace(rect: DOMRect): React.CSSProperties {
  const width = Math.min(360, window.innerWidth - 16);
  const left = Math.max(8, Math.min(rect.left - 16, window.innerWidth - width - 8));
  return rect.bottom + 240 < window.innerHeight ? { left, top: rect.bottom + 6, width } : { left, bottom: window.innerHeight - rect.top + 6, width };
}

function AnswerCard({ turn, result, latest, onAsk }: { turn: QaTurn; result: RetrievalResult; latest: boolean; onAsk: (question: string) => void }) {
  const { inspect, inspector } = useSession();
  const notice = useNotice();
  const [flagging, setFlagging] = useState(false);
  const notes = useLoad(() => api.view<Feedback[]>('feedback', turn.resultRef!), [turn.resultRef]);
  const answer = result.answer!;
  const s = answer.sections;
  const labels = useMemo(() => Object.fromEntries(answer.labels.map((l) => [l.evidence_id, numberOf(l.label)])), [answer.labels]);
  const byLabel = useMemo(() => new Map(answer.labels.map((l) => [l.label, l.evidence_id])), [answer.labels]);
  const byId = useMemo(() => new Map(result.items.map((i) => [i.evidence_id, i])), [result.items]);
  const sent = useMemo(() => answer.labels.map((l) => byId.get(l.evidence_id)).filter((i): i is EvidenceItem => !!i), [answer.labels, byId]);
  const claims = [...s.conclusion, ...s.basis, ...s.calculations];
  const state = claims.some((c) => c.status === 'unsupported') ? 'unsupported' : claims.some((c) => c.status === 'partial') ? 'partial' : claims.some((c) => c.status === 'unchecked') ? 'unchecked' : 'supported';
  const texts = useMemo(() => new Map([...s.conclusion, ...s.basis, ...s.calculations, ...s.next_steps].map((c) => [c.claim_id, c.text])), [s]);
  const flagged = useMemo(() => new Map((notes.data ?? []).filter((n) => n.claim_id).map((n) => [n.claim_id!, ISSUE_LABEL[n.issue]])), [notes.data]);
  const noteText = (n: Feedback) => `${n.claim_id && texts.has(n.claim_id) ? `“${clip(texts.get(n.claim_id)!, 40)}”` : '整体'}：${ISSUE_LABEL[n.issue]}${n.note ? `。${n.note}` : ''}`;

  function panel(current: string | null): EvidencePanel {
    const unsent = result.items.length - sent.length;
    return { title: `依据（${sent.length}）`, note: `本次回答发送并可引用的原文${unsent > 0 ? `；另有 ${unsent} 段只在本机检索到，未发送` : ''}`, items: sent, labels, current };
  }
  function cite(evidence: string) {
    const item = sent.find((i) => i.evidence_id === evidence) ?? byId.get(evidence);
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

  /** Copies carry the explicit AI label and each sentence's check result. */
  function copy() {
    const line = (c: ClaimRow) => `${c.text.replace(/E(\d+)/g, (m, n) => (byLabel.has(m) ? `[${n}]` : m))}${c.evidence.map((e) => `[${numberOf(e.label)}]`).join('')}${checkMark(c)}`;
    const parts: string[] = [];
    const add = (title: string, rows: string[]) => rows.length && parts.push(`${title}：\n${rows.join('\n')}`);
    add('结论', s.conclusion.map(line));
    add('适用前提', s.assumptions);
    add('依据', s.basis.map(line));
    add('计算', s.calculations.map(line));
    add('缺失信息', s.missing_information);
    add('资料之间的矛盾', s.conflicts);
    add('下一步', s.next_steps.map(line));
    add('你标注的问题', (notes.data ?? []).map(noteText));
    const sources = sent.map((i) => `[${labels[i.evidence_id]}] ${i.document.display_name} ${locatorLabel(i.locators)}`);
    add('引用', sources);
    const text = `${aiLabel(result.connection, result.created_at ?? turn.finished)}\n\n问题：${turn.question}\n\n${parts.join('\n\n')}`;
    navigator.clipboard.writeText(text).then(() => notice('ok', '已复制回答与引用（含 AI 生成标识）'), () => notice('error', '无法写入剪贴板'));
  }

  async function withdraw(id: string) {
    try {
      await api.feedbackRemove(id);
      notes.reload();
    } catch (e) {
      notice('error', message(e));
    }
  }

  const other = (turn.warnings ?? []).filter((w) => !['unsupported_claims', 'insufficient_evidence'].includes(w.code));
  const followUps = latest ? (answer.follow_ups ?? []).filter(Boolean).slice(0, 3) : [];
  return (
    <CiteItems.Provider value={byId}>
      <article className="answer" aria-label="回答">
        <div className="answer-head">
          <Icon name="robot" />
          <span title="这段回答由人工智能生成，并已在本机逐句核对依据">AI 生成</span>
          <span>·</span>
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
          {(notes.data?.length ?? 0) > 0 && (
            <Callout tone="attention" icon="flag" title={`你标注了 ${notes.data!.length} 处问题`}>
              <ul className="plain-list">
                {notes.data!.map((n) => (
                  <li key={n.id}>
                    {noteText(n)}
                    <button className="btn btn-link t-caption" onClick={() => withdraw(n.id)}>撤回</button>
                  </li>
                ))}
              </ul>
            </Callout>
          )}
          <Part title="结论" claims={s.conclusion} onCite={cite} byLabel={byLabel} flagged={flagged} className="conclusion" />
          <Part title="适用前提" items={s.assumptions} />
          <Part title="依据" claims={s.basis} onCite={cite} byLabel={byLabel} flagged={flagged} />
          <Part title="计算" claims={s.calculations} onCite={cite} byLabel={byLabel} flagged={flagged} />
          <Part title="缺失信息" items={s.missing_information} />
          <Part title="资料之间的矛盾" items={s.conflicts} />
          <Part title="下一步" claims={s.next_steps} onCite={cite} byLabel={byLabel} flagged={flagged} />
          {result.calculation && <CalculationSummary result={result.calculation} />}
        </div>
        {result.observations && result.observations.length > 0 && <Observations rows={result.observations} visual={result.visual_check} audio={result.direct_audio_check} />}
        {((result.roles?.length ?? 0) > 0 || result.task_plan || result.challenge) && (
          <Roles roles={result.roles ?? []} plan={result.task_plan ?? null} challenge={result.challenge ?? null} labels={labels} onCite={cite} />
        )}
        <Process steps={result.process} running={false} />
        <div className="answer-foot">
          <button className="btn btn-xs btn-subtle" onClick={copy}>
            <Icon name="copy" />
            复制
          </button>
          <ExportButton resultRefs={async () => [turn.resultRef!]} name={turn.question.slice(0, 30)} label="导出" small />
          <button className="btn btn-xs btn-subtle" aria-expanded={flagging} onClick={() => setFlagging(!flagging)}>
            <Icon name="flag" />
            依据有误？
          </button>
          <span style={{ flex: 1 }} />
          <span className="t-caption text-tertiary" title={answer.verification_note}>结论只基于你的资料，不构成法律意见</span>
        </div>
        {flagging && (
          <FeedbackForm
            result={turn.resultRef!}
            sections={[['结论', s.conclusion], ['依据', s.basis], ['计算', s.calculations], ['下一步', s.next_steps]]}
            onClose={() => setFlagging(false)}
            onSaved={() => {
              setFlagging(false);
              notes.reload();
            }}
          />
        )}
      </article>
      {followUps.length > 0 && (
        <div className="followups" role="group" aria-label="追问建议">
          <span className="t-caption text-tertiary">追问：</span>
          {followUps.map((q) => (
            <button key={q} title="填入输入框；发送前仍会预览并请你确认" onClick={() => onAsk(q)}>
              {q}
            </button>
          ))}
        </div>
      )}
    </CiteItems.Provider>
  );
}

/** "依据有误？": the user's own note on one sentence or the whole answer.
 *  It stays in this vault, is never sent, and goes with copies and exports. */
function FeedbackForm({ result, sections, onClose, onSaved }: { result: string; sections: [string, ClaimRow[]][]; onClose: () => void; onSaved: () => void }) {
  const [claim, setClaim] = useState('');
  const [issue, setIssue] = useState<Feedback['issue']>('unsupported');
  const [note, setNote] = useState('');
  const { busy, run } = useAction();
  const ref = useRef<HTMLFormElement>(null);
  useEffect(() => {
    ref.current?.querySelector<HTMLElement>('select')?.focus();
  }, []);
  async function save(e: React.FormEvent) {
    e.preventDefault();
    const done = await run(() => api.object('feedback', { result, claim_id: claim || null, issue, note: note.trim() }), '已记下这条标注');
    if (done) onSaved();
  }
  return (
    <form
      className="feedback-form"
      ref={ref}
      onSubmit={save}
      aria-label="标注依据问题"
      onKeyDown={(e) => {
        if (e.key === 'Escape') {
          e.stopPropagation();
          onClose();
        }
      }}
    >
      <Field label="哪一句有问题">
        <select value={claim} onChange={(e) => setClaim(e.target.value)}>
          <option value="">整体或多处</option>
          {sections.filter(([, rows]) => rows.length > 0).map(([title, rows]) => (
            <optgroup key={title} label={title}>
              {rows.map((c) => (
                <option key={c.claim_id} value={c.claim_id}>{clip(c.text, 40)}</option>
              ))}
            </optgroup>
          ))}
        </select>
      </Field>
      <FieldGroup label="问题">
        <Segmented label="问题" value={issue} onChange={setIssue} items={[['unsupported', '引用不支持'], ['wrong_source', '位置不对'], ['contradiction', '与原文矛盾'], ['other', '其他']]} />
      </FieldGroup>
      <Field label="说明（可选）" hint="标注只保存在本机加密空间，不会发送给任何服务；复制和导出这个回答时一并附上。">
        <textarea rows={2} maxLength={500} value={note} onChange={(e) => setNote(e.target.value)} />
      </Field>
      <div className="actions end">
        <button type="button" className="btn btn-sm" onClick={onClose}>取消</button>
        <button className="btn btn-sm btn-primary" disabled={busy}>保存标注</button>
      </div>
    </form>
  );
}

function Part({ title, claims, items, onCite, byLabel, flagged, className }: {
  title: string; claims?: ClaimRow[]; items?: string[]; onCite?: (evidence: string) => void; byLabel?: Map<string, string>; flagged?: Map<string, string>; className?: string;
}) {
  const rows = claims ?? [];
  const texts = items ?? [];
  if (!rows.length && !texts.length) return null;
  const body = claims
    ? rows.map((c) => <Claim key={c.claim_id} claim={c} onCite={onCite!} byLabel={byLabel ?? new Map()} flag={flagged?.get(c.claim_id)} />)
    : texts.map((t, i) => <React.Fragment key={i}>{t}</React.Fragment>);
  return (
    <section className={`answer-part${className ? ` ${className}` : ''}`}>
      <h3>{title}</h3>
      {body.length === 1 ? claims ? body[0] : <p>{body[0]}</p> : <ul>{body.map((b, i) => <li key={i}>{b}</li>)}</ul>}
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
    out.push(<Cite key={m.index} label={m[1]} evidence={evidence} onOpen={() => onCite(evidence)} />);
    at = m.index + m[0].length;
  }
  if (!out.length) return text;
  out.push(text.slice(at));
  return out;
}

function Claim({ claim, onCite, byLabel, flag }: { claim: ClaimRow; onCite: (evidence: string) => void; byLabel: Map<string, string>; flag?: string }) {
  let text: React.ReactNode = inlineCites(claim.text, byLabel, onCite);
  if (claim.status === 'unsupported') {
    text = <span className="claim-unsupported" title={`未找到依据：${claim.message || '所引原文不支持这句话'}`}>{claim.text}</span>;
  } else if (claim.status === 'partial' && claim.flagged?.length) {
    const pieces: React.ReactNode[] = [];
    let at = 0;
    for (const [i, f] of claim.flagged.entries()) {
      if (f.utf16_start > at) pieces.push(claim.text.slice(at, f.utf16_start));
      pieces.push(<span key={i} className="claim-partial" title={`需要核对这一段：“${claim.text.slice(f.utf16_start, f.utf16_end)}”；核查信号不代替原文判断`}>{claim.text.slice(f.utf16_start, f.utf16_end)}</span>);
      at = Math.max(at, f.utf16_end);
    }
    if (at < claim.text.length) pieces.push(claim.text.slice(at));
    text = pieces;
  } else if (claim.status === 'partial') {
    text = <span className="claim-partial" title={`部分有依据：${claim.message ?? ''}`}>{claim.text}</span>;
  }
  return (
    <div className="claim">
      {text}
      {claim.evidence.map((e) => (
        <Cite key={e.evidence_id} label={numberOf(e.label)} evidence={e.evidence_id} onOpen={() => onCite(e.evidence_id)} />
      ))}
      {claim.status === 'unsupported' && <span className="claim-status"><Chip tone="danger">未找到依据</Chip></span>}
      {claim.status === 'unchecked' && <span className="claim-status"><Chip title="超出核查预算或输入过长，这句话没有核查">未核查</Chip></span>}
      {!!claim.arithmetic?.length && <details className="t-caption"><summary>本机算式已核验（{claim.arithmetic.length}）</summary>{claim.arithmetic.map((proof, i) => <div key={i}>{proof.equation}</div>)}<span className="text-secondary">仅核验运算与所引原文操作数，事实含义和法律适用仍需核查。</span></details>}
      {flag && <span className="claim-status"><Chip tone="warn" icon="flag" title={`你的标注：${flag}`}>已标注</Chip></span>}
    </div>
  );
}

/** Citation marker (design 8.14): after 300 ms of hover, or on keyboard
 *  focus, a preview shows the file, location and the cited passage; click or
 *  Enter opens the passage in the inspector on the right. */
function Cite({ label, evidence, onOpen }: { label: string; evidence: string; onOpen: () => void }) {
  const item = useContext(CiteItems).get(evidence);
  const [place, setPlace] = useState<React.CSSProperties | null>(null);
  const button = useRef<HTMLButtonElement>(null);
  const timer = useRef(0);
  const id = useId();
  const clear = () => window.clearTimeout(timer.current);
  const show = () => {
    if (!item) return;
    clear();
    timer.current = window.setTimeout(() => {
      const rect = button.current?.getBoundingClientRect();
      if (rect) setPlace(previewPlace(rect));
    }, 300);
  };
  const hide = (delay = 150) => {
    clear();
    timer.current = window.setTimeout(() => setPlace(null), delay);
  };
  const open = () => {
    clear();
    setPlace(null);
    onOpen();
  };
  // The preview follows its marker when the thread scrolls (the newest answer
  // keeps itself in view) and closes once the marker leaves the window;
  // scrolling elsewhere, such as the inspector, does not affect it.
  const shown = !!place;
  useEffect(() => {
    if (!shown) return;
    const follow = (e: Event) => {
      const marker = button.current;
      if (!marker || (e.target instanceof Node && e.target !== document && !e.target.contains(marker))) return;
      const rect = marker.getBoundingClientRect();
      setPlace(rect.bottom < 0 || rect.top > window.innerHeight ? null : previewPlace(rect));
    };
    const close = () => setPlace(null);
    window.addEventListener('scroll', follow, true);
    window.addEventListener('resize', close);
    return () => {
      window.removeEventListener('scroll', follow, true);
      window.removeEventListener('resize', close);
    };
  }, [shown]);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  return (
    <>
      <button
        ref={button}
        className="cite"
        aria-label={`依据 ${label}，在右侧打开`}
        aria-describedby={place ? id : undefined}
        aria-expanded={place ? true : undefined}
        onClick={open}
        onMouseEnter={show}
        onMouseLeave={() => hide()}
        onFocus={show}
        onBlur={() => hide(0)}
        onKeyDown={(e) => {
          if (e.key === 'Escape' && place) {
            e.stopPropagation();
            setPlace(null);
          }
        }}
      >
        {label}
      </button>
      {place && item && createPortal(
        <div id={id} role="tooltip" className="popover cite-card fade-in" style={place} onMouseEnter={clear} onMouseLeave={() => hide()}>
          <div className="evidence-head">
            <span className="n">{label}</span>
            <span className="t-strong ellipsis" title={item.document.display_name}>{item.document.display_name}</span>
          </div>
          <div className="t-caption text-tertiary">{[locatorLabel(item.locators), `第 ${item.document.version_no} 版`].filter(Boolean).join(' · ')}</div>
          <Excerpt item={item} />
          <button className="btn btn-xs" tabIndex={-1} onMouseDown={(e) => e.preventDefault()} onClick={open}>
            <Icon name="openPane" />
            在右侧打开
          </button>
        </div>,
        document.body,
      )}
    </>
  );
}

/** The cited passage in a little of its context, the cited part highlighted. */
function Excerpt({ item }: { item: EvidenceItem }) {
  const parts = item.source_parts?.filter((p) => ['frame_ocr', 'audio_asr'].includes(p.kind)) ?? [];
  if (parts.length) return <div className="quote">{parts.map((p, i) => <p key={i}><strong>{p.label}</strong><br />{p.text.slice(0, 200)}{p.text.length > 200 ? '…' : ''}</p>)}</div>;
  const exact = item.exact_text.trim();
  const context = item.text || exact;
  const at = exact ? context.indexOf(exact) : -1;
  if (at < 0) return <div className="quote"><mark>{clip(exact, 220)}</mark></div>;
  const from = Math.max(0, at - 60);
  const to = Math.min(context.length, at + exact.length + 60);
  return (
    <div className="quote">
      {from > 0 ? '…' : ''}
      {context.slice(from, at)}
      <mark>{clip(exact, 220)}</mark>
      {context.slice(at + exact.length, to)}
      {to < context.length ? '…' : ''}
    </div>
  );
}

/** A planner goal names passages by the planner's labels (E1, E2…); they
 *  are shown as the same citations the answer uses. */
function GoalText({ text, taskLabels, labels, onCite }: { text: string; taskLabels: Record<string, string>; labels: Record<string, string>; onCite: (evidence: string) => void }) {
  return (
    <>
      {text.split(/(?<![A-Za-z])(E\d{1,2})(?!\d)/).map((part, i) => {
        if (i % 2 === 0) return part;
        const id = taskLabels[part];
        return id && labels[id] ? <Cite key={i} label={labels[id]} evidence={id} onOpen={() => onCite(id)} /> : null;
      })}
    </>
  );
}

const ISSUE: Record<string, string> = { unsupported: '缺乏依据', contradiction: '与证据矛盾', missing_evidence: '缺少证据', calculation: '计算问题', scope: '超出资料范围' };

/** The roles' candidate analyses, the plan and the checker's challenges:
 *  inputs to the final answer, never evidence themselves. */
function Roles({ roles, plan, challenge, labels, onCite }: { roles: RoleSummary[]; plan: TaskPlanSummary | null; challenge: ChallengeSummary | null; labels: Record<string, string>; onCite: (evidence: string) => void }) {
  const challenges = challenge?.challenges ?? [];
  return (
    <details className="roles">
      <summary>
        <Icon name="permissions" />
        专业角色分析（{roles.length}）{challenges.length ? ` · 检查者质疑 ${challenges.length} 条` : challenge ? ' · 检查者未提出质疑' : ''}
      </summary>
      <p className="t-caption text-tertiary">各角色的意见只是候选分析，由结果综合者对照原文采纳；最终回答已逐句核查。</p>
      {plan && plan.tasks.length > 0 && (
        <section className="role-block">
          <h4>任务安排</h4>
          <ol>
            {plan.tasks.map((task) => (
              <li key={task.node_id}>
                <span className="t-strong">{ROLE_TITLE[task.role_id] ?? task.role_id}</span>：<GoalText text={task.goal ?? ''} taskLabels={task.labels ?? {}} labels={labels} onCite={onCite} />
                {task.depends_on.length > 0 && <span className="t-caption text-tertiary">（在 {task.depends_on.map((d) => plan.tasks.find((t) => t.node_id === d)).map((t) => (t ? ROLE_TITLE[t.role_id] : '')).join('、')} 之后）</span>}
              </li>
            ))}
          </ol>
        </section>
      )}
      {roles.map((role) => (
        <section key={role.analysis_ref} className="role-block">
          <h4>{role.role_title || ROLE_TITLE[role.role] || role.role}</h4>
          <ul>
            {role.claims.map((claim, i) => (
              <li key={i}>
                {claim.text}
                {claim.evidence.map((e) => (labels[e.evidence_id] ? <Cite key={e.evidence_id} label={labels[e.evidence_id]} evidence={e.evidence_id} onOpen={() => onCite(e.evidence_id)} /> : null))}
              </li>
            ))}
            {role.limitations.map((limit, i) => (
              <li key={`l${i}`} className="text-secondary">局限：{limit}</li>
            ))}
          </ul>
        </section>
      ))}
      {challenges.length > 0 && (
        <section className="role-block">
          <h4>检查者质疑（已交给修订）</h4>
          <ul>
            {challenges.map((c, i) => (
              <li key={i}>
                <Chip tone="warn">{ISSUE[c.issue_type] ?? c.issue_type}</Chip> {c.proposed_action}
              </li>
            ))}
          </ul>
        </section>
      )}
    </details>
  );
}

const FIELD: Record<string, string> = { amount: '金额', date: '日期', name: '姓名', account: '账号', id_number: '证件号' };

/** What the model read directly from the approved media, and how its key
 *  fields compare with the local OCR/ASR of the same source. */
function Observations({ rows, visual, audio }: { rows: ObservationSummary[]; visual?: string; audio?: string }) {
  // A targeted re-look replaces the first reading of the same images.
  const superseded = new Set(rows.map((r) => r.supersedes).filter((r): r is string => !!r));
  const current = rows.filter((r) => !superseded.has(r.observation_ref));
  const disagreements = current.flatMap((r) => r.comparison.disagreements);
  const relooked = rows.some((r) => r.mode === 'targeted_compare');
  return (
    <details className="roles" open={disagreements.length > 0}>
      <summary>
        <Icon name="view" />
        直接读取媒体{visual === 'performed' ? ' · 已看图' : ''}{relooked ? ' · 已定向复看' : ''}{audio === 'performed' ? ' · 已听音频' : ''}{disagreements.length ? ` · ${disagreements.length} 处与本机识别不一致` : ' · 关键字段与本机识别一致'}
      </summary>
      <p className="t-caption text-tertiary">直接读取与本机识别来自同一份原件，一致只是核对信号，不证明原件真实；不一致处请回到原图或回放核对。</p>
      {rows.map((row) => (
        <section key={row.observation_ref} className="role-block">
          {row.mode === 'targeted_compare' && <h4>定向复看：只针对与本机识别不一致的图片再读一次</h4>}
          {superseded.has(row.observation_ref) && <p className="t-caption text-tertiary">首次读取；不一致的字段已由定向复看重新核对，以复看结果为准。</p>}
          <ul>
            {row.observations.map((text, i) => <li key={i}>{text}</li>)}
            {row.limitations.map((text, i) => <li key={`l${i}`} className="text-secondary">局限：{text}</li>)}
          </ul>
          {(row.comparison.agreements.length > 0 || row.comparison.disagreements.length > 0) && (
            <table className="table compact">
              <thead>
                <tr><th>字段</th><th>直接读取</th><th>本机识别</th></tr>
              </thead>
              <tbody>
                {[...row.comparison.disagreements, ...row.comparison.agreements].map((f, i) => (
                  <tr key={i}>
                    <td>{FIELD[f.name] ?? f.name}</td>
                    <td>{f.observed}</td>
                    <td>{f.extracted ? <span className="cap-supported">一致</span> : <Chip tone="warn">未在识别文字中找到</Chip>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>
      ))}
    </details>
  );
}

const STEP_TEXT: Record<string, (s: ProcessStep) => string> = {
  plan: (s) => (s.status === 'completed' ? `理解问题并改写检索词（${s.rewrites ?? 0} 条）` : '查询规划未完成，改用原问题检索'),
  search: (s) => `关键词＋语义检索：${s.queries ?? 1} 组查询，融合 ${s.candidates ?? 0} 段`,
  rerank: (s) => `本机重排，取前 ${s.evidence ?? 0} 段`,
  plan_tasks: (s) => (s.status === 'completed' ? `任务规划：安排 ${s.roles ?? 0} 个专业角色` : '任务规划未完成，改为直接综合'),
  role: (s) => `${ROLE_TITLE[s.role ?? ''] ?? '专业角色'}分析${s.status === 'completed' ? '' : '（未完成）'}`,
  observe: (s) => (s.status === 'completed' ? `直接读取媒体：${s.observations ?? 0} 条观察，并与本机识别核对` : '直接读取媒体未完成'),
  draft: () => '起草回答（只依据已批准发送的原文）',
  verify: (s) => `逐句核查：${s.supported ?? 0} 句有依据${s.problems ? `，${s.problems} 处需要修订` : ''}`,
  check: (s) => (s.status === 'completed' ? `证据检查：${s.challenges ?? 0} 条质疑` : '证据检查未完成'),
  revise: () => '按核查与检查结果修订回答',
};

function Process({ steps, running }: { steps: ProcessStep[]; running: boolean }) {
  if (!steps.length) return null;
  const total = steps.reduce((sum, s) => sum + (s.ms ?? 0), 0) / 1000;
  const search = steps.find((s) => s.step === 'search');
  const verify = [...steps].reverse().find((s) => s.step === 'verify');
  const roles = steps.filter((s) => s.step === 'role').length;
  const path = [
    search ? search.status === 'completed' ? `检索 ${search.candidates ?? 0} 段` : '检索未完成' : '',
    steps.some((s) => s.step === 'rerank') ? '重排' : '',
    steps.some((s) => s.step === 'observe') ? '直接读取媒体' : '',
    roles ? `${roles} 个角色分析` : '',
    steps.some((s) => s.step === 'draft') ? '起草' : '',
    verify ? `核查（${verify.status !== 'completed' ? '部分完成' : verify.problems ? `${verify.problems} 处待改` : '完成'}）` : '',
    steps.some((s) => s.step === 'check') ? '证据检查' : '',
  ].filter(Boolean).join(' → ');
  return (
    <details className="process" open={running}>
      <summary>
        <Icon name="list" />
        执行过程：{path} · 共 {total.toFixed(1)} 秒
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
