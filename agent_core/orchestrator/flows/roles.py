"""Narrow-role steps shared by the deep answer and the written analyses
(design 7.2-7.4): a role analyses its packet once; the synthesizer drafts
from the evidence and the candidate analyses; sentence verification; one
clean-context check; at most one revision. Budgets stay in the core."""
from __future__ import annotations

import operator
from typing import Annotated, Any

from ..core import Core, data, status, warnings
from ..rpc import CoreError
from . import common


def merge(left: dict[str, Any] | None, right: dict[str, Any] | None) -> dict[str, Any]:
    return {**(left or {}), **(right or {})}


class Roles(common.Answering, total=False):
    owner: str
    analyses: Annotated[list[str], operator.add]
    done: Annotated[dict[str, Any], merge]
    synth_agent: str | None
    challenge: str | None
    challenges: list[str]
    checked: bool


INTERRUPTED = ('cancelled', 'channel_closed')


def role_task(core: Core):
    """One analysis role on one task (dispatched with Send)."""
    def run(payload: dict[str, Any]) -> dict[str, Any]:
        task = payload['task']
        node, role = task['node_id'], task['role_id']
        step = core.step('role')
        try:
            agent = core.register(f'role:{node}:reg', payload['owner'], role)
            packet = task.get('context_packet_ref')
            if not packet:
                spec = {'kind': 'role', 'goal': task['goal'], 'evidence_bundle_id': payload['bundle'], 'calculation_ids': payload['calculations']}
                packet = core.packet(f'role:{node}:packet', payload['owner'], agent, spec)['packet']
            result = core.generate(f'role:{node}', payload['owner'], agent, {'question_id': payload['question_id'], 'context_packet_ref': packet,
                                                                          'evidence_bundle_id': payload['bundle'], 'calculation_ids': payload['calculations']})
        except CoreError as error:
            if error.code in INTERRUPTED:
                raise
            return {'done': {node: None}, 'process': [step.done('failed', code=error.code, role=role)], 'warnings': ['role_incomplete']}
        envelope = result['envelope']
        if status(envelope) == 'cancelled':
            raise CoreError('cancelled')
        reference = data(envelope) if status(envelope) == 'completed' else None
        return {'done': {node: reference}, 'analyses': [reference] if reference else [], 'process': [step.done(status(envelope), role=role)],
                'warnings': warnings(envelope) + ([] if reference else ['role_incomplete'])}
    return run


def payload(state: dict[str, Any], task: dict[str, Any]) -> dict[str, Any]:
    return {'task': task, 'owner': state['owner'], 'question_id': state['question_id'], 'bundle': state['bundle'], 'calculations': state.get('calculations') or []}


def synthesize(core: Core, state: Roles) -> dict[str, Any]:
    """The synthesizer's draft (round 0) or its one revision."""
    round_ = state.get('round', 0)
    step = core.step('draft' if round_ == 0 else 'revise')
    try:
        agent = state.get('synth_agent') or core.register('synth:reg', state['owner'], 'synthesizer')
        revising = round_ > 0
        result = core.generate(f'synth:{round_}', state['owner'], agent, {
            'question_id': state['question_id'], 'evidence_bundle_id': state['bundle'], 'calculation_ids': state.get('calculations') or [],
            'draft_ref': state.get('draft') if revising else None, 'validation_report_ref': state.get('report') if revising else None})
    except CoreError as error:
        if error.code in INTERRUPTED or not state.get('draft'):
            raise
        return {'stop': 'revision_stopped', 'process': [step.done('failed', code=error.code)], 'warnings': ['revision_stopped']}
    envelope = result['envelope']
    if status(envelope) == 'cancelled':
        raise CoreError('cancelled')
    next_draft = data(envelope) if status(envelope) == 'completed' else None
    if next_draft is None:
        return {'synth_agent': agent, 'process': [step.done(status(envelope))], 'warnings': warnings(envelope),
                'stop': 'no_draft' if not state.get('draft') else 'revision_stopped'}
    return {'synth_agent': agent, 'draft': next_draft, 'revisions': state.get('revisions', 0) + (1 if round_ > 0 else 0), 'process': [step.done('completed')], 'stop': None}


def check(core: Core, state: Roles) -> dict[str, Any]:
    """One structured challenge from a clean context (no author reasoning)."""
    step = core.step('check')
    try:
        agent = core.register('check:reg', state['owner'], 'evidence-checker')
        packet = core.packet('check:packet', state['owner'], agent, {'kind': 'check', 'evidence_bundle_id': state['bundle'],
                                                                     'calculation_ids': state.get('calculations') or [], 'draft_ref': state['draft']})['packet']
        result = core.generate('check', state['owner'], agent, {'question_id': state['question_id'], 'context_packet_ref': packet, 'evidence_bundle_id': state['bundle'],
                                                                 'calculation_ids': state.get('calculations') or [], 'draft_ref': state['draft']})
    except CoreError as error:
        if error.code in INTERRUPTED:
            raise
        return {'checked': True, 'challenges': [], 'process': [step.done('failed', code=error.code)], 'warnings': ['check_skipped']}
    envelope, view = result['envelope'], result.get('view') or {}
    if status(envelope) == 'cancelled':
        raise CoreError('cancelled')
    challenges = [f"{row.get('claim_id', '')}:{row.get('issue_type', '')}" for row in view.get('challenges') or []]
    return {'checked': True, 'challenge': data(envelope), 'challenges': challenges, 'process': [step.done(status(envelope), challenges=len(challenges))],
            'warnings': warnings(envelope)}


def after_synthesize(state: Roles) -> str:
    return 'end' if state.get('stop') else 'verify'


def after_verify(state: Roles) -> str:
    return 'end' if state.get('checked') else 'check'


def after_check(state: Roles) -> str:
    # One revision at most: the budget is shared with the plan and the roles.
    if (state.get('problems') or state.get('challenges')) and state.get('revisions', 0) < 1:
        return 'revise'
    return 'end'


def add_answering(graph, core: Core, end: str, prefix: str = '') -> None:
    """synthesize → verify → check → (revise → verify) → `end`."""
    graph.add_node('synthesize', lambda state: synthesize(core, state))
    graph.add_node('verify', lambda state: common.verify(core, state, prefix))
    graph.add_node('check', lambda state: check(core, state))
    graph.add_conditional_edges('synthesize', after_synthesize, {'end': end, 'verify': 'verify'})
    graph.add_conditional_edges('verify', after_verify, {'end': end, 'check': 'check'})
    graph.add_conditional_edges('check', after_check, {'end': end, 'revise': 'synthesize'})
