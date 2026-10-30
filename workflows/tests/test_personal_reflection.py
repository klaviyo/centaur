from __future__ import annotations

import copy
import os
import unittest
from contextlib import ExitStack
from unittest.mock import AsyncMock, patch

from workflows import personal_memory as memory
from workflows import personal_reflection as reflection
from workflows.tests.test_personal_memory import (
    CLAIM,
    ENV,
    REFLECTION,
    SOURCE,
    FakeConnection,
    FakeContext,
    FakeMemoryTool,
    source,
)


class ReflectionConfigurationTests(unittest.TestCase):
    def test_daily_cadence_and_separate_opt_out(self):
        self.assertEqual(reflection._interval({}), 86400)
        self.assertEqual(reflection._interval({"CENTAUR_MEMORY_REFLECTION_INTERVAL_SECONDS": "invalid"}), 86400)
        self.assertEqual(reflection._interval({"CENTAUR_MEMORY_REFLECTION_INTERVAL_SECONDS": "0"}), 60)
        self.assertFalse(reflection._enabled({}))
        self.assertTrue(reflection._enabled(ENV))
        self.assertFalse(reflection._enabled({**ENV, "CENTAUR_MEMORY_REFLECTION_ENABLED": "false"}))
        self.assertFalse(reflection._enabled({**ENV, "CENTAUR_MEMORY_ENABLED": "false", "CENTAUR_MEMORY_REFLECTION_ENABLED": "true"}))
        self.assertEqual(memory.MAX_BATCHES, {"consolidation": 8, "reflection": 32})


class ReflectionWorkflowTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.connection = FakeConnection()
        self.ctx = FakeContext(self.connection)
        self.env = patch.dict(os.environ, {**ENV, "CENTAUR_MEMORY_REFLECTION_ENABLED": "true"}, clear=False)
        self.env.start()
        self.addCleanup(self.env.stop)

    def stages(self, tool, *, candidates=None, approved=None, preservation=None):
        stack = ExitStack()
        stack.enter_context(patch.object(memory, "_tool", new=tool))
        propose = stack.enter_context(patch.object(memory.memory_consolidation, "reflect", new_callable=AsyncMock, return_value=copy.deepcopy(candidates if candidates is not None else [REFLECTION])))
        verify = stack.enter_context(patch.object(memory.memory_consolidation, "verify_reflections", new_callable=AsyncMock, return_value=copy.deepcopy(approved if approved is not None else [REFLECTION])))
        preserve = stack.enter_context(patch.object(memory.memory_consolidation, "preserve", new_callable=AsyncMock, return_value=preservation or {"approved": True, "reason": "Supported."}))
        self.addCleanup(stack.close)
        return propose, verify, preserve

    async def test_disabling_reflection_does_not_touch_store_or_tools(self):
        with patch.dict(os.environ, {"CENTAUR_MEMORY_REFLECTION_ENABLED": "false"}), patch.object(memory, "_tool", new_callable=AsyncMock) as tool:
            self.assertEqual(await reflection.handler({}, self.ctx), {"status": "disabled"})
            tool.assert_not_called()
        self.assertEqual(self.connection.calls, [])
        self.assertIsNotNone(memory.Config.read())

    async def test_idle_dreaming_has_no_history_model_or_git_write(self):
        tool = FakeMemoryTool()
        propose, verify, preserve = self.stages(tool)
        result = await reflection.handler({}, self.ctx)
        self.assertEqual(result["status"], "no_new_sources")
        self.assertEqual([method for method, _ in tool.calls], ["snapshot", "snapshot"])
        self.assertEqual(tool.writes, [])
        self.assertEqual(self.connection.cache["head"], tool.current["head"])
        propose.assert_not_called()
        verify.assert_not_called()
        preserve.assert_not_called()

    async def test_scheduler_targets_only_daily_loop_and_cannot_override_binding(self):
        tool = FakeMemoryTool()
        self.stages(tool)
        scheduled = {"metadata": {"source": "workflow_schedule", "workflow_name": "personal_reflection", "no_delivery": True}}
        self.assertEqual((await reflection.handler(scheduled, self.ctx))["status"], "no_new_sources")
        for params in ({"repository": "another/repo"}, {"metadata": {**scheduled["metadata"], "workflow_name": "personal_memory"}}):
            with self.subTest(params=params), self.assertRaises(ValueError):
                await reflection.handler(params, self.ctx)

    async def test_daily_pipeline_uses_reflection_specific_proposal_review_and_raw_sources(self):
        self.connection.set_sources([SOURCE])
        tool = FakeMemoryTool()
        responses = [
            {"claims": [REFLECTION], "basis": [{"index": 0, "kind": "explicit_correction"}]},
            {"decisions": [{"index": 0, "approved": True, "reason": "Direct boundary."}], "basis": [{"index": 0, "kind": "explicit_correction"}]},
            {"approved": True, "reason": "Preserves the boundary."},
        ]
        with patch.object(memory, "_tool", new=tool), patch.object(memory.memory_models, "_request", new_callable=AsyncMock, side_effect=responses) as request, patch.object(memory.memory_models, "verify", new_callable=AsyncMock) as generic_verify, patch.object(memory.memory_consolidation, "reconcile", new_callable=AsyncMock) as reconcile:
            result = await reflection.handler({}, self.ctx)
        self.assertEqual(result["claims"], 1)
        self.assertEqual([call.args[3] for call in request.await_args_list], ["personal_memory_reflection", "personal_memory_reflection_verification", "personal_memory_preservation"])
        self.assertEqual(tool.writes[0]["sources"], [SOURCE])
        self.assertEqual(tool.writes[0]["claims"], [REFLECTION])
        self.assertTrue(tool.writes[0]["window_id"].startswith("personal-reflection:"))
        self.assertEqual(tool.writes[0]["checkpoint_source_ids"], [SOURCE["id"]])
        self.assertEqual(memory.processed_source_ids(tool.current["state"], "reflection"), [SOURCE["id"]])
        self.assertEqual(memory.processed_source_ids(tool.current["state"]), [])
        generic_verify.assert_not_called()
        reconcile.assert_not_called()

    async def test_daily_proposer_rejects_facts_before_any_write(self):
        self.connection.set_sources([SOURCE])
        tool = FakeMemoryTool()
        with (
            patch.object(memory, "_tool", new=tool),
            patch.object(memory.memory_models, "_request", new_callable=AsyncMock, return_value={"claims": [CLAIM], "basis": [{"index": 0, "kind": "explicit_correction"}]}),
            self.assertRaises(memory.memory_models.MemoryModelError),
        ):
            await reflection.handler({}, self.ctx)
        self.assertEqual(tool.writes, [])
        self.assertIsNone(self.connection.cache)

    async def test_hourly_ingestion_and_daily_reflection_have_independent_checkpoints(self):
        self.connection.set_sources([SOURCE])
        tool = FakeMemoryTool()
        self.stages(tool, candidates=[])
        with patch.object(memory.memory_consolidation, "reconcile", new_callable=AsyncMock, return_value=[]):
            hourly = await memory.handler({}, FakeContext(self.connection))
        self.assertEqual(hourly["sources"], 1)
        added = source(2)
        self.connection.set_sources([SOURCE, added])
        daily = await reflection.handler({}, self.ctx)
        self.assertEqual(daily["sources"], 2)
        state = tool.current["state"]
        self.assertEqual(memory.processed_source_ids(state), [SOURCE["id"]])
        self.assertEqual(memory.processed_source_ids(state, "reflection"), sorted([SOURCE["id"], added["id"]]))
        legacy_seen = {source_id for run in state["runs"].values() for source_id in run["source_ids"]}
        self.assertNotIn(added["id"], legacy_seen)
        idle = await reflection.handler({}, FakeContext(self.connection))
        self.assertEqual(idle["status"], "no_new_sources")
        self.assertEqual(len(tool.writes), 2)

    async def test_daily_backlog_is_bounded_and_historical_evidence_never_advances_checkpoint(self):
        events = [source(index) for index in range(1, 14)]
        historical = {**source(0), "id": "message:earlier-evidence"}
        self.connection.set_sources(events)
        tool = FakeMemoryTool(historical=[historical])
        propose, verify, _ = self.stages(tool, candidates=[])
        with patch.object(memory, "MAX_BATCHES", {"consolidation": 8, "reflection": 2}):
            result = await reflection.handler({}, self.ctx)
        self.assertEqual(result["sources"], 12)
        self.assertTrue(result["batch_limit_reached"])
        self.assertEqual(propose.await_count, 2)
        verify.assert_not_called()
        self.assertTrue(all(historical in write["sources"] for write in tool.writes))
        self.assertTrue(all(historical["id"] not in write["checkpoint_source_ids"] for write in tool.writes))
        self.assertNotIn(events[12]["id"], memory.processed_source_ids(tool.current["state"], "reflection"))
        self.assertEqual(memory.processed_source_ids(tool.current["state"]), [])

    async def test_daily_and_hourly_share_lock_but_daily_wait_has_own_durable_names(self):
        tool = FakeMemoryTool()
        self.stages(tool)
        self.connection.lock_results = [False, True]
        result = await reflection.handler({}, self.ctx)
        self.assertEqual(result["status"], "no_new_sources")
        self.assertEqual(self.ctx.sleeps, [("reflection_v2_wait_for_lock_0", 30)])
        lock_args = [args for query, args in self.connection.calls if "try_advisory_lock" in query]
        self.assertEqual(lock_args, [("personal_memory:slack:DEXAMPLE:",)] * 2)

    async def test_daily_rejected_plan_records_no_change_without_partial_guidance(self):
        self.connection.set_sources([SOURCE])
        tool = FakeMemoryTool()
        self.stages(tool, preservation={"approved": False, "reason": "Unjustified generalization."})
        result = await reflection.handler({}, self.ctx)
        self.assertEqual(result["claims"], 0)
        self.assertEqual(tool.writes[0]["claims"], [])
        self.assertEqual(memory.processed_source_ids(tool.current["state"], "reflection"), [SOURCE["id"]])

    async def test_daily_cache_retry_preserves_durable_model_and_git_steps(self):
        self.connection.set_sources([SOURCE])
        self.connection.fail_cache = True
        tool = FakeMemoryTool()
        propose, verify, preserve = self.stages(tool)
        with self.assertRaisesRegex(RuntimeError, "database unavailable"):
            await reflection.handler({}, self.ctx)
        self.connection.fail_cache = False
        tool.current["head"] = "e" * 40
        tool.current["context"] = "Fresh reflection and current project facts."
        await reflection.handler({}, self.ctx)
        self.assertEqual(self.connection.cache["head"], "e" * 40)
        self.assertEqual((propose.await_count, verify.await_count, preserve.await_count), (1, 1, 1))
        self.assertEqual([method for method, _ in tool.calls].count("apply"), 1)
        self.assertEqual(len(tool.writes), 1)


if __name__ == "__main__":
    unittest.main()
