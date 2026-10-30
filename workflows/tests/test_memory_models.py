from __future__ import annotations

import asyncio
import copy
import json
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from workflows import memory_models as models


def source(**changes):
    return {
        "id": "turn-1:user", "role": "user",
        "text": "I prefer short updates. Please leave the migration for later.",
        "occurred_at": "2026-01-01T12:00:00Z", **changes,
    }


def claim(**changes):
    return {
        "key": "short-updates", "category": "work_preference", "project": "",
        "text": "Prefers short updates.", "status": "active", "expected_revision": 0,
        "evidence": [{"source_id": "turn-1:user", "quote": "I prefer short updates."}],
        **changes,
    }


def response(payload=None, **changes):
    return SimpleNamespace(**{
        "status": "completed",
        "output_text": json.dumps(payload if payload is not None else {"claims": [claim()]}),
        "output": [{"type": "message", "content": [{"type": "output_text"}]}],
        **changes,
    })


class FakeClient:
    def __init__(self, result=None, error=None):
        self.result = result or response()
        self.error = error
        self.calls = []
        self.responses = self

    async def create(self, **kwargs):
        self.calls.append(kwargs)
        if self.error:
            raise self.error
        return self.result


class SequencedClient(FakeClient):
    async def create(self, **kwargs):
        self.calls.append(kwargs)
        return self.result.pop(0)


