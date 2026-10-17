"""Rule-based candidate fields (amounts, dates) for user proofreading.

Every result is a *candidate* with its exact source span. Nothing here becomes a
confirmed fact without an explicit user confirmation in the trusted core.
Chinese numerals (大写/小写) and 万/亿 units are converted with Decimal only.
"""
from __future__ import annotations

import re
from datetime import date
from decimal import Decimal, InvalidOperation

EXTRACTOR_VERSION = 'lvjin-rule-extract/1'

_DIGITS = {'零': 0, '〇': 0, '○': 0, 'О': 0, '一': 1, '壹': 1, '二': 2, '贰': 2, '貳': 2, '两': 2, '俩': 2, '三': 3, '叁': 3, '參': 3,
           '四': 4, '肆': 4, '五': 5, '伍': 5, '六': 6, '陆': 6, '陸': 6, '七': 7, '柒': 7, '八': 8, '捌': 8, '九': 9, '玖': 9}
_SMALL_UNITS = {'十': 10, '拾': 10, '百': 100, '佰': 100, '千': 1000, '仟': 1000}
_BIG_UNITS = {'万': 10 ** 4, '萬': 10 ** 4, '亿': 10 ** 8, '億': 10 ** 8}
_CN_NUM = '零〇○一二两三四五六七八九十百千万亿壹贰貳叁參肆伍陆陸柒捌玖拾佰仟萬億'

_ARABIC_AMOUNT = re.compile(
    r'(?P<prefix>人民币|RMB|CNY|¥|￥)?\s*'
    r'(?P<number>\d{1,3}(?:[,，]\d{3})+(?:\.\d{1,4})?|\d+(?:\.\d{1,4})?)\s*'
    r'(?P<unit>亿元|万元|元整|元|万|亿|块钱|块)?')
_CHINESE_AMOUNT = re.compile(
    r'(?P<prefix>人民币|大写[:：]?\s*)?(?P<number>[' + _CN_NUM + r']+)\s*'
    r'(?P<unit>元整|元|圆整|圆|块钱|块)(?P<fraction>(?:[' + _CN_NUM + r']角)?(?:[' + _CN_NUM + r']分)?)')
# A number directly after a money label (for example a table cell under the
# header "金额（元）") is a candidate even without 元/¥; it still needs review.
_LABELED_AMOUNT = re.compile(
    r'(?P<label>金额|价款|总价|租金|押金|保证金|借款|欠款|还款|本金|利息|违约金|费用)\s*'
    r'(?:[（(]\s*(?:人民币|元|RMB|CNY)\s*[)）])?\s*[:：]?\s*'
    r'(?P<number>\d{1,3}(?:[,，]\d{3})+(?:\.\d{1,4})?|\d+(?:\.\d{1,4})?)(?![\d.%年月日号])')
_DATE_ARABIC = re.compile(r'(?P<y>(?:19|20)\d{2})\s*(?:年|[-/.])\s*(?P<m>\d{1,2})\s*(?:月|[-/.])\s*(?P<d>\d{1,2})\s*日?')
_DATE_CHINESE = re.compile(r'(?P<y>[〇○零一二三四五六七八九]{4})年(?P<m>[一二三四五六七八九十]{1,3})月(?P<d>[一二三四五六七八九十]{1,3})[日号]')


def chinese_integer(text: str) -> int:
    """Parse Chinese numerals such as 两万七千, 壹拾贰万叁仟, 一千零五."""
    if not text:
        raise ValueError('empty')
    if all(ch in _DIGITS for ch in text) and len(text) > 1:
        # Digit-by-digit form such as 二〇二六 (years) or 一二三.
        return int(''.join(str(_DIGITS[ch]) for ch in text))
    total, section, number = 0, 0, 0
    for ch in text:
        if ch in _DIGITS:
            number = _DIGITS[ch]
        elif ch in _SMALL_UNITS:
            section += (number or 1) * _SMALL_UNITS[ch]
            number = 0
        elif ch in _BIG_UNITS:
            section += number
            total += section * _BIG_UNITS[ch]
            section, number = 0, 0
        else:
            raise ValueError('not a Chinese numeral')
    return total + section + number


