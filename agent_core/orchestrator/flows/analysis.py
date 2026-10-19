"""Written analyses (contract, loan, budget): the base workflow opens its
finalizing lvjin-retrieval child, the fixed roles analyse the evidence and
calculations the core prepared (in parallel), the synthesizer drafts,
sentence verification, one check, at most one revision, and the core seals
the child's result."""
from __future__ import annotations

from typing import Any

from langgraph.graph import END, START, StateGraph
from langgraph.types import Send

from ..core import Core, data, status
from ..rpc import CoreError
from . import roles

VERSION = '1'

ROLES = {
    'contract_review': [('t1', 'legal-analyst'), ('t2', 'finance-analyst')],
    'loan_analysis': [('t1', 'finance-analyst'), ('t2', 'legal-analyst')],
    'budget_analysis': [('t1', 'finance-analyst')],
}


class State(roles.Roles, total=False):
    base: str
    retrieval_input: dict[str, Any]
    retrieval: str | None
    partial: bool


class Flow:
    """One fixed analysis flow (module-like: VERSION, initial, build, output)."""

    VERSION = VERSION

    def __init__(self, name: str):
        self.name = name

    def initial(self, inputs: dict[str, Any]) -> State:
        return {'base': inputs['owner'], 'question_id': inputs['question_id'], 'bundle': inputs['evidence_bundle'],
                'calculations': list(inputs.get('calculation_ids') or []), 'retrieval_input': inputs['retrieval_input'],
                'round': 0, 'revisions': 0, 'process': [], 'warnings': [], 'analyses': [], 'done': {}}

    def build(self, core: Core) -> StateGraph:
        flow = self.name

        def open_owner(state: State) -> dict[str, Any]:
            # The finalizing child owns every generation step of the analysis.
            return {'owner': core.open('open', state['base'], 'lvjin-retrieval', state['retrieval_input'], finalizer=True)}

        def route(state: State):
            return [Send('role', roles.payload(state, {'node_id': node, 'role_id': role, 'goal': f'{flow}.{role}'})) for node, role in ROLES[flow]]

        def close(state: State) -> dict[str, Any]:
            result = core.close('close', state['owner'], {'evidence_bundle': state.get('bundle'), 'draft': state.get('draft'), 'report': state.get('report'),
                                                        'revisions': state.get('revisions', 0), 'partial': bool(state.get('partial')), 'analyses': state.get('analyses', []),
                                                        'challenge': state.get('challenge'), 'process': state.get('process', []), 'warnings': state.get('warnings', [])})
            envelope = result['envelope']
            if status(envelope) == 'cancelled':
                raise CoreError('cancelled')
            return {'retrieval': data(envelope)}

        graph = StateGraph(State)
        graph.add_node('open', open_owner)
        graph.add_node('role', roles.role_task(core))
        graph.add_node('close', close)
        graph.add_edge(START, 'open')
        graph.add_conditional_edges('open', route, ['role'])
        graph.add_edge('role', 'synthesize')
        roles.add_answering(graph, core, 'close')
        graph.add_edge('close', END)
        return graph

    @staticmethod
    def output(state: State) -> dict[str, Any]:
        return {'retrieval': state.get('retrieval'), 'analyses': state.get('analyses', []), 'challenge': state.get('challenge'),
                'process': state.get('process', []), 'warnings': state.get('warnings', [])}


FLOWS = {name: Flow(name) for name in ROLES}
