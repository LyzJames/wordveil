"""Deterministic financial calculators. Money is Decimal end to end.

Inputs are confirmed records and explicit assumptions supplied by the trusted
core. Currencies are never mixed; rounding happens once, at output, with the
declared rule. An irregular-cash-flow annual rate is reported only when the
root is unique and verified; multiple, missing or unstable roots are explained
instead of producing a falsely precise figure (design 11.2).
"""
from __future__ import annotations

from collections import defaultdict
from datetime import date
from decimal import ROUND_HALF_EVEN, ROUND_HALF_UP, Decimal, InvalidOperation, localcontext
import re

ENGINE_VERSION = 'lvjin-finance/1'
_ROUNDING = {'half_up': ROUND_HALF_UP, 'half_even': ROUND_HALF_EVEN}
_BASIS = {'act/365': 365, 'act/365.25': 365.25, 'act/360': 360}
_DEFAULT_PRECISION = {'CNY': 2, 'USD': 2, 'EUR': 2, 'HKD': 2, 'JPY': 0}


class InputError(ValueError):
    def __init__(self, code: str, detail: str = ''):
        super().__init__(code)
        self.code = code
        self.detail = detail


def _decimal(value, field: str) -> Decimal:
    if isinstance(value, float):
        raise InputError('binary_float_rejected', field)
    try:
        number = Decimal(str(value))
    except (InvalidOperation, TypeError) as error:
        raise InputError('invalid_decimal', field) from error
    if not number.is_finite():
        raise InputError('invalid_decimal', field)
    return number


def _date(value, field: str) -> date:
    try:
        return date.fromisoformat(str(value))
    except ValueError as error:
        raise InputError('invalid_date', field) from error


class Context:
    def __init__(self, assumptions: dict):
        self.assumptions = assumptions or {}
        self.rounding = _ROUNDING.get(self.assumptions.get('rounding', 'half_up'))
        if self.rounding is None:
            raise InputError('invalid_rounding')
        basis = self.assumptions.get('day_count', 'act/365')
        if basis not in _BASIS:
            raise InputError('invalid_day_count')
        self.basis_name = basis
        self.basis = _BASIS[basis]
        self.precision = dict(_DEFAULT_PRECISION)
        self.precision.update({k: int(v) for k, v in (self.assumptions.get('currency_precision') or {}).items()})
        self.used = {'rounding': self.assumptions.get('rounding', 'half_up'), 'day_count': basis}

    def money(self, value: Decimal, currency: str) -> str:
        places = self.precision.get(currency, 2)
        return format(value.quantize(Decimal(1).scaleb(-places), rounding=self.rounding), 'f')


def _records(raw: list, require_dates: bool = True) -> list[dict]:
    out = []
    for item in raw:
        direction = item.get('direction')
        if direction not in ('in', 'out'):
            raise InputError('invalid_direction', item.get('id', ''))
        currency = item.get('currency') or ''
        if not re.fullmatch(r'[A-Z]{3}', currency):
            raise InputError('invalid_currency', item.get('id', ''))
        amount = _decimal(item.get('amount'), 'amount')
        if amount < 0:
            raise InputError('negative_amount_use_direction', item.get('id', ''))
        record = {**item, 'amount': amount, 'currency': currency}
        if require_dates or item.get('date'):
            record['date'] = _date(item.get('date'), 'date')
        out.append(record)
    return out


def _in_period(record: dict, assumptions: dict) -> bool:
    start, end = assumptions.get('period_start'), assumptions.get('period_end')
    if start and record['date'] < _date(start, 'period_start'):
        return False
    if end and record['date'] > _date(end, 'period_end'):
        return False
    return True


def _refund_policy(ctx: Context) -> str:
    policy = ctx.assumptions.get('refund_policy', 'separate')
    if policy not in ('separate', 'net'):
        raise InputError('invalid_refund_policy')
    ctx.used['refund_policy'] = policy
    return policy


