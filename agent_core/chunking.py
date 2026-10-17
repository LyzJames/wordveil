"""Structure-aware chunking measured with the bundled Qwen3 tokenizer.

Units follow headings and clause markers (第X条, 一、, （一）, 1.) so a hit can be
expanded to its parent clause. Chunks target 400-800 Qwen tokens and never
exceed the embedding stage budget. Every chunk keeps exact character spans of
the source blocks it covers; text is never paraphrased or dropped silently.
"""
from __future__ import annotations

import re

TARGET_TOKENS = 600
MAX_TOKENS = 800
HARD_LIMIT = 2048            # embedding initial working budget (design 9.6)
OVERLAP_TOKENS = 60
CHUNKER_VERSION = 'lvjin-chunker/1'

_HEADING_PATTERNS = [
    (1, re.compile(r'^第[一二三四五六七八九十百千零〇两0-9]+[编章]')),
    (2, re.compile(r'^第[一二三四五六七八九十百千零〇两0-9]+节')),
    (3, re.compile(r'^第[一二三四五六七八九十百千零〇两0-9]+条')),
    (3, re.compile(r'^[一二三四五六七八九十]+[、．.]')),
    (4, re.compile(r'^[（(][一二三四五六七八九十]+[)）]')),
    (4, re.compile(r'^\d+(\.\d+)+[\s、]')),
    (4, re.compile(r'^\d{1,3}[、．.](?!\d)')),
]
_SENTENCE = re.compile(r'(?<=[。！？；!?;])|\n')


class TokenCounter:
    def __init__(self, tokenizer_path: str | None):
        self._tokenizer = None
        if tokenizer_path:
            from tokenizers import Tokenizer
            self._tokenizer = Tokenizer.from_file(tokenizer_path)

    def count(self, text: str) -> int:
        if not text:
            return 0
        if self._tokenizer is None:
            # Conservative fallback used only by unit tests without the model.
            return max(1, len(text.encode('utf-8')) // 3)
        return len(self._tokenizer.encode(text, add_special_tokens=False).ids)


def heading_level(block: dict) -> int | None:
    if block.get('kind') == 'heading':
        level = block.get('level')
        return 1 if level in (None, 0) else min(int(level), 4)
    if block.get('kind') not in ('paragraph', 'ocr_line', 'text_line'):
        return None
    text = block.get('text', '').strip()
    if len(text) > 120:
        return None
    for level, pattern in _HEADING_PATTERNS:
        if pattern.match(text):
            return level
    return None


def _split_long(text: str, counter: TokenCounter, limit: int) -> list[tuple[int, int]]:
    """Character ranges of sentence windows; falls back to fixed character steps."""
    pieces, start = [], 0
    for match in _SENTENCE.finditer(text):
        end = match.end()
        if end > start:
            pieces.append((start, end))
            start = end
    if start < len(text):
        pieces.append((start, len(text)))
    windows, current = [], None
    for piece in pieces:
        if counter.count(text[piece[0]:piece[1]]) > limit:
            # A single sentence above the limit: cut by characters.
            step = max(32, limit)
            for offset in range(piece[0], piece[1], step):
                windows.append((offset, min(piece[1], offset + step)))
            current = None
            continue
        if current is None:
            current = piece
        elif counter.count(text[current[0]:piece[1]]) <= limit:
            current = (current[0], piece[1])
        else:
            windows.append(current)
            current = piece
    if current is not None:
        windows.append(current)
    return windows


def chunk_blocks(blocks: list[dict], tokenizer_path: str | None = None, mode: str = 'prose') -> dict:
    """Return units and chunks for an ordered list of blocks.

    mode='prose' for paragraphs/OCR lines/transcripts; mode='rows' for table rows
    (each block is one row; header_text is repeated as context in every chunk).
    """
    counter = TokenCounter(tokenizer_path)
    if mode == 'rows':
        return _chunk_rows(blocks, counter)
    units, chunks = [], []
    current = None
    for index, block in enumerate(blocks):
        level = heading_level(block)
        if current is None or level is not None:
            current = {'index': len(units), 'title': block['text'][:120] if level is not None else '', 'level': level, 'blocks': []}
            units.append(current)
        current['blocks'].append(index)
    for unit in units:
        heading = unit['title']
        spans, tokens = [], 0

        def flush():
            nonlocal spans, tokens
            if not spans:
                return
            body = '\n'.join(blocks[s['block']]['text'][s['start']:s['end']] for s in spans)
            first = spans[0]
            continues = heading and not (first['block'] == unit['blocks'][0] and first['start'] == 0)
            text = (heading + '\n' + body) if continues else body
            chunks.append({'ordinal': len(chunks), 'unit': unit['index'], 'text': text, 'body': body,
                           'token_count': counter.count(text), 'spans': spans})
            spans, tokens = [], 0

        for block_index in unit['blocks']:
            text = blocks[block_index]['text']
            size = counter.count(text)
            if size > MAX_TOKENS:
                flush()
                for start, end in _split_long(text, counter, TARGET_TOKENS):
                    spans = [{'block': block_index, 'start': start, 'end': end}]
                    tokens = counter.count(text[start:end])
                    flush()
                continue
            if tokens and tokens + size > MAX_TOKENS:
                flush()
            spans.append({'block': block_index, 'start': 0, 'end': len(text)})
            tokens += size
            if tokens >= TARGET_TOKENS:
                flush()
        flush()
        unit['token_count'] = counter.count('\n'.join(blocks[i]['text'] for i in unit['blocks']))
    for chunk in chunks:
        if chunk['token_count'] > HARD_LIMIT:
            raise ValueError('chunk_exceeds_embedding_budget')
    return {'version': CHUNKER_VERSION, 'units': units, 'chunks': chunks}


def _chunk_rows(blocks: list[dict], counter: TokenCounter) -> dict:
    chunks, spans, tokens = [], [], 0
    header = ''
    if blocks and blocks[0].get('header_text'):
        header = blocks[0]['header_text']
    header_tokens = counter.count(header)

    def flush():
        nonlocal spans, tokens
        if not spans:
            return
        body = '\n'.join(blocks[s['block']]['text'] for s in spans)
        text = (header + '\n' + body) if header else body
        chunks.append({'ordinal': len(chunks), 'unit': 0, 'text': text, 'body': body,
                       'token_count': counter.count(text), 'spans': spans})
        spans, tokens = [], 0

    for index, block in enumerate(blocks):
        text = block['text']
        size = counter.count(text)
        if size + header_tokens > MAX_TOKENS:
            flush()
            for start, end in _split_long(text, counter, TARGET_TOKENS - header_tokens):
                spans = [{'block': index, 'start': start, 'end': end}]
                flush()
            continue
        if tokens and tokens + size + header_tokens > TARGET_TOKENS:
            flush()
        spans.append({'block': index, 'start': 0, 'end': len(text)})
        tokens += size
    flush()
    return {'version': CHUNKER_VERSION, 'units': [{'index': 0, 'title': header[:120], 'level': None, 'blocks': list(range(len(blocks)))}], 'chunks': chunks}
