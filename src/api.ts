// Typed bridge to the trusted core. The webview never passes paths, keys or
// confirmed actions: those come from native dialogs inside the Rust commands.
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export type Warning = { code: string; message: string };
export type Envelope = {
  status: string;
  data_ref: string | null;
  evidence_refs: string[];
  warnings: Warning[];
  coverage: { processed_refs: string[]; omitted_refs: string[]; limitations: string[] };
  usage: { generation_attempts: number; native_media_attempts: number; grounding_units: number };
  root_task_id: string;
  invocation_id: string;
  skill_id: string;
  audit_id: string;
};
export type Declined = { status: 'declined' };
export type VaultSummary = { handle: string; vault_id: string; created_at: string };
export type AppStatus = {
  milestone: string;
  version: string;
  installation_ok: boolean;
  error?: string;
  production?: boolean;
  unlocked?: boolean;
  session?: string | null;
  vaults?: VaultSummary[];
};
export type UnlockInfo = { handle: string; vault_id: string; root_matter: string; name: string };
export type Progress = { skill: string; stage: string; done: number; total: number };
export type TaskEvent = { task_id: string; kind: 'progress' | 'finished'; progress: Progress | null; envelope: Envelope | null };
export type TaskState = { task_id: string; skill: string; root: string; status: string; progress: Progress | null; envelope: Envelope | null; started_at: string };

export type Locator = { type: string; page_no?: number; page_id?: string; bbox?: number[]; polygon?: number[][]; frame?: string; low_confidence?: boolean; start_s?: number; end_s?: number; segment_id?: string; sheet?: string; row?: number; line?: number; paragraph?: number; table?: number; outside_page?: boolean; rotation_turns?: number; unwarped?: boolean };
export type Block = { block: number; text: string; locator: Locator; confidence: number | null; kind: string };
export type PageView = { page_id: string; version_id: string; page_no: number; source: string; width: number | null; height: number | null; image_token: string | null; rotation_turns: number; unwarped: boolean; blocks: Block[]; chunks: string[] };
export type VersionPages = { version_id: string; pages: { page_id: string; page_no: number; source: string; ocr_status: string }[]; blocks: { block: number; kind: string; text: string; locator: Locator & { cells?: unknown[]; header?: string[] } }[] };
export type MediaView = { version_id: string; media_id: string | null; duration_s?: number; has_audio?: boolean; has_video?: boolean; segments?: { segment_id: string; seq: number; start_s: number; end_s: number; transcript: string | null; status: string; truncated: boolean | null }[]; frames?: { frame_id: string; seq: number; time_s: number; page_id: string | null; ocr_status: string | null }[] };

export type Matter = { id: string; title: string; matter_type: string; status: string; updated_at: string; documents?: number; candidate_facts?: number };
export type DocumentRow = { document_id: string; display_name: string; version_id: string; version_no: number; media_kind: string; status: string; pages: number; bytes: number; created_at: string; error_code: string | null; media_id: string | null };
export type Fact = { id: string; fact_type: string; value: Record<string, string>; label: string | null; status: string; origin: string; source_version: string | null; source: { block?: number; start?: number; end?: number; page_id?: string | null; block_locator?: Locator } | null; created_at: string };
export type Cashflow = { id: string; date: string; amount: string; currency: string; direction: 'in' | 'out'; kind: string; category: string | null; counterparty: string | null; description: string | null; status: string; account_id: string | null; source_version: string | null };
export type Todo = { id: string; title: string; due_on: string | null; status: string; source_fact: string | null };
export type Relation = { id: string; src: string; dst: string; relation_type: string; origin: string; status: string; created_at: string };
export type Snapshot = {
  matter: { id: string; title: string; matter_type: string; region: string | null; status: string; is_root: number; notes: string | null };
  matters?: Matter[];
  documents: DocumentRow[];
  facts: Fact[];
  cashflows: Cashflow[];
  accounts: { id: string; name: string; currency: string }[];
  todos: Todo[];
  relations: Relation[];
  assumptions: { id: string; kind: string; value: unknown }[];
  calculations: { id: string; calculation_type: string; created_at: string }[];
  timeline: { date: string; kind: string; ref: string; label: string; status: string }[];
};
export type EvidenceItem = { evidence_id: string; version_id: string; matter_id: string; document: { display_name: string; version_no: number; media_kind: string }; unit_title: string; exact_text: string; text: string; locators: Locator[]; highlight_indexes: number[]; pages: string[] };

