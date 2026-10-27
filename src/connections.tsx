// Generation connections (design 9.6): the user's own service, key sealed
// on this computer, a synthetic capability self-test with an explicit
// confirmation, and a record of every request that left the computer.
import React, { useEffect, useMemo, useState } from 'react';
import { api, ensure, relative, type Capability, type Connection, type ConnectionDraft } from './api';
import { Icon, type IconName } from './icons';
import { useSession } from './session';
import { Callout, Chip, Empty, ErrorLine, Field, Skeleton, useAction, useLoad } from './ui';

type ProviderId = 'deepseek' | 'openai' | 'anthropic' | 'local' | 'custom';
type Preset = { id: ProviderId; logo: string; name: string; note: string; protocol: string; base: string; models: string[]; keyless?: boolean };

// Model lists are suggestions only; the service decides what exists.
const PRESETS: Preset[] = [
  { id: 'deepseek', logo: 'DS', name: 'DeepSeek', note: 'deepseek-flash 等', protocol: 'chat_completions', base: 'https://api.deepseek.com', models: ['deepseek-flash', 'deepseek-v4-pro'] },
  { id: 'openai', logo: 'AI', name: 'OpenAI GPT', note: 'Responses / Chat', protocol: 'responses', base: 'https://api.openai.com/v1', models: [] },
  { id: 'anthropic', logo: 'C', name: 'Anthropic Claude', note: 'Messages', protocol: 'anthropic_messages', base: 'https://api.anthropic.com/v1', models: ['claude-opus-5-5', 'claude-sonnet-5-5', 'claude-haiku-5-5'] },
  { id: 'local', logo: '本', name: '本机兼容服务', note: '127.0.0.1 等', protocol: 'chat_completions', base: 'http://127.0.0.1:8000/v1', models: [], keyless: true },
  { id: 'custom', logo: '+', name: '自定义兼容', note: '选择协议', protocol: 'chat_completions', base: '', models: [] },
];
const PROTOCOLS: Record<string, string> = { chat_completions: 'OpenAI 兼容 Chat Completions', responses: 'OpenAI Responses', anthropic_messages: 'Anthropic Messages' };
const PROTOCOLS_FOR: Record<ProviderId, string[]> = {
  deepseek: ['chat_completions', 'anthropic_messages'], openai: ['responses', 'chat_completions'], anthropic: ['anthropic_messages'],
  local: ['chat_completions', 'responses', 'anthropic_messages'], custom: ['chat_completions', 'responses', 'anthropic_messages'],
};
const CAPABILITIES: [string, string][] = [
  ['text', '文本生成'], ['streaming', '流式输出'], ['tool_calling', '工具调用'], ['json_object', 'JSON 输出'], ['strict_schema', '严格结构化输出'],
  ['vision', '图片输入'], ['audio_input', '音频输入'], ['native_video', '原生视频'], ['joint_media', '图文音联合输入'],
];
const STATUS: Record<Capability['status'], [string, IconName]> = {
  supported: ['支持', 'check'], unsupported: ['不支持', 'close'], unknown: ['未知', 'unknown'], unavailable: ['暂不可用', 'warning'], policy_blocked: ['策略禁止', 'warning'],
};
const PURPOSE: Record<string, string> = {
  answer:'对话回答',media_read:'媒体阅读（最低推理）',web_search:'受保护的联网搜索',
  capability_probe: '能力自检（合成内容）', search_plan: '改写检索词', draft_answer: '起草或修订回答', task_plan: '安排专业角色', role_analysis: '专业角色分析',
  role_check: '证据检查', context_brief: '上下文简报', direct_media: '直接读取媒体',
};

type Form = ConnectionDraft & { provider: ProviderId };

function formFor(preset: Preset): Form {
  return { connection_id: null, name: preset.id === 'custom' ? '' : preset.name, provider: preset.id, protocol: preset.protocol, base_url: preset.base, model_id: preset.models[0] ?? '', timeout_s: 120, max_output_tokens: 4096, proxy: 'system' };
}

