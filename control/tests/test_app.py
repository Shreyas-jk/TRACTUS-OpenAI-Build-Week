import asyncio

from fastapi.testclient import TestClient

from control.app import create_app
from control.intent import ContractSpec, Operation


class FakeDaemon:
    def __init__(self) -> None:
        self.contracts = []
        self.resolutions = []

    async def set_contract(self, contract):
        self.contracts.append(contract)
        return {"action": "set"}

    async def resolve(self, command_id, decision):
        self.resolutions.append((command_id, decision))
        return {"id": command_id, "resolved": True}

    async def subscribe(self):
        yield {
            "type": "blocked",
            "id": "c17",
            "cmd": "cargo add axios",
            "proofs": [{"clause": "deps_may_change = false"}],
        }
        await asyncio.sleep(0.01)


async def fake_extractor(request: str) -> ContractSpec:
    return ContractSpec(
        task=request,
        allowed_paths=["src/api/**", "target/**", "node_modules/**", "**/__pycache__/**", ".venv/**"],
        allowed_ops=[Operation.READ, Operation.EDIT, Operation.TEST, Operation.BUILD],
        deps_may_change=False,
        git_ops=[],
        network=False,
    )


async def fake_explainer(task: str, command: str, evidence) -> str:
    return "The command changes dependencies outside the approved task."


def test_task_returns_toggle_card_and_websocket_bridges_events() -> None:
    daemon = FakeDaemon()
    app = create_app(daemon=daemon, extractor=fake_extractor, explainer=fake_explainer)

    with TestClient(app) as client:
        response = client.post("/task", json={"request": "Fix the API test"})
        assert response.status_code == 200
        card = response.json()
        path_rows = next(group for group in card["groups"] if group["id"] == "paths")["clauses"]
        operation_rows = next(group for group in card["groups"] if group["id"] == "operations")["clauses"]
        assert any(row["value"] == "target/**" and row["de_emphasized"] for row in path_rows)
        assert any(row["value"] == "build" and row["de_emphasized"] for row in operation_rows)

        confirm = client.post("/contract/confirm", json={"contract": card["contract"]})
        assert confirm.status_code == 200
        assert daemon.contracts[-1]["allowed_ops"] == 51

        with client.websocket_connect("/events") as websocket:
            event = websocket.receive_json()
            explanation_update = websocket.receive_json()

    assert event["type"] == "blocked"
    assert explanation_update["explanation"] == "The command changes dependencies outside the approved task."


def test_static_xterm_assets_are_served() -> None:
    app = create_app(daemon=FakeDaemon(), extractor=fake_extractor, explainer=fake_explainer)

    with TestClient(app) as client:
        page = client.get("/")
        xterm = client.get("/static/vendor/xterm.js")
        fit = client.get("/static/vendor/xterm-addon-fit.js")
        stylesheet = client.get("/static/vendor/xterm.css")

    assert page.status_code == 200
    assert "/static/vendor/xterm.js" in page.text
    assert xterm.status_code == fit.status_code == stylesheet.status_code == 200
