"""PTY-backed terminal bridge for the Tractus control plane."""

from __future__ import annotations

import asyncio
import contextlib
import errno
import fcntl
import json
import os
import pty
import shlex
import signal
import struct
import subprocess
import termios
from dataclasses import dataclass
from pathlib import Path

from fastapi import WebSocket, WebSocketDisconnect


PROJECT_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_DEMO_SHELL = (PROJECT_ROOT / "target" / "debug" / "tractus-shim").resolve()


@dataclass
class PtyProcess:
    """The master side of a child process attached to a pseudo-terminal."""

    master_fd: int
    child: subprocess.Popen[bytes]

    def resize(self, cols: int, rows: int) -> None:
        if cols <= 0 or rows <= 0:
            raise ValueError("terminal dimensions must be positive")
        winsize = struct.pack("HHHH", rows, cols, 0, 0)
        fcntl.ioctl(self.master_fd, termios.TIOCSWINSZ, winsize)

    def close(self) -> None:
        with contextlib.suppress(OSError):
            os.close(self.master_fd)
        if self.child.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                self.child.terminate()
            try:
                self.child.wait(timeout=1)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    self.child.kill()
                with contextlib.suppress(subprocess.TimeoutExpired):
                    self.child.wait(timeout=1)


def demo_command() -> list[str]:
    """Return the demo shell command without interpreting it through a shell."""
    configured = os.environ.get("DEMO_SHELL")
    return shlex.split(configured) if configured else [str(DEFAULT_DEMO_SHELL)]


def spawn_terminal() -> PtyProcess:
    """Start the configured command with stdin/stdout/stderr attached to one PTY."""
    master_fd, slave_fd = pty.openpty()
    try:
        child = subprocess.Popen(
            demo_command(),
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            close_fds=True,
            start_new_session=True,
        )
    except Exception:
        os.close(master_fd)
        raise
    finally:
        os.close(slave_fd)
    return PtyProcess(master_fd=master_fd, child=child)


async def bridge_terminal(websocket: WebSocket) -> None:
    """Bridge browser WebSocket frames to and from a pseudo-terminal."""
    await websocket.accept()
    terminal = spawn_terminal()
    output_task = asyncio.create_task(_forward_output(websocket, terminal.master_fd))
    input_task = asyncio.create_task(_forward_input(websocket, terminal))
    try:
        _done, pending = await asyncio.wait(
            {output_task, input_task}, return_when=asyncio.FIRST_COMPLETED
        )
        for task in pending:
            task.cancel()
        for task in pending:
            with contextlib.suppress(asyncio.CancelledError):
                await task
    finally:
        output_task.cancel()
        input_task.cancel()
        terminal.close()


async def _forward_output(websocket: WebSocket, master_fd: int) -> None:
    while True:
        try:
            data = await asyncio.to_thread(os.read, master_fd, 4096)
        except OSError as error:
            if error.errno in {errno.EIO, errno.EBADF}:
                return
            raise
        if not data:
            return
        await websocket.send_bytes(data)


async def _forward_input(websocket: WebSocket, terminal: PtyProcess) -> None:
    try:
        while True:
            message = await websocket.receive()
            if message["type"] == "websocket.disconnect":
                return
            if data := message.get("bytes"):
                await asyncio.to_thread(os.write, terminal.master_fd, data)
                continue
            text = message.get("text")
            if text is None:
                continue
            if _handle_resize(text, terminal):
                continue
            await asyncio.to_thread(os.write, terminal.master_fd, text.encode("utf-8"))
    except WebSocketDisconnect:
        return


def _handle_resize(text: str, terminal: PtyProcess) -> bool:
    try:
        control = json.loads(text)
    except json.JSONDecodeError:
        return False
    if not isinstance(control, dict) or control.get("type") != "resize":
        return False
    cols = control.get("cols")
    rows = control.get("rows")
    if isinstance(cols, int) and isinstance(rows, int):
        terminal.resize(cols, rows)
    return True
