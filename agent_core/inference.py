"""Fixed-model CPU adapters. No provider client, downloads, credentials or SQL.

Qwen pooling: official Qwen model card. HaS: fixed local NER. Paddle: explicit
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






class ImageRequest(StrictModel):
    kind: Literal['ocr-det', 'ocr-rec', 'uvdoc', 'doc-ori', 'textline-ori']
    png_base64: Annotated[str, StringConstraints(min_length=1, max_length=16_777_216)]


class ImageBatchRequest(StrictModel):
    kind: Literal['ocr-rec', 'doc-ori', 'textline-ori']
    images: Annotated[list[Annotated[str, StringConstraints(min_length=1, max_length=4_194_304)]], Field(min_length=1, max_length=64)]
    rotate180: list[bool] | None = None


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




class CpuModel:
    """One instance owns one model. The trusted pool owns the worker lifetime."""
    def __init__(self, identity: str, artifact: Path, threads: int = 4):
        if identity not in {*PADDLE_MODELS, 'embedding', 'asr', 'privacy-has'}:
            raise ValueError('Unregistered model')
        self.identity = identity
        if identity == 'privacy-has':
            from .privacy_has import HasModel
            self.privacy_model = HasModel(artifact, threads)
            return
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
        from transformers import AutoModel, AutoTokenizer, AutoProcessor, AutoModelForMultimodalLM
        torch.set_num_threads(threads)
        common = {'local_files_only': True, 'trust_remote_code': False}
        if identity == 'asr':
            self.processor = AutoProcessor.from_pretrained(str(artifact), **common)
            self.model = AutoModelForMultimodalLM.from_pretrained(str(artifact), dtype=torch.float32, attn_implementation='eager', **common).eval()
        else:
            self.tokenizer = AutoTokenizer.from_pretrained(str(artifact), padding_side='left', **common)
            cls = AutoModel
            self.model = cls.from_pretrained(str(artifact), dtype=torch.float32, attn_implementation='eager', **common).eval()
    def predict(self, payload: dict) -> dict:
        if payload.get('kind') != self.identity:
            raise ValueError('Model/request identity mismatch')
        if self.identity == 'privacy-has':
            return self.privacy_model.predict(payload)
        if self.identity in PADDLE_MODELS:
            if 'images' in payload:
                return self.image_batch(ImageBatchRequest.model_validate(payload))
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
            raise ValueError('Unregistered model')

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

    def image_batch(self, request: ImageBatchRequest) -> dict:
        """Batched line recognition/orientation. Order is preserved exactly."""
        import numpy as np
        from PIL import Image
        if request.rotate180 is not None and (request.kind != 'ocr-rec' or len(request.rotate180) != len(request.images)):
            raise ValueError('rotate180 must align with recognition crops')
        arrays = []
        for index, encoded in enumerate(request.images):
            image = Image.open(io.BytesIO(base64.b64decode(encoded, validate=True)))
            if image.width * image.height > 4_194_304:
                raise ValueError('Image pixel budget exceeded')
            image = image.convert('RGB')
            if request.rotate180 and request.rotate180[index]:
                image = image.transpose(Image.Transpose.ROTATE_180)
            arrays.append(np.asarray(image)[:, :, ::-1].copy())
        predictions = list(self.model.predict(input=arrays, batch_size=min(len(arrays), 16)))
        if len(predictions) != len(arrays):
            raise ValueError('Unexpected Paddle output cardinality')
        def value(result, name):
            item = result.get(name)
            return item.tolist() if hasattr(item, 'tolist') else item
        if self.identity == 'ocr-rec':
            return {'results': [{'text': value(r, 'rec_text'), 'score': value(r, 'rec_score')} for r in predictions]}
        return {'results': [{'class_ids': value(r, 'class_ids'), 'labels': value(r, 'label_names'), 'scores': value(r, 'scores')} for r in predictions]}

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
