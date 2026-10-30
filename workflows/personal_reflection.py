"""Daily, activity-gated dreaming over original evidence and fallible priors."""

from __future__ import annotations

import os
from typing import Any

from workflows import personal_memory

WORKFLOW_NAME = "personal_reflection"
WORKFLOW_STARTERS = {"admin"}


def _enabled(env: Any = os.environ) -> bool:
    return personal_memory._enabled(env) and env.get(
        "CENTAUR_MEMORY_REFLECTION_ENABLED", "true").lower() in {"true", "1"}


def _interval(env: Any = os.environ) -> int:
    try:
        value = int(env.get("CENTAUR_MEMORY_REFLECTION_INTERVAL_SECONDS", "86400"))
    except ValueError:
        return 86400
    return max(60, value)


WORKFLOW_PRINCIPAL = os.getenv("CENTAUR_MEMORY_PRINCIPAL", "").strip() if _enabled() else None
SCHEDULE = {
    "schedule_id": WORKFLOW_NAME, "interval_seconds": _interval(),
    "enabled": _enabled(), "no_delivery": True,
}


async def handler(params: Any, ctx: Any) -> dict:
    if not _enabled():
        return {"status": "disabled"}
    return await personal_memory.run_loop(params, ctx, "reflection")