function formOf(c: Connection): Form {
  return { connection_id: c.id, name: c.name, provider: c.provider, protocol: c.protocol, base_url: c.base_url, model_id: c.model_id, timeout_s: c.timeout_s, max_output_tokens: c.max_output_tokens, proxy: c.proxy };
}

export function ConnectionsPage() {
  const { connections, reloadConnections, tasks } = useSession();
  const [form, setForm] = useState<Form | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [probing, setProbing] = useState<string | null>(null);
  const { busy, run } = useAction();
  const current = connections.find((c) => c.id === (selected ?? form?.connection_id)) ?? connections.find((c) => c.is_default) ?? connections[0] ?? null;

  // A finished self-test refreshes the capability table (the shell reports
  // the outcome in a toast).
  useEffect(() => {
    if (!probing) return;
    const task = tasks.find((t) => t.task_id === probing);
    if (task && task.status !== 'running') {
      setProbing(null);
      reloadConnections();
    }
  }, [tasks, probing, reloadConnections]);

  async function probe(connection: Connection) {
    const task = await run(() => api.connectionProbe(connection.id));
    if (task) {
      setSelected(connection.id);
      setProbing(task);
    }
  }

  async function revoke(connection: Connection) {
    const done = await run(async () => ensure(await api.connectionRevoke(connection.id)), '已删除连接与本机保存的密钥');
    if (done) {
      if (form?.connection_id === connection.id) setForm(null);
      setSelected(null);
      reloadConnections();
    }
  }

  return (
    <div className="page fade-in">
      <header className="page-head">
        <div className="titles">
          <h1 className="t-page">生成接口</h1>
          <p className="text-secondary" style={{ maxWidth: 'var(--reading-w)' }}>
            生成服务由你提供。资料默认只在本机处理；把资料发送给生成接口前，会显示将发送的内容并请你确认。
          </p>
        </div>
      </header>

      {connections.length > 0 && (
        <section className="section" aria-labelledby="saved">
          <div className="section-head">
            <h2 id="saved">已保存的连接</h2>
          </div>
          <div className="connection-list">
            {connections.map((c) => (
              <div key={c.id} className="card connection-card" aria-current={current?.id === c.id ? 'true' : undefined}>
                <div className="row-between">
                  <div style={{ minWidth: 0 }}>
                    <div className="title-row">
                      <span className="t-strong ellipsis">{c.name}</span>
                      {c.is_default && <Chip tone="accent">默认</Chip>}
                      {c.loopback && <Chip title="本机端点，用户管理；它是否继续转发到别处无法由本应用核验">本机端点</Chip>}
                    </div>
                    <div className="t-caption text-secondary ellipsis">{c.host} · {c.model_id} · {PROTOCOLS[c.protocol]}</div>
                  </div>
                  <CapabilitySummary connection={c} />
                </div>
                <div className="actions">
                  <button className="btn btn-sm" onClick={() => { setForm(formOf(c)); setSelected(c.id); }}>
                    <Icon name="edit" />
                    编辑
                  </button>
                  <button className="btn btn-sm" disabled={busy || probing !== null} onClick={() => probe(c)}>
                    <Icon name="refresh" />
                    {probing && current?.id === c.id ? '正在自检…' : '能力自检…'}
                  </button>
                  {!c.is_default && <button className="btn btn-sm btn-subtle" disabled={busy} onClick={() => run(async () => { await api.connectionDefault(c.id); reloadConnections(); }, '已设为默认')}>设为默认</button>}
                  <button className="btn btn-sm btn-subtle" onClick={() => setSelected(c.id)}>查看能力</button>
                  <span style={{ flex: 1 }} />
                  <button className="btn btn-sm btn-danger" disabled={busy} onClick={() => revoke(c)}>
                    <Icon name="delete" />
                    删除…
                  </button>
                </div>
              </div>
            ))}
          </div>
        </section>
      )}

      <section className="section" aria-labelledby="providers">
        <div className="section-head">
          <h2 id="providers">{connections.length ? '添加连接' : '选择服务'}</h2>
          {!connections.length && <span className="t-caption text-tertiary">不连接也能检索原文、校对和计算</span>}
        </div>
        <div className="grid-cards provider-grid">
          {PRESETS.map((p) => (
            <button key={p.id} className="provider" aria-pressed={!!form && form.connection_id === null && form.provider === p.id} onClick={() => { setForm(formFor(p)); setSelected(null); }}>
              <span className="logo">{p.logo}</span>
              <span className="t-strong">{p.name}</span>
              <span className="t-caption text-tertiary">{p.note}</span>
            </button>
          ))}
        </div>
      </section>

      {form && <ConnectionForm key={`${form.connection_id ?? 'new'}-${form.provider}`} initial={form} existing={connections.find((c) => c.id === form.connection_id) ?? null} onDone={(id) => { setForm(null); setSelected(id); reloadConnections(); }} onCancel={() => setForm(null)} />}

      {current && <CapabilityTable connection={current} probing={!!probing} />}
      <RequestLog />
    </div>
  );
}

