"""Async JSON-lines client for the chaosd Unix-domain-socket protocol."""

from __future__ import annotations

import asyncio
import json
import os
from collections.abc import AsyncIterator, Mapping
from pathlib import Path
from typing import Any


class DaemonProtocolError(RuntimeError):
    """chaosd returned an incomplete or malformed JSON-lines response."""


def default_socket_path() -> Path:
    """Match ct-shim's socket selection exactly."""
    configured = os.environ.get("CHAOSTWIN_SOCK")
    if configured:
        return Path(configured)
    runtime_dir = os.environ.get("XDG_RUNTIME_DIR")
    if runtime_dir:
        return Path(runtime_dir) / "chaostwin.sock"
    return Path(f"/tmp/chaostwin-{os.environ.get('UID', '0')}.sock")


class DaemonClient:
    """One request per connection, except for the long-lived subscribe stream."""

    def __init__(self, socket_path: str | Path | None = None) -> None:
        self.socket_path = Path(socket_path) if socket_path is not None else default_socket_path()

    async def set_contract(self, contract: Mapping[str, Any]) -> dict[str, Any]:
        return await self._request({"type": "set_contract", "contract": dict(contract)})

    async def resolve(self, command_id: str, decision: str) -> dict[str, Any]:
        return await self._request(
            {"type": "resolve", "id": command_id, "decision": decision}
        )

    async def subscribe(self) -> AsyncIterator[dict[str, Any]]:
        reader, writer = await asyncio.open_unix_connection(str(self.socket_path))
        try:
            await _write_json(writer, {"type": "subscribe"})
            while line := await reader.readline():
                yield _decode_json(line)
        finally:
            writer.close()
            await writer.wait_closed()

    async def _request(self, message: Mapping[str, Any]) -> dict[str, Any]:
        reader, writer = await asyncio.open_unix_connection(str(self.socket_path))
        try:
            await _write_json(writer, message)
            line = await reader.readline()
            if not line:
                raise DaemonProtocolError("chaosd closed the connection without responding")
            return _decode_json(line)
        finally:
            writer.close()
            await writer.wait_closed()


async def _write_json(writer: asyncio.StreamWriter, value: Mapping[str, Any]) -> None:
    writer.write(json.dumps(value, separators=(",", ":")).encode("utf-8") + b"\n")
    await writer.drain()


def _decode_json(line: bytes) -> dict[str, Any]:
    try:
        value = json.loads(line)
    except (TypeError, ValueError) as error:
        raise DaemonProtocolError("chaosd sent malformed JSON") from error
    if not isinstance(value, dict):
        raise DaemonProtocolError("chaosd JSON-lines messages must be objects")
    return value