def _chinese_fraction(text: str) -> Decimal:
    value = Decimal(0)
    for match in re.finditer(r'([' + _CN_NUM + r'])([角分])', text):
        digit = _DIGITS.get(match.group(1))
        if digit is None:
            continue
        value += Decimal(digit) * (Decimal('0.1') if match.group(2) == '角' else Decimal('0.01'))
    return value


def _amount_candidates(text: str):
    for match in _ARABIC_AMOUNT.finditer(text):
        prefix, unit = match.group('prefix'), match.group('unit')
        if not prefix and not unit:
            continue                       # bare numbers are not monetary evidence
        raw_number = match.group('number').replace(',', '').replace('，', '')
        try:
            value = Decimal(raw_number)
        except InvalidOperation:
            continue
        if unit in ('万', '万元'):
            value *= 10 ** 4
        elif unit in ('亿', '亿元'):
            value *= 10 ** 8
        start = match.start('number') if not prefix else match.start('prefix')
        yield {'type': 'amount', 'value': format(value.normalize(), 'f'), 'currency': 'CNY', 'start': start, 'end': match.end(),
               'raw': text[start:match.end()], 'basis': 'arabic'}
    for match in _CHINESE_AMOUNT.finditer(text):
        try:
            value = Decimal(chinese_integer(match.group('number'))) + _chinese_fraction(match.group('fraction') or '')
        except ValueError:
            continue
        start = match.start('prefix') if match.group('prefix') else match.start('number')
        yield {'type': 'amount', 'value': format(value.normalize(), 'f'), 'currency': 'CNY', 'start': start, 'end': match.end(),
               'raw': text[start:match.end()], 'basis': 'chinese_numerals'}
    for match in _LABELED_AMOUNT.finditer(text):
        try:
            value = Decimal(match.group('number').replace(',', '').replace('，', ''))
        except InvalidOperation:
            continue
        start = match.start('label')
        yield {'type': 'amount', 'value': format(value.normalize(), 'f'), 'currency': 'CNY', 'start': start, 'end': match.end(),
               'raw': text[start:match.end()], 'basis': 'labeled'}


def _date_candidates(text: str):
    for match in _DATE_ARABIC.finditer(text):
        try:
            value = date(int(match.group('y')), int(match.group('m')), int(match.group('d')))
        except ValueError:
            continue
        yield {'type': 'date', 'value': value.isoformat(), 'start': match.start(), 'end': match.end(), 'raw': match.group(0), 'basis': 'arabic'}
    for match in _DATE_CHINESE.finditer(text):
        try:
            value = date(chinese_integer(match.group('y')), chinese_integer(match.group('m')), chinese_integer(match.group('d')))
        except ValueError:
            continue
        yield {'type': 'date', 'value': value.isoformat(), 'start': match.start(), 'end': match.end(), 'raw': match.group(0), 'basis': 'chinese_numerals'}


def extract_candidates(blocks: list[dict]) -> dict:
    """Candidate amounts/dates with block index and exact character span."""
    out = []
    for index, block in enumerate(blocks):
        text = block.get('text', '')
        if not text:
            continue
        seen = []
        for candidate in list(_amount_candidates(text)) + list(_date_candidates(text)):
            span = (candidate['start'], candidate['end'])
            if any(span[0] < other[1] and other[0] < span[1] and kind == candidate['type'] for other, kind in seen):
                continue                   # keep the first, most specific match
            seen.append((span, candidate['type']))
            out.append({**candidate, 'block': index})
        if len(out) > 20000:
            break
    return {'version': EXTRACTOR_VERSION, 'candidates': out}
