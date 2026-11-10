"""Call Sanity through the credential proxy and record content-free audit events."""

from __future__ import annotations

import json
import mimetypes
import os
import re
import time
import uuid
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import httpx

from centaur_sdk.tool_sdk import secret


class SanityError(RuntimeError):
    """Report an operation failure without exposing credentials or content."""


def _setting(name: str, default: str = "") -> str:
    return os.environ.get(name, default)  # noqa: TID251 -- non-secret runtime settings


def _document_ids(value: Any) -> list[str]:
    if not isinstance(value, dict):
        return []
    return sorted(
        {
            candidate
            for key, candidate in value.items()
            if key in {"_id", "id", "draftId", "publishedId", "documentId"}
            and isinstance(candidate, str)
            and re.fullmatch(r"[\w.-]{1,256}", candidate)
        }
    )


def _mutation_ids(mutations: list[dict[str, Any]]) -> list[str]:
    return sorted(
        {
            document_id
            for mutation in mutations
            for value in mutation.values()
            for document_id in _document_ids(value)
        }
    )


class SanityClient:
    """Use one fixed project and dataset for the lifetime of a client."""

    def __init__(
        self,
        project_id: str | None = None,
        dataset: str | None = None,
        *,
        http_client: httpx.Client | None = None,
    ) -> None:
        self.project_id = project_id or _setting("SANITY_PROJECT_ID")
        self.dataset = dataset or _setting("SANITY_DATASET")
        if not re.fullmatch(r"[a-z0-9]+", self.project_id):
            raise SanityError("Set SANITY_PROJECT_ID to a valid project ID")
        if not re.fullmatch(r"[a-z0-9_-]+", self.dataset):
            raise SanityError("Set SANITY_DATASET to a valid dataset name")
        self.http = http_client or httpx.Client(timeout=60, follow_redirects=False)
        self._owns_http = http_client is None
        self._base_url = f"https://{self.project_id}.api.sanity.io/v2025-02-19"

    def _audit(self, event: dict[str, Any]) -> None:
        record = {
            "timestamp": datetime.now(UTC).isoformat(),
            "level": "info",
            "service": "sandbox",
            "event": "sanity_operation",
            "project_id": self.project_id,
            "dataset": self.dataset,
            "thread_key": _setting("CENTAUR_THREAD_KEY"),
            **event,
        }
        path = _setting("CENTAUR_TOOL_ANALYTICS_LOG_PATH", "/proc/1/fd/2")
        try:
            with open(path, "a", encoding="utf-8") as log_file:
                log_file.write(json.dumps(record, separators=(",", ":")) + "\n")
                log_file.flush()
        except OSError:
            raise SanityError(
                "Sanity audit sink unavailable; if a request already started, "
                "check its outcome before retrying"
            ) from None

    def _request(
        self,
        operation: str,
        path: str,
        *,
        body: dict[str, Any] | None = None,
        params: dict[str, str] | None = None,
        content: Any = None,
        content_type: str | None = None,
        document_ids: list[str] | None = None,
        operation_types: list[str] | None = None,
        dry_run: bool = False,
    ) -> dict[str, Any]:
        event = {
            "operation_id": str(uuid.uuid4()),
            "operation": operation,
            "document_ids": document_ids or [],
            "operation_types": operation_types or [],
            "dry_run": dry_run,
        }
        self._audit({**event, "outcome": "started"})
        started_at = time.monotonic()
        status_code = None
        result: dict[str, Any] = {}
        outcome = "unknown"
        try:
            headers = {"Authorization": f"Bearer {secret('SANITY_API_TOKEN', 'SANITY_API_TOKEN')}"}
            if content_type:
                headers["Content-Type"] = content_type
            response = self.http.post(
                f"{self._base_url}/{path}",
                headers=headers,
                params=params,
                json=body if content is None else None,
                content=content,
                follow_redirects=False,
            )
            status_code = response.status_code
            if not response.is_success:
                outcome = "rejected" if 400 <= status_code < 500 else "unknown"
                raise SanityError(f"Sanity returned HTTP {status_code}; no automatic retry")
            parsed = response.json()
            if not isinstance(parsed, dict):
                raise ValueError("unexpected response")
            result = parsed
            outcome = "succeeded"
            return result
        except httpx.HTTPError:
            raise SanityError(
                "Sanity transport failed; check the outcome before retrying"
            ) from None
        except ValueError:
            raise SanityError(
                "Sanity returned an invalid response; check the outcome before retrying"
            ) from None
        finally:
            response_ids = _document_ids(result.get("document"))
            for mutation in (
                result.get("results", []) if isinstance(result.get("results"), list) else []
            ):
                response_ids.extend(_document_ids(mutation))
            transaction_id = result.get("transactionId")
            self._audit(
                {
                    **event,
                    "outcome": outcome,
                    "status_code": status_code,
                    "duration_ms": round((time.monotonic() - started_at) * 1000, 3),
                    "document_ids": sorted(set(event["document_ids"] + response_ids)),
                    "transaction_id": transaction_id
                    if isinstance(transaction_id, str)
                    and re.fullmatch(r"[\w.-]{1,256}", transaction_id)
                    else None,
                }
            )

    def query(
        self, query: str, params: dict[str, Any] | None = None, perspective: str = "raw"
    ) -> dict[str, Any]:
        """Run GROQ without putting the query or parameters into audit logs."""
        return self._request(
            "query",
            f"data/query/{self.dataset}",
            body={
                "query": query,
                "params": params or {},
            },
            params={"perspective": perspective},
        )

    def mutate(self, mutations: list[dict[str, Any]], dry_run: bool = False) -> dict[str, Any]:
        """Apply Sanity mutations once, or validate them with dry_run."""
        return self._request(
            "mutate",
            f"data/mutate/{self.dataset}",
            body={"mutations": mutations},
            params={"returnIds": "true", "dryRun": str(dry_run).lower()},
            document_ids=_mutation_ids(mutations),
            operation_types=sorted(
                {
                    name
                    for mutation in mutations
                    for name in mutation
                    if name in {"create", "createIfNotExists", "createOrReplace", "patch", "delete"}
                }
            ),
            dry_run=dry_run,
        )

    def actions(self, actions: list[dict[str, Any]], dry_run: bool = False) -> dict[str, Any]:
        """Execute document or release actions, including publication, once."""
        return self._request(
            "actions",
            f"data/actions/{self.dataset}",
            body={"actions": actions},
            params={"dryRun": str(dry_run).lower()},
            document_ids=sorted({item for action in actions for item in _document_ids(action)}),
            operation_types=sorted(
                {
                    action["actionType"]
                    for action in actions
                    if isinstance(action.get("actionType"), str)
                    and re.fullmatch(r"sanity\.action\.[a-zA-Z.]{1,128}", action["actionType"])
                }
            ),
            dry_run=dry_run,
        )

    def upload_asset(self, file_path: str, kind: str = "images") -> dict[str, Any]:
        """Upload a local image or file and return its Sanity asset document."""
        if kind not in {"images", "files"}:
            raise SanityError("Asset kind must be images or files")
        asset_path = Path(file_path)
        with asset_path.open("rb") as asset:
            return self._request(
                "upload_asset",
                f"assets/{kind}/{self.dataset}",
                content=asset,
                content_type=mimetypes.guess_type(asset_path.name)[0] or "application/octet-stream",
                params={"filename": asset_path.name},
            )

    def close(self) -> None:
        """Close an internally owned HTTP client."""
        if self._owns_http:
            self.http.close()


def _client() -> SanityClient:
    return SanityClient()
