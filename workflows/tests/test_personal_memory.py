from __future__ import annotations

import copy
import datetime as dt
import hashlib
import json
import os
import unittest
from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

from workflows import personal_memory as memory

ENV = {
    "CENTAUR_MEMORY_ENABLED": "true", "CENTAUR_MEMORY_THREAD_KEY": "slack:DEXAMPLE:",
    "CENTAUR_MEMORY_PRINCIPAL": "prn_owner", "CENTAUR_MEMORY_REPOSITORY": "example/memory",
    "CENTAUR_MEMORY_BRANCH": "main", "CENTAUR_MEMORY_MODEL": "configured-model",
    "WORKFLOW_HOST_SANDBOX": "true",
}
CONFIG = memory.Config.read(ENV)
SOURCE = {"id": "message:msg_one", "role": "user", "text": "Keep the prototype opt-in.", "occurred_at": "2026-01-01T00:00:00+00:00"}
CLAIM = {
    "category": "project_constraint", "key": "opt-in", "project": "prototype",
    "text": "The prototype is opt-in.", "status": "active", "expected_revision": 0,
    "evidence": [{"source_id": SOURCE["id"], "quote": "Keep the prototype opt-in."}],
}
REFLECTION = {
    **CLAIM, "category": "lesson", "key": "reflection:opt-in",
    "text": "Derived guidance: Confirm opt-in boundaries when changing the prototype.",
}


def snapshot(head="a" * 40):
    return {"head": head, "state": {"records": [], "runs": {}, "schema_version": 1, "processed_through": None}, "context": "Evidence only."}


def source(index):
    return {
        **SOURCE, "id": f"message:msg_{index}", "text": f"Keep prototype {index} opt-in.",
        "occurred_at": f"2026-01-01T00:{index:02}:00+00:00",
    }


def stored_record(claim=CLAIM, **changes):
    return {
        **{key: value for key, value in claim.items() if key != "expected_revision"},
        "id": "mem_existing", "revision": 2, "updated_at": "2026-01-01T00:00:00Z",
        "evidence": [{**item, "window_id": "prior-window"} for item in claim["evidence"]],
        **changes,
    }


class FakeConnection:
    def __init__(self):
        self.locked = True
        self.lock_results = []
        self.bound = True
        self.calls = []
        self.rows = []
        self.cache = None
        self.fail_cache = False
        self.acquired = 0

    def set_sources(self, sources):
        self.rows = [{**item, "occurred_at": dt.datetime.fromisoformat(item["occurred_at"])} for item in sources]

    async def fetchval(self, query, *args):
        self.calls.append((query, args))
        if "try_advisory_lock" in query:
            return self.lock_results.pop(0) if self.lock_results else self.locked
        if "EXISTS" in query:
            return self.bound
        return True

    async def fetch(self, query, *args):
        self.calls.append((query, args))
        return [row for row in self.rows if row["id"] not in args[2]][:args[3]]

    async def execute(self, query, *args):
        self.calls.append((query, args))
        if self.fail_cache:
            raise RuntimeError("database unavailable")
        self.cache = json.loads(args[2])
        return "UPDATE 1" if self.bound else "UPDATE 0"


class Acquisition:
    def __init__(self, connection):
        self.connection = connection

    async def __aenter__(self):
        self.connection.acquired += 1
        return self.connection

    async def __aexit__(self, *_args):
        self.connection.acquired -= 1
        return False


class FakePool:
    def __init__(self, connection):
        self.connection = connection

    def acquire(self):
        return Acquisition(self.connection)


class FakeContext:
    def __init__(self, connection):
        self._pool = FakePool(connection)
        self.checkpoints = {}
        self.logs = []
        self.sleeps = []

    async def step(self, name, fn):
        if name not in self.checkpoints:
            self.checkpoints[name] = copy.deepcopy(await fn())
        return copy.deepcopy(self.checkpoints[name])

    async def sleep(self, name, seconds):
        assert self._pool.connection.acquired == 0, "durable sleep must release the connection"
        self.sleeps.append((name, seconds))

    def log(self, event, **fields):
        self.logs.append((event, fields))


