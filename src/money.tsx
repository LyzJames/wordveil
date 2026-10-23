// Cash flows, explicit column mapping for bank/payment tables, and the
// deterministic Decimal calculators (only confirmed inputs are used).
import React, { useMemo, useState } from 'react';
import { api, applyChange, commitChange, ensure, KINDS, money, prepareChange, type Cashflow, type Snapshot, type VersionPages } from './api';
import { ExportButton } from './export';
import { Icon } from './icons';
import { useSession } from './session';
import { Amount, Chip, Empty, ErrorLine, Field, Modal, useAction } from './ui';

const KIND_RULE_DEFAULTS = [
  { contains: '本人账户转账', kind: 'transfer' },
  { contains: '退款', kind: 'refund' },
];

const SCALE = 1_000_000n;
/** Exact sum of decimal strings (up to six decimals) without floating point. */
function sum(values: string[]): string {
  let total = 0n;
  for (const v of values) {
    const m = /^(-?)(\d+)(?:\.(\d{1,6}))?$/.exec(v.trim());
    if (!m) continue;
    const scaled = BigInt(m[2]) * SCALE + BigInt((m[3] ?? '').padEnd(6, '0') || '0');
    total += m[1] ? -scaled : scaled;
  }
  const negative = total < 0n;
  const abs = negative ? -total : total;
  return `${negative ? '-' : ''}${abs / SCALE}.${(abs % SCALE).toString().padStart(6, '0')}`;
}

