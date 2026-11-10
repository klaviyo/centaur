"""Verify credential routing, audit redaction, and ambiguous write failures."""

import importlib.util
import json
import sys
from pathlib import Path

import httpx
import pytest

SPEC = importlib.util.spec_from_file_location(
    "sanity_client", Path(__file__).parents[1] / "client.py"
)
client_module = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = client_module
SPEC.loader.exec_module(client_module)


@pytest.fixture
def audit_path(tmp_path, monkeypatch):
    path = tmp_path / "audit.jsonl"
    monkeypatch.setenv("CENTAUR_TOOL_ANALYTICS_LOG_PATH", str(path))
    monkeypatch.setenv("CENTAUR_THREAD_KEY", "slack:T1:C1:123")
    monkeypatch.setattr(client_module, "secret", lambda *_: "SANITY_API_TOKEN")
    return path


def make_client(handler):
    return client_module.SanityClient(
        "project123",
        "content-prod",
        http_client=httpx.Client(
            transport=httpx.MockTransport(handler),
        ),
    )


def test_mutation_records_ids_and_transaction_without_content(audit_path):
    requests = []

    def respond(request):
        requests.append(request)
        return httpx.Response(200, json={"transactionId": "tx-123", "results": [{"id": "article"}]})

    client = make_client(respond)
    client.mutate([{"patch": {"id": "article", "set": {"body": "PRIVATE CONTENT"}}}])
    assert len(requests) == 1
    assert str(requests[0].url).startswith(
        "https://project123.api.sanity.io/v2025-02-19/data/mutate/content-prod?"
    )
    assert requests[0].headers["Authorization"] == "Bearer SANITY_API_TOKEN"
    records = [json.loads(line) for line in audit_path.read_text().splitlines()]
    assert [record["outcome"] for record in records] == ["started", "succeeded"]
    assert records[0]["operation_id"] == records[1]["operation_id"]
    assert records[1]["thread_key"] == "slack:T1:C1:123"
    assert records[1]["document_ids"] == ["article"]
    assert records[1]["transaction_id"] == "tx-123"
    assert records[1]["operation_types"] == ["patch"]
    assert "PRIVATE CONTENT" not in audit_path.read_text()
    assert "SANITY_API_TOKEN" not in audit_path.read_text()


@pytest.mark.parametrize("status", [302, 403, 500])
def test_failed_requests_are_not_retried_or_redirected(audit_path, status):
    requests = []

    def respond(request):
        requests.append(request)
        return httpx.Response(
            status, headers={"Location": "https://attacker.invalid"}, text="UPSTREAM PRIVATE BODY"
        )

    client = make_client(respond)
    with pytest.raises(client_module.SanityError, match=f"HTTP {status}"):
        client.mutate([{"delete": {"id": "article"}}])
    assert len(requests) == 1
    assert "UPSTREAM PRIVATE BODY" not in audit_path.read_text()


def test_timeout_records_unknown_and_does_not_replay(audit_path):
    requests = []

    def respond(request):
        requests.append(request)
        raise httpx.ReadTimeout("PRIVATE URL OR HEADER", request=request)

    with pytest.raises(client_module.SanityError, match="check the outcome") as error:
        make_client(respond).actions(
            [
                {
                    "actionType": "sanity.action.document.publish",
                    "draftId": "drafts.article",
                    "publishedId": "article",
                }
            ]
        )
    assert len(requests) == 1
    assert "PRIVATE" not in str(error.value)
    assert json.loads(audit_path.read_text().splitlines()[-1])["outcome"] == "unknown"


def test_unavailable_audit_sink_blocks_request(audit_path, monkeypatch):
    monkeypatch.setenv("CENTAUR_TOOL_ANALYTICS_LOG_PATH", str(audit_path / "missing"))
    requests = []
    client = make_client(lambda request: requests.append(request))
    with pytest.raises(client_module.SanityError, match="audit sink unavailable"):
        client.mutate([{"create": {"_type": "article", "title": "Private"}}])
    assert requests == []


@pytest.mark.parametrize(
    "project,dataset",
    [
        ("evil.invalid/path", "prod"),
        ("project123", "../other"),
        ("project123", "prod?redirect=bad"),
    ],
)
def test_target_cannot_inject_host_or_path(project, dataset):
    with pytest.raises(client_module.SanityError):
        client_module.SanityClient(project, dataset)


def test_query_parameters_not_logged_and_dry_run_preserved(audit_path):
    requests = []

    def respond(request):
        requests.append(request)
        return httpx.Response(200, json={"result": []})

    client = make_client(respond)
    client.query("*[_type == $type]", {"type": "PRIVATE TYPE"})
    client.actions(
        [
            {
                "actionType": "sanity.action.document.publish",
                "draftId": "drafts.a",
                "publishedId": "a",
            }
        ],
        dry_run=True,
    )
    assert json.loads(requests[0].content)["params"] == {"type": "PRIVATE TYPE"}
    assert requests[1].url.params["dryRun"] == "true"
    assert "PRIVATE TYPE" not in audit_path.read_text()
    assert json.loads(audit_path.read_text().splitlines()[-1])["dry_run"] is True
    assert json.loads(audit_path.read_text().splitlines()[-1])["operation_types"] == [
        "sanity.action.document.publish"
    ]


def test_asset_upload_stream_and_audit(audit_path, tmp_path):
    asset = tmp_path / "private-filename.png"
    asset.write_bytes(b"PRIVATE IMAGE")
    requests = []

    def respond(request):
        requests.append(request)
        return httpx.Response(200, json={"document": {"_id": "image-123"}})

    make_client(respond).upload_asset(str(asset))
    assert requests[0].content == b"PRIVATE IMAGE"
    assert requests[0].headers["Content-Type"] == "image/png"
    assert "private-filename" not in audit_path.read_text()
    assert "PRIVATE IMAGE" not in audit_path.read_text()
    assert json.loads(audit_path.read_text().splitlines()[-1])["document_ids"] == ["image-123"]
