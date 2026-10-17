"""Non-model local service: parsing, OCR geometry, chunking, extraction, finance.

Runs in the same no-network AppContainer worker as the models. Requests carry
document bytes in bounded chunks (never paths); results are bounded and errors
are reported only as stable codes so document text never reaches diagnostics.
"""
from __future__ import annotations

import base64
from typing import Annotated, Literal

from pydantic import Field, StringConstraints

from .contracts import StrictModel

MAX_UPLOAD_BYTES = 200 * 1024 * 1024
MAX_RESIDENT_UPLOADS = 512 * 1024 * 1024
UploadId = Annotated[str, StringConstraints(pattern=r'^u[0-9a-f]{32}$')]


class UploadBegin(StrictModel):
    op: Literal['upload_begin']
    upload_id: UploadId
    bytes: Annotated[int, Field(ge=0, le=MAX_UPLOAD_BYTES)]


class UploadChunk(StrictModel):
    op: Literal['upload_chunk']
    upload_id: UploadId
    offset: Annotated[int, Field(ge=0)]
    data_b64: Annotated[str, StringConstraints(min_length=0, max_length=12_000_000)]


class UploadRelease(StrictModel):
    op: Literal['upload_release']
    upload_id: UploadId


class DocumentOp(StrictModel):
    op: Literal['sniff', 'pdf_info', 'pdf_pages', 'pdf_render', 'docx_blocks', 'xlsx_info', 'xlsx_rows', 'delimited_rows', 'text_blocks', 'image_pages']
    upload_id: UploadId
    first: Annotated[int, Field(ge=0)] = 0
    last: Annotated[int, Field(ge=0)] = 0
    purpose: Literal['ocr', 'display'] = 'display'
    sheet: Annotated[str, StringConstraints(max_length=256)] | None = None
    offset: Annotated[int, Field(ge=0)] = 0
    limit: Annotated[int, Field(ge=1, le=2000)] = 2000
    password: Annotated[str, StringConstraints(max_length=256)] | None = None


class RotateOp(StrictModel):
    op: Literal['rotate_quarter']
    png_base64: Annotated[str, StringConstraints(min_length=1, max_length=24_000_000)]
    clockwise_turns: Annotated[int, Field(ge=0, le=3)]


class CropOp(StrictModel):
    op: Literal['crop_polygons']
    png_base64: Annotated[str, StringConstraints(min_length=1, max_length=24_000_000)]
    polygons: Annotated[list[list[list[float]]], Field(max_length=2000)]


class Block(StrictModel):
    text: Annotated[str, StringConstraints(max_length=1_000_000)]
    kind: Annotated[str, StringConstraints(max_length=32)] = 'paragraph'
    level: int | None = None
    header_text: Annotated[str, StringConstraints(max_length=20000)] | None = None


class ChunkOp(StrictModel):
    op: Literal['chunk_blocks']
    blocks: Annotated[list[Block], Field(max_length=200000)]
    mode: Literal['prose', 'rows'] = 'prose'


class ExtractOp(StrictModel):
    op: Literal['extract_candidates']
    blocks: Annotated[list[Block], Field(max_length=200000)]


class FinanceOp(StrictModel):
    op: Literal['finance']
    request: dict


class OriOp(StrictModel):
    op: Literal['doc_ori_turns']
    label: Annotated[str, StringConstraints(max_length=8)]


class LocalService:
    def __init__(self, tokenizer_path: str | None):
        self.tokenizer_path = tokenizer_path
        self.uploads: dict[str, bytearray] = {}
        self.expected: dict[str, int] = {}

    def _resident(self) -> int:
        return sum(self.expected.values())

    def handle(self, request: dict) -> dict:
        op = request.get('op')
        if op == 'upload_begin':
            item = UploadBegin.model_validate(request)
            if item.upload_id in self.uploads or self._resident() + item.bytes > MAX_RESIDENT_UPLOADS:
                raise ServiceError('upload_rejected')
            self.uploads[item.upload_id] = bytearray()
            self.expected[item.upload_id] = item.bytes
            return {'accepted': True}
        if op == 'upload_chunk':
            item = UploadChunk.model_validate(request)
            buffer = self.uploads.get(item.upload_id)
            if buffer is None or item.offset != len(buffer):
                raise ServiceError('upload_sequence')
            data = base64.b64decode(item.data_b64, validate=True)
            if len(buffer) + len(data) > self.expected[item.upload_id]:
                raise ServiceError('upload_overflow')
            buffer.extend(data)
            return {'received': len(buffer)}
        if op == 'upload_release':
            item = UploadRelease.model_validate(request)
            self.uploads.pop(item.upload_id, None)
            self.expected.pop(item.upload_id, None)
            return {'released': True}
        if op in DocumentOp.model_fields['op'].annotation.__args__:
            item = DocumentOp.model_validate(request)
            data = self.uploads.get(item.upload_id)
            if data is None or len(data) != self.expected.get(item.upload_id):
                raise ServiceError('upload_incomplete')
            return self._document(item, bytes(data))
        if op == 'rotate_quarter':
            from .ocr_ops import rotate_quarter
            item = RotateOp.model_validate(request)
            return rotate_quarter(item.png_base64, item.clockwise_turns)
        if op == 'crop_polygons':
            from .ocr_ops import crop_polygons
            item = CropOp.model_validate(request)
            return crop_polygons(item.png_base64, item.polygons)
        if op == 'doc_ori_turns':
            from .ocr_ops import doc_ori_clockwise_turns
            return {'clockwise_turns': doc_ori_clockwise_turns(OriOp.model_validate(request).label)}
        if op == 'chunk_blocks':
            from .chunking import chunk_blocks
            item = ChunkOp.model_validate(request)
            return chunk_blocks([b.model_dump() for b in item.blocks], self.tokenizer_path, item.mode)
        if op == 'extract_candidates':
            from .extract import extract_candidates
            item = ExtractOp.model_validate(request)
            return extract_candidates([b.model_dump() for b in item.blocks])
        if op == 'finance':
            from .finance import calculate, InputError
            item = FinanceOp.model_validate(request)
            try:
                return calculate(item.request)
            except InputError as error:
                raise ServiceError('finance_' + error.code) from error
        if op == 'health':
            return {'service': 'local', 'uploads': len(self.uploads)}
        raise ServiceError('unknown_operation')

    def _document(self, item: DocumentOp, data: bytes) -> dict:
        from . import parsers
        try:
            if item.op == 'sniff':
                return {'kind': parsers.sniff(data), 'bytes': len(data)}
            if item.op == 'pdf_info':
                return parsers.pdf_info(data, item.password)
            if item.op == 'pdf_pages':
                return parsers.pdf_pages(data, item.first, item.last, item.password)
            if item.op == 'pdf_render':
                return parsers.pdf_render(data, item.first, item.purpose, item.password)
            if item.op == 'docx_blocks':
                return parsers.docx_blocks(data)
            if item.op == 'xlsx_info':
                return parsers.xlsx_info(data)
            if item.op == 'xlsx_rows':
                return parsers.xlsx_rows(data, item.sheet or '', item.offset, item.limit)
            if item.op == 'delimited_rows':
                return parsers.delimited_rows(data, item.offset, item.limit)
            if item.op == 'text_blocks':
                return parsers.text_blocks(data)
            if item.op == 'image_pages':
                return parsers.image_pages(data)
        except parsers.ParseError as error:
            raise ServiceError(error.code) from error
        raise ServiceError('unknown_operation')


class ServiceError(Exception):
    def __init__(self, code: str):
        super().__init__(code)
        self.code = code