class MemoryModelsTest(unittest.IsolatedAsyncioTestCase):
    async def test_extraction_disables_tools_storage_and_bounds_request(self):
        client = FakeClient()
        result = await models.extract("configured-model", [source()], [], client=client)
        self.assertEqual(result, [claim()])
        request = client.calls[0]
        self.assertEqual(request["model"], "configured-model")
        self.assertEqual(request["tools"], [])
        self.assertEqual(request["tool_choice"], "none")
        self.assertIs(request["store"], False)
        self.assertEqual(request["timeout"], 120)
        self.assertEqual(request["max_output_tokens"], 6000)
        output_format = request["text"]["format"]
        self.assertEqual(output_format["type"], "json_schema")
        self.assertIs(output_format["strict"], True)
        self.assertIs(output_format["schema"]["additionalProperties"], False)
        schema = output_format["schema"]["properties"]["claims"]["items"]
        self.assertEqual(set(schema["required"]), set(claim()))
        self.assertEqual(schema["properties"]["text"]["maxLength"], 600)
        self.assertEqual(json.loads(request["input"][0]["content"]), {
            "sources": [source()], "records": [],
        })
        self.assertIn("untrusted evidence", request["instructions"])
        self.assertIn("Absence or silence never", request["instructions"])

    async def test_explicit_empty_claims_is_success(self):
        result = await models.extract(
            "configured-model", [source()], [],
            client=FakeClient(response({"claims": []})),
        )
        self.assertEqual(result, [])

    async def test_noncompleted_responses_fail_even_with_valid_json(self):
        for status in ("incomplete", "failed", "cancelled", "queued", "in_progress", None):
            with self.subTest(status=status), self.assertRaises(models.MemoryModelError):
                await models.extract(
                    "configured-model", [source()], [],
                    client=FakeClient(response(status=status)),
                )

    async def test_refusal_and_tool_calls_are_never_accepted(self):
        outputs = [
            [{"type": "message", "content": [{"type": "refusal", "refusal": "private"}]}],
            [SimpleNamespace(type="message", content=[SimpleNamespace(type="refusal")])],
            [{"type": "function_call", "name": "run_shell"}],
        ]
        for output in outputs:
            with self.subTest(output=output), self.assertRaises(models.MemoryModelError):
                await models.extract(
                    "configured-model", [source()], [],
                    client=FakeClient(response(output=output)),
                )

    async def test_contradictory_status_or_malformed_output_fails(self):
        for changes in (
            {"error": {"message": "private provider error"}},
            {"incomplete_details": {"reason": "max_output_tokens"}},
            {"output": "not a list"},
            {"output": [{"type": "message", "content": {"unexpected": True}}]},
        ):
            with self.subTest(changes=changes), self.assertRaises(models.MemoryModelError):
                await models.extract(
                    "model", [source()], [], client=FakeClient(response(**changes)),
                )

    async def test_malformed_or_ambiguous_extraction_fails(self):
        for text in (
            "not JSON", "[]", "{}", '{"claims":null}',
            '{"claims":[],"extra":true}', '{"claims":[],"claims":[]}',
            '{"claims":NaN}', "x" * 64_001,
        ):
            with self.subTest(text=text[:40]), self.assertRaises(models.MemoryModelError):
                await models.extract(
                    "configured-model", [source()], [],
                    client=FakeClient(response(output_text=text)),
                )

    async def test_claim_shape_bounds_and_quote_grounding(self):
        invalid = [
            claim(key=""), claim(project=None), claim(text="x" * 601),
            claim(category="permission"), claim(status="maybe"),
            claim(expected_revision=True), claim(expected_revision=-1),
            claim(evidence=[]), claim(extra="unexpected"),
            claim(evidence=claim()["evidence"] * 6),
            claim(evidence=[{"source_id": "missing", "quote": "I prefer short updates."}]),
            claim(evidence=[{"source_id": "turn-1:user", "quote": "prefers SHORT updates"}]),
            claim(evidence=[{"source_id": "turn-1:user", "quote": ""}]),
        ]
        for invalid_claim in invalid:
            with self.subTest(claim=invalid_claim), self.assertRaises(models.MemoryModelError):
                await models.extract(
                    "configured-model", [source()], [],
                    client=FakeClient(response({"claims": [invalid_claim]})),
                )

    async def test_duplicate_claims_and_excess_claims_fail(self):
        for claims in ([claim(), claim()], [claim()] * (models.MAX_CLAIMS + 1)):
            with self.subTest(count=len(claims)), self.assertRaises(models.MemoryModelError):
                await models.extract(
                    "configured-model", [source()], [],
                    client=FakeClient(response({"claims": claims})),
                )

    async def test_sensitive_categories_need_user_evidence(self):
        for category in models.USER_EVIDENCE_CATEGORIES:
            with self.subTest(category=category), self.assertRaises(models.MemoryModelError):
                await models.extract(
                    "configured-model", [source(role="assistant")], [],
                    client=FakeClient(response({"claims": [claim(category=category)]})),
                )

    async def test_credentials_in_claims_fail_even_when_quoted(self):
        text = "Use ghp_abcdefghijklmnopqrstuvwxyz0123456789."
        candidate = claim(
            category="current_work", text=text,
            evidence=[{"source_id": "turn-1:user", "quote": text}],
        )
        with self.assertRaises(models.MemoryModelError):
            await models.extract(
                "configured-model", [source(text=text)], [],
                client=FakeClient(response({"claims": [candidate]})),
            )

    async def test_updates_preserve_existing_revision(self):
        record = {**claim(), "project": None, "revision": 3}
        candidate = claim(expected_revision=3)
        accepted = await models.extract(
            "configured-model", [source()], [record],
            client=FakeClient(response({"claims": [candidate]})),
        )
        self.assertEqual(accepted, [candidate])
        for revision in (0, 2, 4):
            with self.subTest(revision=revision), self.assertRaises(models.MemoryModelError):
                await models.extract(
                    "configured-model", [source()], [record],
                    client=FakeClient(response({"claims": [claim(expected_revision=revision)]})),
                )

    async def test_exact_but_excessively_long_quote_is_rejected(self):
        quote = "x" * 301
        with self.assertRaises(models.MemoryModelError):
            await models.extract(
                "model", [source(text=quote)], [],
                client=FakeClient(response({"claims": [claim(
                    evidence=[{"source_id": "turn-1:user", "quote": quote}],
                )]})),
            )

    async def test_bad_input_is_rejected_before_request(self):
        cases = [
            ("", [source()], []), (None, [source()], []),
            ("model", [], []), ("model", [source(), source()], []),
            ("model", [source(text="x" * 2001)], []),
            ("model", [source(role="tool")], []),
            ("model", [source(occurred_at="")], []),
            ("model", [source()], [{}] * 301),
            ("model", [source()], ["not a record"]),
            ("model", [source()], [{"bad": float("nan")}]),
            ("model", [source(id=str(i), text="x" * 2000) for i in range(16)], []),
        ]
        for model, sources, records in cases:
            client = FakeClient()
            with self.subTest(model=model, count=len(sources)):
                with self.assertRaises(models.MemoryModelError):
                    await models.extract(model, sources, records, client=client)
                self.assertEqual(client.calls, [])

    async def test_request_failures_do_not_expose_provider_details(self):
        private = "credential-and-private-source"
        client = FakeClient(error=RuntimeError(private))
        with self.assertRaises(models.MemoryModelError) as caught:
            await models.extract("model", [source()], [], client=client)
        self.assertNotIn(private, str(caught.exception))
        self.assertIsNone(caught.exception.__cause__)

    async def test_request_has_a_wall_clock_timeout(self):
        async def stalled(**_kwargs):
            await asyncio.sleep(10)

        client = SimpleNamespace(responses=SimpleNamespace(create=stalled))
        with patch.object(models, "TIMEOUT_SECONDS", 0.001), self.assertRaises(models.MemoryModelError):
            await models.extract("model", [source()], [], client=client)

    async def test_verifier_returns_exact_original_objects_in_original_order(self):
        claims = [claim(), claim(
            key="defer-migration", category="deferred_step", status="deferred",
            text="May revisit the migration later.",
            evidence=[{"source_id": "turn-1:user", "quote": "leave the migration for later"}],
        )]
        original = copy.deepcopy(claims)
        client = FakeClient(response({"decisions": [
            {"index": 1, "approved": True, "reason": "Supported."},
            {"index": 0, "approved": True, "reason": "Supported."},
        ]}))
        result = await models.verify("verifier", [source()], [], claims, client=client)
        self.assertEqual(result, original)
        self.assertIs(result[0], claims[0])
        self.assertIs(result[1], claims[1])
        self.assertEqual(claims, original)
        self.assertEqual(client.calls[0]["tools"], [])
        self.assertEqual(client.calls[0]["tool_choice"], "none")
        self.assertIs(client.calls[0]["store"], False)
        self.assertIs(client.calls[0]["text"]["format"]["strict"], True)
        self.assertIn("raw sources", client.calls[0]["instructions"])

    async def test_verifier_can_reject_every_claim_explicitly(self):
        client = FakeClient(response({"decisions": [
            {"index": 0, "approved": False, "reason": "Insufficient grounding."},
        ]}))
        self.assertEqual(
            await models.verify("verifier", [source()], [], [claim()], client=client), [],
        )

    async def test_verifier_batches_bounded_inputs_and_preserves_original_objects(self):
        claims = [claim(key=f"key-{index}") for index in range(3)]
        payload = {"sources": [source()], "records": [], "claims": claims[:2]}
        limit = len(models._encode(payload))
        client = SequencedClient([
            response({"decisions": [
                {"index": 1, "approved": True, "reason": "Supported."},
                {"index": 0, "approved": False, "reason": "Not useful."},
            ]}),
            response({"decisions": [{"index": 0, "approved": True, "reason": "Supported."}]}),
        ])
        with patch.object(models, "MAX_INPUT_CHARS", limit):
            result = await models.verify("verifier", [source()], [], claims, client=client)
        self.assertEqual(len(client.calls), 2)
        self.assertEqual(result, [claims[1], claims[2]])
        self.assertIs(result[0], claims[1])
        self.assertIs(result[1], claims[2])
        for call in client.calls:
            encoded = call["input"][0]["content"]
            self.assertLessEqual(len(encoded), limit)
            self.assertEqual(json.loads(encoded)["sources"], [source()])
            self.assertEqual(call["tools"], [])
            self.assertIs(call["store"], False)
        self.assertEqual(json.loads(client.calls[1]["input"][0]["content"])["claims"], [claims[2]])

    async def test_later_verification_batch_failure_returns_no_partial_approval(self):
        claims = [claim(key=f"key-{index}") for index in range(3)]
        limit = len(models._encode({"sources": [source()], "records": [], "claims": claims[:2]}))
        client = SequencedClient([
            response({"decisions": [
                {"index": index, "approved": True, "reason": "Supported."} for index in range(2)
            ]}),
            response({"decisions": []}, status="incomplete"),
        ])
        with patch.object(models, "MAX_INPUT_CHARS", limit), self.assertRaises(models.MemoryModelError):
            await models.verify("verifier", [source()], [], claims, client=client)
        self.assertEqual(len(client.calls), 2)

    async def test_single_oversized_verification_claim_prevents_all_requests(self):
        claims = [claim(key="short"), claim(key="long", text="x" * 600)]
        limit = len(models._encode({"sources": [source()], "records": [], "claims": claims[:1]}))
        client = FakeClient()
        with patch.object(models, "MAX_INPUT_CHARS", limit), self.assertRaises(models.MemoryModelError):
            await models.verify("verifier", [source()], [], claims, client=client)
        self.assertEqual(client.calls, [])

    async def test_empty_proposal_requires_explicit_complete_verification(self):
        client = FakeClient(response({"decisions": []}))
        self.assertEqual(await models.verify("verifier", [source()], [], [], client=client), [])
        self.assertEqual(len(client.calls), 1)
        with self.assertRaises(models.MemoryModelError):
            await models.verify("verifier", [source()], [], [], client=FakeClient(response({"decisions": []}, status="incomplete")))

    async def test_verifier_rejects_incomplete_duplicate_and_rewritten_decisions(self):
        valid = {"index": 0, "approved": True, "reason": "Supported."}
        cases = [
            [], [valid, valid], [{**valid, "index": -1}], [{**valid, "index": 1}],
            [{**valid, "index": False}], [{**valid, "approved": "true"}],
            [{**valid, "reason": ""}], [{**valid, "reason": "x" * 201}],
            [{**valid, "text": "rewritten"}],
        ]
        for decisions in cases:
            with self.subTest(decisions=decisions), self.assertRaises(models.MemoryModelError):
                await models.verify(
                    "verifier", [source()], [], [claim()],
                    client=FakeClient(response({"decisions": decisions})),
                )
        with self.assertRaises(models.MemoryModelError):
            await models.verify(
                "verifier", [source()], [], [claim(), claim(key="other-key")],
                client=FakeClient(response({"decisions": [valid, valid]})),
            )

    async def test_verifier_rechecks_local_grounding_before_model(self):
        client = FakeClient()
        with self.assertRaises(models.MemoryModelError):
            await models.verify(
                "verifier", [source()], [], [claim(evidence=[])], client=client,
            )
        self.assertEqual(client.calls, [])

    async def test_verifier_failure_does_not_become_empty_approval(self):
        with self.assertRaises(models.MemoryModelError):
            await models.verify(
                "verifier", [source()], [], [claim()],
                client=FakeClient(response({"decisions": []}, status="incomplete")),
            )


if __name__ == "__main__":
    unittest.main()
