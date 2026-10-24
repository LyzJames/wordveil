// Separate approvals for public search, public knowledge and read-only MCP.
// Remote text is rendered as text; no HTML, scripts, images or instructions run.
import React, { useEffect, useState } from 'react';
import { api, ensure, relative, warningText, type ActionPreview, type ExternalOverview, type PublicPackageInfo, type ToolCatalog, type WebResearch } from './api';
import { useSession } from './session';
import { Busy, Callout, Chip, Empty, ErrorLine, Field, Modal, Tabs, useAction, useLoad, useNotice } from './ui';
import './external.css';

function Preview({ value, close, started }: { value: ActionPreview; close: () => void; started: (task: string) => void }) {
  const { busy, run } = useAction();
  const update = value.operation === 'knowledge_apply' ? value.sent as { package: PublicPackageInfo; previous_version: string | null; changed: unknown[]; removed: string[]; sha256: string; note: string } : null;
  return <Modal title={value.operation === 'knowledge_apply' ? '知识包更新预览' : '核对本次发送内容'} wide onClose={close} footer={<>
    <button className="btn" disabled={busy} onClick={() => run(async () => { await api.externalDiscard(value.action_id); close(); })}>取消</button>
    <button className="btn primary" disabled={busy} onClick={() => run(async () => { const result = await api.externalStart(value.action_id); if (typeof result === 'string') { started(result); close(); } })}>{busy ? '等待确认…' : '打开系统确认'}</button>
  </>}>
    <div className="external-stack">
      <div><strong>目的地：</strong>{value.destination} <Chip>{value.max_requests ? `最多 ${value.max_requests} 次请求` : '离线导入'}</Chip></div>
      {value.purpose && <p>用途：{value.purpose}</p>}
      {value.tool && <p>工具：{value.tool}</p>}
      {value.sensitive_hint && <Callout tone="attention">{value.sensitive_hint}</Callout>}
      {update ? <section className="external-stack" aria-label="本次知识包变更">
        <h2 className="t-section">{update.package.title}</h2>
        <p>{update.previous_version ? `由 ${update.previous_version} 更新至 ${update.package.version}` : '首次加入公共检索'} · 新增或变更 {update.changed.length} 份 · 移除 {update.removed.length} 份</p>
        <PackageDetails info={update.package} />
        <p className="text-secondary">{update.note}</p>
        <details><summary>查看完整核验记录</summary><pre className="external-json">{JSON.stringify(value.sent, null, 2)}</pre></details>
      </section> : <pre className="external-json" aria-label="本次实际内容">{typeof value.sent === 'string' ? value.sent : JSON.stringify(value.sent, null, 2)}</pre>}
      {value.domains && <p className="text-secondary">来源白名单：{value.domains.join('、')}</p>}
      {value.pricing && <p className="text-secondary">{value.pricing}</p>}
      <p className="text-secondary">{value.note} 确认仅在十分钟内有效，且只用一次。</p>
    </div>
  </Modal>;
}

function useTaskResult(onResult: (kind: string, ref: string) => Promise<void>) {
  const { tasks, refresh } = useSession();
  const notice = useNotice();
  const [pending, setPending] = useState<string | null>(null);
  const task = tasks.find(t => t.task_id === pending);
  useEffect(() => {
    if (!task || task.status === 'running' || !task.envelope) return;
    setPending(null);
    if (task.envelope.status === 'completed' && task.envelope.data_ref) {
      onResult(task.skill, task.envelope.data_ref).then(refresh).catch(e => notice('error', String(e)));
    } else notice('error', warningText(task.envelope));
  }, [task?.status, task?.envelope, pending]); // One receipt per finished task.
  return { pending, setPending, status: pending ? <Callout action={<button className="btn" onClick={() => api.cancelTask(pending).catch(e => notice('error', String(e)))}>停止</button>}><Busy label={task?.progress ? `${task.progress.stage} · ${task.progress.done}/${task.progress.total}` : '正在执行已确认的任务…'} /></Callout> : null };
}

