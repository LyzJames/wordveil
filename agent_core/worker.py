"""Bounded JSON-lines worker. Spawned only by the trusted isolated process broker.

The broker supplies immutable resource paths on the command line. Business input
has no paths, actor, grants, SQL, model downloads or credentials.
"""
from __future__ import annotations

import argparse
import contextlib
import json
import os
from pathlib import Path
import sys

MAX_REQUEST=18*1024*1024
MAX_RESPONSE=24*1024*1024


def emit(protocol, value):
    encoded=json.dumps(value,separators=(',',':'),ensure_ascii=False).encode('utf-8')+b'\n'
    if len(encoded)>MAX_RESPONSE:
        encoded=b'{"event":"error","code":"output_budget"}\n'
    protocol.write(encoded)
    protocol.flush()


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--model')
    parser.add_argument('--artifact',type=Path)
    parser.add_argument('--backend',choices=['cpu'],default='cpu')
    parser.add_argument('--backend-profile',type=Path)
    parser.add_argument('--service',choices=['local'],help='Run the non-model local service (parsing, OCR geometry, chunking, finance) without networking.')
    parser.add_argument('--tokenizer',type=Path,help='Core-selected read-only Qwen tokenizer.json for chunk token counts.')
    parser.add_argument('--diagnose-installation',action='store_true',help='Return model-load diagnostics before accepting any business input (0.1.0 verification).')
    parser.add_argument('--installation-fixture',type=Path,help='With installation diagnostics, run one core-selected synthetic fixture and exit without reading stdin.')
    args=parser.parse_args()
    if args.backend!='cpu' and (args.model!='reranker' or not args.backend_profile or args.service):
        parser.error('The qualified backend is restricted to the reranker')
    if args.backend=='cpu' and args.backend_profile:
        parser.error('CPU reference workers take no backend policy')
    if args.service:
        if args.model or args.artifact or args.installation_fixture:
            parser.error('The local service takes no model arguments')
        return serve_local(args.tokenizer)
    if not args.model or not args.artifact:
        parser.error('--model and --artifact are required')
    if args.installation_fixture and not args.diagnose_installation:
        parser.error('Installation fixtures require explicit installation diagnostics')
    os.environ.update(HF_HUB_OFFLINE='1',TRANSFORMERS_OFFLINE='1',HF_HUB_DISABLE_TELEMETRY='1',PADDLE_PDX_DISABLE_MODEL_SOURCE_CHECK='True',CUDA_VISIBLE_DEVICES='0' if args.backend=='cuda_or_cpu' else '',TOKENIZERS_PARALLELISM='false')
    protocol=sys.stdout.buffer
    # Vendor diagnostics may contain inputs. They never enter ordinary app logs.
    with open(os.devnull,'w',encoding='utf-8') as sink:
        try:
            with contextlib.redirect_stdout(sink),contextlib.redirect_stderr(sink):
                from agent_core.inference import CpuModel
                model=CpuModel(args.model,args.artifact)
        except Exception as error:
            result={'event':'error','code':'model_load_failed','error_type':type(error).__name__}
            if args.diagnose_installation:
                import traceback
                result['installation_diagnostics']=traceback.format_exc(limit=12)[-6000:]
            emit(protocol,result)
            return 1
        if args.installation_fixture:
            # The diagnostic broker grants only this public/synthetic fixture.
            # No business stdin is accepted and ordinary workers never take it.
            try:
                request=json.loads(args.installation_fixture.read_text(encoding='utf-8'))
                with contextlib.redirect_stdout(sink),contextlib.redirect_stderr(sink):
                    result=model.predict(request)
                emit(protocol,{'event':'result','data':result})
                return 0
            except Exception:
                import traceback
                emit(protocol,{'event':'error','code':'installation_fixture_failed','installation_diagnostics':traceback.format_exc(limit=14)[-6500:]})
                return 1
        emit(protocol,{'event':'ready','model_id':args.model,'backend':{'device':'cpu'}})
        while True:
            line=sys.stdin.buffer.readline(MAX_REQUEST+1)
            if not line:
                return 0
            if len(line)>MAX_REQUEST or not line.endswith(b'\n'):
                emit(protocol,{'event':'error','code':'input_budget'})
                return 1
            try:
                request=json.loads(line)
                with contextlib.redirect_stdout(sink),contextlib.redirect_stderr(sink):
                    result=model.predict(request)
                emit(protocol,{'event':'result','data':result})
            except Exception:
                emit(protocol,{'event':'error','code':'invalid_or_failed_inference'})


def serve_local(tokenizer):
    os.environ.update(HF_HUB_OFFLINE='1',TRANSFORMERS_OFFLINE='1',HF_HUB_DISABLE_TELEMETRY='1',CUDA_VISIBLE_DEVICES='',TOKENIZERS_PARALLELISM='false')
    protocol=sys.stdout.buffer
    with open(os.devnull,'w',encoding='utf-8') as sink:
        from agent_core.local_service import LocalService,ServiceError
        service=LocalService(str(tokenizer) if tokenizer else None)
        emit(protocol,{'event':'ready','service':'local'})
        while True:
            line=sys.stdin.buffer.readline(MAX_REQUEST+1)
            if not line:
                return 0
            if len(line)>MAX_REQUEST or not line.endswith(b'\n'):
                emit(protocol,{'event':'error','code':'input_budget'})
                return 1
            try:
                request=json.loads(line)
                with contextlib.redirect_stdout(sink),contextlib.redirect_stderr(sink):
                    result=service.handle(request)
                emit(protocol,{'event':'result','data':result})
            except ServiceError as error:
                emit(protocol,{'event':'error','code':error.code})
            except Exception:
                emit(protocol,{'event':'error','code':'invalid_or_failed_request'})


if __name__=='__main__':
    raise SystemExit(main())
