"""FastAPI control plane and the minimal Ghost UI host."""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any, Literal

from fastapi import FastAPI, HTTPException, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse
from pydantic import BaseModel

from .daemon import DaemonClient
from .explain import explain_divergence
from .intent import ARTIFACT_PATHS, ContractSpec, GitOperation, Operation, extract_intent
from .terminal import bridge_terminal


STATIC_INDEX = Path(__file__).parent / "static" / "index.html"


class TaskRequest(BaseModel):
    request: str


class ResolveRequest(BaseModel):
    id: str
    decision: Literal["approve_once", "reject"]


IntentExtractor = Callable[[str], Awaitable[ContractSpec]]
Explainer = Callable[[str, str, Any], Awaitable[str]]


def create_app(
    daemon: DaemonClient | Any | None = None,
    extractor: IntentExtractor | None = None,
    explainer: Explainer | None = None,
) -> FastAPI:
    """Create an injectable app so the test suite never needs a daemon or API key."""
    daemon = daemon or DaemonClient()
    extractor = extractor or extract_intent
    explainer = explainer or explain_divergence
    active_task = "the approved task"
    app = FastAPI(title="Chaos Twin Control Plane")

    @app.get("/", include_in_schema=False)
    async def index() -> FileResponse:
        return FileResponse(STATIC_INDEX)

    @app.post("/task")
    async def task(payload: TaskRequest) -> dict[str, Any]:
        contract = await extractor(payload.request)
        return toggle_card(contract)

    @app.post("/contract/confirm")
    async def confirm_contract(payload: dict[str, Any]) -> dict[str, Any]:
        nonlocal active_task
        raw_contract = payload.get("contract", payload)
        try:
            contract = ContractSpec.model_validate(raw_contract)
        except Exception as error:
            raise HTTPException(status_code=422, detail="invalid contract") from error
        acknowledgement = await daemon.set_contract(contract.daemon_wire())
        active_task = contract.task
        return {"contract": contract.model_dump(mode="json"), "daemon": acknowledgement}

    @app.post("/resolve")
    async def resolve(payload: ResolveRequest) -> dict[str, Any]:
        return await daemon.resolve(payload.id, payload.decision)

    @app.websocket("/events")
    async def events(websocket: WebSocket) -> None:
        await websocket.accept()
        tasks: set[asyncio.Task[None]] = set()
        try:
            async for event in daemon.subscribe():
                await websocket.send_json(event)
                if event.get("type") == "blocked":
                    task = asyncio.create_task(
                        _send_explanation(websocket, event, explainer, active_task)
                    )
                    tasks.add(task)
                    task.add_done_callback(tasks.discard)
        except WebSocketDisconnect:
            pass
        finally:
            for task in tasks:
                task.cancel()

    @app.websocket("/terminal")
    async def terminal(websocket: WebSocket) -> None:
        await bridge_terminal(websocket)

    return app


async def _send_explanation(
    websocket: WebSocket,
    event: dict[str, Any],
    explainer: Explainer,
    active_task: str,
) -> None:
    """Attach an advisory sentence later; never block the daemon event bridge."""
    sentence = await explainer(
        str(event.get("task") or active_task),
        str(event.get("cmd", "the command")),
        event.get("proofs") or event.get("twin_diff") or event.get("reason"),
    )
    try:
        await websocket.send_json({**event, "explanation": sentence, "event_update": True})
    except RuntimeError:
        # The browser can disconnect before the advisory call completes.
        pass


def toggle_card(contract: ContractSpec) -> dict[str, Any]:
    """Render the user-facing, plain-language representation of a contract."""
    implied_ops = {Operation.TEST, Operation.BUILD} if Operation.EDIT in contract.allowed_ops else set()
    return {
        "contract": contract.model_dump(mode="json"),
        "groups": [
            {
                "id": "paths",
                "label": "May edit files in",
                "clauses": [
                    {
                        "kind": "path",
                        "value": path,
                        "enabled": True,
                        "de_emphasized": path in ARTIFACT_PATHS,
                    }
                    for path in contract.allowed_paths
                ],
            },
            {
                "id": "operations",
                "label": "May perform",
                "clauses": [
                    {
                        "kind": "operation",
                        "value": operation.value,
                        "enabled": operation in contract.allowed_ops,
                        "de_emphasized": operation in implied_ops,
                    }
                    for operation in Operation
                ],
            },
            {
                "id": "dependencies",
                "label": "May change dependencies",
                "clauses": [
                    {
                        "kind": "deps_may_change",
                        "value": contract.deps_may_change,
                        "enabled": contract.deps_may_change,
                        "de_emphasized": False,
                    }
                ],
            },
            {
                "id": "git",
                "label": "Git permissions",
                "clauses": [
                    {
                        "kind": "git",
                        "value": operation.value,
                        "enabled": operation in contract.git_ops,
                        "de_emphasized": False,
                    }
                    for operation in GitOperation
                ],
            },
            {
                "id": "network",
                "label": "May access network",
                "clauses": [
                    {
                        "kind": "network",
                        "value": contract.network,
                        "enabled": contract.network,
                        "de_emphasized": False,
                    }
                ],
            },
        ],
    }


app = create_app()