function CapabilitySummary({ connection }: { connection: Connection }) {
  const caps = Object.values(connection.capabilities);
  if (!caps.length) return <Chip tone="outline">尚未自检</Chip>;
  if (caps.some((c) => c.stale)) return <Chip tone="warning" icon="warning">需重新检测</Chip>;
  const text = connection.capabilities.text?.status === 'supported';
  return text ? <Chip tone="ok" icon="check">已检测</Chip> : <Chip tone="danger" icon="error">文本生成不可用</Chip>;
}

function ConnectionForm({ initial, existing, onDone, onCancel }: { initial: Form; existing: Connection | null; onDone: (id: string | null) => void; onCancel: () => void }) {
  const [form, setForm] = useState<Form>(initial);
  const [key, setKey] = useState('');
  const [error, setError] = useState('');
  const { busy, run } = useAction();
  const preset = PRESETS.find((p) => p.id === form.provider)!;
  const set = <K extends keyof Form>(field: K, value: Form[K]) => setForm((f) => ({ ...f, [field]: value }));
  const loopback = /^http:\/\/(127\.0\.0\.1|localhost|\[::1\])(:\d+)?(\/|$)/i.test(form.base_url.trim());
  const keyless = preset.keyless || loopback;
  const changesBinding = existing && (existing.base_url !== form.base_url.trim().replace(/\/+$/, '') || existing.model_id !== form.model_id.trim() || existing.protocol !== form.protocol);

  async function save(e: React.FormEvent) {
    e.preventDefault();
    setError('');
    if (!existing && !keyless && !key.trim()) return setError('请填写 API Key；本机端点可以不填。');
    const draft: ConnectionDraft = { ...form, name: form.name.trim(), base_url: form.base_url.trim(), model_id: form.model_id.trim(), timeout_s: Number(form.timeout_s), max_output_tokens: Number(form.max_output_tokens) };
    const envelope = await run(async () => ensure(await api.connectionSave(draft, key.trim() ? key.trim() : null)), existing ? '已保存修改' : '已保存连接；建议先做一次能力自检');
    setKey('');
    if (!envelope) return;
    const payload = envelope.data_ref ? await api.result<{ connection_id?: string }>(envelope.data_ref).catch(() => null) : null;
    onDone(payload?.connection_id ?? existing?.id ?? null);
  }

  return (
    <section className="section" aria-labelledby="form-title">
      <div className="section-head">
        <h2 id="form-title">{existing ? `编辑：${existing.name}` : `新建 ${preset.name} 连接`}</h2>
        {existing && <Chip tone={existing.has_key ? 'ok' : 'neutral'} icon={existing.has_key ? 'check' : undefined}>{existing.has_key ? '密钥已保存' : '未保存密钥'}</Chip>}
      </div>
      <form className="form-grid" onSubmit={save} aria-labelledby="form-title">
        <Field label="显示名称">
          <input value={form.name} maxLength={60} onChange={(e) => set('name', e.target.value)} required />
        </Field>
        <Field label="协议">
          <select value={form.protocol} onChange={(e) => set('protocol', e.target.value)} disabled={PROTOCOLS_FOR[form.provider].length === 1}>
            {PROTOCOLS_FOR[form.provider].map((p) => (
              <option key={p} value={p}>{PROTOCOLS[p]}</option>
            ))}
          </select>
        </Field>
        <Field label="Base URL" className="full" hint="填写到服务根地址（如 https://api.deepseek.com），不要包含 /chat/completions；只接受 HTTPS，本机地址除外。">
          <input className="mono" value={form.base_url} onChange={(e) => set('base_url', e.target.value)} spellCheck={false} required />
        </Field>
        <Field label="模型 ID" hint="列表只是参考；以服务实际提供的为准。">
          <input className="mono" value={form.model_id} list={`models-${form.provider}`} onChange={(e) => set('model_id', e.target.value)} spellCheck={false} required />
        </Field>
        <datalist id={`models-${form.provider}`}>
          {preset.models.map((m) => <option key={m} value={m} />)}
        </datalist>
        <Field
          label={keyless ? 'API Key（可选）' : 'API Key'}
          hint={
            <>
              <Icon name="shield" /> 保存在本机 Windows 加密存储（DPAPI），不进入备份，也不会显示在界面上。
            </>
          }
        >
          <input className="mono" type="password" value={key} onChange={(e) => setKey(e.target.value)} autoComplete="off" spellCheck={false} placeholder={existing?.has_key ? '已保存；留空表示不更换' : ''} />
        </Field>
        <Field label="超时">
          <select value={form.timeout_s} onChange={(e) => set('timeout_s', Number(e.target.value))}>
            {[30, 60, 120, 300, 600].map((s) => <option key={s} value={s}>{s} 秒</option>)}
          </select>
        </Field>
        <Field label="单次输出令牌上限" hint="每次请求最多生成的令牌数（256–65,536），用来控制费用。">
          <input className="num" type="number" min={256} max={65536} step={256} value={form.max_output_tokens} onChange={(e) => set('max_output_tokens', Number(e.target.value))} required />
        </Field>
        <Field label="代理">
          <select value={form.proxy} onChange={(e) => set('proxy', e.target.value as Form['proxy'])}>
            <option value="system">使用系统代理设置</option>
            <option value="none">不使用代理</option>
          </select>
        </Field>
        {loopback && (
          <Callout className="full" title="本机端点，用户管理">本应用无法核验这个本机服务是否把内容继续转发到别处。</Callout>
        )}
        {changesBinding && (
          <Callout className="full" tone="warning" title="更换地址、模型或协议后需要重新做能力自检">已有的检测结果会标为“需重新检测”。</Callout>
        )}
        <ErrorLine text={error} />
        <div className="actions full">
          <button className="btn btn-primary" disabled={busy}>{busy ? '正在保存…' : '保存'}</button>
          <button className="btn" type="button" onClick={onCancel} disabled={busy}>取消</button>
          <span className="t-caption text-tertiary">保存只写入本机，不会发送任何请求；能力自检需要另外确认。</span>
        </div>
      </form>
    </section>
  );
}

