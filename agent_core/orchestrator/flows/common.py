"""Steps shared by the flows: fixed hybrid retrieval and the bounded
draft → verify → revise loop (at most two revisions; root limits stay in
the core)."""
from __future__ import annotations

import operator
from typing import Annotated, Any, TypedDict

from ..core import Core, data, status, warnings
from ..rpc import CoreError

MAX_REVISIONS = 2


class Answering(TypedDict, total=False):
    owner: str
    question_id: str
    bundle: str | None
    evidence: list[str]
    calculations: list[str]
    draft: str | None
    report: str | None
    problems: list[str]
    previous: list[str] | None
    round: int
    revisions: int
    stop: str | None
    process: Annotated[list[dict[str, Any]], operator.add]
    warnings: Annotated[list[str], operator.add]


def retrieve(core: Core, state: dict[str, Any], prefix: str = '') -> dict[str, Any]:
    """knowledge-search (q0 first) → evidence-rerank → evidence-read."""
    owner, question = state['owner'], state['question_id']
    out: dict[str, Any] = {'process': [], 'warnings': [], 'evidence': [], 'bundle': None, 'read': None, 'candidate_set': None}
    step = core.step('search')
    result = core.skill(f'{prefix}search', owner, 'knowledge-search', {'queries': state['queries'], 'filter_ids': [], 'snapshot_id': state['snapshot_id']})
    search, view = result['envelope'], result.get('view') or {}
    if status(search) != 'completed':
        out['warnings'] += warnings(search)
        if status(search) != 'partial':
            out['partial'] = True
    out['candidate_set'] = data(search)
    out['process'].append(step.done(status(search), queries=len(state['queries']), candidates=int(view.get('candidates', 0))))
    if out['candidate_set'] and view.get('graph_enabled') is True:
        step = core.step('graph')
        expanded = core.skill(f'{prefix}graph', owner, 'graph-relations', {
            'seed_ids': [], 'relation_types': [], 'max_hops': 2,
            'candidate_set_id': out['candidate_set'], 'snapshot_id': state['snapshot_id'],
        })
        graph_envelope, graph_view = expanded['envelope'], expanded.get('view') or {}
        if isinstance(graph_view.get('candidate_set'), str):
            out['candidate_set'] = graph_view['candidate_set']
        out['warnings'] += warnings(graph_envelope)
        out['process'].append(step.done(status(graph_envelope), candidates=int(graph_view.get('added', 0))))
    if out['candidate_set']:
        step = core.step('rerank')
        reranked = core.skill(f'{prefix}rerank', owner, 'evidence-rerank', {'query_ref': question, 'candidate_set_id': out['candidate_set'], 'facet_ids': []})['envelope']
        out['bundle'] = data(reranked)
        out['evidence'] = [ref for ref in reranked.get('evidence_refs') or [] if isinstance(ref, str)]
        if status(reranked) != 'completed':
            out['partial'] = True
            out['warnings'] += warnings(reranked)
        out['process'].append(step.done(status(reranked), evidence=len(out['evidence'])))
        if out['evidence']:
            reading = core.skill(f'{prefix}read', owner, 'evidence-read', {'evidence_ids': out['evidence'], 'context_mode': 'exact'})['envelope']
            out['read'] = data(reading)
    return out


def compose(core: Core, state: Answering, prefix: str = '') -> dict[str, Any]:
    """One draft (round 0) or revision; the core counts answer versions."""
    round_ = state.get('round', 0)
    step = core.step('draft' if round_ == 0 else 'revise')
    payload = {'question_id': state['question_id'], 'evidence_bundle_id': state['bundle'], 'calculation_ids': state.get('calculations') or [],
               'draft_ref': state.get('draft'), 'validation_report_ref': state.get('report')}
    try:
        envelope = core.skill(f'{prefix}compose:{round_}', state['owner'], 'answer-compose', payload)['envelope']
    except Exception as error:
        code = getattr(error, 'code', 'failed')
        if code in ('cancelled', 'channel_closed') or not state.get('draft'):
            raise
        return {'stop': 'revision_stopped', 'process': [step.done('failed', code=code)], 'warnings': ['revision_stopped']}
    if status(envelope) == 'cancelled':
        raise CoreError('cancelled')
    next_draft = data(envelope) if status(envelope) == 'completed' else None
    if next_draft is None:
        out = {'process': [step.done(status(envelope))], 'warnings': warnings(envelope)}
        out['stop'] = 'no_draft' if not state.get('draft') else 'revision_stopped'
        return out
    return {'draft': next_draft, 'revisions': state.get('revisions', 0) + (1 if round_ > 0 else 0), 'process': [step.done('completed')], 'stop': None}


def verify(core: Core, state: Answering, prefix: str = '') -> dict[str, Any]:
    round_ = state.get('round', 0)
    step = core.step('verify')
    result = core.skill(f'{prefix}verify:{round_}', state['owner'], 'answer-grounding',
                        {'question_id': state['question_id'], 'draft_ref': state['draft'], 'evidence_bundle_id': state['bundle'], 'calculation_ids': state.get('calculations') or []})
    envelope, view = result['envelope'], result.get('view') or {}
    report = data(envelope)
    problems = [f"{row.get('claim_id', '')}:{row.get('reason', '')}" for row in view.get('required_actions') or []]
    out = {'process': [step.done(status(envelope), supported=int(view.get('supported', 0)), problems=len(problems))], 'problems': problems,
           'previous': state.get('problems'), 'round': round_ + 1}
    if report:
        out['report'] = report
    else:
        out['warnings'] = warnings(envelope)
    return out


def after_compose(state: Answering) -> str:
    return 'end' if state.get('stop') else 'verify'


def after_verify(state: Answering) -> str:
    problems = state.get('problems') or []
    if not problems or state.get('round', 0) > MAX_REVISIONS or state.get('previous') == problems:
        return 'end'
    return 'compose'
