"""Flow registry. Each flow defines VERSION, build(core) → StateGraph,
initial(inputs) → state and output(state) → the refs returned to the core."""
from __future__ import annotations

from . import analysis, answer, deep_answer, media_review

FLOWS = {
    'answer': answer,
    'deep_answer': deep_answer,
    'media_review': media_review,
    **analysis.FLOWS,
}
VERSIONS = {name: flow.VERSION for name, flow in FLOWS.items()}
