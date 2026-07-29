"""Async JSON-lines client for the tractusd Unix-domain-socket protocol."""

from __future__ import annotations

import asyncio
import json
import os
from collections.abc import AsyncIterator, Mapping
from pathlib import Path
from typing import Any


class DaemonProtocolError(RuntimeError):
    """tractusd returned an incomplete or malformed JSON-lines response."""


def default_socket_path() -> Path:
    """Find the workspace daemon first, then retain the legacy shared default."""
    configured = os.environ.get("TRACTUS_SOCK")
    if configured:
        return Path(configured)
    workspace_root = os.environ.get("TRACTUS_WORKSPACE_ROOT")
    if workspace_root:
        return Path(workspace_root) / ".tractus" / "tractusd.sock"
    # `tractus codex` owns a daemon per workspace. When the dashboard is
    # started from that repository, its contract directory is sufficient to
    # select the same socket without making the presenter export an env var.
    local_store = Path.cwd() / ".tractus"
    if local_store.is_dir():
        return local_store / "tractusd.sock"
    runtime_dir = os.environ.get("XDG_RUNTIME_DIR")
    if runtime_dir:
        return Path(runtime_dir) / "tractus.sock"
    return Path(f"/tmp/tractus-{os.getuid()}.sock")


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
                raise DaemonProtocolError("tractusd closed the connection without responding")
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
        raise DaemonProtocolError("tractusd sent malformed JSON") from error
    if not isinstance(value, dict):
        raise DaemonProtocolError("tractusd JSON-lines messages must be objects")
    return value