export type Capability = { status: 'supported' | 'unsupported' | 'unknown' | 'unavailable' | 'policy_blocked'; stale: boolean; evidence: string; limits: unknown; tested_at: string };
export type Connection = {
  id: string; name: string; provider: 'deepseek' | 'openai' | 'anthropic' | 'local' | 'custom'; protocol: 'chat_completions' | 'responses' | 'anthropic_messages';
  base_url: string; host: string; loopback: boolean; model_id: string; timeout_s: number; max_output_tokens: number; proxy: 'system' | 'none';
  has_key: boolean; is_default: boolean; adapter_version: string; capabilities: Record<string, Capability>;
};
export type ConnectionDraft = { connection_id: string | null; name: string; provider: string; protocol: string; base_url: string; model_id: string; timeout_s: number; max_output_tokens: number; proxy: 'system' | 'none' };
export type Excerpt = { evidence_id: string; display_name: string; unit_title: string; pages: string[]; locators: Locator[]; excerpt: string };
export type AnswerPreview = {
  grant_id: string; question_id: string; scope_id: string; question: string; purpose: string; matters: number;
  connection: { id: string; name: string; host: string; model_id: string }; excerpts: Excerpt[]; max_items: number; max_requests: number; max_output_tokens: number;
};
export type ClaimRow = { claim_id: string; text: string; evidence: { label: string; evidence_id: string }[]; status: 'supported' | 'partial' | 'unsupported' | 'unchecked' | 'advice' | null; message: string | null; flagged: { start: number; end: number; utf16_start: number; utf16_end: number }[] | null };
export type Answer = {
  draft_ref: string; validation_report_ref: string; version: number; revisions: number; insufficient_evidence: boolean;
  sections: { conclusion: ClaimRow[]; basis: ClaimRow[]; calculations: ClaimRow[]; next_steps: ClaimRow[]; assumptions: string[]; missing_information: string[]; conflicts: string[] };
  labels: { label: string; evidence_id: string }[]; coverage: { checked: number; groundable: number }; verification_note: string;
};
export type ProcessStep = { step: string; status: string; ms?: number; rewrites?: number; queries?: number; candidates?: number; evidence?: number; supported?: number; problems?: number; code?: string };
export type RetrievalResult = {
  question: string; items: EvidenceItem[]; answer: Answer | null; process: ProcessStep[]; query_rewrites: string[];
  plan: { fallback: boolean; rewrites: { purpose: string; query: string }[]; missing_facts: string[] };
  usage: { requests: number; input_tokens: number; output_tokens: number }; connection: { name: string; host: string; model_id: string; provider: string } | null; requested_output: string;
  evidence_bundle: string | null;
};
export type GenerationRequestRow = { purpose: string; host: string; model_id: string; status: string; error: string | null; input_tokens: number | null; output_tokens: number | null; started_at: string; connection: string | null };

export class CoreError extends Error {}

function wrap<T>(promise: Promise<T>): Promise<T> {
  return promise.catch((error: unknown) => {
    throw new CoreError(typeof error === 'string' ? error : error instanceof Error ? error.message : '操作失败');
  });
}

