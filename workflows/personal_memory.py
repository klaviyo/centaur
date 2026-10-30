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
from functools import partial
from typing import Any

from workflows import memory_consolidation, memory_models

WORKFLOW_NAME = "personal_memory"
WORKFLOW_STARTERS = {"admin"}
SOURCE_BATCH_SIZE = 6
MAX_SOURCE_IDS = 100_000
MAX_CONTEXT_BYTES = 12_000
MAX_SOURCE_CHARS = 16_000
MAX_BATCHES = {"consolidation": 8, "reflection": 32}
REFLECTION_WINDOW_PREFIX = "personal-reflection:"
LOCK_ATTEMPTS = 60


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


def processed_source_ids(state: dict, mode: str = "consolidation") -> list[str]:
    """Use exact durable IDs, not an unsafe timestamp/sequence high-water mark.

    A message transaction may commit after a later timestamp has been observed.
    Remembering the IDs actually committed to Git lets the next scan catch it.
    """
    ids: set[str] = set()
    for run in state["runs"].values():
        reflection = run.get("window_id", "").startswith(REFLECTION_WINDOW_PREFIX)
        if reflection != (mode == "reflection"):
            continue
        for source_id in run["source_ids"]:
            if not isinstance(source_id, str) or len(source_id) > 160:
                raise ValueError("invalid memory source checkpoint")
            if reflection:
                if not source_id.startswith("reflection:"):
                    raise ValueError("invalid reflection source checkpoint")
                source_id = source_id.removeprefix("reflection:")
            ids.add(source_id)
    if len(ids) > MAX_SOURCE_IDS:
        raise ValueError("personal memory checkpoint capacity reached")
    return sorted(ids)


def evidence_window(fresh: list[dict], historical: list[dict]) -> list[dict]:
    """Keep fresh events and retrieve whole historical excerpts, never summaries."""
    selected = {source["id"]: source for source in fresh}
    terms = set(re.findall(r"[a-z0-9_-]{4,}", " ".join(s["text"] for s in fresh).lower()))

    def rank(source: dict) -> tuple:
        words = set(re.findall(r"[a-z0-9_-]{4,}", source["text"].lower()))
        return (len(words & terms), source["occurred_at"], source["id"])

    if len(_json(list(selected.values()))) > MAX_SOURCE_CHARS:
        raise ValueError("fresh memory evidence exceeds input budget")
    for source in sorted(historical, key=rank, reverse=True):
        if source["id"] in selected:
            if source != selected[source["id"]]:
                raise ValueError("conflicting memory evidence for the same source")
            continue
        candidate = [*selected.values(), source]
        if len(candidate) <= 50 and len(_json(candidate)) <= MAX_SOURCE_CHARS:
            selected[source["id"]] = source
    return list(selected.values())


def _is_reflection(record: dict) -> bool:
    return record["category"] == "lesson" and record["key"].startswith("reflection:")


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
    return await run_loop(params, ctx, "consolidation")


async def _review_plan(ctx: Any, step: str, config: Config, mode: str,
                       sources: list[dict], records: list[dict], fresh_ids: list[str],
                       *, max_candidates: int = 4) -> list[dict]:
    proposer = memory_consolidation.reflect if mode == "reflection" else memory_consolidation.reconcile
    claims = await ctx.step(step + "propose", partial(proposer,
        config.model, sources, records, fresh_source_ids=fresh_ids, max_candidates=max_candidates))
    if mode == "consolidation" and any(_is_reflection(claim) for claim in claims):
        raise ValueError("consolidation cannot write derived reflection guidance")
    if not claims:
        return []
    if mode == "reflection":
        return await ctx.step(step + "verify", partial(memory_consolidation.verify_reflections,
            config.model, sources, records, claims, fresh_source_ids=fresh_ids))
    return await ctx.step(step + "verify", partial(memory_models.verify,
        config.model, sources, records, claims))


def _affected(records: list[dict], claims: list[dict]) -> list[dict]:
    identities = {(c["category"], c["key"], c.get("project") or "") for c in claims}
    return [record for record in records
            if (record["category"], record["key"], record.get("project") or "") in identities]


async def run_loop(params: Any, ctx: Any, mode: str) -> dict:
    if mode not in MAX_BATCHES:
        raise ValueError("invalid personal memory loop")
    config = Config.read()
    if config is None:
        return {"status": "disabled"}
    workflow_name = "personal_reflection" if mode == "reflection" else WORKFLOW_NAME
    scheduled_input = {"metadata": {
        "source": "workflow_schedule", "workflow_name": workflow_name, "no_delivery": True,
    }}
    if params not in ({}, None, scheduled_input):
        raise ValueError("personal memory targets cannot be overridden by workflow input")
    if ctx._pool is None:
        raise RuntimeError("personal memory requires the durable session store")

    # Daily and hourly ticks may coincide. Release the connection and use a
    # durable sleep on contention rather than dropping the day's reflection.
    # Git CAS still protects against independent editors and connection loss.
    lock_name = "personal_memory:" + config.thread_key
    for attempt in range(LOCK_ATTEMPTS + 1):
        async with ctx._pool.acquire() as connection:
            locked = await connection.fetchval(
                "SELECT pg_try_advisory_lock(hashtextextended($1, 0))", lock_name)
            if locked:
                try:
                    bound = await connection.fetchval(
                        "SELECT EXISTS(SELECT 1 FROM sessions WHERE thread_key=$1 AND iron_control_principal=$2)",
                        config.thread_key, config.principal,
                    )
                    if not bound:
                        raise ValueError("personal memory session does not match the configured principal")
                    return await _maintain(config, ctx, connection, mode)
                finally:
                    await connection.fetchval("SELECT pg_advisory_unlock(hashtextextended($1, 0))", lock_name)
        if attempt < LOCK_ATTEMPTS:
            await ctx.sleep(f"{mode}_v2_wait_for_lock_{attempt}", 30)
    raise RuntimeError("personal memory lock remained busy after durable retries")