export function CashflowTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { refresh } = useSession();
  const { busy, run } = useAction();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [mapping, setMapping] = useState(false);
  const [adding, setAdding] = useState(false);
  const candidates = s.cashflows.filter((c) => c.status === 'candidate');
  // Same default basis as the calculator: transfers excluded, refunds listed separately.
  const counted = s.cashflows.filter((c) => c.status === 'confirmed' && c.kind !== 'transfer' && c.kind !== 'refund' && c.currency === 'CNY');
  const totals = useMemo(() => ({ income: sum(counted.filter((c) => c.direction === 'in').map((c) => c.amount)), expense: sum(counted.filter((c) => c.direction === 'out').map((c) => c.amount)) }), [counted]);
  async function batch(type: 'confirm_cashflows' | 'reject_cashflows', ids: string[]) {
    const done = await run(() => applyChange(matter, { type, cashflow_ids: ids }), type === 'confirm_cashflows' ? '已确认' : '已排除');
    if (done) {
      setSelected(new Set());
      refresh();
    }
  }
  const toggle = (id: string) => setSelected((set) => {
    const next = new Set(set);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    return next;
  });
  return (
    <section>
      <div className="row-between toolbar">
        <p className="t-caption text-secondary">金额以字符串精确保存，计算只使用<strong>已确认</strong>的记录；“本人转账”默认不计入收支。</p>
        <div className="actions">
          <button className="btn btn-primary" onClick={() => setMapping(true)}>
            <Icon name="list" />
            从表格生成
          </button>
          <button className="btn" onClick={() => setAdding(true)}>
            <Icon name="add" />
            手动添加
          </button>
        </div>
      </div>
      {candidates.length > 0 && (
        <div className="callout attention" style={{ marginBottom: 12 }}>
          <Icon name="highlight" />
          <div className="body">
            <strong>{candidates.length} 条候选记录待确认</strong>
            <span className="text-secondary">对照表格原文核对后确认；确认会弹出系统确认框。</span>
          </div>
          <div className="actions">
            <button className="btn btn-sm" disabled={busy || !selected.size} onClick={() => batch('reject_cashflows', [...selected])}>排除所选</button>
            <button className="btn btn-sm" disabled={busy || !selected.size} onClick={() => batch('confirm_cashflows', [...selected])}>确认所选（{selected.size}）</button>
            <button className="btn btn-sm btn-primary" disabled={busy} onClick={() => batch('confirm_cashflows', candidates.map((c) => c.id))}>全部确认（{candidates.length}）</button>
          </div>
        </div>
      )}
      {s.cashflows.length === 0 ? (
        <Empty icon="calculator" title="还没有金额记录" action={<button className="btn btn-primary" onClick={() => setMapping(true)}>从表格生成</button>}>
          可从导入的银行流水（CSV）或支付账单（XLSX）按列生成候选，也可以手动添加。
        </Empty>
      ) : (
        <div className="table-wrap cash-table">
          <table className="table">
            <thead>
              <tr><th className="check-cell" aria-label="选择" /><th>日期</th><th>对方 / 摘要</th><th className="num">收入（元）</th><th className="num">支出（元）</th><th>类型</th><th>状态</th></tr>
            </thead>
            <tbody>
              {s.cashflows.map((c: Cashflow) => (
                <tr key={c.id} className={c.status === 'rejected' || c.kind === 'transfer' ? 'muted' : undefined}>
                  <td className="check-cell">{c.status === 'candidate' && <input type="checkbox" aria-label="选择" checked={selected.has(c.id)} onChange={() => toggle(c.id)} />}</td>
                  <td className="num">{c.date}</td>
                  <td className="name-cell">
                    {[c.counterparty, c.description].filter(Boolean).join(' · ')}
                    {c.kind === 'transfer' && <> <Chip>转账</Chip></>}
                  </td>
                  <td className="num">{c.direction === 'in' && <Amount value={c.amount} direction="in" currency={c.currency} kind={c.kind} />}</td>
                  <td className="num">{c.direction === 'out' && <Amount value={c.amount} direction="out" currency={c.currency} kind={c.kind} />}</td>
                  <td>{KINDS[c.kind] ?? c.kind}</td>
                  <td>
                    <Chip tone={c.status === 'confirmed' ? 'ok' : c.status === 'candidate' ? 'warn' : 'neutral'}>{c.status === 'confirmed' ? '已确认' : c.status === 'candidate' ? '待确认' : '已排除'}</Chip>
                  </td>
                </tr>
              ))}
            </tbody>
            {counted.length > 0 && (
              <tfoot>
                <tr>
                  <td />
                  <td colSpan={2}>合计（已确认，人民币，不含转账与退款）</td>
                  <td className="num">{money(totals.income)}</td>
                  <td className="num">{money(totals.expense)}</td>
                  <td colSpan={2} />
                </tr>
              </tfoot>
            )}
          </table>
        </div>
      )}
      {mapping && <TableMapping matter={matter} snapshot={s} onClose={() => setMapping(false)} onDone={() => { setMapping(false); refresh(); }} />}
      {adding && <AddCashflow matter={matter} onClose={() => setAdding(false)} onDone={() => { setAdding(false); refresh(); }} />}
    </section>
  );
}

function AddCashflow({ matter, onClose, onDone }: { matter: string; onClose: () => void; onDone: () => void }) {
  const [form, setForm] = useState({ date: '', amount: '', direction: 'out', kind: 'expense', counterparty: '', description: '' });
  const { busy, run } = useAction();
  const set = (key: keyof typeof form) => (e: React.ChangeEvent<HTMLInputElement | HTMLSelectElement>) => setForm({ ...form, [key]: e.target.value });
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const done = await run(() => applyChange(matter, { type: 'add_cashflow', ...form, amount: form.amount.trim(), counterparty: form.counterparty || null, description: form.description || null }), '已添加');
    if (done) onDone();
  }
  return (
    <Modal title="手动添加金额记录" icon="add" onClose={onClose}>
      <form className="stack" onSubmit={submit}>
        <div className="grid-2">
          <Field label="日期"><input type="date" value={form.date} onChange={set('date')} required /></Field>
          <Field label="金额（元）"><input value={form.amount} onChange={set('amount')} inputMode="decimal" pattern="\d{1,15}(\.\d{1,6})?" required /></Field>
          <Field label="方向"><select value={form.direction} onChange={set('direction')}><option value="out">支出</option><option value="in">收入</option></select></Field>
          <Field label="类型"><select value={form.kind} onChange={set('kind')}>{Object.entries(KINDS).map(([id, l]) => <option key={id} value={id}>{l}</option>)}</select></Field>
          <Field label="对方（可选）"><input value={form.counterparty} onChange={set('counterparty')} maxLength={80} /></Field>
          <Field label="摘要（可选）"><input value={form.description} onChange={set('description')} maxLength={200} /></Field>
        </div>
        <p className="t-caption text-tertiary">手动添加的记录直接记为“已确认”。</p>
        <div className="actions end">
          <button type="button" className="btn" onClick={onClose}>取消</button>
          <button className="btn btn-primary" disabled={busy}>添加</button>
        </div>
      </form>
    </Modal>
  );
}