const PACKAGE_STATE: Record<string, string> = { active: '正在使用', superseded: '历史版本', disabled: '已停用', pending: '正在导入', incomplete: '导入未完成' };
function PackageDetails({ info }: { info: PublicPackageInfo }) {
  return <div className="external-stack">
    <p className="text-secondary">版本 {info.version} · 中国大陆 · 核验截至 {info.as_of}</p>
    <div className="external-documents">{info.documents.map(doc => <details key={doc.id} className="external-document"><summary>{doc.title}</summary>
      <p>{doc.issuer}</p><p className="text-secondary">公布：{doc.published_on}　施行：{doc.effective_on}　核验：{doc.verified_on}</p>
      <p className="text-secondary">状态：{doc.status === 'in_force_as_of' ? '来源核验日有效，个案适用需另核对' : doc.status}</p>
      <p className="external-wrap">来源：{doc.source_url}</p><p className="t-caption external-wrap">正文 SHA-256：{doc.sha256}</p>
    </details>)}</div>
    {info.limitations.map((text, i) => <p className="text-secondary t-caption" key={i}>{text}</p>)}
  </div>;
}

export function KnowledgePage() {
  const { revision, go } = useSession();
  const { data, loading, error, reload } = useLoad(api.externalOverview, [revision]);
  const { busy, run } = useAction();
  const [preview, setPreview] = useState<ActionPreview | null>(null);
  const task = useTaskResult(async () => { reload(); });
  return <div className="page fade-in external-page">
    <header className="page-head"><div className="titles"><h1 className="t-page">知识中心</h1><p className="text-secondary">查看公共资料的来源、日期、版本和更新差异。</p></div><div className="inline-actions">
      <button className="btn" disabled={busy || !!task.pending} onClick={() => run(async () => setPreview(await api.knowledgePreview(false)))}>导入离线知识包</button>
      <button className="btn primary" disabled={busy || !!task.pending} onClick={() => run(async () => setPreview(await api.knowledgePreview(true)))}>预览内置基础包</button>
    </div></header>
    <Callout title="先预览，再加入检索">首次导入也需要确认。导入完成的公共包会加入新的问答检索范围；历史回答保留原来的版本引用。联网精读与知识保存分别确认。</Callout>
    {task.status}<ErrorLine text={error} />
    {loading && <Busy label="正在读取知识包…" />}
    {!loading && !data?.packages.length && <Empty icon="library" title="还没有导入公共知识包" action={<button className="btn" onClick={() => go({ name: 'tools' })}>去联网研究</button>}>内置基础包覆盖合同租房、借贷、劳动、消费、保险、婚姻继承和个人税务，可在断网时预览并导入。它不包含全部地方规定、案例或税收优惠政策。</Empty>}
    <div className="external-stack">{data?.packages.map(pkg => <section className="card" key={pkg.id}><div className="external-row"><h2 className="t-section">{pkg.metadata.title}</h2><Chip>{PACKAGE_STATE[pkg.state] || pkg.state}</Chip>
      {pkg.state === 'active' && <button className="btn small" disabled={busy} onClick={() => run(async () => { const e = await api.externalSave('disable_package', { id: pkg.id }); if ('warnings' in e) ensure(e); reload(); })}>停用</button>}</div>
      <PackageDetails info={pkg.metadata} /><p className="t-caption external-wrap">知识包 SHA-256：{pkg.sha256}</p>
      {pkg.state === 'incomplete' && <Callout tone="attention">本次内容尚未进入默认检索。可重新预览相同包并确认继续，已完成的文档不会重复导入。</Callout>}
    </section>)}</div>
    {preview && <Preview value={preview} close={() => setPreview(null)} started={task.setPending} />}
  </div>;
}

function SourceSettings({ overview, saved }: { overview: ExternalOverview; saved: () => void }) {
  const { connections } = useSession();
  const eligible = connections.filter(c => c.provider === 'deepseek' && ['api.deepseek.com','api.deepseek.com:443'].includes(c.host));
  const [connection, setConnection] = useState('');
  const [domains, setDomains] = useState('gov.cn, npc.gov.cn, court.gov.cn, pbc.gov.cn, nfra.gov.cn, samr.gov.cn');
  const { busy, run } = useAction();
  return <details className="card"><summary className="t-strong">公共搜索配置与来源白名单</summary><div className="external-stack">
    <p className="text-secondary">Search / Fetch 使用已保存的 DeepSeek Key。选择连接后单独确认启用，生成授权不会自动开启搜索。</p>
    <Field label="DeepSeek 连接"><select value={connection} onChange={e => setConnection(e.target.value)}><option value="">请选择</option>{eligible.map(c => <option key={c.id} value={c.id}>{c.name}</option>)}</select></Field>
    <Field label="允许采纳的来源域名" hint="用英文逗号分隔；只按完整域名或子域名匹配。"><input value={domains} onChange={e => setDomains(e.target.value)} /></Field>
    <button className="btn" disabled={busy || !connection} onClick={() => run(async () => { const e = await api.externalSave('source', { connection_id: connection, domains: domains.split(',').map(s => s.trim()).filter(Boolean) }); if ('warnings' in e) ensure(e); saved(); })}>核对并启用搜索来源</button>
    {overview.sources.map(source => <div className="external-row" key={source.id}><span>{source.name} · {source.enabled ? '可逐次授权使用' : '已停用'}</span><span className="text-secondary external-wrap">{source.domains.join('、')}</span>{source.enabled && <button className="btn small" onClick={() => run(async () => { const e = await api.externalSave('disable_source', { id: source.id }); if ('warnings' in e) ensure(e); saved(); })}>撤销</button>}</div>)}
  </div></details>;
}

