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
import ast

ENGINE_VERSION = 'lvjin-finance/3'
_ROUNDING = {'half_up': ROUND_HALF_UP, 'half_even': ROUND_HALF_EVEN}
_BASIS = {'act/365': 365, 'act/365.25': 365.25, 'act/360': 360}
_DEFAULT_PRECISION = {'CNY': 2, 'USD': 2, 'EUR': 2, 'HKD': 2, 'JPY': 0}

# Grounding checks explicit arithmetic, never infers a transaction or a legal
# rule. Leaves must come from the claim's cited sources (or an earlier verified
# expression whose source dependencies are also cited). No Python is executed.
_CN_SCALAR = '零〇一二两三四五六七八九十百千万亿壹贰貳叁參肆伍陆陸柒捌玖拾佰仟萬億'
_SCALAR = rf'(?:\d{{1,3}}(?:[,，]\d{{3}})+(?:\.\d{{1,6}})?|\d+(?:\.\d{{1,6}})?)(?:万|亿)?|[{_CN_SCALAR}]+(?:点[{_CN_SCALAR}]+)?'
_SCALAR_RE = re.compile(rf'(?<![A-Za-z0-9.])(?:{_SCALAR})(?![\d.])')
_MATH_UNIT = r'(?:元(?:/月)?|个月|月|节|次|人|%|％)?'
# A bounded character window, rather than nested repetitions of number
# tokens: ordinary account numbers without an equation must stay linear.
_MATH_CHARS = frozenset(f'0123456789{_CN_SCALAR}点.,，元个月节次人%％()（）+＋-−－*＊×÷/／加上减去乘以除负分之')
_RELATION = re.compile(r'约等于|约为|等于|=|＝|≈')


def _equations(text: str) -> tuple[list[dict], bool]:
    """Read both complete sides of each link, including overlapping chains.

    A right-hand expression must never be truncated to its first number.
    Character windows and link count remain bounded independently of input.
    """
    out, incomplete = [], False
    def math_char(index):
        char = text[index]
        if char in ',，':
            # Keep numeric thousands separators, not clause separators.
            prefix = re.search(r'[\d.,，]+$', text[max(0, index - 256):index])
            digits = text[index + 1:index + 4]
            return bool(prefix and '.' not in prefix[0] and text[index - 1].isascii()
                        and text[index - 1].isdigit() and len(digits) == 3
                        and digits.isascii() and digits.isdigit()
                        and (index + 4 >= len(text) or not text[index + 4].isdigit()))
        return char in _MATH_CHARS or char in ' \t\u3000'
    for relation in _RELATION.finditer(text):
        start, right = relation.start(), relation.end()
        while start > 0 and relation.start() - start < 256 and math_char(start - 1):
            start -= 1
        while right < len(text) and text[right].isspace():
            right += 1
        approx = right < len(text) and text[right] == '约'
        right += int(approx)
        end = right
        while end < len(text) and end - right < 256 and math_char(end):
            end += 1
        left, result = text[start:relation.start()].strip(' \t\r\n,，'), text[right:end].strip(' \t\r\n,，')
        if not left or not result:
            continue
        if len(out) >= 8 or start > 0 and math_char(start - 1) or end < len(text) and math_char(end):
            incomplete = True
            if len(out) >= 8:
                break
            continue
        out.append(dict(expression=left, result=result, relation=relation.group(), approx=approx, start=start, end=end))
    return out, incomplete


def _scalar(raw: str) -> Decimal:
    from .extract import chinese_integer
    raw = raw.strip().replace(',', '').replace('，', '')
    negative = raw.startswith(('-', '−', '－', '负'))
    if negative:
        raw = raw[1:]
    percent = raw.endswith(('%', '％'))
    raw = re.sub(r'(?:元(?:/月)?|个月|月|节|次|人|%|％)$', '', raw).strip()
    if re.fullmatch(r'\d+(?:\.\d+)?[万亿]?', raw):
        factor = Decimal(10_000 if raw.endswith('万') else 100_000_000 if raw.endswith('亿') else 1)
        value = Decimal(raw.rstrip('万亿')) * factor
    else:
        whole, dot, fraction = raw.partition('点')
        value = Decimal(chinese_integer(whole))
        if dot:
            digits = ''.join(str(chinese_integer(c)) for c in fraction)
            value += Decimal('0.' + digits)
    if percent:
        value = value.scaleb(-2)
    if negative:
        value = -value
    if not value.is_finite() or abs(value) > Decimal('1e18'):
        raise ValueError('arithmetic_bound')
    return value