function CapabilityTable({ connection, probing }: { connection: Connection; probing: boolean }) {
  const rows = CAPABILITIES.map(([id, label]) => [id, label, connection.capabilities[id]] as const);
  const tested = Object.values(connection.capabilities).length > 0;
  return (
    <section className="section" aria-labelledby="caps">
      <div className="section-head">
        <h2 id="caps">能力自检结果：{connection.name}</h2>
        <span className="t-caption text-tertiary">绑定：{connection.host} · {connection.model_id} · 适配器 {connection.adapter_version}；更换任一项后需重新检测</span>
      </div>
      {probing && <Callout title="正在自检">正在向 {connection.host} 发送几条合成测试内容（不含你的资料）。</Callout>}
      {!tested && !probing ? (
        <Empty icon="refresh" title="尚未自检">自检会发送约 6 条合成测试请求（不含你的资料），确认文本、流式、工具调用、结构化输出和图片输入能力；可能产生少量费用。</Empty>
      ) : (
        <div className="table-wrap" style={{ maxWidth: 980 }}>
          <table className="table">
            <thead>
              <tr><th>能力</th><th>状态</th><th>依据</th><th>检测时间</th></tr>
            </thead>
            <tbody>
              {rows.map(([id, label, cap]) => {
                const [text, icon] = cap ? STATUS[cap.status] : ['未检测', 'unknown' as IconName];
                return (
                  <tr key={id}>
                    <td className="label">{label}</td>
                    <td className={`cap-${cap?.status ?? 'unknown'}`}>
                      <span className="inline-icon"><Icon name={icon} />{text}</span>
                      {cap?.stale && <Chip tone="warning">需重新检测</Chip>}
                    </td>
                    <td className="text-secondary">{cap?.evidence ?? '—'}</td>
                    <td className="num text-tertiary" title={cap?.tested_at}>{cap ? relative(cap.tested_at) : '—'}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function RequestLog() {
  const { revision } = useSession();
  const log = useLoad(() => api.generationRequests(), [revision]);
  const rows = log.data ?? [];
  const totals = useMemo(() => rows.reduce((t, r) => ({ input: t.input + (r.input_tokens ?? 0), output: t.output + (r.output_tokens ?? 0) }), { input: 0, output: 0 }), [rows]);
  return (
    <section className="section" aria-labelledby="log">
      <div className="section-head">
        <h2 id="log">外发记录</h2>
        <span className="t-caption text-tertiary">只记录用途、接收方、状态与用量，不保存发送的正文；最近 200 条</span>
      </div>
      {log.error && <ErrorLine text={log.error} />}
      {!log.data && !log.error && <Skeleton rows={3} />}
      {log.data && rows.length === 0 && <p className="text-secondary">还没有发出过请求。</p>}
      {rows.length > 0 && (
        <>
          <p className="t-caption text-secondary" style={{ marginBottom: 8 }}>
            合计 {rows.length} 次请求 · 输入 {totals.input.toLocaleString('zh-CN')} 令牌 · 输出 {totals.output.toLocaleString('zh-CN')} 令牌
          </p>
          <div className="table-wrap table-scroll">
            <table className="table">
              <thead>
                <tr><th>时间</th><th>用途</th><th>接收方</th><th>模型</th><th>状态</th><th className="num">输入</th><th className="num">输出</th></tr>
              </thead>
              <tbody>
                {rows.map((r, i) => (
                  <tr key={i}>
                    <td className="num text-secondary" title={r.started_at}>{relative(r.started_at)}</td>
                    <td>{PURPOSE[r.purpose] ?? r.purpose}</td>
                    <td className="mono t-caption">{r.host}</td>
                    <td className="mono t-caption">{r.model_id}</td>
                    <td>{r.status === 'completed' ? <Chip tone="ok">完成</Chip> : r.status === 'sent' ? <Chip tone="info">进行中</Chip> : r.status === 'unknown' || r.error === 'cancelled' ? <Chip tone="warn" title="请求已发出，但中断前没有收到回应；已计入额度">结果未知</Chip> : <Chip tone="danger" title={r.error ?? ''}>失败</Chip>}</td>
                    <td className="num">{r.input_tokens?.toLocaleString('zh-CN') ?? '—'}</td>
                    <td className="num">{r.output_tokens?.toLocaleString('zh-CN') ?? '—'}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </section>
  );
}
