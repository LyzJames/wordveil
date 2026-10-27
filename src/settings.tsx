// Settings (design 9.10): security, appearance, backup, diagnostics, Skill
// status and the bundled models. Appearance preferences are display-only and
// live in local storage (not private data).
import React, { useEffect, useState } from 'react';
import { api, bytes, ensure, relative, SKILL_STATUS, type Envelope, type LockPolicy, type GenerationRequestRow } from './api';
import { Icon } from './icons';
import { useSession } from './session';
import { Busy, Callout, Chip, ErrorLine, Field, FieldGroup, Segmented, Skeleton, Tabs, useAction, useLoad, type Tone } from './ui';

// ------------------------------------------------------------- preferences

export function preference(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function savePreference(key: string, value: string | null) {
  try {
    if (value === null) window.localStorage.removeItem(key);
    else window.localStorage.setItem(key, value);
  } catch {
    // Display preferences only; ignore storage errors.
  }
}

/** Lock shortcut (design 9.10); a display preference like the theme. */
export type LockKey = 'ctrl+l' | 'ctrl+shift+l' | 'ctrl+alt+l' | 'off';
export const LOCK_KEYS: [LockKey, string][] = [['ctrl+l', 'Ctrl+L'], ['ctrl+shift+l', 'Ctrl+Shift+L'], ['ctrl+alt+l', 'Ctrl+Alt+L'], ['off', '不使用']];

export function lockKey(): LockKey {
  const value = preference('lvjin-lock-key');
  return LOCK_KEYS.some(([key]) => key === value) ? (value as LockKey) : 'ctrl+l';
}

/** "Ctrl+L", or empty when the shortcut is turned off. */
export function lockKeyLabel(key: LockKey = lockKey()): string {
  return key === 'off' ? '' : LOCK_KEYS.find(([k]) => k === key)?.[1] ?? 'Ctrl+L';
}

export function isLockKey(e: KeyboardEvent, key: LockKey = lockKey()): boolean {
  if (key === 'off' || !e.ctrlKey || e.metaKey || e.key.toLowerCase() !== 'l') return false;
  if (key === 'ctrl+l') return !e.shiftKey && !e.altKey;
  if (key === 'ctrl+shift+l') return e.shiftKey && !e.altKey;
  return e.altKey && !e.shiftKey;
}

const PREFERENCES_EVENT = 'lvjin:preferences';

/** Re-render when a preference changes on the settings page. */
export function usePreferences() {
  const [, setTick] = useState(0);
  useEffect(() => {
    const changed = () => setTick((t) => t + 1);
    window.addEventListener(PREFERENCES_EVENT, changed);
    return () => window.removeEventListener(PREFERENCES_EVENT, changed);
  }, []);
}

/** Theme, interface scale and motion from the saved preferences. */
export function applyPreferences() {
  const root = document.documentElement;
  const theme = preference('lvjin-theme');
  if (theme === 'dark' || theme === 'light') root.dataset.theme = theme;
  else delete root.dataset.theme;
  const scale = Number(preference('lvjin-scale'));
  root.style.setProperty('--ui-scale', scale >= 0.9 && scale <= 1.25 ? String(scale) : '1');
  if (preference('lvjin-motion') === 'reduce') root.dataset.motion = 'reduce';
  else delete root.dataset.motion;
  window.dispatchEvent(new Event(PREFERENCES_EVENT));
}

type Tab = 'security' | 'appearance' | 'backup' | 'diagnostics' | 'skills' | 'about';

export function SettingsPage({ initialTab }: { initialTab?: string }) {
  const [tab, setTab] = useState<Tab>((initialTab as Tab) ?? 'security');
  return (
    <div className="page fade-in">
      <header className="page-head">
        <div className="titles">
          <h1 className="t-page">设置</h1>
        </div>
      </header>
      <Tabs<Tab>
        label="设置分类"
        value={tab}
        onChange={setTab}
        items={[['security', '安全'], ['appearance', '界面'], ['backup', '备份与恢复'], ['about', '关于']]}
      />
      <div className="tab-body">
        {tab === 'security' && <Security />}
        {tab === 'appearance' && <Appearance />}
        {tab === 'backup' && <Backup />}
        {tab === 'diagnostics' && <Diagnostics />}
        {tab === 'skills' && <Skills />}
        {tab === 'about' && <About />}
      </div>
    </div>
  );
}

const ACTION: Record<string, string> = { ui: '界面操作', confirm: '明确确认', protected: '受保护流程', system: '系统' };

function Security() {
  const { lock } = useSession();
  const { busy, run } = useAction();
  const [form, setForm] = useState({ current: '', next: '', again: '' });
  const [error, setError] = useState('');
  const [requests,setRequests]=useState<GenerationRequestRow[]|null>(null);
  const [minutes, setMinutes] = useState('15');
  const [system, setSystem] = useState(true);
  const [shortcut, setShortcut] = useState<LockKey>(lockKey);
  const policy = useLoad(() => api.view<LockPolicy>('lock_policy'), []);
  useEffect(() => {
    if (!policy.data) return;
    setMinutes(String(policy.data.auto_lock_minutes));
    setSystem(policy.data.lock_on_system_lock);
  }, [policy.data]);
  const [grants, setGrants] = useState<{ root: string; action: string; cancelled: boolean; created_at: string | null; capabilities: string | null }[] | null>(null);
  async function change(e: React.FormEvent) {
    e.preventDefault();
    setError('');
    if (form.next !== form.again) return setError('两次输入的新口令不一致。');
    if ([...form.next].length < 8) return setError('新口令至少 8 个字符。');
    const done = await run(() => api.changePassphrase(form.current, form.next), '主口令已更换；恢复码不变');
    if (done !== undefined) setForm({ current: '', next: '', again: '' });
  }
  return (
    <>
      <section className="two-col">
        <form className="form" onSubmit={change}>
          <h2 className="t-subtitle">更换主口令</h2>
          <Field label="当前主口令"><input type="password" value={form.current} onChange={(e) => setForm({ ...form, current: e.target.value })} autoComplete="current-password" required /></Field>
          <Field label="新主口令"><input type="password" value={form.next} onChange={(e) => setForm({ ...form, next: e.target.value })} autoComplete="new-password" required /></Field>
          <Field label="再次输入新主口令" error={error}><input type="password" value={form.again} onChange={(e) => setForm({ ...form, again: e.target.value })} autoComplete="new-password" required /></Field>
          <div><button className="btn btn-primary" disabled={busy}>更换</button></div>
          <p className="t-caption text-tertiary">更换口令只重新封装主密钥，不重新加密资料；原恢复码继续有效。</p>
        </form>
        <div className="form">
          <h2 className="t-subtitle">自动锁定</h2>
          <Field label="无操作多少分钟后锁定" hint="后台任务仍在推进时最多再延后 30 分钟；5 分钟没有进展的任务不会推迟锁定。">
            <select value={minutes} onChange={(e) => setMinutes(e.target.value)}>
              {[...new Set(['5', '10', '15', '30', '60', minutes])].sort((a, b) => Number(a) - Number(b)).map((m) => <option key={m} value={m}>{m} 分钟</option>)}
            </select>
          </Field>
          <label className="check">
            <input type="checkbox" checked={system} onChange={(e) => setSystem(e.target.checked)} />
            Windows 锁屏、切换用户或电脑睡眠时立即锁定私密空间
          </label>
          <div className="actions">
            <button className="btn" disabled={busy || !policy.data} onClick={() => run(async () => ensure(await api.privacy('change_policy', { auto_lock_minutes: Number(minutes), lock_on_system_lock: system })), '已保存')}>保存</button>
            <button className="btn btn-danger" onClick={lock}>
              <Icon name="lock" />
              立即锁定
            </button>
          </div>
          <FieldGroup label="锁定快捷键" hint="只在本应用窗口内有效；偏好保存在这台电脑上。">
            <Segmented label="锁定快捷键" value={shortcut} onChange={(v) => { setShortcut(v); savePreference('lvjin-lock-key', v === 'ctrl+l' ? null : v); applyPreferences(); }} items={LOCK_KEYS} />
          </FieldGroup>
          <p className="t-caption text-tertiary">锁定{lockKeyLabel(shortcut) ? `（${lockKeyLabel(shortcut)}）` : ''}会立即停止进行中的任务、关闭本机处理进程并清除页面图像的临时访问令牌，不必等任务结束；中断的导入可在解锁后明确继续；已停止的模型请求不会自动重发。</p>
        </div>
      </section>
      <section className="section">
        <div className="section-head">
          <h2>授权记录</h2>
          <button className="btn btn-sm" disabled={busy} onClick={async () => { const r = await run(async () => { const e = ensure(await api.privacy('view_grants', {})); return api.result<{ roots: typeof grants }>(e.data_ref!); }); if (r) setGrants(r.roots); }}>
            {grants ? '刷新' : '查看最近的授权记录'}
          </button>
        </div>
        <p className="t-caption text-secondary reading" style={{ marginBottom: 8 }}>
          模型连接首次使用时由系统窗口确认。此后普通文字请求先通过本机保护；高风险内容和原始媒体仍需逐次批准。联网搜索需在本轮主动开启。停用模型连接会阻止后续请求。
        </p>
        <button className="btn btn-sm" disabled={busy} onClick={()=>run(async()=>{setRequests(await api.generationRequests());})}>查看外发记录</button>
        {requests&&<div className="table-wrap table-scroll"><table className="table compact"><thead><tr><th>时间</th><th>接收方</th><th>用途</th><th>结果</th></tr></thead><tbody>{requests.map((r,i)=><tr key={i}><td>{relative(r.started_at)}</td><td>{r.host} · {r.model_id}</td><td>{({answer:'回答',search_plan:'搜索子任务',media_read:'媒体子任务',web_search:'联网搜索'} as Record<string,string>)[r.purpose]??r.purpose}</td><td>{r.status}{r.error?` · ${r.error}`:''}</td></tr>)}</tbody></table></div>}
        {grants && (
          <div className="table-wrap table-scroll">
            <table className="table compact">
              <thead><tr><th>时间</th><th>方式</th><th>授予的能力</th></tr></thead>
              <tbody>{grants.map((g) => <tr key={g.root}><td className="num" title={g.created_at ?? ''}>{relative(g.created_at)}</td><td>{ACTION[g.action] ?? g.action}{g.cancelled ? '（已停止）' : ''}</td><td className="t-caption mono">{g.capabilities}</td></tr>)}</tbody>
            </table>
          </div>
        )}
      </section>
    </>
  );
}

function Appearance() {
  const [theme, setTheme] = useState(() => preference('lvjin-theme') ?? 'system');
  const [scale, setScale] = useState(() => preference('lvjin-scale') ?? '1');
  const [motion, setMotion] = useState(() => preference('lvjin-motion') === 'reduce');
  const [sidebar, setSidebar] = useState(() => preference('lvjin-sidebar') ?? 'expanded');
  function save(key: string, value: string | null) {
    savePreference(key, value);
    applyPreferences();
  }
  return (
    <section className="form">
      <FieldGroup label="颜色主题" hint="深色主题同样经过对比度校验。">
        <Segmented label="颜色主题" value={theme} onChange={(v) => { setTheme(v); save('lvjin-theme', v === 'system' ? null : v); }} items={[['system', '跟随系统'], ['light', '浅色'], ['dark', '深色']]} />
      </FieldGroup>
      <FieldGroup label="界面字号" hint="整体缩放文字与控件；也可以使用 Windows 的缩放设置。">
        <Segmented label="界面字号" value={scale} onChange={(v) => { setScale(v); save('lvjin-scale', v === '1' ? null : v); }} items={[['0.9', '90%'], ['1', '100%'], ['1.125', '112%'], ['1.25', '125%']]} />
      </FieldGroup>
      <label className="check">
        <input type="checkbox" checked={motion} onChange={(e) => { setMotion(e.target.checked); save('lvjin-motion', e.target.checked ? 'reduce' : null); }} />
        减弱动态效果（系统已开启“减少动画”时总是减弱）
      </label>
      <FieldGroup label="侧栏默认状态" hint="打开检视面板且窗口较窄时，侧栏会自动收起。">
        <Segmented label="侧栏默认状态" value={sidebar} onChange={(v) => { setSidebar(v); savePreference('lvjin-sidebar', v); }} items={[['expanded', '展开'], ['collapsed', '收起']]} />
      </FieldGroup>
      <p className="t-caption text-tertiary">界面偏好只保存在这台电脑上，不属于私密数据，也不进入备份；侧栏状态在下次解锁时生效。</p>
    </section>
  );
}

async function maintenance(operation: 'backup' | 'verify'): Promise<Envelope | null> {
  const outcome = await api.backup(operation);
  return outcome.status === 'declined' ? null : ensure(outcome as Envelope);
}

function Backup() {
  const { busy, run } = useAction();
  const [result, setResult] = useState<string>('');
  return (
    <section className="two-col">
      <div className="form">
        <h2 className="t-subtitle">创建加密备份</h2>
        <p className="text-secondary">备份包含加密数据库、加密原件与派生文件，以及解锁所需的密钥封装信息；恢复时需要主口令或恢复码。生成接口的密钥与外部连接不在备份内。</p>
        <div>
          <button className="btn btn-primary" disabled={busy} onClick={async () => { const r = await run(async () => { const e = await maintenance('backup'); if (!e) return null; const p = await api.result<{ backup_name: string; objects: number; database_bytes: number }>(e.data_ref!); return `已创建 ${p.backup_name}：数据库 ${bytes(p.database_bytes)}，${p.objects} 个加密文件。`; }); if (r) setResult(r); }}>选择位置并创建备份…</button>
        </div>
        <h2 className="t-subtitle">校验备份</h2>
        <p className="text-secondary">逐个核对备份文件的哈希并检查加密数据库能否打开，不会修改备份。</p>
        <div>
          <button className="btn" disabled={busy} onClick={async () => { const r = await run(async () => { const e = await maintenance('verify'); if (!e) return null; const p = await api.result<{ ok: boolean; checked_files: number; problems: { file: string; problem: string }[] }>(e.data_ref!); return p.ok ? `校验通过：${p.checked_files} 个文件完整。` : `发现问题：${p.problems.map((x) => x.file).join('、')} 缺失或被修改。`; }); if (r) setResult(r); }}>选择备份并校验…</button>
        </div>
        {result && <p className="ok-line">{result}</p>}
      </div>
      <div className="form">
        <h2 className="t-subtitle">在另一台电脑恢复</h2>
        <ol className="plain numbered">
          <li>在新电脑安装同一版本的守语（完整离线介质）。</li>
          <li>在锁屏页选择“从加密备份恢复”，输入主口令或恢复码。</li>
          <li>选择备份文件夹。应用先校验全部文件，再恢复到新的空间目录，不覆盖已有空间。</li>
          <li>恢复后在“生成接口”重新填写 API Key（密钥不随备份迁移）。</li>
        </ol>
        <p className="t-caption text-tertiary">普通删除不能保证擦除硬盘上的物理残留；已有备份不受后续删除影响。</p>
      </div>
    </section>
  );
}

function Diagnostics() {
  const { busy, run } = useAction();
  const [report, setReport] = useState<unknown>(null);
  return (
    <section className="stack reading">
      <p className="text-secondary">诊断报告只包含版本、组件状态、数量与事件计数，不含资料正文、文件名、路径、口令或密钥。</p>
      <div className="actions">
        <button className="btn btn-primary" disabled={busy} onClick={async () => { const r = await run(async () => { const target = await api.object('component', { name: 'installation' }); const e = ensure(await api.run('diagnostics', { target_ref: target, detail: 'redacted_report' })); return api.result(e.data_ref!); }); if (r) setReport(r); }}>
          生成脱敏诊断报告
        </button>
      </div>
      {busy && <Busy />}
      {report !== null && <pre className="preview">{JSON.stringify(report, null, 2)}</pre>}
    </section>
  );
}

type SkillRow = { id: string; title: string; kind: string; version: string; implementation_status: string; direct_capabilities: string[]; conditional_capabilities: string[]; actors: string[] };
const SKILL_TONE: Record<string, Tone> = { local: 'ok', 'local-generation': 'ok', generation: 'info', 'protected-connection': 'accent', 'generation-pending': 'neutral', 'network-pending': 'neutral' };

function Skills() {
  const skills = useLoad(() => api.view<SkillRow[]>('skills'), []);
  if (skills.error) return <ErrorLine text={skills.error} />;
  if (!skills.data) return <Skeleton rows={6} />;
  return (
    <section>
      <p className="t-caption text-secondary reading" style={{ marginBottom: 12 }}>
        应用内置 {skills.data.length} 个能力包（Skill）。界面按钮都经可信运行器调用对应 Skill；条件能力只在你明确确认或受保护流程中授予。没有 Skill 商店，也不能下载或替换内置模型。
      </p>
      <div className="table-wrap">
        <table className="table">
          <thead><tr><th>功能</th><th>标识</th><th>状态</th><th>直接能力</th><th>条件能力</th></tr></thead>
          <tbody>
            {skills.data.map((s) => (
              <tr key={s.id}>
                <td>{s.title}</td>
                <td className="mono t-caption">{s.id}</td>
                <td><Chip tone={SKILL_TONE[s.implementation_status] ?? 'neutral'}>{SKILL_STATUS[s.implementation_status] ?? s.implementation_status}</Chip></td>
                <td className="t-caption mono">{s.direct_capabilities.join(' ')}</td>
                <td className="t-caption mono">{s.conditional_capabilities.join(' ')}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

type Models = { models: { model_id: string; source_model_id: string; revision: string; license: string; installed_bytes: number; compute: string }[]; residency: { max_large: number; coresident: string[] } };
const MODEL_ROLE: Record<string, string> = {
  embedding: '本机语义检索', 'privacy-has':'本机隐私实体识别', 'ocr-det': '文字检测', 'ocr-rec': '文字识别', uvdoc: '弯曲矫正', 'textline-ori': '文字行方向', 'doc-ori': '整页方向', asr: '语音转写',
};

function About() {
  const status = useLoad(() => api.status(), []);
  const models = useLoad(() => api.view<Models>('models'), []);
  const gpu = models.data?.models.some(model => model.compute?.includes('CUDA'));
  return (
    <section className="stack">
      <dl className="figures">
        <div className="figure"><dt>版本</dt><dd>{status.data?.version}</dd></div>
        <div className="figure"><dt>运行</dt><dd>{gpu ? 'CPU / 可用 GPU' : 'CPU'}<small>{gpu ? '重排通过设备自检后使用 GPU' : '本机模型断网可用'}</small></dd></div>
        <div className="figure"><dt>本机模型常驻</dt><dd>{models.data ? `最多 ${models.data.residency.max_large} 个` : '—'}<small>{models.data && models.data.residency.max_large > 1 ? '≥16 GB；隐私与向量模型共享受限模型池' : '按需加载'}</small></dd></div>
      </dl>
      <h2 className="t-subtitle">随软件安装的八个辅助模型</h2>
      <p className="t-caption text-tertiary">内置模型随完整安装介质提供并在本机运行；应用不提供下载、导入、切换或卸载。安装文件损坏时请使用同版本完整介质修复。</p>
      {gpu && <p className="t-caption text-secondary">重排加速已验证 RTX 3060 Laptop GPU；设备、显存或自检不满足要求时使用 CPU。其余辅助阶段继续使用 CPU，模型权重只有一份。</p>}
      {models.data ? (
        <div className="table-wrap">
          <table className="table">
            <thead><tr><th>用途</th><th>模型</th><th>版本（revision）</th><th>许可</th><th className="num">大小</th></tr></thead>
            <tbody>{models.data.models.map((m) => <tr key={m.model_id}><td>{MODEL_ROLE[m.model_id] ?? m.model_id}</td><td className="mono t-caption">{m.source_model_id}</td><td className="mono t-caption">{m.revision.slice(0, 12)}</td><td>{m.license}</td><td className="num">{bytes(m.installed_bytes)}</td></tr>)}</tbody>
          </table>
        </div>
      ) : models.error ? <ErrorLine text={models.error} /> : <Skeleton rows={5} />}
      <Callout title="生成回答使用你自己连接的服务">本应用不安装或启动生成大模型；回答需要你在“生成接口”中连接 DeepSeek、OpenAI、Anthropic 或本机兼容服务。</Callout>
    </section>
  );
}