export const api = {
  status: () => wrap(invoke<AppStatus>('app_status')),
  touch: () => wrap(invoke<null>('touch')),
  createVault: (name: string, passphrase: string) => wrap(invoke<{ info: UnlockInfo; recovery_code: string }>('vault_create', { name, passphrase })),
  unlock: (handle: string, kind: 'passphrase' | 'recovery', secretText: string) => wrap(invoke<UnlockInfo>('vault_unlock', { handle, kind, secretText })),
  lock: () => wrap(invoke<null>('vault_lock')),
  restore: (kind: 'passphrase' | 'recovery', secretText: string) => wrap(invoke<{ handle: string } | null>('vault_restore', { kind, secretText })),
  changePassphrase: (current: string, next: string) => wrap(invoke<null>('change_passphrase', { current, new: next })),
  run: (skill: string, input: object) => wrap(invoke<Envelope>('run_skill', { skill, input })),
  start: (skill: string, input: object) => wrap(invoke<string>('start_skill', { skill, input })),
  tasks: () => wrap(invoke<TaskState[]>('tasks')),
  cancelTask: (taskId: string) => wrap(invoke<null>('task_cancel', { taskId })),
  object: (kind: 'question' | 'scope' | 'range' | 'filter' | 'change' | 'component', value: object) => wrap(invoke<string>('create_object', { kind, value })),
  importFiles: (matter: string, mode: 'copy' | 'new_version') => wrap(invoke<{ task_id: string; display_name: string; selection_id: string }[]>('import_files', { matter, mode })),
  resumeImport: (selection: string, matter: string) => wrap(invoke<string>('resume_import', { selection, matter })),
  commit: (matter: string, changeRef: string) => wrap(invoke<Envelope | Declined>('commit_change', { matter, changeRef })),
  exportCommit: (draft: string, resultRefs: string[], format: 'markdown' | 'csv', fileName: string) => wrap(invoke<Envelope | Declined>('export_commit', { draft, resultRefs, format, fileName })),
  backup: (operation: 'backup' | 'verify') => wrap(invoke<Envelope | Declined>('backup', { operation })),
  privacy: (operation: 'view_grants' | 'revoke_grant' | 'change_policy', payload: object) => wrap(invoke<Envelope>('privacy', { operation, payload })),
  view: <T>(kind: string, id = '') => wrap(invoke<T>('view', { kind, id })),
  result: <T>(id: string) => wrap(invoke<{ id: string; kind: string; payload: T }>('view', { kind: 'result', id })).then((r) => r.payload),
  connections: () => wrap(invoke<Connection[]>('connections')),
  /** The key goes to the core's secure input channel and is sealed with DPAPI there. */
  connectionSave: (draft: ConnectionDraft, apiKey: string | null) => wrap(invoke<Envelope>('connection_save', { draft, apiKey })),
  connectionProbe: (connection: string) => wrap(invoke<string>('connection_probe', { connection })),
  connectionRevoke: (connection: string) => wrap(invoke<Envelope>('connection_revoke', { connection })),
  connectionDefault: (connection: string) => wrap(invoke<null>('connection_default', { connection })),
  answerPreview: (question: string, matters: string[], connection: string | null = null) => wrap(invoke<AnswerPreview>('answer_preview', { question, matters, connection })),
  /** Opens the native confirmation; the task starts only if the user confirms there. */
  answerStart: (grant: string, excluded: string[]) => wrap(invoke<string>('answer_start', { grant, excluded })),
  answerDiscard: (grant: string) => wrap(invoke<null>('answer_discard', { grant })),
  generationRequests: () => wrap(invoke<GenerationRequestRow[]>('generation_requests')),
};

export const onTask = (handler: (event: TaskEvent) => void): Promise<UnlistenFn> => listen<TaskEvent>('lvjin://task', (e) => handler(e.payload));
export const onLocked = (handler: () => void): Promise<UnlistenFn> => listen('lvjin://locked', () => handler());

export function warningText(envelope: Envelope | Declined): string {
  if (envelope.status === 'declined') return '已取消';
  const e = envelope as Envelope;
  return e.warnings.map((w) => w.message).join('；') || statusLabel(e.status);
}

export function ensure(envelope: Envelope, accepted: string[] = ['completed']): Envelope {
  if (!accepted.includes(envelope.status)) throw new CoreError(warningText(envelope));
  return envelope;
}

/** Prepare (preview) a matter change; returns the change ref and its impact. */
export async function prepareChange(matter: string, change: object): Promise<{ ref: string; impact: Record<string, unknown> }> {
  const ref = await api.object('change', { matter, change });
  const prepared = ensure(await api.run('matter-manage', { matter_id: matter, operation: 'prepare_change', change_ref: ref }));
  const payload = await api.result<{ impact: Record<string, unknown> }>(prepared.data_ref!);
  return { ref, impact: payload.impact };
}

/** Commit through the native confirmation dialog. `null` when the user declined. */
export async function commitChange(matter: string, ref: string): Promise<Envelope | null> {
  const done = await api.commit(matter, ref);
  if (done.status === 'declined') return null;
  return ensure(done as Envelope);
}

export async function applyChange(matter: string, change: object): Promise<Envelope | null> {
  const { ref } = await prepareChange(matter, change);
  return commitChange(matter, ref);
}

export async function snapshot(matter: string): Promise<Snapshot> {
  const envelope = ensure(await api.run('matter-manage', { matter_id: matter, operation: 'read' }));
  return api.result<Snapshot>(envelope.data_ref!);
}

const STATUS: Record<string, string> = {
  completed: '已完成', partial: '部分完成', needs_input: '需要补充信息', needs_authorization: '需要授权', requires_generation: '需要生成接口',
  cancelled: '已停止', failed: '失败', installation_fault: '安装故障', running: '进行中', interrupted: '已中断', declined: '已取消',
};
export const statusLabel = (status: string) => STATUS[status] ?? status;

const VERSION_STATUS: Record<string, string> = {
  ready: '可检索', partial: '部分可用', stored: '已保存', stored_media: '已保存（待转写）', parsing: '解析中', ocr: '识别中', indexing: '建立索引中', interrupted: '已中断', failed: '失败',
};
export const versionStatus = (status: string) => VERSION_STATUS[status] ?? status;