async def _maintain(config: Config, ctx: Any, connection: Any, mode: str) -> dict:
    target = {"repo": config.repository, "branch": config.branch}
    prefix = mode + "_v2_"
    snapshot = await ctx.step(prefix + "read_repository", lambda: _tool(ctx, "snapshot", **target))
    historical: list[dict] = []
    source_count = claim_count = batches = 0
    for index in range(MAX_BATCHES[mode]):
        step = prefix + str(index) + "_"
        fresh = await ctx.step(step + "read_sources", partial(_read_sources,
            connection, config, processed_source_ids(snapshot["state"], mode)))
        if not fresh:
            break
        if index == 0 and snapshot["head"] is not None:
            history = await ctx.step(prefix + "read_history", partial(_tool,
                ctx, "history", **target, head=snapshot["head"], limit=24, source_limit=50))
            if history["head"] != snapshot["head"]:
                raise ValueError("memory history is not pinned to the reviewed repository")
            historical = history["sources"]
        sources = evidence_window(fresh, historical)
        fresh_ids = [source["id"] for source in fresh]
        # Hourly reconciliation cannot rewrite the separate derived guidance.
        eligible = [record for record in snapshot["state"]["records"]
                    if mode == "reflection" or not _is_reflection(record)]
        records = model_records(eligible, sources)
        approved = await _review_plan(ctx, step, config, mode, sources, records, fresh_ids)
        # Review the entire approved plan against the complete original records
        # it changes, including evidence. Never prune affected before-records to
        # fit a budget or partially apply a rejected preservation plan.
        affected = _affected(snapshot["state"]["records"], approved)
        if not memory_consolidation.preservation_fits(sources, affected, approved):
            # Discard the whole oversized plan, then independently propose and
            # review one self-contained change. Do not take a partial subset of
            # potentially coupled edits or shorten original evidence to fit.
            # Acknowledge only one event as well: the rest remain eligible for
            # the following batch instead of losing additional useful updates.
            fresh = fresh[:1]
            fresh_ids = [fresh[0]["id"]]
            approved = await _review_plan(ctx, step + "small_", config, mode,
                                          sources, records, fresh_ids, max_candidates=1)
            affected = _affected(snapshot["state"]["records"], approved)
        preservation = await ctx.step(step + "preserve", partial(memory_consolidation.preserve,
            config.model, sources, affected, approved))
        if not preservation["approved"]:
            approved = []
        window_id = (REFLECTION_WINDOW_PREFIX if mode == "reflection" else "personal-memory:consolidation-v2:") + hashlib.sha256(_json({
            "thread": config.thread_key, "sources": fresh_ids,
        }).encode()).hexdigest()
        result = await ctx.step(step + "apply", partial(_tool,
            ctx, "apply", **target, expected_head=snapshot["head"], window_id=window_id,
            sources=sources, claims=approved, processed_through=window_id,
            checkpoint_source_ids=fresh_ids,
        ))
        if result["status"] == "conflict":
            # A fresh run reevaluates unprocessed evidence against the new head.
            await ctx.step(prefix + "refresh_conflict", lambda: _refresh_cache(ctx, connection, config))
            return {"status": "conflict", "head": result["head"], "sources": source_count, "claims": claim_count}
        if result["status"] not in {"applied", "no_duplicate"}:
            raise ValueError("unexpected memory application result")
        snapshot = result
        source_count += len(fresh)
        claim_count += len(approved)
        batches += 1
        # Keep only a bounded bank of whole raw excerpts for later batches.
        history_by_id = {source["id"]: source for source in [*historical, *fresh]}
        historical = sorted(history_by_id.values(), key=lambda source: (source["occurred_at"], source["id"]), reverse=True)[:50]
        ctx.log("personal_memory_batch", loop=mode, source_count=len(fresh), claim_count=len(approved),
                preservation_approved=preservation["approved"], head=result["head"])
    # This is a fresh read inside the retryable step, including after idle ticks
    # and successful Git writes replayed following a projection failure.
    await ctx.step(prefix + "publish_context", lambda: _refresh_cache(ctx, connection, config))
    return {"status": "applied" if batches else "no_new_sources", "head": snapshot["head"],
            "sources": source_count, "claims": claim_count, "batches": batches,
            "batch_limit_reached": batches == MAX_BATCHES[mode]}
