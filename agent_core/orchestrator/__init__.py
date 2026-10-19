"""LangGraph orchestrator for flows with generation steps (M2).

Runs in the same no-network AppContainer as the other workers and is bound
to one root task. It owns control flow only: every business step, model
request, budget, confirmation and audit stays in the Rust core, which it
reaches through typed JSON-RPC (protocol ``lvjin.orchestrator/1``).
Checkpoints are serialised with strict msgpack (no pickle) and stored by the
core in the encrypted vault, so a flow can continue from node boundaries.
"""
PROTOCOL = 'lvjin.orchestrator/1'
