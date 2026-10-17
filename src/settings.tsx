// Settings: security, backup, diagnostics, Skill status and the bundled models.
import React, { useState } from 'react';
import { api, bytes, ensure, SKILL_STATUS, type Envelope } from './api';
import { Busy, Chip, ErrorLine, Field, Tabs, useAction, useLoad } from './ui';

type Tab = 'security' | 'backup' | 'diagnostics' | 'skills' | 'about';

export function SettingsPage({ initialTab, onLocked }: { initialTab?: string; onLocked: () => void }) {
  const [tab, setTab] = useState<Tab>((initialTab as Tab) ?? 'security');
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <p className="eyebrow">设置</p>
          <h1>安全、备份与关于</h1>
        </div>
      </header>
      <Tabs<Tab> value={tab} onChange={setTab} items={[['security', '安全'], ['backup', '备份与恢复'], ['diagnostics', '诊断'], ['skills', '功能与Skill'], ['about', '关于']]} />
      <div className="tab-body">
        {tab === 'security' && <Security onLocked={onLocked} />}
        {tab === 'backup' && <Backup />}
        {tab === 'diagnostics' && <Diagnostics />}
        {tab === 'skills' && <Skills />}
        {tab === 'about' && <About />}
      </div>
    </div>
  );
}

function Security({ onLocked }: { onLocked: () => void }) {
  const { busy, run } = useAction();
  const [form, setForm] = useState({ current: '', next: '', again: '' });
  const [error, setError] = useState('');
  const [minutes, setMinutes] = useState('15');
  const [grants, setGrants] = useState<{ root: string; action: string; cancelled: boolean; created_at: string | null; capabilities: string | null }[] | null>(null);
  const [theme, setTheme] = useState(() => document.documentElement.dataset.theme ?? 'system');
  async function change(e: React.FormEvent) {
    e.preventDefault();
    setError('');
    if (form.next !== form.again) return setError('两次输入的新口令不一致。');
    if ([...form.next].length < 8) return setError('新口令至少 8 个字符。');
    const done = await run(() => api.changePassphrase(form.current, form.next), '主口令已更换；恢复码不变');
    if (done !== undefined) setForm({ current: '', next: '', again: '' });
  }
  function applyTheme(value: string) {
    setTheme(value);
    try {
      if (value === 'system') {
        delete document.documentElement.dataset.theme;
        window.localStorage.removeItem('lvjin-theme');
      } else {
        document.documentElement.dataset.theme = value;
        window.localStorage.setItem('lvjin-theme', value);
      }
    } catch {
      // Only a display preference; ignore storage errors.
    }
  }
  return (
    <section className="split">
      <form className="stack" onSubmit={change}>
        <h3>更换主口令</h3>
        <Field label="当前主口令"><input type="password" value={form.current} onChange={(e) => setForm({ ...form, current: e.target.value })} autoComplete="current-password" required /></Field>
        <Field label="新主口令"><input type="password" value={form.next} onChange={(e) => setForm({ ...form, next: e.target.value })} autoComplete="new-password" required /></Field>
        <Field label="再次输入新主口令"><input type="password" value={form.again} onChange={(e) => setForm({ ...form, again: e.target.value })} autoComplete="new-password" required /></Field>
        <ErrorLine text={error} />
        <button className="primary" disabled={busy}>更换</button>
        <p className="muted small">更换口令只重新封装主密钥，不重新加密资料；原恢复码继续有效。</p>
      </form>
      <div className="stack">
        <h3>自动锁定</h3>
        <Field label="无操作多少分钟后锁定">
          <select value={minutes} onChange={(e) => setMinutes(e.target.value)}>{['5', '10', '15', '30', '60'].map((m) => <option key={m} value={m}>{m} 分钟</option>)}</select>
        </Field>
        <button disabled={busy} onClick={() => run(async () => ensure(await api.privacy('change_policy', { auto_lock_minutes: Number(minutes) })), '已保存')}>保存</button>
        <p className="muted small">锁定会停止进行中的任务、关闭本机处理进程并清除页面图像的临时访问令牌；中断的导入可在解锁后继续。</p>
        <button className="danger" onClick={async () => { await api.lock().catch(() => {}); onLocked(); }}>立即锁定</button>
        <h3>界面</h3>
        <Field label="颜色主题">
          <select value={theme} onChange={(e) => applyTheme(e.target.value)}><option value="system">跟随系统</option><option value="light">浅色</option><option value="dark">深色</option></select>
        </Field>
        <h3>授权记录</h3>
        <button disabled={busy} onClick={async () => { const r = await run(async () => { const e = ensure(await api.privacy('view_grants', {})); return api.result<{ roots: typeof grants }>(e.data_ref!); }); if (r) setGrants(r.roots); }}>查看最近的授权记录</button>
        {grants && (
          <div className="table-scroll">
            <table className="table compact">
              <thead><tr><th>时间</th><th>方式</th><th>授予的本机能力</th></tr></thead>
              <tbody>{grants.map((g) => <tr key={g.root}><td>{g.created_at?.replace('T', ' ').slice(0, 19)}</td><td>{({ ui: '界面操作', confirm: '明确确认', protected: '受保护流程', system: '系统' } as Record<string, string>)[g.action] ?? g.action}{g.cancelled ? '（已停止）' : ''}</td><td className="small">{g.capabilities}</td></tr>)}</tbody>
            </table>
          </div>
        )}
        <p className="muted small">联网、生成接口和外部工具的授权数为 0：当前版本不提供这些能力。</p>
      </div>
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
    <section className="split">
      <div className="stack">
        <h3>创建加密备份</h3>
        <p>备份包含加密数据库、加密原件与派生文件，以及解锁所需的密钥封装信息；恢复时需要主口令或恢复码。生成接口密钥与外部连接不在备份内。</p>
        <button className="primary" disabled={busy} onClick={async () => { const r = await run(async () => { const e = await maintenance('backup'); if (!e) return null; const p = await api.result<{ backup_name: string; objects: number; database_bytes: number }>(e.data_ref!); return `已创建 ${p.backup_name}：数据库 ${bytes(p.database_bytes)}，${p.objects} 个加密文件。`; }); if (r) setResult(r); }}>选择位置并创建备份…</button>
        <h3>校验备份</h3>
        <p>逐个核对备份文件的哈希并检查加密数据库能否打开，不会修改备份。</p>
        <button disabled={busy} onClick={async () => { const r = await run(async () => { const e = await maintenance('verify'); if (!e) return null; const p = await api.result<{ ok: boolean; checked_files: number; problems: { file: string; problem: string }[] }>(e.data_ref!); return p.ok ? `校验通过：${p.checked_files} 个文件完整。` : `发现问题：${p.problems.map((x) => x.file).join('、')} 缺失或被修改。`; }); if (r) setResult(r); }}>选择备份并校验…</button>
        {result && <p className="ok-line">{result}</p>}
      </div>
      <div className="stack">
        <h3>在另一台电脑恢复</h3>
        <ol className="plain numbered">
          <li>在新电脑安装同一版本的律金智库（完整离线介质）。</li>
          <li>在锁屏页选择“从加密备份恢复”，输入主口令或恢复码。</li>
          <li>选择备份文件夹。应用先校验全部文件，再恢复到新的空间目录，不覆盖已有空间。</li>
        </ol>
        <p className="muted small">普通删除不能保证擦除硬盘上的物理残留；已有备份不受后续删除影响。</p>
      </div>
    </section>
  );
}

