"""Structured intent extraction and defensive contract normalization."""

from __future__ import annotations

import os
import re
from enum import Enum
from typing import Any

from openai import AsyncOpenAI
from pydantic import BaseModel, Field


class Operation(str, Enum):
    READ = "read"
    EDIT = "edit"
    CREATE = "create"
    DELETE = "delete"
    TEST = "test"
    BUILD = "build"
    RUN = "run"


class GitOperation(str, Enum):
    STATUS = "status"
    DIFF = "diff"
    LOG = "log"
    ADD = "add"
    COMMIT = "commit"
    CHECKOUT = "checkout"
    PUSH = "push"
    FORCE_PUSH = "force_push"
    RESET_HARD = "reset_hard"
    CLEAN = "clean"


OPERATION_BITS = {
    Operation.READ: 1 << 0,
    Operation.EDIT: 1 << 1,
    Operation.CREATE: 1 << 2,
    Operation.DELETE: 1 << 3,
    Operation.TEST: 1 << 4,
    Operation.BUILD: 1 << 5,
    Operation.RUN: 1 << 6,
}
GIT_OPERATION_BITS = {
    GitOperation.STATUS: 1 << 0,
    GitOperation.DIFF: 1 << 1,
    GitOperation.LOG: 1 << 2,
    GitOperation.ADD: 1 << 3,
    GitOperation.COMMIT: 1 << 4,
    GitOperation.CHECKOUT: 1 << 5,
    GitOperation.PUSH: 1 << 6,
    GitOperation.FORCE_PUSH: 1 << 7,
    GitOperation.RESET_HARD: 1 << 8,
    GitOperation.CLEAN: 1 << 9,
}

# These are glob patterns because this is the control-plane representation sent
# to chaos-core's ContractSpec compiler, not the prose shown in the UI.
ARTIFACT_PATHS = (
    "target/**",
    "node_modules/**",
    "**/__pycache__/**",
    ".venv/**",
)

SYSTEM_PROMPT = """You translate a user's coding task into a least-privilege Tractus Intent Contract.

Return only the structured contract. Apply these rules exactly:
- NEVER emit build-artifact directories (`target/`, `node_modules/`, `**/__pycache__/`, `.venv/`) in `allowed_paths`. The control plane owns those paths and adds them after extraction.
- Any code-editing task implies the `test` and `build` op grants.
- `run` stays a separate, explicit grant: never grant it unless the user's request explicitly needs it.
- Network access is never granted unless the user's request explicitly needs it.
- `deps_may_change` defaults to false; set it true only when the request explicitly asks to add, remove, update, or upgrade dependencies.
- Keep every other path, operation, and Git permission narrowly scoped to the request.
"""


class ContractSpec(BaseModel):
    """Human-readable mirror of chaos-core's ContractSpec fields.

    `chaos-core` serializes its operation sets as bitsets. The control plane uses
    names for structured extraction and the toggle card, then converts them at
    the daemon boundary with :meth:`daemon_wire`.
    """

    task: str
    allowed_paths: list[str] = Field(default_factory=list)
    allowed_ops: list[Operation] = Field(default_factory=list)
    deps_may_change: bool = False
    git_ops: list[GitOperation] = Field(default_factory=list)
    network: bool = False

    def daemon_wire(self) -> dict[str, Any]:
        """Serialize to the exact bitset payload expected by chaosd."""
        return {
            "task": self.task,
            "allowed_paths": self.allowed_paths,
            "allowed_ops": sum(OPERATION_BITS[operation] for operation in self.allowed_ops),
            "deps_may_change": self.deps_may_change,
            "git_ops": sum(GIT_OPERATION_BITS[operation] for operation in self.git_ops),
            "network": self.network,
        }


async def extract_intent(request: str, client: Any | None = None) -> ContractSpec:
    """Use Responses structured outputs, then enforce the non-negotiable defaults."""
    client = client or AsyncOpenAI()
    response = await client.responses.parse(
        model=os.environ.get("INTENT_MODEL", "gpt-5.6-sol"),
        instructions=SYSTEM_PROMPT,
        input=request,
        text_format=ContractSpec,
    )
    parsed = getattr(response, "output_parsed", None)
    if parsed is None:
        raise RuntimeError("intent model returned no structured contract")
    contract = parsed if isinstance(parsed, ContractSpec) else ContractSpec.model_validate(parsed)
    return normalize_contract(contract, request)


def normalize_contract(contract: ContractSpec, request: str) -> ContractSpec:
    """Backstop prompt rules so a model slip cannot widen the contract."""
    allowed_paths = _deduplicated([*contract.allowed_paths, *ARTIFACT_PATHS])
    allowed_ops = set(contract.allowed_ops)
    if allowed_ops.intersection({Operation.EDIT, Operation.CREATE, Operation.DELETE}):
        allowed_ops.update({Operation.TEST, Operation.BUILD})
    if not _explicitly_requests_run(request):
        allowed_ops.discard(Operation.RUN)

    return ContractSpec(
        task=contract.task,
        allowed_paths=allowed_paths,
        allowed_ops=_ordered_operations(allowed_ops),
        deps_may_change=contract.deps_may_change and _explicitly_requests_dependencies(request),
        git_ops=_ordered_git_operations(set(contract.git_ops)),
        network=contract.network and _explicitly_requests_network(request),
    )


def _deduplicated(values: list[str]) -> list[str]:
    """Canonicalize directory grants to one recursive glob before deduplicating."""
    normalized: list[str] = []
    seen: set[str] = set()
    for value in values:
        path = value.strip().lstrip("/").rstrip("/")
        if not path:
            continue
        if not path.endswith("/**") and (
            not any(character in path for character in "*?[")
            or path.startswith("**/")
        ):
            path = f"{path}/**"
        if path not in seen:
            seen.add(path)
            normalized.append(path)
    return normalized


def _ordered_operations(operations: set[Operation]) -> list[Operation]:
    return [operation for operation in Operation if operation in operations]


def _ordered_git_operations(operations: set[GitOperation]) -> list[GitOperation]:
    return [operation for operation in GitOperation if operation in operations]


def _explicitly_requests_run(request: str) -> bool:
    return bool(
        re.search(
            r"\b(?:run|execute|launch|start)\s+(?:the\s+)?(?:app|application|server|service|program|binary)\b"
            r"|\bcargo\s+run\b",
            request,
            flags=re.IGNORECASE,
        )
    )


def _explicitly_requests_network(request: str) -> bool:
    return bool(
        re.search(
            r"https?://|\b(?:network|internet|online|download|fetch|curl|wget|publish|deploy|push|install)\b"
            r"|\bapi\s+(?:call|request|endpoint)\b",
            request,
            flags=re.IGNORECASE,
        )
    )


def _explicitly_requests_dependencies(request: str) -> bool:
    return bool(
        re.search(
            r"\b(?:dependency|dependencies|package|packages|cargo\s+(?:add|remove)|npm\s+install|pip\s+install|add|remove|upgrade|update|install)\b",
            request,
            flags=re.IGNORECASE,
        )
    )
