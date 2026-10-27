"""Pinned HaS NER adapter. No server, networking, downloads, keys or tools.

The native ABI is built against the exact shipped llama.cpp header. The core
grants one read-only artifact directory. Each call gets a fresh context.
"""
from __future__ import annotations

import ctypes
import json
import os
import re
from pathlib import Path


ENTITY_TYPES = ['人名', '地址', '电话号码', '电子邮件', '身份证号码', '银行卡号', '组织', '账号', '密码', '密钥']
MAX_TEXT_BYTES = 16384


class HasModel:
    def __init__(self, artifact: Path, threads: int = 4):
        self.root = artifact.resolve(strict=True)
        self.runtime = (self.root / 'runtime').resolve(strict=True)
        self.model = (self.root / 'has_text_model.gguf').resolve(strict=True)
        if self.runtime.parent != self.root or self.model.parent != self.root:
            raise ValueError('Invalid privacy resource boundary')
        self.dll_directory = os.add_dll_directory(str(self.runtime))
        self.lib = ctypes.CDLL(str(self.runtime / 'privacy_has.dll'), winmode=0x00001100)
        self.lib.has_open.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p, ctypes.c_int]
        self.lib.has_open.restype = ctypes.c_void_p
        self.lib.has_close.argtypes = [ctypes.c_void_p]
        self.lib.has_close.restype = None
        self.lib.has_infer.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
        self.lib.has_infer.restype = ctypes.c_int
        self.handle = self.lib.has_open(str(self.runtime), str(self.model), threads)
        if not self.handle:
            raise RuntimeError('Privacy model initialization failed')

    def predict(self, payload: dict) -> dict:
        if set(payload) != {'kind', 'text'} or payload['kind'] != 'privacy-has':
            raise ValueError('Invalid privacy task')
        text = payload['text']
        if not isinstance(text, str) or not text or len(text.encode('utf-8')) > MAX_TEXT_BYTES or '\0' in text:
            raise ValueError('Invalid privacy text budget')
        if '<|im_' in text or '</text>' in text:
            raise ValueError('Privacy input requires boundary review')
        # The task content matches the vendor's NER template. The fixed ChatML
        # envelope belongs to this pinned model/template compatibility identity.
        content = 'Recognize the following entity types in the text.\nSpecified types:' + json.dumps(ENTITY_TYPES, ensure_ascii=False, separators=(',', ':')) + '\n<text>' + text + '</text>'
        prompt = ('<|im_start|>user\n' + content + '<|im_end|>\n<|im_start|>assistant\n').encode('utf-8')
        output = ctypes.create_string_buffer(131072)
        size = self.lib.has_infer(self.handle, prompt, len(prompt), 1536, output, len(output))
        if size < 0:
            raise RuntimeError('Privacy inference incomplete')
        raw = output.raw[:size].decode('utf-8', errors='strict').strip()
        if raw.startswith('```json\n') and raw.endswith('\n```'):
            raw = raw[8:-4].strip()
        def unique_pairs(pairs):
            result = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError('Duplicate entity type')
                result[key] = value
            return result
        entities = json.loads(raw, object_pairs_hook=unique_pairs)
        if not isinstance(entities, dict) or any(key not in ENTITY_TYPES for key in entities):
            raise ValueError('Invalid privacy entity schema')
        if any(not isinstance(values, list) or len(values) > 128 for values in entities.values()):
            raise ValueError('Invalid privacy entity list')
        out = []
        for kind, values in entities.items():
            for value in values:
                if not isinstance(value, str) or not value.strip() or len(value.encode('utf-8')) > 2048:
                    raise ValueError('Invalid privacy entity value')
                # Core-issued markers already hide known strong-rule matches.
                if re.fullmatch(r'⟦SY:[A-F0-9]{16}:(?:PERSON|ORG|ADDR|PHONE|EMAIL|ID|ACCOUNT|SECRET|PRIVATE):[A-F0-9]{10}⟧', value):
                    continue
                if value not in text:
                    raise ValueError('Privacy entity is absent from source')
                out.append({'kind': kind, 'value': value})
        if len(out) > 512:
            raise ValueError('Privacy entity budget')
        return {'entities': out, 'input_complete': True, 'truncated': False}

    def __del__(self):
        if getattr(self, 'handle', None):
            self.lib.has_close(self.handle)
            self.handle = None
