from __future__ import annotations

import copy
import datetime as dt
import json
import os
import unittest
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


def snapshot(head="a" * 40):
    return {"head": head, "state": {"records": [], "runs": {}, "schema_version": 1, "processed_through": None}, "context": "Evidence only."}


class FakeConnection:
    def __init__(self):
        self.locked = True
        self.bound = True
        self.calls = []
        self.rows = []
        self.cache = None
        self.fail_cache = False

    async def fetchval(self, query, *args):
        self.calls.append((query, args))
        if "try_advisory_lock" in query:
            return self.locked
        if "EXISTS" in query:
            return self.bound
        return True

    async def fetch(self, query, *args):
        self.calls.append((query, args))
        return self.rows

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
        return self.connection

    async def __aexit__(self, *_args):
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

    async def step(self, name, fn):
        if name not in self.checkpoints:
            self.checkpoints[name] = copy.deepcopy(await fn())
        return copy.deepcopy(self.checkpoints[name])

    def log(self, event, **fields):
        self.logs.append((event, fields))


class ConfigurationTests(unittest.TestCase):
    def test_disabled_is_default_and_needs_no_credentials(self):
        self.assertIsNone(memory.Config.read({}))
        self.assertEqual(memory._interval({}), 3600)
        self.assertEqual(memory._interval({"CENTAUR_MEMORY_INTERVAL_SECONDS": "0"}), 60)

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

    def test_exact_source_checkpoints_do_not_depend_on_monotonic_timestamps(self):
        state = {"runs": {"one": {"source_ids": ["event:10", "message:older"]}, "two": {"source_ids": ["event:9", "message:older"]}}}
        self.assertEqual(memory.processed_source_ids(state), ["event:10", "event:9", "message:older"])

    def test_model_context_is_bounded_and_prefers_updated_identity(self):
        records = [{"id": str(i), "category": "project_constraint", "key": str(i), "project": None, "text": "x" * 600, "status": "active", "revision": 1, "updated_at": str(i)} for i in range(200)]
        proposed = [{**CLAIM, "key": "0", "project": ""}]
        selected = memory.model_records(records, [SOURCE], proposed)
        self.assertEqual(selected[0]["key"], "0")
        self.assertLess(len(json.dumps(selected)), 8500)


class WorkflowTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.connection = FakeConnection()
        self.ctx = FakeContext(self.connection)
        self.env = patch.dict(os.environ, ENV, clear=False)
        self.env.start()
        self.addCleanup(self.env.stop)

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
        self.connection.locked = False
        scheduled = {"metadata": {"source": "workflow_schedule", "workflow_name": "personal_memory", "no_delivery": True}}
        self.assertEqual(await memory.handler(scheduled, self.ctx), {"status": "busy"})

    async def test_busy_and_mismatched_binding_prevent_tool_access(self):
        with patch.object(memory, "_tool", new_callable=AsyncMock) as tool:
            self.connection.locked = False
            self.assertEqual(await memory.handler({}, self.ctx), {"status": "busy"})
            self.connection.locked = True
            self.connection.bound = False
            with self.assertRaises(ValueError):
                await memory.handler({}, self.ctx)
            tool.assert_not_called()
            self.assertIn("pg_advisory_unlock", self.connection.calls[-1][0])

    async def test_sql_is_scoped_and_late_commits_remain_eligible(self):
        self.connection.rows = [{**SOURCE, "occurred_at": dt.datetime(2026, 1, 1, tzinfo=dt.UTC)}]
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

    async def test_no_new_sources_refreshes_git_without_model_calls(self):
        fresh = snapshot("b" * 40)
        with patch.object(memory, "_tool", new_callable=AsyncMock, side_effect=[snapshot(), fresh]) as tool, patch.object(memory.memory_models, "extract", new_callable=AsyncMock) as extract:
            result = await memory.handler({}, self.ctx)
        self.assertEqual(result["status"], "no_new_sources")
        self.assertEqual(self.connection.cache["head"], fresh["head"])
        self.assertEqual(tool.await_count, 2)
        extract.assert_not_called()

    async def test_conflict_never_publishes_or_advances_checkpoint(self):
        conflict = {**snapshot("c" * 40), "status": "conflict"}
        with patch.object(memory, "_tool", new_callable=AsyncMock, side_effect=[snapshot(), conflict]), patch.object(memory, "_read_sources", new_callable=AsyncMock, return_value=[SOURCE]), patch.object(memory.memory_models, "extract", new_callable=AsyncMock, return_value=[CLAIM]), patch.object(memory.memory_models, "verify", new_callable=AsyncMock, return_value=[CLAIM]):
            result = await memory.handler({}, self.ctx)
        self.assertEqual(result["status"], "conflict")
        self.assertIsNone(self.connection.cache)

    async def test_resume_after_git_success_reloads_latest_before_cache(self):
        applied = {**snapshot("b" * 40), "status": "applied"}
        newest = snapshot("c" * 40)
        self.connection.fail_cache = True
        tool = AsyncMock(side_effect=[snapshot(), applied, applied, newest])
        with patch.object(memory, "_tool", tool), patch.object(memory, "_read_sources", new_callable=AsyncMock, return_value=[SOURCE]), patch.object(memory.memory_models, "extract", new_callable=AsyncMock, return_value=[CLAIM]) as extract, patch.object(memory.memory_models, "verify", new_callable=AsyncMock, return_value=[CLAIM]) as verify:
            with self.assertRaises(RuntimeError):
                await memory.handler({}, self.ctx)
            self.connection.fail_cache = False
            result = await memory.handler({}, self.ctx)
        self.assertEqual(result["status"], "applied")
        self.assertEqual(self.connection.cache["head"], newest["head"])
        self.assertEqual(extract.await_count, 1)
        self.assertEqual(verify.await_count, 1)
        self.assertEqual([call.args[1] for call in tool.await_args_list].count("apply"), 1)

    async def test_verification_failure_never_writes_git(self):
        with patch.object(memory, "_tool", new_callable=AsyncMock, return_value=snapshot()) as tool, patch.object(memory, "_read_sources", new_callable=AsyncMock, return_value=[SOURCE]), patch.object(memory.memory_models, "extract", new_callable=AsyncMock, return_value=[CLAIM]), patch.object(memory.memory_models, "verify", new_callable=AsyncMock, side_effect=ValueError("invalid review")), self.assertRaises(ValueError):
            await memory.handler({}, self.ctx)
        self.assertEqual(tool.await_count, 1)
        self.assertIsNone(self.connection.cache)

    async def test_projection_cannot_target_new_owner(self):
        self.connection.bound = False
        with self.assertRaises(ValueError):
            await memory._project_cache(self.connection, CONFIG, snapshot())


if __name__ == "__main__":
    unittest.main()
