"""Orchestrator process entry: announce, accept one flow.run, run the graph
with core-mirrored checkpoints (durability "sync": every superstep is stored
before the next starts), answer with the flow's refs, exit."""
from __future__ import annotations

import contextlib
import os
import re
import sys

RUN_ID = re.compile(r'^run_[0-9a-f]{32}$')


def serve_orchestrator() -> int:
    # Tracing exports stay off; the process has no network either way.
    os.environ.update(LANGSMITH_TRACING='false', LANGCHAIN_TRACING_V2='false', LANGSMITH_OTEL_ENABLED='false', LANGGRAPH_STRICT_MSGPACK='true')
    protocol_in, protocol_out = sys.stdin.buffer, sys.stdout.buffer
    with open(os.devnull, 'w', encoding='utf-8') as sink, contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
        from importlib.metadata import version

        from . import PROTOCOL
        from .checkpoint import CoreSaver
        from .core import Core
        from .flows import FLOWS, VERSIONS
        from .rpc import CoreError, Peer

        peer = Peer(protocol_in, protocol_out)
        peer.notify('ready', {'protocol': PROTOCOL, 'flows': VERSIONS, 'langgraph': version('langgraph')})
        request = peer.next_request()
        if request is None:
            return 0
        ident, params = request.get('id'), request.get('params')
        if request.get('method') != 'flow.run' or not isinstance(params, dict):
            peer.respond(ident, error={'code': -32601, 'message': 'method_not_found'})
            return 1
        run_id, flow, inputs = params.get('run_id'), params.get('flow'), params.get('inputs')
        resume, attempt = params.get('resume'), params.get('attempt')
        if not (isinstance(run_id, str) and RUN_ID.match(run_id)) or flow not in FLOWS or not isinstance(inputs, dict) \
                or not isinstance(resume, bool) or not isinstance(attempt, int) or attempt < 1:
            peer.respond(ident, error={'code': -32602, 'message': 'invalid_params'})
            return 1
        module = FLOWS[flow]
        saver = CoreSaver(peer, run_id)
        try:
            restored = saver.load() if resume else 0
            graph = module.build(Core(peer, attempt)).compile(checkpointer=saver)
            config = {'configurable': {'thread_id': run_id}, 'recursion_limit': 80, 'max_concurrency': 2}
            state = graph.invoke(None if restored else module.initial(inputs), config, durability='sync')
        except CoreError as error:
            peer.respond(ident, error={'code': -32000, 'message': error.code})
            return 1
        except Exception as error:
            peer.respond(ident, error={'code': -32000, 'message': 'flow_failed', 'data': {'type': type(error).__name__}})
            return 1
        peer.respond(ident, {'status': 'completed', 'output': module.output(state)})
        return 0
