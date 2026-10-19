"""Checkpoints mirrored to the core.

LangGraph's in-memory saver keeps the working copy; every checkpoint, channel
blob and pending write is also sent to the core, which stores the opaque
serialised bytes in the encrypted vault. A continued run loads them back.
The serializer is strict msgpack: no pickle fallback, only LangGraph's safe
types and plain JSON-like values are revived.
"""
from __future__ import annotations

import base64
from typing import Any, Sequence

from langgraph.checkpoint.memory import InMemorySaver
from langgraph.checkpoint.serde.jsonplus import JsonPlusSerializer

from .rpc import Peer


def _pack(typed: tuple[str, bytes]) -> list[str]:
    return [typed[0], base64.b64encode(typed[1]).decode('ascii')]


def _unpack(value: list[str]) -> tuple[str, bytes]:
    return value[0], base64.b64decode(value[1])


class CoreSaver(InMemorySaver):
    def __init__(self, peer: Peer, thread_id: str):
        super().__init__(serde=JsonPlusSerializer(pickle_fallback=False, allowed_msgpack_modules=None))
        self._peer = peer
        self._thread = thread_id

    def load(self) -> int:
        """Restore everything the core holds for this run; returns the checkpoint count."""
        stored = self._peer.call('checkpoint.load', {})
        for row in stored['checkpoints']:
            self.storage[self._thread][row['ns']][row['id']] = (_unpack(row['checkpoint']), _unpack(row['metadata']), row['parent'])
        for row in stored['blobs']:
            self.blobs[(self._thread, row['ns'], row['channel'], row['version'])] = _unpack(row['value'])
        for row in stored['writes']:
            key = (self._thread, row['ns'], row['checkpoint_id'])
            self.writes.setdefault(key, {})[(row['task_id'], row['idx'])] = (row['task_id'], row['channel'], _unpack(row['value']), row['task_path'])
        return len(stored['checkpoints'])

    def put(self, config, checkpoint, metadata, new_versions):
        saved = super().put(config, checkpoint, metadata, new_versions)
        ns = config['configurable']['checkpoint_ns']
        entry = self.storage[self._thread][ns][checkpoint['id']]
        blobs = [{'channel': channel, 'version': version, 'value': _pack(self.blobs[(self._thread, ns, channel, version)])} for channel, version in new_versions.items()]
        self._peer.call('checkpoint.put', {'ns': ns, 'id': checkpoint['id'], 'parent': entry[2], 'checkpoint': _pack(entry[0]), 'metadata': _pack(entry[1]), 'blobs': blobs})
        return saved

    def put_writes(self, config, writes: Sequence[tuple[str, Any]], task_id: str, task_path: str = '') -> None:
        super().put_writes(config, writes, task_id, task_path)
        ns = config['configurable'].get('checkpoint_ns', '')
        checkpoint_id = config['configurable']['checkpoint_id']
        stored = self.writes.get((self._thread, ns, checkpoint_id), {})
        rows = [{'idx': idx, 'channel': value[1], 'value': _pack(value[2])} for (owner, idx), value in stored.items() if owner == task_id]
        if rows:
            self._peer.call('checkpoint.put_writes', {'ns': ns, 'checkpoint_id': checkpoint_id, 'task_id': task_id, 'task_path': task_path, 'writes': rows})
