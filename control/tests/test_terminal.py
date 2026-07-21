from fastapi.testclient import TestClient

from control.app import create_app


def test_terminal_bridges_pty_input_and_output(monkeypatch) -> None:
    monkeypatch.setenv("DEMO_SHELL", "/bin/sh")
    app = create_app()

    with TestClient(app) as client:
        with client.websocket_connect("/terminal") as terminal:
            terminal.send_text("echo hi\n")
            output = bytearray()
            for _ in range(4):
                output.extend(terminal.receive_bytes())
                if b"hi" in output:
                    break

    assert b"hi" in output