class FakeMemoryTool:
    """A durable Git boundary with atomic writes and window deduplication."""

    def __init__(self, initial=None, historical=()):
        self.current = copy.deepcopy(initial or snapshot())
        self.historical = copy.deepcopy(list(historical))
        self.calls = []
        self.writes = []
        self.conflict = False
        self.fail_after_commit = False
        self.history_head = None

    async def __call__(self, _ctx, method, **kwargs):
        self.calls.append((method, copy.deepcopy(kwargs)))
        if method == "snapshot":
            return copy.deepcopy(self.current)
        if method == "history":
            return {"head": self.history_head or kwargs["head"], "sources": copy.deepcopy(self.historical), "window_ids": []}
        assert method == "apply"
        run_id = hashlib.sha256(kwargs["window_id"].encode()).hexdigest()
        if run_id in self.current["state"]["runs"]:
            return {**copy.deepcopy(self.current), "status": "no_duplicate"}
        if self.conflict:
            self.current["head"] = "c" * 40
            return {**copy.deepcopy(self.current), "status": "conflict"}
        assert kwargs["expected_head"] == self.current["head"]
        self.writes.append(copy.deepcopy(kwargs))
        self.current["head"] = hashlib.sha1(kwargs["window_id"].encode()).hexdigest()
        ids = kwargs["checkpoint_source_ids"]
        if kwargs["window_id"].startswith(memory.REFLECTION_WINDOW_PREFIX):
            ids = ["reflection:" + source_id for source_id in ids]
        self.current["state"]["runs"][run_id] = {"window_id": kwargs["window_id"], "source_ids": sorted(ids)}
        self.current["state"]["processed_through"] = kwargs["processed_through"]
        if self.fail_after_commit:
            self.fail_after_commit = False
            raise RuntimeError("worker interrupted after Git commit")
        return {**copy.deepcopy(self.current), "status": "applied"}


class ConfigurationTests(unittest.TestCase):
    def test_disabled_is_default_and_hourly_cadence_needs_no_credentials(self):
        self.assertIsNone(memory.Config.read({}))
        self.assertEqual(memory._interval({}), 3600)
        self.assertEqual(memory._interval({"CENTAUR_MEMORY_INTERVAL_SECONDS": "0"}), 60)
        self.assertEqual(memory._interval({"CENTAUR_MEMORY_INTERVAL_SECONDS": "invalid"}), 3600)

    def test_rejects_wrong_surface_missing_model_and_bad_target(self):
        for key, value in (("THREAD_KEY", "slack:CCHANNEL:"), ("THREAD_KEY", "slack:DEXAMPLE:123"), ("PRINCIPAL", ""), ("MODEL", ""), ("REPOSITORY", "https://other.example/repo"), ("BRANCH", "../main")):
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                memory.Config.read({**ENV, "CENTAUR_MEMORY_" + key: value})

    def test_sources_are_redacted_and_bounded_before_checkpointing(self):
        raw = "ghp_" + "x" * 40 + "\nBearer " + "z" * 30
        result = memory._excerpt(raw)
        self.assertNotIn("ghp_", result)
        self.assertNotIn("Bearer", result)
        self.assertLessEqual(len(memory._excerpt("a" * 10_000)), 2000)
        self.assertNotIn("PRIVATE KEY", memory._excerpt("-----BEGIN PRIVATE KEY-----\nsecret material"))

    def test_exact_independent_checkpoints_do_not_depend_on_monotonic_timestamps(self):
        state = {"runs": {
            "one": {"window_id": "personal-memory:old", "source_ids": ["event:10", "message:older"]},
            "two": {"window_id": "legacy-window", "source_ids": ["event:9", "message:older"]},
            "dream": {"window_id": "personal-reflection:daily", "source_ids": ["reflection:event:8", "reflection:message:new"]},
        }}
        self.assertEqual(memory.processed_source_ids(state), ["event:10", "event:9", "message:older"])
        self.assertEqual(memory.processed_source_ids(state, "reflection"), ["event:8", "message:new"])
        old_seen = {source_id for run in state["runs"].values() for source_id in run["source_ids"]}
        self.assertNotIn("message:new", old_seen)
        state["runs"]["dream"]["source_ids"] = ["message:raw-unqualified"]
        with self.assertRaises(ValueError):
            memory.processed_source_ids(state, "reflection")

    def test_model_context_is_bounded_and_prefers_updated_identity(self):
        records = [{"id": str(i), "category": "project_constraint", "key": str(i), "project": None, "text": "x" * 600, "status": "active", "revision": 1, "updated_at": str(i)} for i in range(200)]
        proposed = [{**CLAIM, "key": "0", "project": ""}]
        selected = memory.model_records(records, [SOURCE], proposed)
        self.assertEqual(selected[0]["key"], "0")
        self.assertLess(len(json.dumps(selected)), 8500)

    def test_evidence_window_preserves_original_excerpts_and_rejects_conflicts(self):
        historical = [{**source(index), "text": ("prototype " + str(index) + " ") * 150} for index in range(1, 30)]
        selected = memory.evidence_window([SOURCE], historical)
        self.assertEqual(selected[0], SOURCE)
        self.assertLessEqual(len(memory._json(selected)), memory.MAX_SOURCE_CHARS)
        self.assertTrue(all(item in [SOURCE, *historical] for item in selected))
        self.assertEqual(memory.evidence_window([SOURCE], [SOURCE]), [SOURCE])
        for changed in ({**SOURCE, "text": "Different original text."}, {**SOURCE, "occurred_at": "different"}):
            with self.subTest(changed=changed), self.assertRaises(ValueError):
                memory.evidence_window([SOURCE], [changed])


class WorkflowTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.connection = FakeConnection()
        self.ctx = FakeContext(self.connection)
        self.env = patch.dict(os.environ, ENV, clear=False)
        self.env.start()
        self.addCleanup(self.env.stop)

    def stages(self, tool, *, candidates=None, approved=None, preservation=None):
        stack = ExitStack()
        stack.enter_context(patch.object(memory, "_tool", new=tool))
        propose = stack.enter_context(patch.object(memory.memory_consolidation, "reconcile", new_callable=AsyncMock, return_value=copy.deepcopy(candidates if candidates is not None else [CLAIM])))
        verify = stack.enter_context(patch.object(memory.memory_models, "verify", new_callable=AsyncMock, return_value=copy.deepcopy(approved if approved is not None else [CLAIM])))
        preserve = stack.enter_context(patch.object(memory.memory_consolidation, "preserve", new_callable=AsyncMock, return_value=preservation or {"approved": True, "reason": "Supported."}))
        self.addCleanup(stack.close)
        return propose, verify, preserve

    async def test_disabled_does_no_work(self):
        with patch.dict(os.environ, {"CENTAUR_MEMORY_ENABLED": "false"}), patch.object(memory, "_tool", new_callable=AsyncMock) as tool:
            self.assertEqual(await memory.handler({}, self.ctx), {"status": "disabled"})
            tool.assert_not_called()
        self.assertFalse(self.connection.calls)

    async def test_workflow_input_cannot_change_binding(self):
        with self.assertRaises(ValueError):
            await memory.handler({"repository": "someone/else"}, self.ctx)
        self.assertFalse(self.connection.calls)

    async def test_scheduler_envelope_is_accepted_without_target_overrides(self):
        tool = FakeMemoryTool()
        self.stages(tool)
        scheduled = {"metadata": {"source": "workflow_schedule", "workflow_name": "personal_memory", "no_delivery": True}}
        result = await memory.handler(scheduled, self.ctx)
        self.assertEqual(result["status"], "no_new_sources")

    async def test_contention_uses_durable_sleep_and_eventually_acquires(self):
        tool = FakeMemoryTool()
        self.stages(tool)
        self.connection.lock_results = [False, False, True]
        result = await memory.handler({}, self.ctx)
        self.assertEqual(result["status"], "no_new_sources")
        self.assertEqual(self.ctx.sleeps, [("consolidation_v2_wait_for_lock_0", 30), ("consolidation_v2_wait_for_lock_1", 30)])
        self.assertIn("pg_advisory_unlock", self.connection.calls[-1][0])

    async def test_contention_exhaustion_is_visible_and_never_accesses_tools(self):
        self.connection.locked = False
        with patch.object(memory, "LOCK_ATTEMPTS", 2), patch.object(memory, "_tool", new_callable=AsyncMock) as tool:
            with self.assertRaisesRegex(RuntimeError, "busy after durable retries"):
                await memory.handler({}, self.ctx)
            tool.assert_not_called()
        self.assertEqual(len(self.ctx.sleeps), 2)
        self.assertEqual(sum("try_advisory_lock" in query for query, _ in self.connection.calls), 3)

    async def test_mismatched_binding_prevents_tool_access_and_unlocks(self):
        self.connection.bound = False
        with patch.object(memory, "_tool", new_callable=AsyncMock) as tool:
            with self.assertRaises(ValueError):
                await memory.handler({}, self.ctx)
            tool.assert_not_called()
        self.assertIn("pg_advisory_unlock", self.connection.calls[-1][0])

    async def test_sql_is_scoped_and_late_commits_remain_eligible(self):
        self.connection.set_sources([SOURCE])
        sources = await memory._read_sources(self.connection, CONFIG, ["event:42"])
        query, args = self.connection.calls[-1]
        self.assertEqual(args[:3], (CONFIG.thread_key, CONFIG.principal, ["event:42"]))
        self.assertEqual(query.count("s.iron_control_principal = $2"), 2)
        self.assertNotIn("created_at >", query)
        self.assertEqual(sources, [SOURCE])

    async def test_unscoped_host_cannot_fall_back_to_tool_rpc(self):
        with patch.dict(os.environ, {"WORKFLOW_HOST_SANDBOX": "false"}), self.assertRaises(RuntimeError):
            await memory._tool(self.ctx, "snapshot")

    async def test_missing_catalog_shim_cannot_fall_back_to_tool_rpc(self):
        with patch.object(memory.shutil, "which", side_effect=lambda name: "/bin/memory" if name == "centaur-memory" else None), self.assertRaises(RuntimeError):
            await memory._tool(self.ctx, "snapshot")

    async def test_idle_skips_history_models_and_writes_but_refreshes_cache(self):
        tool = FakeMemoryTool()
        propose, verify, preserve = self.stages(tool)
        result = await memory.handler({}, self.ctx)
        self.assertEqual(result["status"], "no_new_sources")
        self.assertEqual(self.connection.cache["head"], tool.current["head"])
        self.assertEqual([method for method, _ in tool.calls], ["snapshot", "snapshot"])
        propose.assert_not_called()
        verify.assert_not_called()
        preserve.assert_not_called()
        self.assertEqual(tool.writes, [])

    async def test_bounded_backlog_drains_batches_with_only_fresh_checkpoint_ids(self):
        events = [source(index) for index in range(1, 16)]
        historical = {**source(0), "id": "message:historical"}
        self.connection.set_sources(events)
        tool = FakeMemoryTool(historical=[historical])
        propose, verify, _ = self.stages(tool, candidates=[])
        with patch.object(memory, "MAX_BATCHES", {"consolidation": 2, "reflection": 32}):
            result = await memory.handler({}, self.ctx)
        self.assertEqual(result["sources"], 12)
        self.assertEqual(result["batches"], 2)
        self.assertTrue(result["batch_limit_reached"])
        self.assertEqual(propose.await_count, 2)
        verify.assert_not_called()
        self.assertEqual([method for method, _ in tool.calls].count("history"), 1)
        for index, write in enumerate(tool.writes):
            expected = [item["id"] for item in events[index * 6:(index + 1) * 6]]
            self.assertEqual(write["checkpoint_source_ids"], expected)
            self.assertEqual(propose.await_args_list[index].kwargs["fresh_source_ids"], expected)
            self.assertIn(historical, write["sources"])
            self.assertNotIn(historical["id"], write["checkpoint_source_ids"])
        self.assertEqual(len(memory.processed_source_ids(tool.current["state"])), 12)
        self.assertTrue(all(item["id"] not in memory.processed_source_ids(tool.current["state"]) for item in events[12:]))

    async def test_history_must_match_pinned_head_and_original_source_identity(self):
        for wrong_head, historical in (("b" * 40, []), (None, [{**SOURCE, "text": "Conflicting excerpt."}])):
            with self.subTest(wrong_head=wrong_head), ExitStack() as stack:
                tool = FakeMemoryTool(historical=historical)
                tool.history_head = wrong_head
                self.connection.set_sources([SOURCE])
                self.ctx = FakeContext(self.connection)
                stack.enter_context(patch.object(memory, "_tool", new=tool))
                propose = stack.enter_context(patch.object(memory.memory_consolidation, "reconcile", new_callable=AsyncMock))
                with self.assertRaises(ValueError):
                    await memory.handler({}, self.ctx)
                propose.assert_not_called()
                self.assertEqual(tool.writes, [])

    async def test_conflict_refreshes_cache_without_advancing_local_sources(self):
        self.connection.set_sources([SOURCE])
        tool = FakeMemoryTool()
        tool.conflict = True
        self.stages(tool)
        result = await memory.handler({}, self.ctx)
        self.assertEqual(result["status"], "conflict")
        self.assertEqual(memory.processed_source_ids(tool.current["state"]), [])
        self.assertEqual(self.connection.cache["head"], "c" * 40)
        self.assertEqual(tool.writes, [])

    async def test_rejected_whole_plan_checkpoints_empty_claims_and_preserves_full_before_records(self):
        original = stored_record(text="Keep the prototype opt-in and retain explicit rollback constraints.")
        initial = snapshot()
        initial["state"]["records"] = [original]
        self.connection.set_sources([SOURCE])
        tool = FakeMemoryTool(initial)
        candidates = [{**CLAIM, "expected_revision": 2}, {**CLAIM, "key": "second"}]
        _, _, preserve = self.stages(tool, candidates=candidates, approved=candidates, preservation={"approved": False, "reason": "The plan drops a constraint."})
        result = await memory.handler({}, self.ctx)
        self.assertEqual(result["claims"], 0)
        self.assertEqual(preserve.await_args.args[2], [original])
        self.assertEqual(preserve.await_args.args[3], candidates)
        self.assertEqual(tool.writes[0]["claims"], [])
        self.assertEqual(tool.writes[0]["checkpoint_source_ids"], [SOURCE["id"]])
        self.assertEqual(tool.current["state"]["records"], [original])

    async def test_malformed_preservation_review_fails_before_git_write(self):
        self.connection.set_sources([SOURCE])
        tool = FakeMemoryTool()
        with (
            patch.object(memory, "_tool", new=tool),
            patch.object(memory.memory_consolidation, "reconcile", new_callable=AsyncMock, return_value=[CLAIM]),
            patch.object(memory.memory_models, "verify", new_callable=AsyncMock, return_value=[CLAIM]),
            patch.object(memory.memory_models, "_request", new_callable=AsyncMock, return_value={"approved": "yes", "reason": "Invalid boolean."}),
            self.assertRaises(memory.memory_models.MemoryModelError),
        ):
            await memory.handler({}, self.ctx)
        self.assertEqual(tool.writes, [])
        self.assertIsNone(self.connection.cache)

    async def test_oversized_plan_is_replaced_and_only_first_fresh_event_is_checkpointed(self):
        second_source = source(2)
        self.connection.set_sources([SOURCE, second_source])
        second_claim = {
            **CLAIM, "key": "second-project", "text": "Keep the second prototype opt-in.",
            "evidence": [{"source_id": second_source["id"], "quote": second_source["text"]}],
        }
        original = [CLAIM, second_claim]
        replacement = [{**CLAIM, "key": "independently-reconsidered", "text": "Retain the explicit opt-in boundary."}]
        tool = FakeMemoryTool()
        propose, verify, preserve = self.stages(tool)
        propose.side_effect = [original, replacement]
        verify.side_effect = [original, replacement]
        with patch.object(memory.memory_consolidation, "preservation_fits", return_value=False), patch.object(memory, "MAX_BATCHES", {"consolidation": 1, "reflection": 32}):
            result = await memory.handler({}, self.ctx)
        self.assertEqual([call.kwargs["max_candidates"] for call in propose.await_args_list], [4, 1])
        self.assertEqual(propose.await_args_list[0].kwargs["fresh_source_ids"], [SOURCE["id"], second_source["id"]])
        self.assertEqual(propose.await_args_list[1].kwargs["fresh_source_ids"], [SOURCE["id"]])
        self.assertEqual(verify.await_count, 2)
        self.assertEqual(preserve.await_count, 1)
        self.assertEqual(preserve.await_args.args[3], replacement)
        self.assertEqual(tool.writes[0]["claims"], replacement)
        self.assertEqual(tool.writes[0]["sources"], [SOURCE, second_source])
        self.assertEqual(tool.writes[0]["checkpoint_source_ids"], [SOURCE["id"]])
        self.assertEqual(result["sources"], 1)
        self.assertNotIn(second_source["id"], memory.processed_source_ids(tool.current["state"]))
        self.assertIn("consolidation_v2_0_small_propose", self.ctx.checkpoints)
        self.assertIn("consolidation_v2_0_small_verify", self.ctx.checkpoints)

    async def test_hourly_consolidation_cannot_read_or_write_derived_guidance(self):
        initial = snapshot()
        initial["state"]["records"] = [stored_record(REFLECTION)]
        tool = FakeMemoryTool(initial)
        self.connection.set_sources([SOURCE])
        propose, verify, preserve = self.stages(tool, candidates=[REFLECTION])
        with self.assertRaisesRegex(ValueError, "cannot write derived reflection"):
            await memory.handler({}, self.ctx)
        self.assertEqual(propose.await_args.args[2], [])
        verify.assert_not_called()
        preserve.assert_not_called()
        self.assertEqual(tool.writes, [])

    async def test_resume_after_cache_failure_reloads_latest_without_repeating_models_or_write(self):
        self.connection.set_sources([SOURCE])
        self.connection.fail_cache = True
        tool = FakeMemoryTool()
        propose, verify, preserve = self.stages(tool)
        with self.assertRaisesRegex(RuntimeError, "database unavailable"):
            await memory.handler({}, self.ctx)
        self.connection.fail_cache = False
        tool.current["head"] = "d" * 40
        tool.current["context"] = "A newer independent Git update."
        result = await memory.handler({}, self.ctx)
        self.assertEqual(result["status"], "applied")
        self.assertEqual(self.connection.cache["head"], "d" * 40)
        self.assertEqual(self.connection.cache["context"], tool.current["context"])
        self.assertEqual((propose.await_count, verify.await_count, preserve.await_count), (1, 1, 1))
        self.assertEqual([method for method, _ in tool.calls].count("apply"), 1)
        self.assertEqual(len(tool.writes), 1)

    async def test_resume_after_git_commit_before_checkpoint_reuses_window_and_models(self):
        self.connection.set_sources([SOURCE])
        tool = FakeMemoryTool()
        tool.fail_after_commit = True
        propose, verify, preserve = self.stages(tool)
        with self.assertRaisesRegex(RuntimeError, "after Git commit"):
            await memory.handler({}, self.ctx)
        result = await memory.handler({}, self.ctx)
        self.assertEqual(result["status"], "applied")
        self.assertEqual((propose.await_count, verify.await_count, preserve.await_count), (1, 1, 1))
        self.assertEqual([method for method, _ in tool.calls].count("apply"), 2)
        self.assertEqual(len(tool.writes), 1)
        self.assertEqual(self.connection.cache["head"], tool.current["head"])

    async def test_verification_failure_never_writes_git(self):
        self.connection.set_sources([SOURCE])
        tool = FakeMemoryTool()
        _, verify, preserve = self.stages(tool)
        verify.side_effect = ValueError("invalid review")
        with self.assertRaises(ValueError):
            await memory.handler({}, self.ctx)
        self.assertEqual(tool.writes, [])
        preserve.assert_not_called()
        self.assertIsNone(self.connection.cache)

    async def test_projection_cannot_target_new_owner(self):
        self.connection.bound = False
        with self.assertRaises(ValueError):
            await memory._project_cache(self.connection, CONFIG, snapshot())


if __name__ == "__main__":
    unittest.main()
