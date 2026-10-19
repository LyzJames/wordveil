"""Typed calls into the core for one flow run.

Every state-changing call carries an idempotency key derived from the graph
state, so a node replayed after an interruption gets the core's recorded
receipt instead of repeating work. Keys never contain document text.
"""
from __future__ import annotations

import re
import time
from typing import Any

from .rpc import CoreError, Peer

KEY = re.compile(r'^[A-Za-z0-9:._-]{1,160}$')


class Core:
    def __init__(self, peer: Peer, attempt: int):
        self.peer = peer
        # Continued runs may retry a step whose earlier outcome is unknown.
        self.attempt = attempt

    def _keyed(self, method: str, key: str, params: dict[str, Any]) -> Any:
        if not KEY.match(key):
            raise CoreError('invalid_key')
        try:
            return self.peer.call(method, {'key': key, **params})
        except CoreError as error:
            if error.code != 'outcome_unknown' or self.attempt <= 1:
                raise
            # The user confirmed this continuation: one new request in budget.
            return self.peer.call(method, {'key': f'{key}:r{self.attempt}', **params})

    def open(self, key: str, parent: str, skill: str, payload: dict[str, Any], finalizer: bool = False) -> str:
        return self._keyed('invocation.open', key, {'parent': parent, 'skill': skill, 'input': payload, 'finalizer': finalizer})['owner']

    def close(self, key: str, owner: str, output: dict[str, Any]) -> dict[str, Any]:
        return self._keyed('invocation.close', key, {'owner': owner, 'output': output})

    def skill(self, key: str, owner: str, skill: str, payload: dict[str, Any]) -> dict[str, Any]:
        return self._keyed('skill.invoke', key, {'owner': owner, 'skill': skill, 'input': payload})

    def register(self, key: str, owner: str, role: str) -> str:
        return self._keyed('role.register', key, {'owner': owner, 'role': role})['agent']

    def packet(self, key: str, owner: str, agent: str, spec: dict[str, Any]) -> dict[str, Any]:
        return self._keyed('context.packet', key, {'owner': owner, 'agent': agent, 'spec': spec})

    def generate(self, key: str, owner: str, agent: str, payload: dict[str, Any]) -> dict[str, Any]:
        return self._keyed('role.generate', key, {'owner': owner, 'agent': agent, 'input': payload})

    def tool(self, key: str, owner: str, agent: str, skill: str, payload: dict[str, Any]) -> dict[str, Any]:
        return self._keyed('role.tool', key, {'owner': owner, 'agent': agent, 'skill': skill, 'input': payload})

    def progress(self, node: str, status: str, **detail: Any) -> None:
        self.peer.notify('progress', {'node': node, 'status': status, 'detail': detail})

    def step(self, name: str) -> 'Step':
        self.progress(name, 'running')
        return Step(name)


def status(envelope: dict[str, Any]) -> str:
    return str(envelope.get('status') or 'failed')


def data(envelope: dict[str, Any]) -> str | None:
    value = envelope.get('data_ref')
    return value if isinstance(value, str) else None


def warnings(envelope: dict[str, Any]) -> list[str]:
    return [str(row.get('code')) for row in envelope.get('warnings') or [] if isinstance(row, dict) and row.get('code')]


class Step:
    """Timing and status of one process step shown to the user."""

    def __init__(self, name: str):
        self.name = name
        self.started = time.monotonic()

    def done(self, status_: str, **detail: Any) -> dict[str, Any]:
        return {'step': self.name, 'status': status_, 'ms': int((time.monotonic() - self.started) * 1000), **detail}