export function ToolsPage() {
  const { revision } = useSession();
  const { data, loading, error, reload } = useLoad(api.externalOverview, [revision]);
  const { busy, run } = useAction();
  const [tab, setTab] = useState<'search' | 'mcp' | 'audit'>('search');
  const [preview, setPreview] = useState<ActionPreview | null>(null);
  const [source, setSource] = useState(''); const [query, setQuery] = useState(''); const [purpose, setPurpose] = useState('查阅公共法律金融资料');
  const [web, setWeb] = useState<WebResearch | null>(null);
  const [webRef,setWebRef]=useState('');
  const [reviewItem,setReviewItem]=useState<number|null>(null);
  const [review,setReview]=useState({title:'',issuer:'',published_on:'',effective_on:'',version:new Date(Date.now()+8*3600_000).toISOString().slice(0,10),confirmed_official_text:false});
  const [catalog, setCatalog] = useState<{ ref: string; value: ToolCatalog } | null>(null);
  const [toolResult, setToolResult] = useState<unknown>(null);
  const [name, setName] = useState(''); const [endpoint, setEndpoint] = useState('https://learn.microsoft.com/api/mcp'); const [key, setKey] = useState('');
  const [binding, setBinding] = useState(''); const [args, setArgs] = useState('{}');
  const task = useTaskResult(async (kind, ref) => {
    if (kind === 'web-research') {setWeb(await api.result<WebResearch>(ref));setWebRef(ref);}
    if (kind === 'knowledge-update') setToolResult(await api.result(ref));
    if (kind === 'external-tools') {
      const value = await api.result<ToolCatalog | Record<string, unknown>>(ref);
      if ('exchange' in value && (value as ToolCatalog).exchange?.result?.tools) setCatalog({ ref, value: value as ToolCatalog }); else setToolResult(value);
    }
    reload();
  });
  if (loading && !data) return <Busy label="正在读取外部能力配置…" />;
  return <div className="page fade-in external-page"><header className="page-head"><div className="titles"><h1 className="t-page">工具与连接</h1><p className="text-secondary">公共搜索和只读工具。发送前核对内容，每次独立授权。</p></div></header>
    <Tabs value={tab} onChange={setTab} items={[["search", "联网研究"], ["mcp", "只读 MCP"], ["audit", "请求记录"]]} />
    <ErrorLine text={error} />{task.status}
    {data && tab === 'search' && <div className="external-stack"><SourceSettings overview={data} saved={reload} />
      <section className="card external-stack"><h2 className="t-section">研究公开资料</h2><Field label="已批准的搜索来源"><select value={source} onChange={e => setSource(e.target.value)}><option value="">请选择</option>{data.sources.filter(s => s.enabled).map(s => <option key={s.id} value={s.id}>{s.name} · {s.domains.join('、')}</option>)}</select></Field>
        <Field label="实际搜索词" hint="最多 128 字；请先移除姓名、证件号、账号和详细案情。"><input maxLength={128} value={query} onChange={e => setQuery(e.target.value)} placeholder="例如：住房租赁 押金退还 site:gov.cn" /></Field>
        <Field label="本次用途"><input maxLength={200} value={purpose} onChange={e => setPurpose(e.target.value)} /></Field>
        <button className="btn primary" disabled={!source || !query.trim() || busy || !!task.pending} onClick={() => run(async () => setPreview(await api.networkPreview('search', { source_id: source, query, purpose })))}>预览搜索内容</button>
      </section>
      {web && <section className="external-stack"><div className="external-row"><h2 className="t-section">研究结果</h2><Chip>待核实资料</Chip><span className="text-secondary">{relative(web.accessed_at)}</span></div><p className="text-secondary">{web.note}</p>
        {!web.items.length && <Empty title="白名单内没有可用结果">没有自动补发请求；可修改查询后重新预览。</Empty>}
        {web.items.map((item, i) => <article key={i} className="card external-stack"><h3 className="t-strong">{item.title || '未提供标题'}</h3><p className="external-wrap t-caption">{item.url}</p><p className="text-secondary t-caption">{item.publisher || '发布者未知'} · 发布日期：{item.published_at || '未知'} · 访问：{item.accessed_at}</p><pre className="external-text">{item.text}</pre>
          {item.snapshot !== 'fetched-markdown' && <button className="btn" disabled={busy || !!task.pending} onClick={() => run(async () => setPreview(await api.networkPreview('fetch', { source_id: source, query: item.url, purpose: '精读已选官方网页' })))}>预览精读此页</button>}
          {item.snapshot === 'fetched-markdown' && <button className="btn" disabled={busy || !!task.pending} onClick={()=>{setReviewItem(i);setReview({...review,title:item.title,confirmed_official_text:false});}}>核对并保存为公共知识</button>}
          <details><summary className="text-secondary t-caption">快照校验信息</summary><p className="t-caption external-wrap">{item.sha256} · {item.snapshot === 'fetched-markdown' ? '网页正文快照' : '搜索摘要，未核实原文'}</p></details>
        </article>)}
      </section>}
    </div>}
    {data && tab === 'mcp' && <div className="external-stack"><Callout title="工具需要单独批准">先连接并读取目录，审阅工具说明与参数后才建立绑定。每次调用重新检查服务器和 Schema；变化后停止调用。只读注解不代替你对服务的信任。</Callout>
      <section className="card external-stack"><h2 className="t-section">律金公共法律金融服务</h2><p className="text-secondary">独立服务只查询随应用发布的公共法律金融文本，具体范围与核验日期见知识中心。默认关闭；本机随机端口与随机凭据，锁定或退出会停止。</p><div className="inline-actions"><button className="btn" disabled={busy} onClick={() => run(async () => { await api.publicMcp(true); reload(); })}>启动本机只读服务</button><button className="btn" disabled={busy} onClick={() => run(async () => { await api.publicMcp(false); reload(); })}>停止本机服务</button></div></section>
      <details className="card"><summary className="t-strong">添加远程只读 MCP</summary><div className="external-stack"><Field label="服务名称"><input value={name} maxLength={80} onChange={e => setName(e.target.value)} /></Field><Field label="Streamable HTTP 地址"><input value={endpoint} onChange={e => setEndpoint(e.target.value)} spellCheck={false} /></Field><Field label="专用 Bearer Token（若服务需要）" hint="仅交给核心加密保存，不作为工具参数。"><input type="password" autoComplete="off" value={key} onChange={e => setKey(e.target.value)} /></Field><button className="btn" disabled={busy || !name.trim()} onClick={() => run(async () => { const e = await api.externalSave('mcp', { name, endpoint, timeout_s: 30, proxy: 'system' }, key || null); setKey(''); if ('warnings' in e) ensure(e); reload(); })}>核对并保存连接</button><p className="text-secondary t-caption">支持 MCP 2026-07-28 与 SDK 协商的旧版握手。任意本地可执行程序与 stdio 服务暂不开放。</p></div></details>
      {data.connections.map(c => <section className="card external-stack" key={c.id}><div className="external-row"><h3 className="t-strong">{c.name}</h3><Chip>{c.enabled ? '可读取目录' : '已撤销'}</Chip></div><p className="external-wrap text-secondary">{c.endpoint}</p>{c.enabled && <div className="inline-actions"><button className="btn" disabled={busy || !!task.pending} onClick={() => run(async () => setPreview(await api.networkPreview('catalog', { connection_id: c.id })))}>预览连接与工具枚举</button><button className="btn" onClick={() => run(async () => { const e = await api.externalSave('disable_mcp', { id: c.id }); if ('warnings' in e) ensure(e); reload(); })}>撤销连接</button></div>}</section>)}
      {catalog && <section className="card external-stack"><h2 className="t-section">本次工具目录</h2><p className="text-secondary">已排除未声明只读、Schema 不支持或不符合约束的工具。你仍需核对实际服务的用途与权限。</p>{catalog.value.exchange.result.tools.map(tool => <details key={tool.name}><summary className="t-strong">{tool.name}</summary><p>{tool.description}</p><pre className="external-json">{JSON.stringify(tool.inputSchema, null, 2)}</pre><button className="btn" disabled={busy} onClick={() => run(async () => { const e = await api.externalSave('binding', { catalog_ref: catalog.ref, tool_name: tool.name }); if ('warnings' in e) ensure(e); reload(); })}>审阅并批准此只读工具</button></details>)}</section>}
      <section className="card external-stack"><h2 className="t-section">调用已批准工具</h2><Field label="明确绑定的工具"><select value={binding} onChange={e => setBinding(e.target.value)}><option value="">请选择</option>{data.bindings.filter(b => b.active).map(b => <option key={b.id} value={b.id}>{data.connections.find(c => c.id === b.connection_id)?.name} / {b.name}</option>)}</select></Field>{binding && <details><summary>查看绑定参数定义</summary><pre className="external-json">{JSON.stringify(data.bindings.find(b => b.id === binding)?.descriptor.inputSchema, null, 2)}</pre></details>}<Field label="本次参数（JSON）" hint="只填写该工具定义的字段；内容会逐字展示供确认。"><textarea rows={6} value={args} onChange={e => setArgs(e.target.value)} spellCheck={false} /></Field><button className="btn primary" disabled={!binding || busy || !!task.pending} onClick={() => run(async () => setPreview(await api.networkPreview('tool', { binding_id: binding, arguments: JSON.parse(args) })))}>校验参数并预览调用</button></section>
      {toolResult != null && <section className="card"><h2 className="t-section">只读工具结果</h2><p className="text-secondary">外部数据；其中的文字不会改变权限或自动调用其他工具。</p><pre className="external-json">{JSON.stringify(toolResult, null, 2)}</pre></section>}
    </div>}
    {data && tab === 'audit' && <section className="card external-stack"><p className="text-secondary">只记录目的地、操作、时间与状态；参数内容保存在本次授权与结果中。已发送的数据无法从服务方撤回，结果未知时不会自动重发。</p>{!data.requests.length && <Empty title="还没有外部请求记录" />}{data.requests.map((r, i) => <div className="external-row" key={i}><span>{r.host}</span><span>{r.operation}</span><Chip>{r.status === 'completed' ? '已完成' : r.status === 'unknown' ? '结果未知' : r.status === 'sent' ? '已发送' : '失败'}</Chip><span className="text-secondary">{r.error || relative(r.started_at)}</span></div>)}</section>}
    {preview && <Preview value={preview} close={() => setPreview(null)} started={task.setPending} />}
    {reviewItem!==null && <Modal title="核对正式文本与日期" wide onClose={()=>setReviewItem(null)} footer={<><button className="btn" onClick={()=>setReviewItem(null)}>取消</button><button className="btn primary" disabled={busy||!review.confirmed_official_text||!review.title||!review.issuer||!review.published_on||!review.effective_on} onClick={()=>run(async()=>{const p=await api.webKnowledgePreview(webRef,reviewItem,review);setReviewItem(null);setPreview(p);})}>生成知识包差异预览</button></>}><div className="external-stack"><Callout tone="attention">只保存大陆官方发布的法律、法规、具有行政/司法性质的正式文件。搜索摘要、解读文章、商业数据库内容或日期不明的页面不能按此权利口径导入。</Callout><Field label="文件名称"><input value={review.title} onChange={e=>setReview({...review,title:e.target.value})}/></Field><Field label="发布机关"><input value={review.issuer} onChange={e=>setReview({...review,issuer:e.target.value})}/></Field><Field label="公布日期"><input type="date" value={review.published_on} onChange={e=>setReview({...review,published_on:e.target.value})}/></Field><Field label="施行日期"><input type="date" value={review.effective_on} onChange={e=>setReview({...review,effective_on:e.target.value})}/></Field><Field label="本次版本"><input value={review.version} maxLength={80} onChange={e=>setReview({...review,version:e.target.value})}/></Field><label className="external-row"><input type="checkbox" checked={review.confirmed_official_text} onChange={e=>setReview({...review,confirmed_official_text:e.target.checked})}/><span>我已核对官方原文、正式文件权利、日期与目前效力；核验日期记为本次确认日期。</span></label></div></Modal>}
  </div>;
}
