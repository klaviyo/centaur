"""Opt-in memory maintenance: durable sources -> proposals -> review -> Git -> cache.

The repository is authoritative. Session metadata is only a bounded projection.
No transcript is scanned outside the one configured canonical DM and principal.
No schedules, tools, messages, or goals are created from extracted memory.
"""

from __future__ import annotations

import datetime as dt
import hashlib
import json
import os
import re
import shutil
from dataclasses import dataclass
from typing import Any

from workflows import memory_models

WORKFLOW_NAME = "personal_memory"
WORKFLOW_STARTERS = {"admin"}
SOURCE_BATCH_SIZE = 6
MAX_SOURCE_IDS = 100_000
MAX_CONTEXT_BYTES = 12_000


def _enabled(env: Any = os.environ) -> bool:
    return env.get("CENTAUR_MEMORY_ENABLED", "").lower() in {"true", "1"}


def _interval(env: Any = os.environ) -> int:
    try:
        value = int(env.get("CENTAUR_MEMORY_INTERVAL_SECONDS", "3600"))
    except ValueError:
        return 3600
    return max(60, value)


WORKFLOW_PRINCIPAL = (
    os.getenv("CENTAUR_MEMORY_PRINCIPAL", "").strip() if _enabled() else None
)
SCHEDULE = {
    "schedule_id": WORKFLOW_NAME,
    "interval_seconds": _interval(),
    "enabled": _enabled(),
    "no_delivery": True,
}


@dataclass(frozen=True)
class Config:
    thread_key: str
    principal: str
    repository: str
    branch: str
    model: str

    @classmethod
    def read(cls, env: Any = os.environ) -> Config | None:
        if not _enabled(env):
            return None
        values = [env.get("CENTAUR_MEMORY_" + key, "").strip() for key in (
            "THREAD_KEY", "PRINCIPAL", "REPOSITORY", "BRANCH", "MODEL"
        )]
        values[3] = values[3] or "main"
        if (
            not re.fullmatch(r"slack:D[A-Z0-9]+:", values[0])
            or not re.fullmatch(r"[^\s\x00-\x1f]{1,200}", values[1])
            or not re.fullmatch(r"[A-Za-z0-9-]{1,100}/[A-Za-z0-9_.-]{1,100}", values[2])
            or values[2].split("/")[-1] in {".", ".."}
            or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]{0,99}", values[3])
            or any(not p or p.startswith(".") or p.endswith((".lock", ".")) for p in values[3].split("/"))
            or ".." in values[3]
            or not re.fullmatch(r"[A-Za-z0-9._:/-]{1,100}", values[4])
        ):
            raise ValueError("invalid personal memory configuration")
        return cls(*values)


def _json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


# Redact before the model, durable checkpoints, or Git see an excerpt. This is
# defense in depth, not a promise to recognize every possible secret format.
_PRIVATE_KEY = re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)", re.DOTALL)
_CREDENTIAL = re.compile(
    r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|"
    r"xox[baprs]-[A-Za-z0-9-]{10,}|sk-(?:proj-)?[A-Za-z0-9_-]{20,}|AKIA[A-Z0-9]{16})\b|"
    r"\bBearer\s+[A-Za-z0-9._~+/-]{12,}|"
    r"\b(?:password|api[_ -]?key|access[_ -]?token|secret)\s*[:=]\s*[\"']?[^\s\"']{12,}",
    re.IGNORECASE,
)


def _excerpt(text: str) -> str:
    text = _CREDENTIAL.sub("[credential redacted]", _PRIVATE_KEY.sub("[private key redacted]", text))
    text = "".join(c for c in text if ord(c) >= 32 or c in "\n\t\r")
    if len(text) > 2000:
        marker = "\n[Excerpt truncated; consult original source for omitted text.]\n"
        side = (2000 - len(marker)) // 2
        text = text[:side] + marker + text[-side:]
    return text.strip() or "[No supported text; attachments are not ingested.]"