type Preview = { rows: number; rows_with_errors: number; preview: { row: number; date: string | null; amount: string | null; direction: string | null; kind: string; counterparty: string | null; description: string | null; error?: string }[] };

function TableMapping({ matter, snapshot: s, onClose, onDone }: { matter: string; snapshot: Snapshot; onClose: () => void; onDone: () => void }) {
  const tables = s.documents.filter((d) => d.media_kind === 'document');
  const [version, setVersion] = useState('');
  const [header, setHeader] = useState<string[]>([]);
  const [sample, setSample] = useState<string[][]>([]);
  const [map, setMap] = useState<Record<string, number>>({});
  const [rules, setRules] = useState(KIND_RULE_DEFAULTS);
  const [prepared, setPrepared] = useState<{ ref: string; impact: Preview } | null>(null);
  const [error, setError] = useState('');
  const { busy, run } = useAction();

  async function pick(id: string) {
    setVersion(id);
    setPrepared(null);
    setError('');
    const pages = await run(() => api.view<VersionPages>('version', id));
    if (!pages) return;
    const head = pages.blocks.find((b) => b.kind === 'table_header');
    const rows = pages.blocks.filter((b) => b.kind === 'sheet_row').slice(0, 5);
    if (!head && !rows.length) return setError('该资料没有可识别的表格行（支持 CSV、TSV 与 XLSX）。');
    const cells = (b: (typeof pages.blocks)[number]) => ((b.locator.cells ?? []) as { value?: string | null }[]).map((c) => (c && typeof c === 'object' ? c.value ?? '' : ''));
    const names = head ? cells(head) : cells(rows[0]).map((_, i) => `第 ${i + 1} 列`);
    setHeader(names);
    setSample(rows.map(cells));
    const guess: Record<string, number> = {};
    names.forEach((n, i) => {
      const col = i + 1;
      if (/日期|时间/.test(n) && !guess.date) guess.date = col;
      else if (/收入/.test(n) && !/支出/.test(n)) guess.income = col;
      else if (/支出/.test(n) && !/收入/.test(n)) guess.expense = col;
      else if (/^金额|交易金额|金额（元）/.test(n)) guess.amount = col;
      else if (/收\/支|收支|方向|借贷/.test(n)) guess.direction = col;
      else if (/对方|户名|商户/.test(n)) guess.counterparty = col;
      else if (/摘要|说明|备注|商品/.test(n)) guess.description = col;
      else if (/分类|类别/.test(n)) guess.category = col;
      else if (/币种/.test(n)) guess.currency = col;
    });
    setMap(guess);
  }

  async function preview() {
    setError('');
    if (!map.date || !(map.amount || map.income || map.expense)) return setError('至少需要“日期”列，以及“金额”或“收入/支出”列。');
    const change = { type: 'import_cashflows_from_table', version_id: version, mapping: map, currency: 'CNY', kind_rules: rules.filter((r) => r.contains.trim()) };
    const result = await run(() => prepareChange(matter, change));
    if (result) setPrepared({ ref: result.ref, impact: result.impact as unknown as Preview });
  }

  const fields: [string, string][] = [['date', '日期 *'], ['amount', '金额（带正负号）'], ['income', '收入金额'], ['expense', '支出金额'], ['direction', '收/支方向'], ['counterparty', '对方'], ['description', '摘要'], ['category', '分类'], ['currency', '币种']];
  return (
    <Modal
      title="从表格生成金额记录"
      icon="list"
      onClose={onClose}
      wide
      footer={
        prepared ? (
          <>
            <button className="btn" onClick={onClose}>取消</button>
            <button className="btn btn-primary" disabled={busy} onClick={async () => { const done = await run(() => commitChange(matter, prepared.ref), '已生成候选金额记录'); if (done) onDone(); }}>生成候选…</button>
          </>
        ) : undefined
      }
    >
      <Field label="选择已导入的表格资料">
        <select value={version} onChange={(e) => pick(e.target.value)}>
          <option value="">选择…</option>
          {tables.map((d) => <option key={d.version_id} value={d.version_id}>{d.display_name}（第 {d.version_no} 版）</option>)}
        </select>
      </Field>
      <ErrorLine text={error} />
      {header.length > 0 && (
        <>
          <div className="table-wrap table-scroll">
            <table className="table compact">
              <thead><tr>{header.map((h, i) => <th key={i}>{i + 1}. {h}</th>)}</tr></thead>
              <tbody>{sample.map((r, i) => <tr key={i}>{header.map((_, j) => <td key={j}>{r[j]}</td>)}</tr>)}</tbody>
            </table>
          </div>
          <div className="grid-3">
            {fields.map(([key, label]) => (
              <Field key={key} label={label}>
                <select value={map[key] ?? ''} onChange={(e) => { const next = { ...map }; if (e.target.value) next[key] = Number(e.target.value); else delete next[key]; setMap(next); setPrepared(null); }}>
                  <option value="">（不使用）</option>
                  {header.map((h, i) => <option key={i} value={i + 1}>{i + 1}. {h}</option>)}
                </select>
              </Field>
            ))}
          </div>
          <fieldset className="rules">
            <legend>类型规则（摘要或整行包含以下文字时）</legend>
            {rules.map((r, i) => (
              <div className="inline-form" key={i}>
                <input aria-label="包含文字" value={r.contains} onChange={(e) => { const next = [...rules]; next[i] = { ...r, contains: e.target.value }; setRules(next); setPrepared(null); }} placeholder="例如：房租" />
                <select aria-label="记为" value={r.kind} onChange={(e) => { const next = [...rules]; next[i] = { ...r, kind: e.target.value }; setRules(next); setPrepared(null); }}>{Object.entries(KINDS).map(([id, l]) => <option key={id} value={id}>{l}</option>)}</select>
                <button type="button" className="btn btn-xs btn-subtle" onClick={() => setRules(rules.filter((_, j) => j !== i))}>删除</button>
              </div>
            ))}
            <div><button type="button" className="btn btn-xs" onClick={() => setRules([...rules, { contains: '', kind: 'expense' }])}>添加规则</button></div>
          </fieldset>
          <div className="actions"><button className="btn" disabled={busy} onClick={preview}>预览</button></div>
        </>
      )}
      {prepared && (
        <>
          <p>共 {prepared.impact.rows} 行，其中 {prepared.impact.rows_with_errors} 行无法识别为交易（例如合计行、说明行），将被跳过。以下为前 20 行预览：</p>
          <div className="table-wrap table-scroll">
            <table className="table compact">
              <thead><tr><th>行</th><th>日期</th><th className="num">金额</th><th>类型</th><th>对方</th><th>摘要</th><th /></tr></thead>
              <tbody>
                {prepared.impact.preview.map((r) => (
                  <tr key={r.row} className={r.error ? 'muted' : undefined}>
                    <td>{r.row}</td><td>{r.date}</td>
                    <td className="num">{r.amount && <Amount value={r.amount} direction={(r.direction as 'in' | 'out') ?? undefined} kind={r.kind} />}</td>
                    <td>{KINDS[r.kind] ?? r.kind}</td><td>{r.counterparty}</td><td>{r.description}</td>
                    <td>{r.error && <Chip>跳过</Chip>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </Modal>
  );
}

type CalcResult = { calculation_type: string; result: Record<string, any>; assumptions_used: Record<string, string>; engine: string };
type CalcOutcome = { envelope: string; payload: CalcResult; status: string; warnings: string[]; key: string; inputs: number };

const CALC_TYPES: Record<string, string> = { sum: '收支汇总', budget: '月度预算与异常', loan: '借款对账', annualized: '年化利率（XIRR）' };

export function CalcTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { refresh } = useSession();
  const confirmed = s.cashflows.filter((c) => c.status === 'confirmed');
  const [type, setType] = useState('sum');
  const [chosen, setChosen] = useState<Set<string>>(() => new Set(confirmed.map((c) => c.id)));
  const [period, setPeriod] = useState({ start: '', end: '' });
  const [excludeTransfer, setExcludeTransfer] = useState(true);
  const [refund, setRefund] = useState('separate');
  const [loanMode, setLoanMode] = useState('reconcile');
  const [perspective, setPerspective] = useState('auto');
  const [result, setResult] = useState<CalcOutcome | null>(null);
  const { busy, run } = useAction();
  const candidates = s.cashflows.filter((c) => c.status === 'candidate').length;
  const key = JSON.stringify([type, [...chosen].sort(), period, excludeTransfer, refund, loanMode, perspective]);

  async function calculate() {
    const values: Record<string, unknown> = {};
    if (period.start) values.period_start = period.start;
    if (period.end) values.period_end = period.end;
    if (!excludeTransfer) values.exclude_kinds = [];
    if (refund !== 'separate') values.refund_policy = refund;
    if (type === 'loan' && loanMode !== 'reconcile') values.loan_mode = loanMode;
    if (type === 'loan' && loanMode === 'reconcile' && perspective !== 'auto') values.loan_perspective = perspective;
    const outcome = await run(async () => {
      let assumptions: string[] = [];
      if (Object.keys(values).length) {
        const saved = await applyChange(matter, { type: 'set_assumptions', values });
        if (!saved) return null;
        assumptions = saved.evidence_refs;
      }
      const envelope = ensure(await api.run('finance-calculate', { calculation_type: type, confirmed_inputs: [...chosen], assumption_ids: assumptions }), ['completed', 'partial']);
      const payload = await api.result<CalcResult>(envelope.data_ref!);
      return { envelope: envelope.data_ref!, payload, status: envelope.status, warnings: envelope.warnings.map((w) => w.message), key, inputs: chosen.size };
    });
    if (outcome) {
      setResult(outcome);
      refresh();
    }
  }

  if (!confirmed.length) {
    return (
      <Empty icon="calculator" title="没有已确认的金额记录">
        {candidates ? `有 ${candidates} 条候选记录待确认，确认后才能计算。` : '先在“金额”页签中生成或添加记录并确认。'}
      </Empty>
    );
  }
  return (
    <section className="calc-layout">
      <form className="card calc-form" aria-label="计算参数" onSubmit={(e) => { e.preventDefault(); calculate(); }}>
        <Field label="计算类型">
          <select value={type} onChange={(e) => setType(e.target.value)}>
            {Object.entries(CALC_TYPES).map(([id, label]) => <option key={id} value={id}>{label}</option>)}
          </select>
        </Field>
        <div className="grid-2">
          <Field label="开始（可选）"><input type="date" value={period.start} onChange={(e) => setPeriod({ ...period, start: e.target.value })} /></Field>
          <Field label="结束（可选）"><input type="date" value={period.end} onChange={(e) => setPeriod({ ...period, end: e.target.value })} /></Field>
        </div>
        {(type === 'sum' || type === 'budget') && (
          <>
            <label className="check"><input type="checkbox" checked={excludeTransfer} onChange={(e) => setExcludeTransfer(e.target.checked)} /> 不计入“本人转账”</label>
            <Field label="退款口径">
              <select value={refund} onChange={(e) => setRefund(e.target.value)}>
                <option value="separate">单独列示退款</option>
                <option value="net">{type === 'sum' ? '退款计入收支' : '退款冲减同类支出'}</option>
              </select>
            </Field>
          </>
        )}
        {type === 'loan' && (
          <>
            <Field label="对账方式">
              <select value={loanMode} onChange={(e) => setLoanMode(e.target.value)}><option value="reconcile">本金对账：本金、还款与利息费用分列</option><option value="compare">借款成本：收到与付出及年化</option></select>
            </Field>
            {loanMode === 'reconcile' && (
              <Field label="你在这笔借款中是" hint="自动判断依据记录类型：有“借出”是出借人，有“收到本金”是借款人；两种都有时请选择。">
                <select value={perspective} onChange={(e) => setPerspective(e.target.value)}>
                  <option value="auto">自动判断</option>
                  <option value="lender">出借人（钱借给别人）</option>
                  <option value="borrower">借款人（向别人借钱）</option>
                </select>
              </Field>
            )}
          </>
        )}
        <details>
          <summary>参与计算的记录（{chosen.size}/{confirmed.length}）</summary>
          <ul className="plain pick-list">
            {confirmed.map((c) => (
              <li key={c.id}>
                <label className="check">
                  <input type="checkbox" checked={chosen.has(c.id)} onChange={() => setChosen((set) => { const n = new Set(set); if (n.has(c.id)) n.delete(c.id); else n.add(c.id); return n; })} />
                  <span className="num">{c.date}</span> <Amount value={c.amount} direction={c.direction} kind={c.kind} /> <span className="ellipsis">{KINDS[c.kind] ?? c.kind} {c.counterparty}</span>
                </label>
              </li>
            ))}
          </ul>
        </details>
        {candidates > 0 && <p className="t-caption text-tertiary">另有 {candidates} 条候选记录未确认，不参与计算。</p>}
        <button className="btn btn-primary" disabled={busy || !chosen.size}>
          <Icon name="calculator" />
          {busy ? '正在计算…' : '计算'}
        </button>
        <p className="t-caption text-tertiary">由本机 Decimal 精确算术完成；口径写入结果。保存非默认口径时会请你确认。</p>
      </form>
      <div>
        {result ? (
          <CalcView result={result} name={s.matter.title} stale={result.key !== key} onRecalculate={calculate} busy={busy} />
        ) : (
          <Empty icon="calculator" title="计算结果">选择类型与口径后点击“计算”。</Empty>
        )}
      </div>
    </section>
  );
}

function Figure({ label, value, unit = '元' }: { label: string; value: React.ReactNode; unit?: string }) {
  return (
    <div className="figure">
      <dt>{label}</dt>
      <dd>
        {value}
        {unit && <small>{unit}</small>}
      </dd>
    </div>
  );
}

function CalcView({ result, name, stale, onRecalculate, busy }: { result: CalcOutcome; name: string; stale: boolean; onRecalculate: () => void; busy: boolean }) {
  const r = result.payload.result;
  const currency = useMemo(() => Object.keys(r.by_currency ?? {})[0] ?? r.currency ?? 'CNY', [r]);
  const unit = currency === 'CNY' ? '元' : currency;
  const c = r.by_currency?.[currency];
  return (
    <section aria-labelledby="calc-result">
      <div className="section-head">
        <h2 id="calc-result">{CALC_TYPES[result.payload.calculation_type] ?? '计算结果'}</h2>
        <div className="actions">
          {stale && (
            <button className="btn btn-sm" onClick={onRecalculate} disabled={busy}>
              <Icon name="refresh" />
              参数已改，重新计算
            </button>
          )}
          <ExportButton resultRefs={async () => [result.envelope]} name={`${name}-计算`} label="导出结果" />
        </div>
      </div>
      <p className="t-caption text-tertiary" style={{ marginBottom: 10 }}>
        {result.inputs} 条已确认记录 · 舍入 {result.payload.assumptions_used.rounding}，计息 {result.payload.assumptions_used.day_count} · 引擎 {result.payload.engine}
        {stale && <> · <span className="chip warning">已过期</span></>}
      </p>
      {result.status === 'partial' && <p className="warn-line">{result.warnings.join('；')}</p>}
      {result.payload.calculation_type === 'sum' && c && (
        <>
          <dl className="figures">
            <Figure label="收入" value={money(c.income)} unit={unit} />
            <Figure label="支出" value={money(c.expense)} unit={unit} />
            <Figure label="退款" value={money(c.refund)} unit={unit} />
            <Figure label="净额" value={money(c.net)} unit={unit} />
          </dl>
          <div className="table-wrap" style={{ marginTop: 16 }}>
            <table className="table">
              <thead><tr><th>分类</th><th className="num">金额（{unit}）</th></tr></thead>
              <tbody>{Object.entries(c.by_category as Record<string, string>).map(([k, v]) => <tr key={k}><td>{k}</td><td className="num"><Amount value={v} /></td></tr>)}</tbody>
            </table>
          </div>
        </>
      )}
      {result.payload.calculation_type === 'budget' && c && (
        <>
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>月份</th><th className="num">收入（{unit}）</th><th className="num">支出（{unit}）</th>
                  {r.trace?.refund_policy !== 'net' && <th className="num">退款（{unit}）</th>}
                  <th className="num">支出环比</th>
                </tr>
              </thead>
              <tbody>
                {(c.months as any[]).map((m) => (
                  <tr key={m.month}>
                    <td className="num">{m.month}</td><td className="num"><Amount value={m.income} /></td><td className="num"><Amount value={m.expense} /></td>
                    {r.trace?.refund_policy !== 'net' && <td className="num"><Amount value={m.refund ?? '0'} /></td>}
                    <td className="num">{m.expense_change_ratio ? `${(Number(m.expense_change_ratio) * 100).toFixed(1)}%` : '—'}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <p className="t-caption text-tertiary" style={{ marginTop: 6 }}>
            {r.trace?.refund_policy === 'net' ? '退款已冲减同类别的支出。' : '退款单独列示，不计入支出与环比；收到退款为正、退还别人为负。'}
          </p>
          <div className="two-col" style={{ marginTop: 16 }}>
            <div>
              <h3 className="t-subtitle">支出异常（环比变化 ≥ {(Number(r.trace?.anomaly_threshold ?? 0.3) * 100).toFixed(0)}%）</h3>
              {(c.anomalies as any[]).length ? <ul className="plain">{(c.anomalies as any[]).map((a) => <li key={a.month}>{a.month}：支出 {money(a.expense)}（上月 {money(a.previous_expense)}，变化 {(Number(a.expense_change_ratio) * 100).toFixed(1)}%）</li>)}</ul> : <p className="text-secondary">无</p>}
            </div>
            <div>
              <h3 className="t-subtitle">固定支出</h3>
              {(c.recurring as any[]).length ? <ul className="plain">{(c.recurring as any[]).map((x) => <li key={x.counterparty}>{x.counterparty}：约 {money(x.typical_amount)}/月（{x.months.length} 个月）</li>)}</ul> : <p className="text-secondary">无</p>}
            </div>
          </div>
        </>
      )}
      {result.payload.calculation_type === 'loan' &&
        (r.mode === 'reconcile' ? (
          <>
            <p className="t-caption text-secondary" style={{ marginBottom: 8 }}>
              {r.perspective === 'borrower' ? '你是借款人（向别人借钱）' : '你是出借人（钱借给别人）'}
              {result.payload.assumptions_used.loan_perspective && result.payload.assumptions_used.loan_perspective !== 'auto' ? '，按你的选择' : '，按记录类型自动判断'}
            </p>
            <dl className="figures">
              {loanFigures(r).map(([label, value]) => <Figure key={label} label={label} value={money(value)} unit={unit} />)}
              <Figure label="本金状态" value={({ outstanding: '未结清', settled: '已结清', overpaid: '多还' } as Record<string, string>)[r.status]} unit="" />
            </dl>
            <div className="table-wrap" style={{ marginTop: 16 }}>
              <table className="table">
                <thead><tr><th>日期</th><th>类型</th><th>计入</th><th className="num">金额（{unit}）</th><th className="num">本金余额（{unit}）</th></tr></thead>
                <tbody>
                  {(r.rows as any[]).map((x) => (
                    <tr key={x.id}>
                      <td className="num">{x.date}</td><td>{KINDS[x.kind] ?? x.kind}</td><td>{LOAN_PART[x.part] ?? x.part}</td>
                      <td className="num"><Amount value={x.amount} direction={x.direction} /></td><td className="num"><Amount value={x.principal_balance_after} /></td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            {r.note && <p className="t-caption text-tertiary" style={{ marginTop: 8 }}>{r.note}</p>}
          </>
        ) : (
          <dl className="figures">
            <Figure label="收到" value={money(r.total_received)} />
            <Figure label="付出" value={money(r.total_paid)} />
            <Figure label="资金成本" value={money(r.finance_cost)} />
            <Figure label="年化（XIRR）" value={<Irr irr={r.irr} />} unit="" />
          </dl>
        ))}
      {result.payload.calculation_type === 'annualized' && (
        <>
          <dl className="figures"><Figure label="年化利率（XIRR，按实际日期计算的年化收益率）" value={<Irr irr={r.irr} />} unit="" /></dl>
          <p className="t-caption text-tertiary" style={{ marginTop: 8 }}>{r.method}</p>
        </>
      )}
    </section>
  );
}

const LOAN_PART: Record<string, string> = { principal: '本金', repayment: '还本', interest: '利息', fee: '费用' };

/** Principal, repayments, interest and fees named from the user's side. */
export function loanFigures(r: Record<string, any>): [string, string][] {
  const lender = r.perspective !== 'borrower';
  const rows: [string, string | undefined][] = [
    [lender ? '借出本金' : '借入本金', r.principal],
    [lender ? '已收回本金' : '已还本金', r.principal_repaid],
    [lender ? '对方尚欠本金' : '你尚欠本金', r.outstanding],
    [lender ? '收到利息' : '已付利息', r.interest],
    ['费用', r.fees],
  ];
  return rows.filter((row): row is [string, string] => typeof row[1] === 'string');
}

function Irr({ irr }: { irr: { status: string; reason?: string; annual_rate?: string; roots?: string[] } }) {
  if (irr.status === 'unique' && irr.annual_rate) return <span className="amount">{(Number(irr.annual_rate) * 100).toFixed(6)}%</span>;
  if (irr.status === 'multiple_roots') return <span className="t-reading">已找到多个解（{irr.roots?.map((x) => `${(Number(x) * 100).toFixed(2)}%`).join('、')}），可能还有其他解，不输出单一数字</span>;
  if (irr.reason === 'saved_uniqueness_not_proven') return <span className="t-reading">此历史记录尚未确认唯一解，请重新计算</span>;
  if (irr.status === 'indeterminate') return <span className="t-reading">{irr.reason === 'all_cashflows_cancel' ? '同日现金流全部抵消，无法确定年化率' : irr.reason === 'no_root_found_in_searched_range' ? '在本次搜索范围内未找到可靠解，不代表无解' : '现金流方向多次变化，无法确认唯一解，不输出单一年化率'}</span>;
  return <span className="t-reading">无法确定（现金流不完整或无解）</span>;
}