def calc_sum(records: list[dict], ctx: Context) -> dict:
    exclude = set(ctx.assumptions.get('exclude_kinds', ['transfer']))
    refunds = _refund_policy(ctx)
    totals = defaultdict(lambda: {'in': Decimal(0), 'out': Decimal(0), 'refund': Decimal(0)})
    categories = defaultdict(lambda: defaultdict(Decimal))
    included, excluded = [], []
    for record in records:
        if not _in_period(record, ctx.assumptions):
            excluded.append({'id': record.get('id'), 'reason': 'outside_period'})
            continue
        kind = record.get('kind', '')
        if kind in exclude:
            excluded.append({'id': record.get('id'), 'reason': 'excluded_kind:' + kind})
            continue
        bucket = totals[record['currency']]
        if kind == 'refund' and refunds == 'separate':
            # A refund received adds back; a refund the user paid out takes away.
            bucket['refund'] += record['amount'] if record['direction'] == 'in' else -record['amount']
        else:
            bucket[record['direction']] += record['amount']
        sign = Decimal(1) if record['direction'] == 'in' else Decimal(-1)
        categories[record['currency']][record.get('category') or '未分类'] += sign * record['amount']
        included.append(record.get('id'))
    result = {}
    for currency, bucket in sorted(totals.items()):
        net = bucket['in'] - bucket['out'] + (bucket['refund'] if refunds == 'separate' else 0)
        result[currency] = {'income': ctx.money(bucket['in'], currency), 'expense': ctx.money(bucket['out'], currency),
                            'refund': ctx.money(bucket['refund'], currency), 'net': ctx.money(net, currency),
                            'by_category': {name: ctx.money(value, currency) for name, value in sorted(categories[currency].items())}}
    return {'by_currency': result, 'trace': {'included_ids': included, 'excluded': excluded,
            'refund_policy': refunds, 'excluded_kinds': sorted(exclude)}}


def _month(day: date) -> str:
    return f'{day.year:04d}-{day.month:02d}'


