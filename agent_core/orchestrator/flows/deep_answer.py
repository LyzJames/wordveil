"""Deep answer ("深入分析", design 7.3): local retrieval of the original
question → the planner's bounded task graph (validated by the core) → roles
in dependency waves (parallel within a wave) → synthesis → verification →
one check → at most one revision. No query rewriting: the root's seven
requests go to the plan, the roles and the checks."""
from __future__ import annotations

from typing import Any

from langgraph.graph import END, START, StateGraph
from langgraph.types import Send

from ..core import Core, data, status, warnings
from ..rpc import CoreError
from . import common, roles

VERSION = '1'


class State(roles.Roles, total=False):
    question: str
    scope_id: str
    snapshot_id: str
    queries: list[dict[str, str]]
    candidate_set: str | None
    read: str | None
    partial: bool
    task_plan: str | None
    tasks: list[dict[str, Any]]


def initial(inputs: dict[str, Any]) -> State:
    return {'owner': inputs['owner'], 'question_id': inputs['question_id'], 'question': inputs['question'], 'scope_id': inputs['scope_id'],
            'snapshot_id': inputs['snapshot_id'], 'queries': [{'query_id': 'q0', 'query': inputs['question']}], 'calculations': [],
            'round': 0, 'revisions': 0, 'process': [], 'warnings': [], 'analyses': [], 'done': {}, 'tasks': []}


def build(core: Core) -> StateGraph:
    def retrieve(state: State) -> dict[str, Any]:
        return common.retrieve(core, state)

    def after_retrieve(state: State) -> str:
        return 'plan' if state.get('bundle') and state.get('evidence') else 'insufficient'

    def insufficient(state: State) -> dict[str, Any]:
        return {'warnings': ['insufficient_evidence'], 'partial': True}

    def plan(state: State) -> dict[str, Any]:
        step = core.step('plan_tasks')
        try:
            agent = core.register('planner:reg', state['owner'], 'planner')
            packet = core.packet('planner:packet', state['owner'], agent, {'kind': 'planner', 'evidence_bundle_id': state['bundle'], 'calculation_ids': []})
            refs = packet['refs']
            result = core.generate('planner', state['owner'], agent, {'question_id': state['question_id'], 'context_packet_ref': packet['packet'],
                                                                      'catalog_ref': refs['catalog_ref'], 'goal_ref': refs['goal_ref'], 'stop_constraints_ref': refs['stop_constraints_ref']})
        except CoreError as error:
            if error.code in roles.INTERRUPTED:
                raise
            return {'process': [step.done('failed', code=error.code)], 'warnings': ['plan_fallback']}
        envelope, view = result['envelope'], result.get('view') or {}
        if status(envelope) == 'cancelled':
            raise CoreError('cancelled')
        tasks = [task for task in view.get('task_graph') or [] if isinstance(task, dict)]
        if not tasks:
            return {'task_plan': data(envelope), 'process': [step.done(status(envelope), roles=0)], 'warnings': warnings(envelope) + ['plan_fallback']}
        return {'task_plan': data(envelope), 'tasks': tasks, 'process': [step.done('completed', roles=len(tasks))]}

    def schedule(state: State) -> dict[str, Any]:
        return {}

    def route(state: State):
        done = state.get('done') or {}
        pending = [task for task in state.get('tasks') or [] if task['node_id'] not in done]
        ready = [task for task in pending if all(dependency in done for dependency in task.get('depends_on') or [])]
        if ready:
            return [Send('role', roles.payload(state, task)) for task in ready]
        return 'synthesize'

    graph = StateGraph(State)
    graph.add_node('retrieve', retrieve)
    graph.add_node('insufficient', insufficient)
    graph.add_node('plan', plan)
    graph.add_node('schedule', schedule)
    graph.add_node('role', roles.role_task(core))
    graph.add_edge(START, 'retrieve')
    graph.add_conditional_edges('retrieve', after_retrieve, {'plan': 'plan', 'insufficient': 'insufficient'})
    graph.add_edge('insufficient', END)
    graph.add_edge('plan', 'schedule')
    graph.add_conditional_edges('schedule', route, ['role', 'synthesize'])
    graph.add_edge('role', 'schedule')
    roles.add_answering(graph, core, END)
    return graph


def output(state: State) -> dict[str, Any]:
    return {'queries': state.get('queries', []), 'plan_ref': None, 'candidate_set': state.get('candidate_set'), 'evidence_bundle': state.get('bundle'),
            'evidence_read': state.get('read'), 'evidence': state.get('evidence', []), 'draft': state.get('draft'), 'report': state.get('report'),
            'revisions': state.get('revisions', 0), 'partial': bool(state.get('partial')), 'task_plan': state.get('task_plan'), 'analyses': state.get('analyses', []),
            'challenge': state.get('challenge'), 'process': state.get('process', []), 'warnings': state.get('warnings', [])}