def _source_scalars(text: str) -> set[Decimal]:
    # Dates and reference labels cannot supply arithmetic operands.
    dates = [(m.start(), m.end()) for m in re.finditer(
        rf'(?:\d{{4}}|[{_CN_SCALAR}]{{4}})\s*(?:年|[-/.])\s*(?:\d{{1,2}}|[{_CN_SCALAR}]+)\s*(?:月|[-/.])\s*(?:\d{{1,2}}|[{_CN_SCALAR}]+)日?', text)]
    out = set()
    for match in _SCALAR_RE.finditer(text):
        if any(start <= match.start() < end for start, end in dates):
            continue
        if text[match.end():].lstrip().startswith(('年', '月', '日', '号')):
            continue
        if match.start() > 0 and text[match.start() - 1] == '第' and text[match.end():].lstrip().startswith(('条', '页', '项', '款', '章', '节', '秒')):
            continue
        suffix = text[match.end():].lstrip()
        if match.group() == '百' and suffix.startswith('分之'):
            continue
        try:
            sign = '-' if match.start() > 0 and text[match.start() - 1] in '-−－负' else ''
            percent = suffix.startswith(('%', '％')) or text[max(0, match.start() - 3):match.start()] == '百分之'
            value = _scalar(sign + match.group() + ('%' if percent else ''))
            out.add(value)
        except (ValueError, InvalidOperation):
            continue
    return out


def _arithmetic(expression: str) -> tuple[Decimal, list[frozenset[Decimal]], str]:
    expression = expression.strip().replace('负', '-')
    expression = re.sub(rf'(?<=[\d{_CN_SCALAR}])\s*[(（]\s*(元(?:/月)?|个月|月|节|次|人|%|％)\s*[)）]', r'\1', expression)
    expression = re.sub(rf'百分之([{_CN_SCALAR}]+(?:点[{_CN_SCALAR}]+)?)', lambda m: format(_scalar(m[1] + '%'), 'f'), expression)
    for word, operator in [('加上', '+'), ('减去', '-'), ('乘以', '*'), ('除以', '/'), ('加', '+'), ('减', '-'), ('乘', '*'), ('除', '/')]:
        expression = expression.replace(word, operator)
    expression = expression.translate(str.maketrans({'（': '(', '）': ')', '＋': '+', '＊': '*', '／': '/', '×': '*', '÷': '/', '−': '-', '－': '-'}))
    atom = re.compile(rf'(?:{_SCALAR})\s*{_MATH_UNIT}')
    expression = atom.sub(lambda m: format(_scalar(m.group()), 'f'), expression)
    if len(expression) > 256 or not re.fullmatch(r'[\d.\s()+*/-]+', expression):
        raise ValueError('arithmetic_syntax')
    tree = ast.parse(expression, mode='eval')
    if sum(isinstance(n, ast.BinOp) for n in ast.walk(tree)) not in range(13):
        raise ValueError('arithmetic_bound')
    leaves = []

    def visit(node, depth=0, subtrahend=False):
        if depth > 12:
            raise ValueError('arithmetic_bound')
        if isinstance(node, ast.Constant) and type(node.value) in (int, float):
            value = Decimal(ast.get_source_segment(expression, node))
            leaves.append(frozenset((value, -value)) if subtrahend else frozenset((value,)))
            return value
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, (ast.UAdd, ast.USub)):
            if isinstance(node.operand, ast.Constant):
                value = Decimal(ast.get_source_segment(expression, node.operand))
                if isinstance(node.op, ast.USub): value = -value
                leaves.append(frozenset((value, -value)) if subtrahend else frozenset((value,)))
                return value
            value = visit(node.operand, depth + 1)
            return -value if isinstance(node.op, ast.USub) else value
        if isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Sub, ast.Mult, ast.Div)):
            left, right = visit(node.left, depth + 1), visit(node.right, depth + 1, isinstance(node.op, ast.Sub))
            if isinstance(node.op, ast.Add): value = left + right
            elif isinstance(node.op, ast.Sub): value = left - right
            elif isinstance(node.op, ast.Mult): value = left * right
            elif right: value = left / right
            else: raise ValueError('division_by_zero')
            if not value.is_finite() or abs(value) > Decimal('1e18'):
                raise ValueError('arithmetic_bound')
            return value
        raise ValueError('arithmetic_syntax')

    with localcontext() as context:
        context.prec = 48
        return visit(tree.body), leaves, expression


