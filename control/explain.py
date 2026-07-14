"""Best-effort, advisory explanations for blocked Chaos Twin events."""

from __future__ import annotations

import asyncio
import json
import os
from collections.abc import Sequence
from typing import Any

from openai import AsyncOpenAI


EXPLAIN_TIMEOUT_SECONDS = 5
EXPLAINER_INSTRUCTIONS = """Explain a Chaos Twin block in exactly one plain-English sentence.
It is advisory only: describe the contract mismatch, do not recommend bypassing enforcement.
"""


async def explain_divergence(
    task: str,
    command: str,
    evidence: Any,
    client: Any | None = None,
) -> str:
    """Return one sentence, using a deterministic fallback on every API failure."""
    client = client or AsyncOpenAI()
    fallback = f"Blocked: {_first_proof_clause(evidence)}."
    payload = json.dumps(
        {"contract_task": task, "command": command, "proofs_or_twin_diff": evidence},
        default=str,
    )
    try:
        response = await asyncio.wait_for(
            client.responses.create(
                model=os.environ.get("EXPLAIN_MODEL", "gpt-5.6-luna"),
                instructions=EXPLAINER_INSTRUCTIONS,
                input=payload,
            ),
            timeout=EXPLAIN_TIMEOUT_SECONDS,
        )
        text = str(getattr(response, "output_text", "")).strip()
        return _one_sentence(text) if text else fallback
    except Exception:
        return fallback


def _first_proof_clause(evidence: Any) -> str:
    if isinstance(evidence, dict):
        return str(evidence.get("clause") or evidence.get("rendered") or "scope contract violated")
    if isinstance(evidence, Sequence) and not isinstance(evidence, (str, bytes)) and evidence:
        return _first_proof_clause(evidence[0])
    if isinstance(evidence, str) and evidence:
        return evidence
    return "scope contract violated"


def _one_sentence(text: str) -> str:
    first = text.splitlines()[0].strip()
    for delimiter in (". ", "! ", "? "):
        if delimiter in first:
            first = first.split(delimiter, 1)[0] + delimiter[0]
            break
    return first if first.endswith((".", "!", "?")) else f"{first}."
