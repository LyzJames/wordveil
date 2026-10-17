// Cash flows, explicit column mapping for bank/payment tables, and the
// deterministic Decimal calculators (only confirmed inputs are used).
import React, { useMemo, useState } from 'react';
import { api, applyChange, commitChange, ensure, KINDS, prepareChange, type Cashflow, type Snapshot, type VersionPages } from './api';
import { ExportButton } from './export';
import { useSession } from './main';
import { Amount, Chip, Empty, ErrorLine, Field, Modal, useAction } from './ui';

const KIND_RULE_DEFAULTS = [
  { contains: '本人账户转账', kind: 'transfer' },
  { contains: '退款', kind: 'refund' },
];

export function CashflowTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { refresh } = useSession();
  const { busy, run } = useAction();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [mapping, setMapping] = useState(false);
  const [adding, setAdding] = useState(false);
  const candidates = s.cashflows.filter((c) => c.status === 'candidate');
  async function batch(type: 'confirm_cashflows' | 'reject_cashflows', ids: string[]) {
    const done = await run(() => applyChange(matter, { type, cashflow_ids: ids }), type === 'confirm_cashflows' ? '已确认' : '已排除');
    if (done) { setSelected(new Set()); refresh(); }
  }
  const toggle = (id: string) => setSelected((set) => { const next = new Set(set); next.has(id) ? next.delete(id) : next.add(id); return next; });
  return (
    <section>
      <div className="row-between toolbar">
        <p className="muted small">金额以字符串精确保存，计算只使用<strong>已确认</strong>的记录；“本人转账”默认不计入收支。</p>
        <div className="actions">
          <button className="primary" onClick={() => setMapping(true)}>从表格生成</button>
          <button onClick={() => setAdding(true)}>手动添加</button>
        </div>
      </div>
      {candidates.length > 0 && (
        <div className="actions">
          <button className="primary" disabled={busy || !selected.size} onClick={() => batch('confirm_cashflows', [...selected])}>确认所选（{selected.size}）</button>
          <button disabled={busy} onClick={() => batch('confirm_cashflows', candidates.map((c) => c.id))}>全部确认（{candidates.length}）</button>
          <button disabled={busy || !selected.size} onClick={() => batch('reject_cashflows', [...selected])}>排除所选</button>
        </div>
      )}
      {s.cashflows.length === 0 ? (
        <Empty title="还没有金额记录">可从导入的银行流水（CSV）或支付账单（XLSX）按列生成候选，也可手动添加。</Empty>
      ) : (
        <table className="table">
          <thead><tr><th className="check-cell" /><th>日期</th><th className="num">金额</th><th>类型</th><th>对方</th><th>摘要</th><th>状态</th></tr></thead>
          <tbody>
            {s.cashflows.map((c: Cashflow) => (
              <tr key={c.id} className={c.status === 'rejected' ? 'dim' : ''}>
                <td className="check-cell">{c.status === 'candidate' && <input type="checkbox" aria-label="选择" checked={selected.has(c.id)} onChange={() => toggle(c.id)} />}</td>
                <td>{c.date}</td>
                <td className="num"><Amount value={c.amount} direction={c.direction} currency={c.currency} /></td>
                <td>{KINDS[c.kind] ?? c.kind}</td>
                <td>{c.counterparty}</td>
                <td className="name-cell">{c.description}</td>
                <td><Chip tone={c.status === 'confirmed' ? 'ok' : c.status === 'candidate' ? 'warn' : 'neutral'}>{c.status === 'confirmed' ? '已确认' : c.status === 'candidate' ? '待确认' : '已排除'}</Chip></td>
              </tr>
            ))}
          </tbody>
        </table>
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
    <Modal title="手动添加金额记录" onClose={onClose}>
      <form className="stack" onSubmit={submit}>
        <div className="grid-2">
          <Field label="日期"><input type="date" value={form.date} onChange={set('date')} required /></Field>
          <Field label="金额（元）"><input value={form.amount} onChange={set('amount')} inputMode="decimal" pattern="\d{1,15}(\.\d{1,6})?" required /></Field>
          <Field label="方向"><select value={form.direction} onChange={set('direction')}><option value="out">支出</option><option value="in">收入</option></select></Field>
          <Field label="类型"><select value={form.kind} onChange={set('kind')}>{Object.entries(KINDS).map(([id, l]) => <option key={id} value={id}>{l}</option>)}</select></Field>
          <Field label="对方（可选）"><input value={form.counterparty} onChange={set('counterparty')} maxLength={80} /></Field>
          <Field label="摘要（可选）"><input value={form.description} onChange={set('description')} maxLength={200} /></Field>
        </div>
        <p className="muted small">手动添加的记录直接记为“已确认”。</p>
        <div className="actions end"><button type="button" onClick={onClose}>取消</button><button className="primary" disabled={busy}>添加</button></div>
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
    <Modal title="从表格生成金额记录" onClose={onClose} wide>
      <div className="stack">
        <Field label="选择已导入的表格资料">
          <select value={version} onChange={(e) => pick(e.target.value)}>
            <option value="">选择…</option>
            {tables.map((d) => <option key={d.version_id} value={d.version_id}>{d.display_name}（第 {d.version_no} 版）</option>)}
          </select>
        </Field>
        <ErrorLine text={error} />
        {header.length > 0 && (
          <>
            <div className="table-scroll">
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
                  <button type="button" className="small" onClick={() => setRules(rules.filter((_, j) => j !== i))}>删除</button>
                </div>
              ))}
              <button type="button" className="small" onClick={() => setRules([...rules, { contains: '', kind: 'expense' }])}>添加规则</button>
            </fieldset>
            <div className="actions"><button disabled={busy} onClick={preview}>预览</button></div>
          </>
        )}
        {prepared && (
          <>
            <p>共 {prepared.impact.rows} 行，其中 {prepared.impact.rows_with_errors} 行无法识别为交易（例如合计行、说明行），将被跳过。以下为前 20 行预览：</p>
            <div className="table-scroll">
              <table className="table compact">
                <thead><tr><th>行</th><th>日期</th><th className="num">金额</th><th>类型</th><th>对方</th><th>摘要</th><th /></tr></thead>
                <tbody>
                  {prepared.impact.preview.map((r) => (
                    <tr key={r.row} className={r.error ? 'dim' : ''}>
                      <td>{r.row}</td><td>{r.date}</td>
                      <td className="num">{r.amount && <Amount value={r.amount} direction={(r.direction as 'in' | 'out') ?? undefined} />}</td>
                      <td>{KINDS[r.kind] ?? r.kind}</td><td>{r.counterparty}</td><td>{r.description}</td>
                      <td>{r.error && <Chip tone="neutral">跳过</Chip>}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="actions end">
              <button onClick={onClose}>取消</button>
              <button className="primary" disabled={busy} onClick={async () => { const done = await run(() => commitChange(matter, prepared.ref), '已生成候选金额记录'); if (done) onDone(); }}>生成候选…</button>
            </div>
          </>
        )}
      </div>
    </Modal>
  );
}

type CalcResult = { calculation_type: string; result: Record<string, any>; assumptions_used: Record<string, string>; engine: string };

export function CalcTab({ matter, snapshot: s }: { matter: string; snapshot: Snapshot }) {
  const { refresh } = useSession();
  const confirmed = s.cashflows.filter((c) => c.status === 'confirmed');
  const [type, setType] = useState('sum');
  const [chosen, setChosen] = useState<Set<string>>(() => new Set(confirmed.map((c) => c.id)));
  const [period, setPeriod] = useState({ start: '', end: '' });
  const [excludeTransfer, setExcludeTransfer] = useState(true);
  const [refund, setRefund] = useState('separate');
  const [loanMode, setLoanMode] = useState('reconcile');
  const [result, setResult] = useState<{ envelope: string; payload: CalcResult; status: string; warnings: string[] } | null>(null);
  const { busy, run } = useAction();
  const candidates = s.cashflows.filter((c) => c.status === 'candidate').length;

  async function calculate() {
    setResult(null);
    const values: Record<string, unknown> = {};
    if (period.start) values.period_start = period.start;
    if (period.end) values.period_end = period.end;
    if (!excludeTransfer) values.exclude_kinds = [];
    if (refund !== 'separate') values.refund_policy = refund;
    if (type === 'loan' && loanMode !== 'reconcile') values.loan_mode = loanMode;
    const outcome = await run(async () => {
      let assumptions: string[] = [];
      if (Object.keys(values).length) {
        const saved = await applyChange(matter, { type: 'set_assumptions', values });
        if (!saved) return null;
        assumptions = saved.evidence_refs;
      }
      const envelope = ensure(await api.run('finance-calculate', { calculation_type: type, confirmed_inputs: [...chosen], assumption_ids: assumptions }), ['completed', 'partial']);
      const payload = await api.result<CalcResult>(envelope.data_ref!);
      return { envelope: envelope.data_ref!, payload, status: envelope.status, warnings: envelope.warnings.map((w) => w.message) };
    });
    if (outcome) { setResult(outcome); refresh(); }
  }

  if (!confirmed.length) {
    return <Empty title="没有已确认的金额记录">{candidates ? `有 ${candidates} 条候选记录待确认，确认后才能计算。` : '先在“金额记录”中生成或添加记录并确认。'}</Empty>;
  }
  return (
    <section className="split calc">
      <div className="stack">
        <Field label="计算类型">
          <select value={type} onChange={(e) => { setType(e.target.value); setResult(null); }}>
            <option value="sum">收支汇总</option>
            <option value="budget">月度预算与异常</option>
            <option value="loan">借款对账</option>
            <option value="annualized">年化利率（XIRR）</option>
          </select>
        </Field>
        <div className="grid-2">
          <Field label="期间开始（可选）"><input type="date" value={period.start} onChange={(e) => setPeriod({ ...period, start: e.target.value })} /></Field>
          <Field label="期间结束（可选）"><input type="date" value={period.end} onChange={(e) => setPeriod({ ...period, end: e.target.value })} /></Field>
        </div>
        {(type === 'sum' || type === 'budget') && (
          <>
            <label className="check"><input type="checkbox" checked={excludeTransfer} onChange={(e) => setExcludeTransfer(e.target.checked)} /> 不计入“本人转账”</label>
            {type === 'sum' && <Field label="退款口径"><select value={refund} onChange={(e) => setRefund(e.target.value)}><option value="separate">单独列示退款</option><option value="net">退款计入收入</option></select></Field>}
          </>
        )}
        {type === 'loan' && <Field label="对账方式"><select value={loanMode} onChange={(e) => setLoanMode(e.target.value)}><option value="reconcile">出借人视角：借出与还款余额</option><option value="compare">借款成本：收到与付出及年化</option></select></Field>}
        <details>
          <summary>参与计算的记录（{chosen.size}/{confirmed.length}）</summary>
          <ul className="plain pick-list">
            {confirmed.map((c) => (
              <li key={c.id}><label className="check"><input type="checkbox" checked={chosen.has(c.id)} onChange={() => setChosen((set) => { const n = new Set(set); n.has(c.id) ? n.delete(c.id) : n.add(c.id); return n; })} />{c.date} <Amount value={c.amount} direction={c.direction} /> {KINDS[c.kind] ?? c.kind} {c.counterparty}</label></li>
            ))}
          </ul>
        </details>
        {candidates > 0 && <p className="muted small">另有 {candidates} 条候选记录未确认，不参与计算。</p>}
        <button className="primary" disabled={busy || !chosen.size} onClick={calculate}>{busy ? '正在计算…' : '计算'}</button>
        <p className="muted small">计算由本机 Decimal 精确算术完成，口径会写入结果；保存非默认口径时会请你确认。</p>
      </div>
      <div>{result ? <CalcView result={result} name={s.matter.title} /> : <Empty title="计算结果">选择类型与口径后点击“计算”。</Empty>}</div>
    </section>
  );
}

function CalcView({ result, name }: { result: { envelope: string; payload: CalcResult; status: string; warnings: string[] }; name: string }) {
  const r = result.payload.result;
  const currency = useMemo(() => Object.keys(r.by_currency ?? {})[0] ?? r.currency ?? 'CNY', [r]);
  return (
    <div className="calc-result">
      <div className="row-between">
        <h3>计算结果</h3>
        <ExportButton resultRefs={async () => [result.envelope]} name={`${name}-计算`} label="导出结果" />
      </div>
      {result.status === 'partial' && <p className="warn-line">{result.warnings.join('；')}</p>}
      {result.payload.calculation_type === 'sum' && r.by_currency?.[currency] && (
        <>
          <dl className="figures">
            <div><dt>收入</dt><dd><Amount value={r.by_currency[currency].income} /></dd></div>
            <div><dt>支出</dt><dd><Amount value={r.by_currency[currency].expense} /></dd></div>
            <div><dt>退款</dt><dd><Amount value={r.by_currency[currency].refund} /></dd></div>
            <div><dt>净额</dt><dd><Amount value={r.by_currency[currency].net} /></dd></div>
          </dl>
          <table className="table compact"><thead><tr><th>分类</th><th className="num">金额</th></tr></thead><tbody>{Object.entries(r.by_currency[currency].by_category as Record<string, string>).map(([k, v]) => <tr key={k}><td>{k}</td><td className="num"><Amount value={v} /></td></tr>)}</tbody></table>
        </>
      )}
      {result.payload.calculation_type === 'budget' && r.by_currency?.[currency] && (
        <>
          <table className="table compact"><thead><tr><th>月份</th><th className="num">收入</th><th className="num">支出</th><th className="num">支出环比</th></tr></thead>
            <tbody>{(r.by_currency[currency].months as any[]).map((m) => <tr key={m.month}><td>{m.month}</td><td className="num"><Amount value={m.income} /></td><td className="num"><Amount value={m.expense} /></td><td className="num">{m.expense_change_ratio ? `${(Number(m.expense_change_ratio) * 100).toFixed(1)}%` : '—'}</td></tr>)}</tbody></table>
          <h4>支出异常（环比变化 ≥ {(Number(r.trace?.anomaly_threshold ?? 0.3) * 100).toFixed(0)}%）</h4>
          {(r.by_currency[currency].anomalies as any[]).length ? <ul className="plain">{(r.by_currency[currency].anomalies as any[]).map((a) => <li key={a.month}>{a.month}：支出 {a.expense}（上月 {a.previous_expense}，变化 {(Number(a.expense_change_ratio) * 100).toFixed(1)}%）</li>)}</ul> : <p className="muted">无</p>}
          <h4>固定支出</h4>
          {(r.by_currency[currency].recurring as any[]).length ? <ul className="plain">{(r.by_currency[currency].recurring as any[]).map((x) => <li key={x.counterparty}>{x.counterparty}：约 {x.typical_amount}/月（{x.months.length} 个月）</li>)}</ul> : <p className="muted">无</p>}
        </>
      )}
      {result.payload.calculation_type === 'loan' && (
        r.mode === 'reconcile' ? (
          <>
            <dl className="figures">
              <div><dt>借出合计</dt><dd><Amount value={r.total_lent} /></dd></div>
              <div><dt>已还合计</dt><dd><Amount value={r.total_repaid} /></dd></div>
              <div><dt>尚欠</dt><dd><Amount value={r.outstanding} /></dd></div>
              <div><dt>状态</dt><dd>{({ outstanding: '未结清', settled: '已结清', overpaid: '多还' } as Record<string, string>)[r.status]}</dd></div>
            </dl>
            <table className="table compact"><thead><tr><th>日期</th><th>类型</th><th className="num">金额</th><th className="num">余额</th></tr></thead><tbody>{(r.rows as any[]).map((x) => <tr key={x.id}><td>{x.date}</td><td>{KINDS[x.kind] ?? x.kind}</td><td className="num"><Amount value={x.amount} direction={x.direction} /></td><td className="num"><Amount value={x.balance_after} /></td></tr>)}</tbody></table>
          </>
        ) : (
          <dl className="figures">
            <div><dt>收到</dt><dd><Amount value={r.total_received} /></dd></div>
            <div><dt>付出</dt><dd><Amount value={r.total_paid} /></dd></div>
            <div><dt>资金成本</dt><dd><Amount value={r.finance_cost} /></dd></div>
            <div><dt>年化</dt><dd><Irr irr={r.irr} /></dd></div>
          </dl>
        )
      )}
      {result.payload.calculation_type === 'annualized' && (
        <>
          <dl className="figures"><div><dt>年化利率</dt><dd><Irr irr={r.irr} /></dd></div></dl>
          <p className="muted small">{r.method}</p>
        </>
      )}
      <p className="muted small">口径：舍入 {result.payload.assumptions_used.rounding}，计息 {result.payload.assumptions_used.day_count}；引擎 {result.payload.engine}</p>
    </div>
  );
}

function Irr({ irr }: { irr: { status: string; annual_rate?: string; roots?: string[] } }) {
  if (irr.status === 'unique' && irr.annual_rate) return <span className="amount">{(Number(irr.annual_rate) * 100).toFixed(4)}%</span>;
  if (irr.status === 'multiple_roots') return <span>存在多个解（{irr.roots?.map((x) => `${(Number(x) * 100).toFixed(2)}%`).join('、')}），不输出单一数字</span>;
  return <span>无法确定（现金流不完整或无解）</span>;
}
