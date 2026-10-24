"""Qualified FP32 reranking with one owner and a bounded CPU fallback.

The CPU embedding/grounding/ASR adapters and embedding identity are untouched.
There is one copy of the selected reranker weights. The core supplies this
immutable policy; business requests cannot choose a device, profile or path.
"""
from __future__ import annotations

import gc
import json
from pathlib import Path

from .inference import CpuModel, INSTRUCTION, RerankBatch

PROFILE = 'reranker-fp32-ieee-cuda86-or-cpu-v1'


class RerankerBackend:
    def __init__(self, artifact: Path, policy_path: Path, threads: int = 4, force_cpu: bool = False):
        import torch
        self.torch = torch
        self.artifact = artifact
        self.threads = threads
        self.owner = None
        self.device = 'cpu'
        self.fallback_reason = None
        policy = json.loads(policy_path.read_text(encoding='utf-8'))
        if policy.get('schema_version') != 'lvjin.reranker-backend/1' or policy.get('profile') != PROFILE:
            raise ValueError('Unsupported reranker backend policy')
        if policy.get('precision') != 'float32-ieee' or policy.get('micro_batch') != 2 or policy.get('compute_capabilities') != [[8, 6]] or policy.get('qualified_devices') != ['NVIDIA GeForce RTX 3060 Laptop GPU']:
            raise ValueError('Unqualified numerical profile')
        self.policy = policy
        try:
            if force_cpu:
                self.fallback_reason = 'core_selected_cpu'
            elif not torch.cuda.is_available() or torch.version.cuda != '12.6':
                self.fallback_reason = 'cuda_runtime_unavailable'
            elif list(torch.cuda.get_device_capability(0)) not in policy['compute_capabilities'] or torch.cuda.get_device_name(0) not in policy['qualified_devices']:
                self.fallback_reason = 'gpu_not_qualified'
            elif torch.cuda.get_device_properties(0).total_memory < policy['minimum_total_vram_bytes'] or torch.cuda.mem_get_info(0)[0] < policy['minimum_free_vram_bytes']:
                self.fallback_reason = 'insufficient_vram'
            else:
                torch.backends.fp32_precision = 'ieee'
                torch.backends.cuda.matmul.fp32_precision = 'ieee'
                torch.backends.cudnn.fp32_precision = 'ieee'
                self.owner = CpuModel('reranker', artifact, threads)
                self.owner.model.to('cuda:0')
                self.device = 'cuda:0'
                check = self._predict(RerankBatch.model_validate(policy['self_check']['request']))
                expected = policy['self_check']['scores']
                if len(check['scores']) != len(expected) or max(abs(a-b) for a,b in zip(check['scores'], expected)) > policy['self_check']['absolute_tolerance']:
                    self.fallback_reason = 'numerical_self_check_failed'
        except (RuntimeError, OSError):
            self.fallback_reason = 'gpu_initialization_failed'
        if self.fallback_reason:
            self._cpu()

    def _cpu(self):
        self.owner = None
        gc.collect()
        try:
            if self.torch.cuda.is_initialized():
                self.torch.cuda.empty_cache()
        except RuntimeError:
            pass
        self.owner = CpuModel('reranker', self.artifact, self.threads)
        self.device = 'cpu'

    def metadata(self):
        return {'policy': PROFILE, 'device': self.device, 'precision': 'float32-ieee',
                'micro_batch': 2, 'fallback_reason': self.fallback_reason}

    def predict(self, payload: dict) -> dict:
        request = RerankBatch.model_validate(payload)
        # Validate shape/token limits before deciding whether a device failed.
        batch = self._batch(request)
        failure = False
        try:
            return self._predict(request, batch)
        except RuntimeError:
            if self.device == 'cpu':
                raise
            failure = True
        # Leave the exception scope before releasing the old owner: traceback
        # tensors must not keep GPU weights/activations alive beside the CPU.
        if failure:
            self.fallback_reason = 'gpu_execution_failed'
            self._cpu()
        return self._predict(request, batch)

    def _batch(self, request):
        rows = []
        for document in request.documents:
            text = f'<Instruct>: {INSTRUCTION}\n<Query>: {request.question}\n<Document>: {document}'
            row = self.owner.prefix + self.owner.tokenizer.encode(text, add_special_tokens=False) + self.owner.suffix
            if len(row) > 2048:
                raise ValueError('Input exceeds bounded reranking stage')
            rows.append(row)
        return self.owner.tokenizer.pad({'input_ids': rows}, padding=True, return_tensors='pt')

    def _predict(self, request, batch=None):
        torch = self.torch
        if batch is None:
            batch = self._batch(request)
        scores, differences = [], []
        # Pad once before slicing, preserving the reference positions/masks.
        with torch.inference_mode():
            for offset in range(0, len(request.documents), 2):
                piece = {key: value[offset:offset+2].to(self.device) for key, value in batch.items()}
                logits = self.owner.model(**piece, use_cache=False, logits_to_keep=1).logits[:, -1, :]
                pair = torch.stack([logits[:, self.owner.no], logits[:, self.owner.yes]], dim=1).float()
                if not torch.isfinite(pair).all():
                    raise RuntimeError('Nonfinite reranker output')
                scores.extend(pair.softmax(dim=-1)[:, 1].cpu().tolist())
                differences.extend((pair[:, 1] - pair[:, 0]).cpu().tolist())
                del piece, logits, pair
        return {'scores': scores, 'logit_differences': differences,
                'yes_token_id': self.owner.yes, 'no_token_id': self.owner.no,
                'truncated': False, 'backend': self.metadata()}
