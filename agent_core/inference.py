"""Fixed-model CPU adapters. No provider client, downloads, credentials or SQL.

Qwen pooling/scoring: official Qwen model cards. mmBERT: official KRLabsOrg
plain-transformers input pair and answer-only offset mapping. Paddle: explicit
paddle_static engine configuration. Runtime artifacts are selected by the core.
"""
from __future__ import annotations

import base64
import io
from pathlib import Path
from typing import Annotated, Literal

from pydantic import Field, StringConstraints
from .contracts import StrictModel

Text = Annotated[str, StringConstraints(min_length=1, max_length=16384)]


class TextBatch(StrictModel):
    kind: Literal['embedding']
    texts: Annotated[list[Text], Field(min_length=1, max_length=32)]
    are_queries: bool = False


class RerankBatch(StrictModel):
    kind: Literal['reranker']
    question: Text
    documents: Annotated[list[Text], Field(min_length=1, max_length=20)]


class GroundingRequest(StrictModel):
    kind: Literal['grounding']
    question: Text
    evidence: Text
    answer: Text


class ImageRequest(StrictModel):
    kind: Literal['ocr-det', 'ocr-rec', 'uvdoc', 'doc-ori', 'textline-ori']
    png_base64: Annotated[str, StringConstraints(min_length=1, max_length=16_777_216)]


class AudioRequest(StrictModel):
    kind: Literal['asr']
    samples_f32_base64: Annotated[str, StringConstraints(min_length=1, max_length=2_560_000)]
    sample_rate: Literal[16000]
    language: Literal['Chinese', 'English'] | None = None


PADDLE_MODELS = {
    'ocr-det': ('TextDetection', 'PP-OCRv6_medium_det'),
    'ocr-rec': ('TextRecognition', 'PP-OCRv6_medium_rec'),
    'uvdoc': ('TextImageUnwarping', 'UVDoc'),
    'doc-ori': ('DocImgOrientationClassification', 'PP-LCNet_x1_0_doc_ori'),
    'textline-ori': ('TextLineOrientationClassification', 'PP-LCNet_x1_0_textline_ori'),
}
INSTRUCTION = 'Given a web search query, retrieve relevant passages that answer the query'
PREFIX = '<|im_start|>system\nJudge whether the Document meets the requirements based on the Query and the Instruct provided. Note that the answer can only be "yes" or "no".<|im_end|>\n<|im_start|>user\n'
SUFFIX = '<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n'


def utf16_offset(text: str, character: int) -> int:
    if not 0 <= character <= len(text):
        raise ValueError('Offset is outside the answer')
    return len(text[:character].encode('utf-16-le')) // 2