def calc_budget(records: list[dict], ctx: Context) -> dict:
    exclude = set(ctx.assumptions.get('exclude_kinds', ['transfer']))
    refunds = _refund_policy(ctx)
    threshold = _decimal(ctx.assumptions.get('anomaly_threshold', '0.30'), 'anomaly_threshold')
    months = defaultdict(lambda: defaultdict(lambda: defaultdict(Decimal)))
    refunded = defaultdict(lambda: defaultdict(Decimal))
    payees = defaultdict(list)
    included, excluded = [], []
    for record in records:
        if not _in_period(record, ctx.assumptions) or record.get('kind', '') in exclude:
            excluded.append({'id': record.get('id'), 'reason': 'outside_period' if not _in_period(record, ctx.assumptions) else 'excluded_kind:' + record.get('kind', '')})
            continue
        month = _month(record['date'])
        category = record.get('category') or '未分类'
        signed = record['amount'] if record['direction'] == 'in' else -record['amount']
        if record.get('kind', '') == 'refund':
            # Same rule as the totals: refunds are listed apart by default, or
            # ('net') reduce the spending of their category.
            if refunds == 'separate':
                refunded[record['currency']][month] += signed
            else:
                months[record['currency']][month][('out', category)] -= signed
            included.append(record.get('id'))
            continue
        months[record['currency']][month][(record['direction'], category)] += record['amount']
        if record['direction'] == 'out' and record.get('counterparty'):
            payees[(record['currency'], record['counterparty'].strip())].append(record)
        included.append(record.get('id'))
    output = {}
    for currency in sorted(set(months) | set(refunded)):
        by_month = months[currency]
        ordered = sorted(set(by_month) | set(refunded[currency]))
        table, anomalies = [], []
        previous = None
        for month in ordered:
            cells = by_month[month]
            expense = sum((v for (d, _), v in cells.items() if d == 'out'), Decimal(0))
            income = sum((v for (d, _), v in cells.items() if d == 'in'), Decimal(0))
            row = {'month': month, 'income': ctx.money(income, currency), 'expense': ctx.money(expense, currency),
                   'refund': ctx.money(refunded[currency][month], currency),
                   'categories': {f'{d}:{c}': ctx.money(v, currency) for (d, c), v in sorted(cells.items())}}
            if previous is not None and previous > 0:
                change = (expense - previous) / previous
                row['expense_change_ratio'] = format(change.quantize(Decimal('0.0001'), rounding=ROUND_HALF_UP), 'f')
                if abs(change) >= threshold:
                    anomalies.append({'month': month, 'expense_change_ratio': row['expense_change_ratio'], 'previous_expense': ctx.money(previous, currency), 'expense': row['expense']})
            previous = expense
            table.append(row)
        recurring = []
        for (cur, payee), items in payees.items():
            if cur != currency:
                continue
            distinct = sorted({_month(r['date']) for r in items})
            amounts = [r['amount'] for r in items]
            if len(distinct) >= 3 and max(amounts) <= min(amounts) * Decimal('1.05'):
                recurring.append({'counterparty': payee, 'months': distinct, 'typical_amount': ctx.money(sorted(amounts)[len(amounts) // 2], currency),
                                  'record_ids': [r.get('id') for r in items]})
        output[currency] = {'months': table, 'anomalies': anomalies, 'recurring': sorted(recurring, key=lambda r: r['counterparty'])}
    return {'by_currency': output, 'trace': {'included_ids': included, 'excluded': excluded, 'anomaly_threshold': format(threshold, 'f'), 'refund_policy': refunds}}


# ------------------------------------------------------------------ IRR / XIRR

def _npv(rate: float, flows: list[tuple[float, float]]) -> float:
    return sum(amount / (1.0 + rate) ** years for years, amount in flows)


def _solve(low: float, high: float, flows, tolerance=1e-12, iterations=200) -> float | None:
    f_low, f_high = _npv(low, flows), _npv(high, flows)
    if f_low == 0:
        return low
    if f_high == 0:
        return high
    if (f_low > 0) == (f_high > 0):
        return None
    rate = (low + high) / 2
    for _ in range(iterations):
        value = _npv(rate, flows)
        if abs(value) < tolerance:
            return rate
        if (value > 0) == (f_low > 0):
            low, f_low = rate, value
        else:
            high, f_high = rate, value
        derivative = sum(-years * amount / (1.0 + rate) ** (years + 1) for years, amount in flows)
        candidate = rate - value / derivative if derivative else None
        # Safeguarded Newton: fall back to bisection outside the bracket.
        rate = candidate if candidate is not None and low < candidate < high else (low + high) / 2
        if high - low < 1e-15:
            return rate
    return None


def xirr(dated: list[tuple[date, Decimal]], basis: float) -> dict:
    merged = defaultdict(Decimal)
    for day, amount in dated:
        merged[day] += amount
    ordered = [(day, amount) for day, amount in sorted(merged.items()) if amount != 0]
    if not any(a > 0 for _, a in ordered) or not any(a < 0 for _, a in ordered):
        return {'status': 'no_solution', 'reason': 'needs_inflow_and_outflow', 'roots': []}
    signs = [1 if a > 0 else -1 for _, a in ordered]
    sign_changes = sum(1 for a, b in zip(signs, signs[1:]) if a != b)
    origin = ordered[0][0]
    flows = [((day - origin).days / basis, float(amount)) for day, amount in ordered]
    grid = [-0.9999, -0.999, -0.99, -0.95, -0.9, -0.8, -0.6, -0.4, -0.2, -0.1, -0.05, -0.02, 0.0, 0.01, 0.02, 0.05, 0.1, 0.15,
            0.2, 0.3, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0, 100.0]
    roots = []
    for low, high in zip(grid, grid[1:]):
        root = _solve(low, high, flows)
        if root is not None and not any(abs(root - r) < 1e-9 for r in roots):
            roots.append(root)
    verified = []
    with localcontext() as decimal_context:
        decimal_context.prec = 40
        scale = max(abs(a) for _, a in ordered)
        for root in roots:
            base = Decimal(1) + Decimal(repr(root))
            residual = sum((amount / (base ** (Decimal((day - origin).days) / Decimal(repr(basis)))) for day, amount in ordered), Decimal(0))
            if abs(residual) <= scale * Decimal('1e-8'):
                verified.append(root)
    payload = {'sign_changes': sign_changes, 'roots': [format(Decimal(repr(r)).quantize(Decimal('0.000001'), rounding=ROUND_HALF_UP), 'f') for r in verified]}
    if not verified:
        return {**payload, 'status': 'no_solution', 'reason': 'no_root_found_in_searched_range'}
    if len(verified) > 1:
        return {**payload, 'status': 'multiple_roots', 'reason': 'irregular_flows_have_several_rates'}
    return {**payload, 'status': 'unique', 'annual_rate': payload['roots'][0]}


def _signed(record: dict, perspective: str = 'self') -> Decimal:
    return record['amount'] if record['direction'] == 'in' else -record['amount']


def calc_annualized(records: list[dict], ctx: Context) -> dict:
    currencies = {r['currency'] for r in records}
    if len(currencies) != 1:
        raise InputError('single_currency_required')
    dated = [(r['date'], _signed(r)) for r in records]
    result = xirr(dated, ctx.basis)
    currency = currencies.pop()
    schedule = [{'id': r.get('id'), 'date': r['date'].isoformat(), 'signed_amount': ctx.money(_signed(r), currency)} for r in sorted(records, key=lambda r: (r['date'], r.get('id') or ''))]
    return {'currency': currency, 'irr': result, 'schedule': schedule,
            'method': f'NPV=0 with (date-first_date)/{ctx.basis_name.split("/")[1]} year fractions; bracket scan + safeguarded Newton; Decimal residual check'}


def calc_loan(records: list[dict], ctx: Context) -> dict:
    mode = ctx.assumptions.get('loan_mode', 'reconcile')
    currencies = {r['currency'] for r in records}
    if len(currencies) != 1:
        raise InputError('single_currency_required')
    currency = next(iter(currencies))
    ordered = sorted(records, key=lambda r: (r['date'], r.get('id') or ''))
    if mode == 'compare':
        received = sum((r['amount'] for r in ordered if r['direction'] == 'in'), Decimal(0))
        paid = sum((r['amount'] for r in ordered if r['direction'] == 'out'), Decimal(0))
        irr = xirr([(r['date'], _signed(r)) for r in ordered], ctx.basis)
        return {'mode': 'compare', 'currency': currency, 'total_received': ctx.money(received, currency), 'total_paid': ctx.money(paid, currency),
                'finance_cost': ctx.money(paid - received, currency), 'irr': irr,
                'cashflows': [{'id': r.get('id'), 'date': r['date'].isoformat(), 'direction': r['direction'], 'amount': ctx.money(r['amount'], currency), 'kind': r.get('kind')} for r in ordered]}
    if mode != 'reconcile':
        raise InputError('invalid_loan_mode')
    requested = ctx.assumptions.get('loan_perspective', 'auto')
    perspective = _loan_perspective(ordered, requested)
    ctx.used['loan_perspective'] = requested
    # The user's side of the loan: the lender lends out and is repaid; the
    # borrower receives principal and repays. Interest and fees are kept apart
    # from principal by the record type; payments of type "repayment" or
    # "payment" count as principal (no statutory allocation, see note).
    lending = 'out' if perspective == 'lender' else 'in'
    principal = repaid = interest = fees = Decimal(0)
    balance = Decimal(0)
    rows = []
    for record in ordered:
        kind = record.get('kind', '')
        if kind == 'interest':
            part = 'interest'
            interest += record['amount']
        elif kind == 'fee':
            part = 'fee'
            fees += record['amount']
        elif record['direction'] == lending:
            part = 'principal'
            principal += record['amount']
            balance += record['amount']
        else:
            part = 'repayment'
            repaid += record['amount']
            balance -= record['amount']
        rows.append({'id': record.get('id'), 'date': record['date'].isoformat(), 'direction': record['direction'], 'kind': kind, 'part': part,
                     'amount': ctx.money(record['amount'], currency), 'principal_balance_after': ctx.money(balance, currency)})
    status = 'outstanding' if balance > 0 else ('settled' if balance == 0 else 'overpaid')
    return {'mode': 'reconcile', 'currency': currency, 'perspective': perspective,
            'principal': ctx.money(principal, currency), 'principal_repaid': ctx.money(repaid, currency), 'outstanding': ctx.money(balance, currency),
            'interest': ctx.money(interest, currency), 'fees': ctx.money(fees, currency), 'status': status, 'rows': rows,
            'note': '按记录类型区分本金、利息与费用；类型为“还款/付款”的金额全部按本金冲减。未按约定利率计算应计利息，'
                    '也未按《民法典》第五百六十一条（实现债权的费用→利息→主债务）自动抵充；有约定时以约定为准。'}


def _loan_perspective(records: list[dict], requested: str) -> str:
    """The user's side of the loan, as requested or from the record types:
    money lent out ("loan_out") makes the user the lender, principal received
    ("principal_received") the borrower; both at once cannot be reconciled."""
    if requested in ('lender', 'borrower'):
        return requested
    if requested != 'auto':
        raise InputError('invalid_loan_perspective')
    kinds = {(r.get('kind', ''), r['direction']) for r in records}
    lender = ('loan_out', 'out') in kinds
    borrower = ('principal_received', 'in') in kinds
    if lender and borrower:
        raise InputError('loan_perspective_ambiguous')
    if borrower:
        return 'borrower'
    if lender:
        return 'lender'
    raise InputError('loan_perspective_unknown')


def _add_months(year: int, month: int, delta: int) -> tuple[int, int]:
    total = year * 12 + (month - 1) + delta
    return total // 12, total % 12 + 1


def calc_cashflow(ctx: Context) -> dict:
    scenario = ctx.assumptions.get('scenario') or {}
    currency = scenario.get('currency', 'CNY')
    try:
        start_year, start_month = (int(part) for part in str(scenario.get('start')).split('-')[:2])
    except ValueError as error:
        raise InputError('invalid_scenario_start') from error
    months = int(scenario.get('months', 12))
    if not 1 <= months <= 120:
        raise InputError('invalid_scenario_months')
    initial = _decimal(ctx.assumptions.get('initial_balance', '0'), 'initial_balance')
    items = []
    for item in scenario.get('items', []):
        if item.get('direction') not in ('in', 'out'):
            raise InputError('invalid_direction', item.get('label', ''))
        every = int(item.get('every_months', 1))
        if every < 1:
            raise InputError('invalid_frequency', item.get('label', ''))
        items.append({**item, 'amount': _decimal(item.get('amount'), 'amount'), 'every': every, 'first': int(item.get('first_month_index', 0))})

    def project(expense_factor: Decimal, income_factor: Decimal):
        balance, rows, first_gap, minimum = initial, [], None, initial
        for index in range(months):
            year, month = _add_months(start_year, start_month, index)
            income = expense = Decimal(0)
            for item in items:
                if index >= item['first'] and (index - item['first']) % item['every'] == 0:
                    if item['direction'] == 'in':
                        income += item['amount'] * income_factor
                    else:
                        expense += item['amount'] * expense_factor
            balance = balance + income - expense
            minimum = min(minimum, balance)
            if balance < 0 and first_gap is None:
                first_gap = f'{year:04d}-{month:02d}'
            rows.append({'month': f'{year:04d}-{month:02d}', 'income': ctx.money(income, currency), 'expense': ctx.money(expense, currency), 'balance': ctx.money(balance, currency)})
        return {'rows': rows, 'first_shortfall_month': first_gap, 'minimum_balance': ctx.money(minimum, currency)}

    base = project(Decimal(1), Decimal(1))
    stressed = project(Decimal('1.10'), Decimal('0.90'))
    return {'currency': currency, 'initial_balance': ctx.money(initial, currency), 'base': base,
            'sensitivity': {'expense_plus_10_income_minus_10': {'first_shortfall_month': stressed['first_shortfall_month'], 'minimum_balance': stressed['minimum_balance']}}}


def calculate(request: dict) -> dict:
    kind = request.get('calculation_type')
    ctx = Context(request.get('assumptions') or {})
    if kind == 'cashflow':
        result = calc_cashflow(ctx)
    else:
        records = _records(request.get('records') or [])
        if not records:
            raise InputError('no_confirmed_inputs')
        if kind == 'sum':
            result = calc_sum(records, ctx)
        elif kind == 'budget':
            result = calc_budget(records, ctx)
        elif kind == 'annualized':
            result = calc_annualized(records, ctx)
        elif kind == 'loan':
            result = calc_loan(records, ctx)
        else:
            raise InputError('unregistered_calculation')
    return {'engine': ENGINE_VERSION, 'calculation_type': kind, 'assumptions_used': ctx.used, 'result': result}