function Diagnostics() {
  const { busy, run } = useAction();
  const [report, setReport] = useState<unknown>(null);
  return (
    <section className="stack">
      <p>诊断报告只包含版本、组件状态、数量与事件计数，不含资料正文、文件名、路径、口令或密钥。</p>
      <div className="actions">
        <button className="primary" disabled={busy} onClick={async () => { const r = await run(async () => { const target = await api.object('component', { name: 'installation' }); const e = ensure(await api.run('diagnostics', { target_ref: target, detail: 'redacted_report' })); return api.result(e.data_ref!); }); if (r) setReport(r); }}>生成脱敏诊断报告</button>
      </div>
      {busy && <Busy />}
      {report !== null && <pre className="preview">{JSON.stringify(report, null, 2)}</pre>}
    </section>
  );
}

type SkillRow = { id: string; title: string; kind: string; version: string; implementation_status: string; direct_capabilities: string[]; conditional_capabilities: string[]; actors: string[] };

function Skills() {
  const skills = useLoad(() => api.view<SkillRow[]>('skills'), []);
  if (skills.error) return <ErrorLine text={skills.error} />;
  if (!skills.data) return <Busy />;
  const tone = (s: string) => (s === 'm1-local' ? 'ok' : s === 'm1-local-generation-pending' ? 'info' : 'neutral');
  return (
    <section>
      <p className="muted small">应用内置 30 个能力包（Skill）。界面按钮都经可信运行器调用对应 Skill；条件能力只在你明确确认或受保护流程中授予。没有 Skill 商店，也不能下载或替换内置模型。</p>
      <table className="table">
        <thead><tr><th>功能</th><th>标识</th><th>状态</th><th>直接能力</th><th>条件能力</th></tr></thead>
        <tbody>
          {skills.data.map((s) => (
            <tr key={s.id}>
              <td>{s.title}</td>
              <td className="mono small">{s.id}</td>
              <td><Chip tone={tone(s.implementation_status)}>{SKILL_STATUS[s.implementation_status] ?? s.implementation_status}</Chip></td>
              <td className="small mono">{s.direct_capabilities.join(' ')}</td>
              <td className="small mono">{s.conditional_capabilities.join(' ')}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

type Models = { models: { model_id: string; source_model_id: string; revision: string; license: string; installed_bytes: number; compute: string }[]; residency: { max_large: number; coresident: string[] } };
const MODEL_ROLE: Record<string, string> = {
  embedding: '语义检索向量', reranker: '检索结果重排', grounding: '答案依据核查（M2 启用）', 'ocr-det': '文字检测', 'ocr-rec': '文字识别', uvdoc: '弯曲矫正', 'textline-ori': '文字行方向', 'doc-ori': '整页方向', asr: '语音转写',
};

function About() {
  const status = useLoad(() => api.status(), []);
  const models = useLoad(() => api.view<Models>('models'), []);
  return (
    <section className="stack">
      <dl className="figures">
        <div><dt>版本</dt><dd>{status.data?.version}</dd></div>
        <div><dt>阶段</dt><dd>{status.data?.milestone} 内部版</dd></div>
        <div><dt>运行</dt><dd>CPU · 断网可用</dd></div>
        <div><dt>大模型常驻</dt><dd>{models.data ? (models.data.residency.max_large > 1 ? '检索双模型共驻（≥16 GB 内存）' : '单个') : ''}</dd></div>
      </dl>
      <h3>随软件安装的九个辅助模型</h3>
      <p className="muted small">内置模型随完整安装介质提供并在本机运行；应用不提供下载、导入、切换或卸载。安装文件损坏时请使用同版本完整介质修复。</p>
      {models.data ? (
        <table className="table">
          <thead><tr><th>用途</th><th>模型</th><th>版本（revision）</th><th>许可</th><th className="num">大小</th></tr></thead>
          <tbody>{models.data.models.map((m) => <tr key={m.model_id}><td>{MODEL_ROLE[m.model_id] ?? m.model_id}</td><td className="mono small">{m.source_model_id}</td><td className="mono small">{m.revision.slice(0, 12)}</td><td>{m.license}</td><td className="num">{bytes(m.installed_bytes)}</td></tr>)}</tbody>
        </table>
      ) : models.error ? <ErrorLine text={models.error} /> : <Busy />}
      <p className="muted small">回答生成需要你自行配置的生成接口（本应用不安装或启动生成大模型），将在 M2 提供。</p>
    </section>
  );
}