def processed_source_ids(state: dict) -> list[str]:
    """Use exact durable IDs, not an unsafe timestamp/sequence high-water mark.

    A message transaction may commit after a later timestamp has been observed.
    Remembering the IDs actually committed to Git lets the next scan catch it.
    """
    ids: set[str] = set()
    for run in state["runs"].values():
        for source_id in run["source_ids"]:
            if not isinstance(source_id, str) or len(source_id) > 160:
                raise ValueError("invalid memory source checkpoint")
            ids.add(source_id)
    if len(ids) > MAX_SOURCE_IDS:
        raise ValueError("personal memory checkpoint capacity reached")
    return sorted(ids)


async def _read_sources(pool: Any, config: Config, seen: list[str]) -> list[dict]:
    rows = await pool.fetch(
        """
        SELECT id, role, text, occurred_at FROM (
          SELECT 'message:' || m.message_id AS id, m.role,
            COALESCE((SELECT string_agg(p->>'text', E'\n' ORDER BY n)
              FROM jsonb_array_elements(m.parts) WITH ORDINALITY AS part(p,n)
              WHERE p->>'type' = 'text'), '') AS text,
            m.created_at AS occurred_at
          FROM session_messages m JOIN sessions s USING (thread_key)
          WHERE s.thread_key = $1 AND s.iron_control_principal = $2
            AND m.role IN ('user', 'assistant')
            AND NOT ('message:' || m.message_id = ANY($3::text[]))
          UNION ALL
          SELECT 'event:' || e.event_id::text, 'assistant',
            COALESCE(e.payload->>'result_text', ''), e.created_at
          FROM session_events e JOIN sessions s USING (thread_key)
          WHERE s.thread_key = $1 AND s.iron_control_principal = $2
            AND e.event_type = 'session.execution_completed'
            AND NOT ('event:' || e.event_id::text = ANY($3::text[]))
        ) sources ORDER BY occurred_at, id LIMIT $4
        """,
        config.thread_key, config.principal, seen, SOURCE_BATCH_SIZE,
    )
    return [{
        "id": row["id"], "role": row["role"], "text": _excerpt(row["text"]),
        "occurred_at": row["occurred_at"].astimezone(dt.UTC).isoformat(),
    } for row in rows]


def model_records(records: list[dict], sources: list[dict], claims: list[dict] | None = None) -> list[dict]:
    """Retrieve relevant records inside a separate, bounded model-input budget."""
    terms = set(re.findall(r"[a-z0-9_-]{4,}", " ".join(s["text"] for s in sources).lower()))
    needed = {(c["category"], c["key"], c.get("project") or "") for c in claims or []}

    def rank(record: dict) -> tuple:
        identity = (record["category"], record["key"], record.get("project") or "")
        words = set(re.findall(r"[a-z0-9_-]{4,}", _json(record).lower()))
        return (identity in needed, len(words & terms), record.get("updated_at", ""), record["id"])

    budget = min(8000, 27_000 - len(_json({"sources": sources, "claims": claims or []})))
    selected: list[dict] = []
    for record in sorted(records, key=rank, reverse=True):
        summary = {key: record[key] for key in ("id", "category", "key", "project", "text", "status", "revision")}
        size = len(_json(summary)) + 1
        if size <= budget and len(selected) < 300:
            selected.append(summary)
            budget -= size
    return selected


async def _tool(ctx: Any, method: str, **kwargs: Any) -> Any:
    # Fail closed rather than falling back to the unscoped tool-server RPC.
    if (
        os.getenv("WORKFLOW_HOST_SANDBOX", "").lower() not in {"true", "1"}
        or not shutil.which("centaur-memory")
        or not shutil.which("centaur-tools")
    ):
        raise RuntimeError("personal memory requires its credential-scoped workflow sandbox and installed memory tool")
    return await getattr(getattr(ctx.tools, "centaur-memory"), method)(**kwargs)


