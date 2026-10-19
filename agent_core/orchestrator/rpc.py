"""JSON-RPC 2.0 peer over the inherited pipes: one UTF-8 JSON message per line.

The core is the only peer. It sends exactly one request (``flow.run``); every
other request goes from this process to the core. A reader thread routes the
core's responses to waiting callers, so parallel graph branches may call the
core at the same time; writes are serialised.
"""
from __future__ import annotations

import itertools
import json
import threading
from typing import Any, BinaryIO

MAX_LINE = 20 * 1024 * 1024


class CoreError(Exception):
    """A stable error code from the core (never document text)."""

    def __init__(self, code: str, data: dict | None = None):
        super().__init__(code)
        self.code = code
        self.data = data or {}


class Peer:
    def __init__(self, reader: BinaryIO, writer: BinaryIO):
        self._reader = reader
        self._writer = writer
        self._write_lock = threading.Lock()
        self._state = threading.Condition()
        self._pending: dict[str, dict[str, Any]] = {}
        self._incoming: list[dict[str, Any]] = []
        self._closed = False
        self._ids = itertools.count(1)
        threading.Thread(target=self._read, name='rpc-reader', daemon=True).start()

    def _send(self, message: dict[str, Any]) -> None:
        data = json.dumps(message, ensure_ascii=False, separators=(',', ':')).encode('utf-8') + b'\n'
        if len(data) > MAX_LINE:
            raise CoreError('message_too_large')
        with self._write_lock:
            self._writer.write(data)
            self._writer.flush()

    def notify(self, method: str, params: dict[str, Any]) -> None:
        self._send({'jsonrpc': '2.0', 'method': method, 'params': params})

    def call(self, method: str, params: dict[str, Any]) -> Any:
        ident = f'o{next(self._ids)}'
        slot: dict[str, Any] = {}
        with self._state:
            if self._closed:
                raise CoreError('channel_closed')
            self._pending[ident] = slot
        self._send({'jsonrpc': '2.0', 'id': ident, 'method': method, 'params': params})
        with self._state:
            while not slot and not self._closed:
                self._state.wait()
            self._pending.pop(ident, None)
        if 'error' in slot:
            error = slot['error'] if isinstance(slot['error'], dict) else {}
            raise CoreError(str(error.get('message') or 'failed'), error.get('data') if isinstance(error.get('data'), dict) else None)
        if 'result' not in slot:
            raise CoreError('channel_closed')
        return slot['result']

    def respond(self, ident: Any, result: Any = None, error: dict[str, Any] | None = None) -> None:
        message: dict[str, Any] = {'jsonrpc': '2.0', 'id': ident}
        if error is None:
            message['result'] = result
        else:
            message['error'] = error
        self._send(message)

    def next_request(self) -> dict[str, Any] | None:
        """The next request from the core, or None once the channel closed."""
        with self._state:
            while not self._incoming and not self._closed:
                self._state.wait()
            return self._incoming.pop(0) if self._incoming else None

    def _read(self) -> None:
        try:
            while True:
                line = self._reader.readline(MAX_LINE + 1)
                if not line or len(line) > MAX_LINE or not line.endswith(b'\n'):
                    break
                message = json.loads(line)
                if not isinstance(message, dict) or message.get('jsonrpc') != '2.0':
                    break
                with self._state:
                    if 'method' in message and 'id' in message:
                        self._incoming.append(message)
                    elif 'id' in message and str(message['id']) in self._pending:
                        slot = self._pending[str(message['id'])]
                        if 'error' in message:
                            slot['error'] = message['error']
                        else:
                            slot['result'] = message.get('result')
                    else:
                        break
                    self._state.notify_all()
        except (OSError, ValueError):
            pass
        with self._state:
            self._closed = True
            self._state.notify_all()
