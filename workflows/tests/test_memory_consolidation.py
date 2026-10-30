from __future__ import annotations

import copy
import json
import unittest

from workflows import memory_consolidation as consolidation
from workflows import memory_models as models
from workflows.tests.test_memory_models import (
    FakeClient,
    SequencedClient,
    claim,
    response,
    source,
)


def reflection(**changes):
    return claim(
        key="reflection:short-updates", category="lesson",
        text="Derived guidance: Prefer concise progress updates unless more detail is requested.",
        **changes,
    )


def basis(kind="explicit_correction", index=0):
    return {"index": index, "kind": kind}


def review(*, approved=True, kind="explicit_correction", reason="Supported by a direct correction."):
    return {
        "decisions": [{"index": 0, "approved": approved, "reason": reason}],
        "basis": [basis(kind)],
    }


def previous(candidate=None, **changes):
    candidate = candidate or claim()
    return {
        "id": "memory-1", "category": candidate["category"], "key": candidate["key"],
        "project": candidate["project"] or None, "revision": 2,
        "text": "Prefer detailed progress updates; preserve all deployment constraints.",
        "status": "active", "evidence": [], **changes,
    }


class ConsolidationTest(unittest.IsolatedAsyncioTestCase):
    async def test_explicit_correction_reuses_identity_and_revision(self):
        old = previous()
        correction = source(text="Correction: I prefer short updates. Keep the deployment constraints.")
        candidate = claim(expected_revision=2)
        client = FakeClient(response({"claims": [candidate]}))
        result = await consolidation.reconcile(
            "model", [correction], [old], fresh_source_ids=[correction["id"]], client=client,
        )
        self.assertEqual(result, [candidate])
        self.assertEqual(json.loads(client.calls[0]["input"][0]["content"])["records"], [old])
        self.assertIn("Only an explicit correction", client.calls[0]["instructions"])
        self.assertIn("Preserve", client.calls[0]["instructions"])

    async def test_reconciliation_requires_fresh_cited_evidence(self):
        recent = source(id="recent")
        for fresh in ([recent["id"]], [], ["missing"], "turn-1:user"):
            with self.subTest(fresh=fresh), self.assertRaises(models.MemoryModelError):
                await consolidation.reconcile(
                    "model", [source(), recent], [], fresh_source_ids=fresh,
                    client=FakeClient(response({"claims": [claim()]})),
                )

    async def test_no_change_is_a_valid_reconciliation(self):
        result = await consolidation.reconcile(
            "model", [source()], [], fresh_source_ids={source()["id"]},
            client=FakeClient(response({"claims": []})),
        )
        self.assertEqual(result, [])

    async def test_claim_count_and_revision_are_checked(self):
        for candidates, records in (
            ([claim(key=f"candidate-{index}") for index in range(5)], []),
            ([claim(expected_revision=1)], [previous()]),
            ([claim(expected_revision=2)], []),
        ):
            with self.subTest(candidates=candidates), self.assertRaises(models.MemoryModelError):
                await consolidation.reconcile(
                    "model", [source()], records, fresh_source_ids=[source()["id"]],
                    client=FakeClient(response({"claims": candidates})),
                )

    async def test_adaptive_candidate_cap_controls_schema_prompt_and_validation(self):
        for method, candidates in (
            (consolidation.reconcile, [claim(), claim(key="another")]),
            (consolidation.reflect, [reflection(), {**reflection(), "key": "reflection:another"}]),
        ):
            with self.subTest(method=method.__name__):
                payload = {"claims": candidates[:1]}
                if method is consolidation.reflect:
                    payload["basis"] = [basis()]
                client = FakeClient(response(payload))
                result = await method(
                    "model", [source()], [], fresh_source_ids=[source()["id"]],
                    max_candidates=1, client=client,
                )
                self.assertEqual(result, candidates[:1])
                properties = client.calls[0]["text"]["format"]["schema"]["properties"]
                self.assertEqual(properties["claims"]["maxItems"], 1)
                if method is consolidation.reflect:
                    self.assertEqual(properties["basis"]["maxItems"], 1)
                self.assertIn("at most 1 claims", client.calls[0]["instructions"])
                payload["claims"] = candidates
                if method is consolidation.reflect:
                    payload["basis"] = [basis(), basis(index=1)]
                with self.assertRaises(models.MemoryModelError):
                    await method(
                        "model", [source()], [], fresh_source_ids=[source()["id"]],
                        max_candidates=1, client=FakeClient(response(payload)),
                    )
        self.assertEqual(consolidation._CLAIMS_SCHEMA["properties"]["claims"]["maxItems"], 4)
        self.assertEqual(consolidation._REFLECT_SCHEMA["properties"]["basis"]["maxItems"], 4)

    async def test_invalid_candidate_cap_fails_before_request(self):
        for method in (consolidation.reconcile, consolidation.reflect):
            for limit in (0, 5, True, "1", None):
                client = FakeClient()
                with self.subTest(method=method.__name__, limit=limit), self.assertRaises(models.MemoryModelError):
                    await method(
                        "model", [source()], [], fresh_source_ids=[source()["id"]],
                        max_candidates=limit, client=client,
                    )
                self.assertEqual(client.calls, [])

    async def test_explicit_reflection_has_no_extra_canonical_fields(self):
        candidate = reflection()
        client = FakeClient(response({"claims": [candidate], "basis": [basis()]}))
        result = await consolidation.reflect(
            "model", [source()], [], fresh_source_ids=[source()["id"]], client=client,
        )
        self.assertEqual(result, [candidate])
        self.assertEqual(set(result[0]), set(claim()))
        instructions = client.calls[0]["instructions"]
        self.assertIn("never personality traits", instructions)
        self.assertIn("current request overrides", instructions)
        self.assertIn("fallible", instructions)

    async def test_repeated_pattern_requires_two_distinct_user_sources(self):
        extra = source(id="turn-2:user", text="Please keep this update short as well.")
        candidate = reflection(evidence=claim()["evidence"] + [
            {"source_id": extra["id"], "quote": extra["text"]},
        ])
        valid = await consolidation.reflect(
            "model", [source(), extra], [], fresh_source_ids=[extra["id"]],
            client=FakeClient(response({"claims": [candidate], "basis": [basis("repeated_pattern")]})),
        )
        self.assertEqual(valid, [candidate])
        cases = (
            ([source()], reflection()),
            ([source()], reflection(evidence=claim()["evidence"] * 2)),
            ([source(), {**extra, "role": "assistant"}], candidate),
        )
        for sources, proposed in cases:
            with self.subTest(sources=sources), self.assertRaises(models.MemoryModelError):
                await consolidation.reflect(
                    "model", sources, [], fresh_source_ids=[sources[0]["id"]],
                    client=FakeClient(response({"claims": [proposed], "basis": [basis("repeated_pattern")]})),
                )

    async def test_reflection_requires_user_evidence_and_fresh_evidence(self):
        cases = (
            ([source(role="assistant")], [source()["id"]]),
            ([source(), source(id="fresh")], ["fresh"]),
        )
        for sources, fresh in cases:
            with self.subTest(sources=sources), self.assertRaises(models.MemoryModelError):
                await consolidation.reflect(
                    "model", sources, [], fresh_source_ids=fresh,
                    client=FakeClient(response({"claims": [reflection()], "basis": [basis()]})),
                )

    async def test_prior_reflection_is_not_source_evidence(self):
        candidate = reflection()
        old = previous(candidate, id="prior-derived-record")
        for source_id in (old["id"], "reflection:prior-guidance", "unprovided-record"):
            citation = {"source_id": source_id, "quote": "I prefer short updates."}
            provided = source(id=source_id) if source_id != "unprovided-record" else source()
            with self.subTest(source_id=source_id), self.assertRaises(models.MemoryModelError):
                await consolidation.reflect(
                    "model", [provided], [old], fresh_source_ids=[provided["id"]],
                    client=FakeClient(response({
                        "claims": [reflection(expected_revision=2, evidence=[citation])], "basis": [basis()],
                    })),
                )
        repeated_guidance = source(text="Derived guidance: Prefer concise progress updates.")
        candidate["expected_revision"] = 2
        candidate["evidence"] = [{"source_id": repeated_guidance["id"], "quote": repeated_guidance["text"]}]
        with self.assertRaises(models.MemoryModelError):
            await consolidation.reflect(
                "model", [repeated_guidance], [old], fresh_source_ids=[repeated_guidance["id"]],
                client=FakeClient(response({"claims": [candidate], "basis": [basis()]})),
            )

    async def test_reflection_requires_bounded_lesson_shape(self):
        invalid = []
        for key, value in (
            ("category", "work_preference"), ("key", "short-updates"),
            ("key", "reflection:"), ("text", "Prefers short updates."),
            ("text", "Derived guidance: "), ("status", "completed"),
        ):
            candidate = reflection()
            candidate[key] = value
            invalid.append(candidate)
        for candidate in invalid:
            with self.subTest(candidate=candidate), self.assertRaises(models.MemoryModelError):
                await consolidation.reflect(
                    "model", [source()], [], fresh_source_ids=[source()["id"]],
                    client=FakeClient(response({"claims": [candidate], "basis": [basis()]})),
                )

    async def test_missing_duplicate_or_unknown_basis_fails(self):
        for entries in ([], [basis(), basis()], [basis(index=1)], [basis("guess")], [{"index": True, "kind": "explicit_correction"}]):
            with self.subTest(entries=entries), self.assertRaises(models.MemoryModelError):
                await consolidation.reflect(
                    "model", [source()], [], fresh_source_ids=[source()["id"]],
                    client=FakeClient(response({"claims": [reflection()], "basis": entries})),
                )
        second = reflection()
        second["key"] = "reflection:another"
        with self.assertRaises(models.MemoryModelError):
            await consolidation.reflect(
                "model", [source()], [], fresh_source_ids=[source()["id"]],
                client=FakeClient(response({"claims": [reflection(), second], "basis": [basis(), basis()]})),
            )

    async def test_reflection_empty_result_is_valid(self):
        result = await consolidation.reflect(
            "model", [source()], [], fresh_source_ids=[source()["id"]],
            client=FakeClient(response({"claims": [], "basis": []})),
        )
        self.assertEqual(result, [])

    async def test_verifier_returns_original_objects_and_classifies_independently(self):
        candidate = reflection()
        client = FakeClient(response(review()))
        result = await consolidation.verify_reflections(
            "model", [source()], [], [candidate], fresh_source_ids=[source()["id"]], client=client,
        )
        self.assertIs(result[0], candidate)
        self.assertIn("independently classified", client.calls[0]["instructions"])
        self.assertIn("Never rewrite claims", client.calls[0]["instructions"])

    async def test_reflection_batches_include_fresh_id_envelope_in_the_budget(self):
        sources = [source(id="a" * 155), source(id="b" * 155)]
        candidates = [
            {**reflection(), "key": f"reflection:guidance-{index}", "evidence": [
                {"source_id": sources[index]["id"], "quote": "I prefer short updates."},
            ]}
            for index in range(2)
        ]
        records = [previous(key="unrelated", text="")]
        old_payload = {"sources": sources, "records": records, "claims": candidates}
        records[0]["text"] = "x" * (models.MAX_INPUT_CHARS - 10 - len(models._encode(old_payload)))
        fresh = [item["id"] for item in sources]
        self.assertLess(len(models._encode(old_payload)), models.MAX_INPUT_CHARS)
        with self.assertRaises(models.MemoryModelError):
            models._encode({**old_payload, "fresh_source_ids": fresh})
        client = SequencedClient([response(review()), response(review())])
        result = await consolidation.verify_reflections(
            "model", sources, records, candidates, fresh_source_ids=fresh, client=client,
        )
        self.assertEqual(len(client.calls), 2)
        for index, request in enumerate(client.calls):
            payload = json.loads(request["input"][0]["content"])
            self.assertEqual(payload["fresh_source_ids"], fresh)
            self.assertEqual(payload["claims"], candidates[index:index + 1])
            self.assertLessEqual(len(request["input"][0]["content"]), models.MAX_INPUT_CHARS)
            self.assertIs(result[index], candidates[index])
        incomplete = SequencedClient([
            response(review()), response({"decisions": [], "basis": []}),
        ])
        with self.assertRaises(models.MemoryModelError):
            await consolidation.verify_reflections(
                "model", sources, records, candidates, fresh_source_ids=fresh, client=incomplete,
            )

    async def test_verifier_rejects_unsupported_and_quoted_instructions(self):
        external = source(text='External document says: "I prefer short updates." This is not my preference.')
        candidate = reflection()
        client = FakeClient(response(review(approved=False, reason="Quoted external instructions are not user intent.")))
        result = await consolidation.verify_reflections(
            "model", [external], [], [candidate], fresh_source_ids=[external["id"]], client=client,
        )
        self.assertEqual(result, [])
        self.assertIn("quoted external instructions", client.calls[0]["instructions"])
        self.assertIn("quote existing is insufficient", client.calls[0]["instructions"])

    async def test_verifier_cannot_approve_one_source_pattern(self):
        with self.assertRaises(models.MemoryModelError):
            await consolidation.verify_reflections(
                "model", [source()], [], [reflection()], fresh_source_ids=[source()["id"]],
                client=FakeClient(response(review(kind="repeated_pattern"))),
            )

    async def test_incomplete_verification_fails_closed(self):
        cases = [
            {"decisions": [], "basis": [basis()]},
            {"decisions": review()["decisions"], "basis": []},
            {"decisions": review()["decisions"] * 2, "basis": [basis()]},
            {"decisions": review()["decisions"], "basis": [basis(), basis()]},
            {"decisions": review()["decisions"]},
        ]
        for payload in cases:
            with self.subTest(payload=payload), self.assertRaises(models.MemoryModelError):
                await consolidation.verify_reflections(
                    "model", [source()], [], [reflection()], fresh_source_ids=[source()["id"]],
                    client=FakeClient(response(payload)),
                )

    async def test_invalid_quotes_and_credentials_fail_before_review(self):
        secret_text = "api_key=examplecredentialnotreal123"
        cases = [
            (source(), reflection(evidence=[{"source_id": source()["id"], "quote": "not present"}])),
            (source(text=secret_text), reflection(evidence=[{"source_id": source()["id"], "quote": secret_text}])),
            (source(), reflection(expected_revision=2)),
        ]
        for raw, candidate in cases:
            client = FakeClient()
            with self.subTest(candidate=candidate), self.assertRaises(models.MemoryModelError):
                await consolidation.verify_reflections(
                    "model", [raw], [], [candidate], fresh_source_ids=[raw["id"]], client=client,
                )
            self.assertEqual(client.calls, [])

    async def test_preservation_rejects_whole_plan(self):
        candidate = claim(expected_revision=2)
        old = previous()
        other = previous(key="unrelated", text="x" * 40_000)
        decision = {"approved": False, "reason": "The update drops the existing deployment constraint."}
        client = FakeClient(response(decision))
        result = await consolidation.preserve("model", [source()], [old, other], [candidate], client=client)
        self.assertEqual(result, decision)
        payload = json.loads(client.calls[0]["input"][0]["content"])
        self.assertEqual(payload["affected_records"], [old])
        self.assertEqual(payload["claims"], [candidate])
        self.assertEqual(payload["sources"], [source()])
        self.assertIn("Reject the entire plan", client.calls[0]["instructions"])

    async def test_preservation_never_truncates_affected_record_or_sources(self):
        candidate = claim(expected_revision=2)
        old = previous(text="x" * 31_000)
        client = FakeClient()
        with self.assertRaises(models.MemoryModelError):
            await consolidation.preserve("model", [source()], [old], [candidate], client=client)
        self.assertEqual(client.calls, [])
        sources = [source(id=f"source-{index}", text="x" * 2_000) for index in range(16)] + [source()]
        with self.assertRaises(models.MemoryModelError):
            await consolidation.preserve("model", sources, [], [claim()], client=client)
        self.assertEqual(client.calls, [])

    async def test_fit_helper_matches_full_preservation_payload(self):
        sources = [source(id=f"source-{index}", text="x" * 2_000) for index in range(8)]
        proposed = [claim(
            key=f"update-{index}", expected_revision=2, text="x" * 600,
            evidence=[{"source_id": sources[0]["id"], "quote": "x" * 300}] * 5,
        ) for index in range(4)]
        records = [previous(
            item, text="x" * 600,
            evidence=[{"source_id": "h" * 160, "quote": "x" * 300, "window_id": "w" * 160}] * 5,
        ) for item in proposed]
        original = copy.deepcopy((sources, records, proposed))
        self.assertFalse(consolidation.preservation_fits(sources, records, proposed))
        client = FakeClient(response({"approved": True, "reason": "Preserved"}))
        with self.assertRaises(models.MemoryModelError):
            await consolidation.preserve("model", sources, records, proposed, client=client)
        self.assertEqual(client.calls, [])
        self.assertTrue(consolidation.preservation_fits(sources, records, proposed[:1]))
        await consolidation.preserve("model", sources, records, proposed[:1], client=client)
        payload = json.loads(client.calls[0]["input"][0]["content"])
        self.assertEqual(payload, {
            "sources": sources, "affected_records": records[:1], "claims": proposed[:1],
        })
        self.assertEqual((sources, records, proposed), original)

    async def test_fit_helper_leaves_oversized_single_record_fail_closed(self):
        records = [previous(text="x" * 31_000)]
        candidates = [claim(expected_revision=2)]
        self.assertFalse(consolidation.preservation_fits([source()], records, candidates))
        client = FakeClient()
        with self.assertRaises(models.MemoryModelError):
            await consolidation.preserve("model", [source()], records, candidates, client=client)
        self.assertEqual(client.calls, [])
        self.assertTrue(consolidation.preservation_fits([], records, []))

    async def test_preservation_rejects_malformed_partial_or_sensitive_reviews(self):
        for decision in (
            {"approved": True}, {"approved": 1, "reason": "Okay"},
            {"approved": True, "reason": "", "replacement": []},
            {"approved": True, "reason": "api_key=examplecredentialnotreal123"},
        ):
            with self.subTest(decision=decision), self.assertRaises(models.MemoryModelError):
                await consolidation.preserve(
                    "model", [source()], [], [claim()], client=FakeClient(response(decision)),
                )

    async def test_empty_plans_skip_model_reviews(self):
        client = FakeClient()
        self.assertEqual(
            await consolidation.preserve("model", [], [], [], client=client),
            {"approved": True, "reason": "no_change"},
        )
        self.assertEqual(await consolidation.verify_reflections(
            "model", [source()], [], [], fresh_source_ids=[source()["id"]], client=client,
        ), [])
        self.assertEqual(client.calls, [])

    async def test_model_stages_disable_tools_and_storage(self):
        cases = (
            (consolidation.reconcile, (), {"fresh_source_ids": [source()["id"]]}, {"claims": []}),
            (consolidation.reflect, (), {"fresh_source_ids": [source()["id"]]}, {"claims": [], "basis": []}),
            (consolidation.verify_reflections, ([reflection()],), {"fresh_source_ids": [source()["id"]]}, review()),
            (consolidation.preserve, ([claim()],), {}, {"approved": True, "reason": "Preserved"}),
        )
        for method, args, kwargs, payload in cases:
            with self.subTest(method=method.__name__):
                client = FakeClient(response(payload))
                await method("model", [source()], [], *args, **kwargs, client=client)
                request = client.calls[0]
                self.assertEqual(request["tools"], [])
                self.assertEqual(request["tool_choice"], "none")
                self.assertIs(request["store"], False)
                self.assertTrue(request["text"]["format"]["strict"])

    async def test_oversized_stage_inputs_fail_before_request(self):
        records = [previous(text="x" * 31_000)]
        for method, args in ((consolidation.reconcile, ()), (consolidation.reflect, ()), (consolidation.verify_reflections, ([reflection()],))):
            client = FakeClient()
            with self.subTest(method=method.__name__), self.assertRaises(models.MemoryModelError):
                await method(
                    "model", [source()], copy.deepcopy(records), *args,
                    fresh_source_ids=[source()["id"]], client=client,
                )
            self.assertEqual(client.calls, [])


if __name__ == "__main__":
    unittest.main()