class CpuModel:
    """One instance owns one model. The trusted pool owns the worker lifetime."""
    def __init__(self, identity: str, artifact: Path, threads: int = 4):
        if identity not in {*PADDLE_MODELS, 'embedding', 'reranker', 'grounding', 'asr'}:
            raise ValueError('Unregistered model')
        self.identity = identity
        if identity=='uvdoc' and (artifact/'geometry.json').is_file():
            from .geometry import UVDocCpu
            self.geometry_model=UVDocCpu(artifact,threads)
            return
        if identity in PADDLE_MODELS:
            import paddleocr
            class_name, name = PADDLE_MODELS[identity]
            self.model = getattr(paddleocr, class_name)(
                model_name=name, model_dir=str(artifact), device='cpu',
                engine='paddle_static',
                engine_config={'run_mode': 'paddle', 'device_type': 'cpu', 'cpu_threads': threads},
            )
            return
        import torch
        from transformers import AutoModel, AutoModelForCausalLM, AutoModelForTokenClassification, AutoTokenizer, AutoProcessor, AutoModelForMultimodalLM
        torch.set_num_threads(threads)
        common = {'local_files_only': True, 'trust_remote_code': False}
        if identity == 'asr':
            self.processor = AutoProcessor.from_pretrained(str(artifact), **common)
            self.model = AutoModelForMultimodalLM.from_pretrained(str(artifact), dtype=torch.float32, attn_implementation='eager', **common).eval()
        else:
            self.tokenizer = AutoTokenizer.from_pretrained(str(artifact), padding_side='left' if identity != 'grounding' else 'right', **common)
            cls = {'embedding': AutoModel, 'reranker': AutoModelForCausalLM, 'grounding': AutoModelForTokenClassification}[identity]
            self.model = cls.from_pretrained(str(artifact), dtype=torch.float32, attn_implementation='eager', **common).eval()
        if identity == 'reranker':
            self.prefix = self.tokenizer.encode(PREFIX, add_special_tokens=False)
            self.suffix = self.tokenizer.encode(SUFFIX, add_special_tokens=False)
            self.yes = self.tokenizer.convert_tokens_to_ids('yes')
            self.no = self.tokenizer.convert_tokens_to_ids('no')
            if self.yes == self.no or self.yes is None or self.no is None:
                raise ValueError('Invalid yes/no token mapping')

    def predict(self, payload: dict) -> dict:
        if payload.get('kind') != self.identity:
            raise ValueError('Model/request identity mismatch')
        if self.identity in PADDLE_MODELS:
            return self.image(ImageRequest.model_validate(payload))
        if self.identity == 'asr':
            return self.audio(AudioRequest.model_validate(payload))
        import torch
        with torch.inference_mode():
            if self.identity == 'embedding':
                request = TextBatch.model_validate(payload)
                texts = [f'Instruct: {INSTRUCTION}\nQuery:{text}' for text in request.texts] if request.are_queries else request.texts
                batch = self.tokenizer(texts, padding=True, truncation=False, return_tensors='pt')
                if batch.input_ids.shape[1] > 4096:
                    raise ValueError('Input exceeds bounded embedding stage')
                states = self.model(**batch, use_cache=False).last_hidden_state
                if torch.all(batch.attention_mask[:, -1] == 1):
                    pooled = states[:, -1]
                else:
                    lengths = batch.attention_mask.sum(dim=1) - 1
                    pooled = states[torch.arange(states.shape[0]), lengths]
                vectors = torch.nn.functional.normalize(pooled.float(), p=2, dim=1)
                if vectors.shape[-1] != 1024 or not torch.isfinite(vectors).all():
                    raise ValueError('Embedding numerical contract failed')
                return {'vectors': vectors.tolist(), 'dimensions': 1024, 'normalized': True, 'dtype': 'float32', 'truncated': False}
            if self.identity == 'reranker':
                request = RerankBatch.model_validate(payload)
                rows = []
                for document in request.documents:
                    text = f'<Instruct>: {INSTRUCTION}\n<Query>: {request.question}\n<Document>: {document}'
                    row = self.prefix + self.tokenizer.encode(text, add_special_tokens=False) + self.suffix
                    if len(row) > 2048:
                        raise ValueError('Input exceeds bounded reranking stage')
                    rows.append(row)
                batch = self.tokenizer.pad({'input_ids': rows}, padding=True, return_tensors='pt')
                logits = self.model(**batch, use_cache=False, logits_to_keep=1).logits[:, -1, :]
                pair = torch.stack([logits[:, self.no], logits[:, self.yes]], dim=1).float()
                return {'scores': pair.softmax(dim=-1)[:, 1].tolist(), 'logit_differences': (pair[:, 1] - pair[:, 0]).tolist(), 'yes_token_id': self.yes, 'no_token_id': self.no, 'truncated': False}
            request = GroundingRequest.model_validate(payload)
            context = request.evidence + '\n\n问题：' + request.question
            batch = self.tokenizer(context, request.answer, truncation=False, return_offsets_mapping=True, return_tensors='pt')
            if batch.input_ids.shape[1] > 4096:
                raise ValueError('Coverage incomplete: grounding unit exceeds 4096 tokens')
            sequence_ids = batch.sequence_ids(0)
            offsets = batch.pop('offset_mapping')[0].tolist()
            batch.pop('token_type_ids', None)
            probabilities = self.model(**batch).logits.float().softmax(-1)[0, :, 1].tolist()
            tokens = []
            for segment, (start, end), risk in zip(sequence_ids, offsets, probabilities):
                if segment == 1 and start != end:
                    tokens.append({'start': start, 'end': end, 'utf16_start': utf16_offset(request.answer, start), 'utf16_end': utf16_offset(request.answer, end), 'unsupported_score': risk})
            return {'answer_tokens': tokens, 'coverage_complete': True, 'max_position_embeddings': self.model.config.max_position_embeddings}

    def image(self, request: ImageRequest) -> dict:
        import numpy as np
        from PIL import Image
        image = Image.open(io.BytesIO(base64.b64decode(request.png_base64, validate=True)))
        if image.width * image.height > 4_194_304:
            raise ValueError('Image pixel budget exceeded')
        rgb = np.asarray(image.convert('RGB'))
        if self.identity=='uvdoc' and hasattr(self,'geometry_model'):
            return self.geometry_model.predict(rgb)
        # Paddle images use BGR, including numpy input. No file/URL is accepted.
        predictions = list(self.model.predict(input=rgb[:, :, ::-1].copy(), batch_size=1))
        if len(predictions) != 1:
            raise ValueError('Unexpected Paddle output cardinality')
        result = predictions[0]
        def value(name):
            item = result.get(name)
            return item.tolist() if hasattr(item, 'tolist') else item
        if self.identity == 'ocr-det':
            return {'polygons': value('dt_polys'), 'scores': value('dt_scores'), 'coordinate_frame': 'input_pixels'}
        if self.identity == 'ocr-rec':
            return {'text': value('rec_text'), 'score': value('rec_score')}
        if self.identity in {'doc-ori', 'textline-ori'}:
            return {'class_ids': value('class_ids'), 'labels': value('label_names'), 'scores': value('scores')}
        output = result.get('doctr_img')
        if output is None:
            raise ValueError('Missing UVDoc rectified output')
        output = np.asarray(output)
        encoded = io.BytesIO()
        # PaddleX DocTrPostProcess already converts UVDoc's BGR tensor to RGB.
        # Do not exchange channels a second time when encoding a PIL PNG.
        Image.fromarray(output.astype('uint8')).save(encoded, format='PNG')
        return {'png_base64': base64.b64encode(encoded.getvalue()).decode('ascii'), 'shape': list(output.shape), 'geometry_mapping_verified': False}

    def audio(self, request: AudioRequest) -> dict:
        import numpy as np
        import torch
        samples = np.frombuffer(base64.b64decode(request.samples_f32_base64, validate=True), dtype='<f4').copy()
        if not 0 < len(samples) <= 30 * 16000 or not np.isfinite(samples).all():
            raise ValueError('Audio must be finite 16 kHz samples of at most 30 seconds')
        kwargs = {'audio': samples, 'sampling_rate': 16000, 'return_tensors': 'pt'}
        if request.language:
            kwargs['language'] = request.language
        inputs = self.processor.apply_transcription_request(**kwargs)
        with torch.inference_mode():
            output = self.model.generate(**inputs, max_new_tokens=256, do_sample=False)
        generated = output[:, inputs['input_ids'].shape[1]:]
        raw = self.processor.decode(generated, skip_special_tokens=False)[0]
        parsed = self.processor.decode(generated, return_format='parsed')[0]
        return {'raw_output': raw, 'transcription': parsed['transcription'], 'language': parsed.get('language'), 'source_samples': len(samples), 'sample_rate': 16000, 'truncated': generated.shape[1] >= 256, 'word_timestamps': None}
