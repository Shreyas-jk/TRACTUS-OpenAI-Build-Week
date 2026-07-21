"""Tractus's FastAPI control plane."""

from __future__ import annotations

import os
from pathlib import Path


def _load_local_env() -> None:
    """Load only control-plane settings without executing a shell file."""
    path = Path(__file__).with_name(".env")
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return

    allowed = {"OPENAI_API_KEY", "INTENT_MODEL", "EXPLAIN_MODEL"}
    for line in lines:
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, value = line.split("=", 1)
        name = name.strip()
        value = value.strip().strip("\"'")
        if name in allowed and value:
            os.environ.setdefault(name, value)


_load_local_env()
