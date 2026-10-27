"""Grounded answer (design 7.1, 9.1): plan → hybrid retrieval → rerank →
exact read → draft → sentence verification → at most two revisions."""
from __future__ import annotations

from typing import Any

from langgraph.graph import END, START, StateGraph

from ..core import Core, data, status, warnings
from . import common

VERSION = '1'


class State(common.Answering, total=False):
    scope_id: str
    constraint_ids: list[str]
    snapshot_id: str
    generate: bool
    queries: list[dict[str, str]]
    plan_ref: str | None
    candidate_set: str | None
    read: str | None
    partial: bool


def initial(inputs: dict[str, Any]) -> State:
    return {'owner': inputs['owner'], 'question_id': inputs['question_id'], 'scope_id': inputs['scope_id'], 'constraint_ids': inputs['constraint_ids'],
            'snapshot_id': inputs['snapshot_id'], 'generate': bool(inputs['generate']), 'queries': [{'query_id': 'q0', 'query': inputs['question']}],
            'calculations': [], 'round': 0, 'revisions': 0, 'process': [], 'warnings': []}


def build(core: Core) -> StateGraph:
    def plan(state: State) -> dict[str, Any]:
        if not state['generate']:
            return {}
        step = core.step('plan')
        try:
            result = core.skill('plan', state['owner'], 'query-plan', {'question_id': state['question_id'], 'scope_id': state['scope_id'], 'constraint_ids': state['constraint_ids']})
        except Exception as error:
            if getattr(error, 'code', '') in ('cancelled', 'channel_closed'):
                raise
            return {'process': [step.done('failed', code=getattr(error, 'code', 'failed'))], 'warnings': ['plan_fallback']}
        envelope, view = result['envelope'], result.get('view') or {}
        if status(envelope) != 'completed':
            return {'process': [step.done(status(envelope))], 'warnings': warnings(envelope) + ['plan_fallback']}
        rewrites = [text for text in view.get('rewritten_queries') or [] if isinstance(text, str)][:3]
        queries = state['queries'][:1] + [{'query_id': f'q{index + 1}', 'query': text} for index, text in enumerate(rewrites)]
        return {'queries': queries, 'plan_ref': data(envelope), 'process': [step.done('completed', rewrites=len(rewrites))]}

    def retrieve(state: State) -> dict[str, Any]:
        return common.retrieve(core, state)

    def after_retrieve(state: State) -> str:
        if not state['generate'] or state.get('stop'):
            return 'end'
        return 'compose' if state.get('bundle') and state.get('evidence') else 'insufficient'

    def insufficient(state: State) -> dict[str, Any]:
        return {'warnings': ['insufficient_evidence'], 'partial': True}

    graph = StateGraph(State)
    graph.add_node('plan', plan)
    graph.add_node('retrieve', retrieve)
    graph.add_node('insufficient', insufficient)
    graph.add_node('compose', lambda state: common.compose(core, state))
    graph.add_node('verify', lambda state: common.verify(core, state))
    graph.add_edge(START, 'plan')
    graph.add_edge('plan', 'retrieve')
    graph.add_conditional_edges('retrieve', after_retrieve, {'end': END, 'compose': 'compose', 'insufficient': 'insufficient'})
    graph.add_edge('insufficient', END)
    graph.add_conditional_edges('compose', common.after_compose, {'end': END, 'verify': 'verify'})
    graph.add_conditional_edges('verify', common.after_verify, {'end': END, 'compose': 'compose'})
    return graph


def output(state: State) -> dict[str, Any]:
    return {'queries': state.get('queries', []), 'plan_ref': state.get('plan_ref'), 'candidate_set': state.get('candidate_set'), 'evidence_bundle': state.get('bundle'),
            'evidence_read': state.get('read'), 'evidence': state.get('evidence', []), 'draft': state.get('draft'), 'report': state.get('report'),
            'revisions': state.get('revisions', 0), 'partial': bool(state.get('partial')), 'stop': state.get('stop'), 'process': state.get('process', []), 'warnings': state.get('warnings', [])}