export const MATTER_TYPES: Record<string, string> = { rental: '租房', loan: '借款', employment: '工作', household: '家庭财务', consumer: '消费', insurance: '保险', other: '其他' };
export const KINDS: Record<string, string> = {
  income: '收入', expense: '支出', transfer: '本人转账', refund: '退款', loan_out: '借出', repayment: '还款', fee: '费用', interest: '利息', principal_received: '收到本金', payment: '付款',
};
export const FACT_TYPES: Record<string, string> = { amount: '金额', date: '日期', party: '当事人', event: '事件', region: '地区', identity: '身份', note: '备注' };
export const RELATION_TYPES: Record<string, string> = { cites: '引用', amends: '修订', supersedes: '取代', evidence_for: '依据', confirmed_fact: '确认事实', derived_from: '来源于' };
export const SKILL_STATUS: Record<string, string> = {
  local: '本机可用', 'local-generation': '本机检索；授权后生成回答', generation: '需要生成接口', 'protected-connection': '受保护设置',
  'generation-pending': '本机部分可用；生成分析后续提供', 'network-pending': '联网与外部工具（后续）',
};

/** Exact decimal strings as `77,000.00` (two decimals, more only when the
 *  value has them); no floating point is involved. Other input is unchanged. */
export function money(value: string | number): string {
  const text = String(value).replace(/,/g, '').trim();
  const match = /^([+-]?)(\d+)(?:\.(\d+))?$/.exec(text);
  if (!match) return String(value);
  const [, sign, whole, fraction = ''] = match;
  const trimmed = fraction.replace(/0+$/, '');
  const decimals = trimmed.length <= 2 ? trimmed.padEnd(2, '0') : trimmed;
  const grouped = whole.replace(/^0+(?=\d)/, '').replace(/\B(?=(\d{3})+(?!\d))/g, ',');
  return `${sign === '-' ? '−' : sign}${grouped}.${decimals}`;
}

const SKILLS: Record<string, string> = {
  'document-import': '导入资料', 'document-ocr': '识别文字', 'knowledge-index': '建立索引', 'audio-review': '音频转写', 'video-review': '视频识别',
  'lvjin-retrieval': '检索与回答', 'contract-review': '合同阅读', 'loan-analysis': '借款核对', 'budget-analysis': '预算分析', 'media-decode': '媒体解码',
  'audio-transcribe': '语音转写', 'image-review': '图片识别', 'connection-settings': '生成接口自检', 'report-export': '导出', 'vault-maintenance': '备份',
};
export const skillName = (id: string) => SKILLS[id] ?? id;

const STAGES: Record<string, string> = {
  parse_pdf: '解析 PDF', orientation: '整页方向', unwarp: '弯曲矫正', detection: '文字检测', line_orientation: '行方向', recognition: '文字识别',
  index: '建立索引', embedding: '向量化', probe: '读取媒体信息', decode_audio: '解码音频', frames: '抽取画面', asr: '语音转写', backup: '备份',
};
export const stageName = (id: string) => STAGES[id] ?? id;

/** Relative time for lists ("3 分钟前", "昨天 14:20"); absolute date beyond a week. */
export function relative(iso: string | null | undefined): string {
  if (!iso) return '';
  const then = new Date(iso);
  if (Number.isNaN(then.getTime())) return iso.slice(0, 10);
  const minutes = Math.round((Date.now() - then.getTime()) / 60000);
  const hm = `${String(then.getHours()).padStart(2, '0')}:${String(then.getMinutes()).padStart(2, '0')}`;
  if (minutes < 1) return '刚刚';
  if (minutes < 60) return `${minutes} 分钟前`;
  const today = new Date();
  const days = Math.floor((new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime() - new Date(then.getFullYear(), then.getMonth(), then.getDate()).getTime()) / 86400000);
  if (days === 0) return `今天 ${hm}`;
  if (days === 1) return `昨天 ${hm}`;
  if (days < 7) return `${days} 天前`;
  return iso.slice(0, 10);
}

/** Prose date: 2026年11月1日. */
export function longDate(iso: string | null | undefined): string {
  if (!iso) return '';
  const [y, m, d] = iso.slice(0, 10).split('-').map(Number);
  return y && m && d ? `${y}年${m}月${d}日` : iso;
}

export function bytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

export function seconds(value: number): string {
  const m = Math.floor(value / 60);
  const s = Math.floor(value % 60);
  return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
}

export function day(iso: string | null | undefined): string {
  return iso ? iso.slice(0, 10) : '';
}

export const assetUrl = (token: string) => `http://lvjin.localhost/${token}`;
