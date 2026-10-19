"""Native media review (multimodal design 4, 6): the media reader looks at
the approved images (and listens to approved audio) without the local text,
the core compares the key fields with the local OCR/ASR, then the
synthesizer drafts from the local text evidence with the observation as a
candidate, verification, one check, at most one revision. Images and audio
go in separate requests (no joint media input)."""
from __future__ import annotations

from typing import Any

from langgraph.graph import END, START, StateGraph

from ..core import Core, data, status, warnings
from ..rpc import CoreError
from . import roles

VERSION = '1'


class State(roles.Roles, total=False):
    question: str
    images: list[str]
    audio: list[str]
    observations: list[str]
    partial: bool


def initial(inputs: dict[str, Any]) -> State:
    return {'owner': inputs['owner'], 'question_id': inputs['question_id'], 'question': inputs['question'], 'bundle': inputs['evidence_bundle'],
            'images': list(inputs.get('images') or [])[:12], 'audio': list(inputs.get('audio') or [])[:1], 'calculations': [],
            'round': 0, 'revisions': 0, 'process': [], 'warnings': [], 'analyses': [], 'done': {}, 'observations': []}


def build(core: Core) -> StateGraph:
    def observe(state: State) -> dict[str, Any]:
        groups = [('images', state.get('images') or []), ('audio', state.get('audio') or [])]
        if not any(media for _, media in groups):
            return {'warnings': ['native_media_not_sent']}
        out: dict[str, Any] = {'process': [], 'warnings': [], 'observations': []}
        try:
            agent = core.register('reader:reg', state['owner'], 'media-reader')
        except CoreError as error:
            if error.code in roles.INTERRUPTED:
                raise
            return {'process': [core.step('observe').done('failed', code=error.code)], 'warnings': ['native_media_not_sent']}
        for name, media in groups:
            if not media:
                continue
            step = core.step('observe')
            try:
                result = core.generate(f'observe:{name}', state['owner'], agent, {'media_refs': media, 'question_id': state['question_id'], 'observation_mode': 'first_read'})
            except CoreError as error:
                if error.code in roles.INTERRUPTED:
                    raise
                out['process'].append(step.done('failed', code=error.code))
                out['warnings'].append('native_media_not_sent')
                continue
            envelope, view = result['envelope'], result.get('view') or {}
            if status(envelope) == 'cancelled':
                raise CoreError('cancelled')
            if data(envelope):
                out['observations'].append(data(envelope))
            out['process'].append(step.done(status(envelope), observations=int(view.get('observations', 0))))
            out['warnings'] += warnings(envelope)
        return out

    def after_observe(state: State) -> str:
        return 'synthesize' if state.get('bundle') else 'insufficient'

    def insufficient(state: State) -> dict[str, Any]:
        return {'warnings': ['insufficient_evidence'], 'partial': True}

    graph = StateGraph(State)
    graph.add_node('observe', observe)
    graph.add_node('insufficient', insufficient)
    graph.add_edge(START, 'observe')
    graph.add_conditional_edges('observe', after_observe, {'synthesize': 'synthesize', 'insufficient': 'insufficient'})
    graph.add_edge('insufficient', END)
    roles.add_answering(graph, core, END)
    return graph


def output(state: State) -> dict[str, Any]:
    return {'evidence_bundle': state.get('bundle'), 'draft': state.get('draft'), 'report': state.get('report'), 'revisions': state.get('revisions', 0),
            'partial': bool(state.get('partial')), 'observations': state.get('observations', []), 'challenge': state.get('challenge'),
            'process': state.get('process', []), 'warnings': state.get('warnings', [])}