def verify_arithmetic(claims: list[dict]) -> dict:
    """Source-bound, Decimal proofs for explicit + - * / equations only.

    Proofs support numeric derivation, not the factual meaning of the operands.
    Missing operands, unshown computations and unsupported syntax remain subject
    to the normal grounding checks; a wrong equation is never excused by NLI.
    """
    sources = {source['id']: _source_scalars(source['text']) for claim in claims for source in claim['sources']}
    parsed = {claim['id']: _equations(claim['text']) for claim in claims}
    equations = [(claim, match) for claim in claims for match in parsed[claim['id']][0]]
    proofs, accepted, wrong = [], set(), {}
    for _ in range(4):
        changed = False
        for claim, match in equations:
            identity = (claim['id'], match['start'], match['end'])
            if identity in accepted:
                continue
            try:
                value, leaves, expression = _arithmetic(match['expression'])
                claimed, right_leaves, _ = _arithmetic(match['result'])
                if len(leaves) == len(right_leaves) == 1:
                    def unit(raw):
                        raw = raw.strip().strip('()（）').strip()
                        if raw.endswith(('%', '％')) or raw.startswith('百分之'):
                            return 'percent'
                        found = re.search(r'(元(?:/月)?|个月|月|节|次|人)$', raw)
                        return found[0] if found else ''
                    left_unit, right_unit = unit(match['expression']), unit(match['result'])
                    if left_unit != right_unit and (left_unit not in ('', 'percent') or right_unit not in ('', 'percent')):
                        continue
                # A result atom is derived; operands of a displayed RHS
                # expression must themselves have cited/verified premises.
                if len(right_leaves) > 1:
                    leaves += right_leaves
            except (ValueError, SyntaxError, InvalidOperation, ArithmeticError) as error:
                if str(error) == 'division_by_zero':
                    wrong.setdefault(claim['id'], []).append(claim['text'][match['start']:match['end']].strip())
                continue
            allowed = {source['id'] for source in claim['sources']}
            dependencies, parents = set(), []
            for leaf in leaves:
                direct = next((ref for ref in sorted(allowed) if leaf & sources[ref]), None)
                previous = next((p for p in proofs if p['value'] in leaf and p['dependencies'] <= allowed), None)
                if direct is not None:
                    dependencies.add(direct)
                elif previous is not None:
                    dependencies |= previous['dependencies']
                    parents.append(previous)
                else:
                    break
            else:
                # Exact equality; rounded results require an explicit rounding
                # qualification and keep its precision in the recorded proof.
                prefix = re.split(r'[;；。\n]', claim['text'][:match['start']])[-1][-32:]
                suffix = re.split(r'[;；。\n]', claim['text'][match['end']:], maxsplit=1)[0][:48]
                rounded = match['relation'] in ('≈', '约为', '约等于') or bool(match['approx']) or any(word in prefix + suffix for word in ('四舍五入', '约为', '约等于'))
                scale = claimed.as_tuple().exponent
                equal = value == claimed or (rounded and len(right_leaves) == 1 and -8 <= scale <= 0 and value.quantize(Decimal(1).scaleb(scale), rounding=ROUND_HALF_UP) == claimed)
                if not equal:
                    wrong.setdefault(claim['id'], []).append(claim['text'][match['start']:match['end']].strip())
                    continue
                canonical = f'{expression} {"=" if value == claimed else "≈"} {match["result"].strip()}'
                if value != claimed:
                    canonical += '（按原结果精度四舍五入）'
                proof = {'value': claimed, 'dependencies': dependencies, 'parents': parents, 'equation': canonical, 'claim': claim['id']}
                proofs.append(proof)
                accepted.add(identity)
                changed = True
        if not changed:
            break
    rows = {}
    for claim in claims:
        allowed = {source['id'] for source in claim['sources']}
        mentioned = _source_scalars(claim['text'])
        relevant = [p for p in proofs if p['dependencies'] <= allowed and p['claim'] == claim['id']]
        selected = []
        visited = set()
        incomplete = parsed[claim['id']][1]
        def include(proof):
            nonlocal incomplete
            if proof['equation'] in visited:
                return
            if len(selected) >= 24:
                incomplete = True
                return
            visited.add(proof['equation'])
            for parent in proof['parents']:
                include(parent)
            if proof['equation'] not in [p['equation'] for p in selected] and len(selected) < 24:
                selected.append(proof)
            elif len(selected) >= 24:
                incomplete = True
        for proof in relevant:
            include(proof)
        for value in sorted(mentioned):
            if any(p['value'] == value for p in selected):
                continue
            proof = next((p for p in proofs if p['value'] == value and p['dependencies'] <= allowed), None)
            if proof is not None:
                include(proof)
        if wrong.get(claim['id']):
            selected = []
        rows[claim['id']] = {'verified': [{'equation': p['equation'], 'evidence_refs': sorted(p['dependencies'])} for p in selected],
                             'invalid': list(dict.fromkeys(wrong.get(claim['id'], []))), 'coverage_incomplete': incomplete}
    return {'engine': 'lvjin-arithmetic/1', 'claims': rows, 'limits': '每句最多24个依赖步骤，超出标为未完整核验；仅核验显式算式、原文操作数及运算结果，不确认交易事实、扣款顺序或法律适用。'}


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
        self.assumptions = dict(assumptions or {})
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
        for key in ('period_start', 'period_end'):
            if self.assumptions.get(key):
                self.assumptions[key] = _date(self.assumptions[key], key).isoformat()
                self.used[key] = self.assumptions[key]
        if self.assumptions.get('period_start') and self.assumptions.get('period_end') and self.assumptions['period_start'] > self.assumptions['period_end']:
            raise InputError('invalid_period_range')

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
    if not ordered:
        return {'status': 'indeterminate', 'reason': 'all_cashflows_cancel', 'roots': []}
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
        try:
            root = _solve(low, high, flows)
        except (OverflowError, ZeroDivisionError):
            # The search is bounded and may be numerically unusable for very
            # long date ranges. Failure to bracket is not proof of no root.
            continue
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
    payload = {'sign_changes': sign_changes, 'roots': [format(Decimal(repr(r)).quantize(Decimal('0.000001'), rounding=ROUND_HALF_UP), 'f') for r in verified],
               'search_range': {'minimum': str(grid[0]), 'maximum': str(grid[-1])}, 'roots_complete': False}
    if not verified:
        return {**payload, 'status': 'indeterminate', 'reason': 'no_root_found_in_searched_range'}
    if len(verified) > 1:
        return {**payload, 'status': 'multiple_roots', 'reason': 'irregular_flows_have_several_rates'}
    # With x=(1+r)^(-1/basis), dated NPV is a polynomial in x whose
    # exponents are integer day offsets. Descartes' rule bounds its positive
    # roots by cash-flow sign changes. One change and one verified root prove
    # uniqueness; a grid (and a small residual) cannot prove it otherwise.
    if sign_changes != 1:
        return {**payload, 'status': 'indeterminate', 'reason': 'uniqueness_not_proven'}
    return {**payload, 'roots_complete': True, 'status': 'unique', 'annual_rate': payload['roots'][0]}


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
        excluded = []
        if kind in ('loan', 'annualized'):
            excluded = [{'id': r.get('id'), 'reason': 'outside_period'} for r in records if not _in_period(r, ctx.assumptions)]
            records = [r for r in records if _in_period(r, ctx.assumptions)]
            if not records:
                raise InputError('no_inputs_in_period')
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
        if kind in ('loan', 'annualized'):
            result['trace'] = {'included_ids': [r.get('id') for r in records], 'excluded': excluded,
                               'period_start': ctx.assumptions.get('period_start'), 'period_end': ctx.assumptions.get('period_end')}
    return {'engine': ENGINE_VERSION, 'calculation_type': kind, 'assumptions_used': ctx.used, 'result': result}
