import asyncio
import json
import os
from pathlib import Path

import pytest

from control.daemon import DaemonClient, default_socket_path


def test_default_socket_prefers_the_current_workspace_contract_store(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.delenv("TRACTUS_SOCK", raising=False)
    monkeypatch.delenv("TRACTUS_WORKSPACE_ROOT", raising=False)
    (tmp_path / ".tractus").mkdir()
    monkeypatch.chdir(tmp_path)

    assert default_socket_path() == tmp_path / ".tractus" / "chaosd.sock"


def test_default_socket_honors_an_explicit_workspace_root(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    workspace = tmp_path / "project"
    monkeypatch.delenv("TRACTUS_SOCK", raising=False)
    monkeypatch.setenv("TRACTUS_WORKSPACE_ROOT", str(workspace))

    assert default_socket_path() == workspace / ".tractus" / "chaosd.sock"


@pytest.mark.asyncio
async def test_client_sends_requests_and_streams_subscription() -> None:
    # macOS limits UDS paths to 104 bytes, shorter than pytest's temp hierarchy.
    socket = Path(f"/tmp/ct-{os.getpid()}-daemon.sock")
    socket.unlink(missing_ok=True)

    async def echo(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        request = json.loads(await reader.readline())
        if request["type"] == "subscribe":
            writer.write(b'{"type":"blocked","id":"c17"}\n')
        else:
            writer.write(json.dumps({"type": request["type"], "ok": True}).encode() + b"\n")
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    server = await asyncio.start_unix_server(echo, path=str(socket))
    try:
        client = DaemonClient(socket)
        contract_reply = await client.set_contract({"task": "test"})
        resolve_reply = await client.resolve("c17", "approve_once")
        events = [event async for event in client.subscribe()]
    finally:
        server.close()
        await server.wait_closed()
        socket.unlink(missing_ok=True)

    assert contract_reply == {"type": "set_contract", "ok": True}
    assert resolve_reply == {"type": "resolve", "ok": True}
    assert events == [{"type": "blocked", "id": "c17"}]