async def _project_cache(pool: Any, config: Config, snapshot: dict) -> None:
    context = snapshot["context"]
    head = snapshot["head"]
    if head is None:
        return
    if not re.fullmatch(r"[a-f0-9]{40}", head) or len(context.encode()) > MAX_CONTEXT_BYTES:
        raise ValueError("invalid memory context projection")
    cache = {
        "schema_version": 1, "repository": config.repository, "branch": config.branch,
        "principal": config.principal, "head": head, "context": context,
        "updated_at": dt.datetime.now(dt.UTC).isoformat(),
    }
    result = await pool.execute(
        """UPDATE sessions SET metadata = jsonb_set(metadata, '{centaur_memory}', $3::jsonb, true)
        WHERE thread_key = $1 AND iron_control_principal = $2""",
        config.thread_key, config.principal, _json(cache),
    )
    if result != "UPDATE 1":
        raise ValueError("memory session binding changed before projection")


async def _refresh_cache(ctx: Any, connection: Any, config: Config) -> None:
    # Read inside the retryable projection step. Replaying an earlier Git-write
    # checkpoint must not overwrite a newer worker's cache with its old snapshot.
    latest = await _tool(ctx, "snapshot", repo=config.repository, branch=config.branch)
    await _project_cache(connection, config, latest)


async def handler(params: Any, ctx: Any) -> dict:
    config = Config.read()
    if config is None:
        return {"status": "disabled"}
    scheduled_input = {"metadata": {
        "source": "workflow_schedule", "workflow_name": WORKFLOW_NAME, "no_delivery": True,
    }}
    if params not in ({}, None, scheduled_input):
        raise ValueError("personal memory targets cannot be overridden by workflow input")
    if ctx._pool is None:
        raise RuntimeError("personal memory requires the durable session store")

    # This lock limits duplicate model work. Git's compare-and-swap remains the
    # correctness boundary if a worker crashes, loses its connection, or an
    # independent editor changes the branch while a proposal is being reviewed.
    async with ctx._pool.acquire() as connection:
        lock_name = "personal_memory:" + config.thread_key
        locked = await connection.fetchval("SELECT pg_try_advisory_lock(hashtextextended($1, 0))", lock_name)
        if not locked:
            return {"status": "busy"}
        try:
            bound = await connection.fetchval(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE thread_key=$1 AND iron_control_principal=$2)",
                config.thread_key, config.principal,
            )
            if not bound:
                raise ValueError("personal memory session does not match the configured principal")
            return await _maintain(config, ctx, connection)
        finally:
            await connection.fetchval("SELECT pg_advisory_unlock(hashtextextended($1, 0))", lock_name)


async def _maintain(config: Config, ctx: Any, connection: Any) -> dict:
    target = {"repo": config.repository, "branch": config.branch}
    snapshot = await ctx.step("read_repository", lambda: _tool(ctx, "snapshot", **target))
    sources = await ctx.step("read_sources", lambda: _read_sources(
        connection, config, processed_source_ids(snapshot["state"])))
    if not sources:
        # Also refresh externally corrected Git data when there is no new chat.
        await ctx.step("refresh_context", lambda: _refresh_cache(ctx, connection, config))
        return {"status": "no_new_sources", "head": snapshot["head"]}
    window_id = "personal-memory:" + hashlib.sha256(_json({
        "thread": config.thread_key, "sources": [s["id"] for s in sources],
    }).encode()).hexdigest()
    records = model_records(snapshot["state"]["records"], sources)
    claims = await ctx.step("extract_claims", lambda: memory_models.extract(config.model, sources, records))
    approved = await ctx.step("verify_claims", lambda: memory_models.verify(
        config.model, sources, records, claims))
    result = await ctx.step("apply_verified_memory", lambda: _tool(
        ctx, "apply", **target, expected_head=snapshot["head"], window_id=window_id,
        sources=sources, claims=approved, processed_through=window_id,
    ))
    if result["status"] == "conflict":
        # Do not replay a pinned, stale proposal forever. A fresh scheduled run
        # rereads Git and re-evaluates the same unprocessed sources.
        return {"status": "conflict", "head": result["head"]}
    if result["status"] not in {"applied", "no_duplicate"}:
        raise ValueError("unexpected memory application result")
    await ctx.step("publish_context", lambda: _refresh_cache(ctx, connection, config))
    ctx.log("personal_memory_applied", source_count=len(sources), claim_count=len(approved), head=result["head"])
    return {"status": result["status"], "head": result["head"], "sources": len(sources), "claims": len(approved)}
