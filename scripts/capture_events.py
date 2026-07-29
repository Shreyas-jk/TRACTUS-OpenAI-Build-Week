#!/usr/bin/env python3
"""Capture the tractus-console /events stream while driving a real block.

Stdlib only. Opens a WebSocket to the console (which subscribes to the daemon),
then sends `set_contract` + a blocking `propose` straight to the daemon socket,
and prints every event frame — including the console's gpt-5.6-luna enrichment
that is attached to each block. Useful for demoing or verifying the advisory
explainer without a browser.

Usage:
    # start a daemon (short socket path) and the console pointed at it, e.g.
    #   TRACTUS_SOCK=/tmp/tractus-demo.sock tractusd &
    #   OPENAI_API_KEY=... tractus-console --sock /tmp/tractus-demo.sock &
    python3 scripts/capture_events.py <console_port> <daemon_socket> <workspace_dir>

Example:
    python3 scripts/capture_events.py 8787 /tmp/tractus-demo.sock "$PWD"
"""

import base64
import json
import os
import socket
import struct
import sys
import threading
import time

if len(sys.argv) != 4:
    sys.exit(f"usage: {sys.argv[0]} <console_port> <daemon_socket> <workspace_dir>")

HOST = "127.0.0.1"
PORT = int(sys.argv[1])
DAEMON_SOCK = sys.argv[2]
WORKSPACE = sys.argv[3]


def ws_connect(host, port, path):
    sock = socket.create_connection((host, port), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    sock.sendall(request.encode())
    buffer = b""
    while b"\r\n\r\n" not in buffer:
        buffer += sock.recv(4096)
    assert b"101" in buffer.split(b"\r\n", 1)[0], buffer[:80]
    return sock


def recv_exact(sock, count):
    data = b""
    while len(data) < count:
        chunk = sock.recv(count - len(data))
        if not chunk:
            raise ConnectionError("socket closed")
        data += chunk
    return data


def read_frame(sock):
    first, second = recv_exact(sock, 2)
    opcode = first & 0x0F
    length = second & 0x7F
    if length == 126:
        length = struct.unpack(">H", recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack(">Q", recv_exact(sock, 8))[0]
    payload = recv_exact(sock, length) if length else b""
    return opcode, payload


def reader(sock, stop, seen):
    sock.settimeout(1.0)
    while not stop.is_set():
        try:
            opcode, payload = read_frame(sock)
        except (socket.timeout, TimeoutError):
            continue
        except Exception:
            return
        if opcode == 0x8:  # close
            return
        if opcode != 0x1:  # only text frames carry events
            continue
        try:
            event = json.loads(payload.decode("utf-8", "replace"))
        except ValueError:
            continue
        tag = event.get("type", "?")
        if event.get("explanation"):
            seen.append(event)
            print(f"\n  * ENRICHED [{tag}] explainer={event.get('explainer_model')}")
            print(f"    explanation: {event['explanation']}")
        else:
            extra = ""
            if tag == "blocked":
                proofs = event.get("proofs") or []
                extra = " proofs=" + ",".join(p.get("rule", "?") for p in proofs)
            print(f"  . event [{tag}]{extra}")


def daemon_line(sock_path, message):
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(sock_path)
    sock.sendall((json.dumps(message) + "\n").encode())
    line = b""
    while not line.endswith(b"\n"):
        line += sock.recv(4096)
    sock.close()
    return json.loads(line)


def main():
    print(f"-> WS connecting to ws://{HOST}:{PORT}/events")
    ws = ws_connect(HOST, PORT, "/events")
    stop = threading.Event()
    seen = []
    thread = threading.Thread(target=reader, args=(ws, stop, seen), daemon=True)
    thread.start()
    time.sleep(0.8)  # let the console subscribe to the daemon

    # read|edit|test|build = 1|2|16|32 = 51; no deps, no network -> cargo add blocks.
    contract = {
        "task": "Fix the failing test in tests/api_test.rs without changing dependencies.",
        "allowed_paths": ["src/**", "tests/**"],
        "allowed_ops": 51,
        "deps_may_change": False,
        "git_ops": 0,
        "network": False,
    }
    ack = daemon_line(DAEMON_SOCK, {"type": "set_contract", "contract": contract})
    print("-> set_contract:", ack.get("action"))
    time.sleep(0.3)

    verdict = daemon_line(
        DAEMON_SOCK,
        {
            "type": "propose",
            "id": "blk-1",
            "cmd": "cargo add axios@1.6",
            "cwd": WORKSPACE,
            "agent_session": "demo",
        },
    )
    print(f"-> propose 'cargo add axios@1.6' -> verdict: {verdict.get('action')}")

    deadline = time.time() + 10
    while time.time() < deadline and not seen:
        time.sleep(0.2)
    stop.set()
    time.sleep(0.2)
    if seen:
        print("\nOK: gpt-5.6-luna explanation received.")
    else:
        print("\nNo enriched explanation (check EXPLAIN_MODEL / OpenAI quota).")
        sys.exit(1)


if __name__ == "__main__":
    main()
