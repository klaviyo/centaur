"""Thin JSON CLI. Apply reads evidence from stdin, never command-line arguments."""

from __future__ import annotations

import argparse
import json
import sys

from .client import MAX_INPUT_BYTES, MemoryClient, MemoryError


def app() -> None:
    parser = argparse.ArgumentParser(description="Private Git-backed personal memory")
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("snapshot", "history", "apply", "search", "get", "explain"):
        command = commands.add_parser(name)
        command.add_argument("--repo", required=True, help="Private repository owner/name")
        command.add_argument("--branch", default="main")
        if name == "search":
            command.add_argument("query")
            command.add_argument("--limit", type=int, default=10)
        if name == "history":
            command.add_argument("--head", help="Pinned Git commit SHA; defaults to the branch head")
            command.add_argument("--limit", type=int, default=12, help="Recent episodes (1 to 24)")
            command.add_argument("--source-limit", type=int, default=24, help="Unique source excerpts (1 to 50)")
        if name in {"get", "explain"}:
            command.add_argument("record_id")
    args = parser.parse_args()
    client = MemoryClient()
    try:
        if args.command == "apply":
            raw = sys.stdin.buffer.read(MAX_INPUT_BYTES + 1)
            if len(raw) > MAX_INPUT_BYTES:
                raise MemoryError("proposal exceeds size limit")
            try:
                payload = json.loads(raw)
            except (ValueError, UnicodeError):
                raise MemoryError("stdin must contain a JSON proposal") from None
            required = {"expected_head", "window_id", "sources", "claims"}
            if not isinstance(payload, dict) or not required <= payload.keys() or payload.keys() - required - {"processed_through", "checkpoint_source_ids"}:
                raise MemoryError("invalid proposal fields")
            result = client.apply(repo=args.repo, branch=args.branch, **payload)
        elif args.command == "search":
            result = client.search(args.repo, args.branch, args.query, args.limit)
        elif args.command == "history":
            result = client.history(args.repo, args.branch, args.head, args.limit, args.source_limit)
        elif args.command in {"get", "explain"}:
            result = getattr(client, args.command)(args.repo, args.branch, args.record_id)
        else:
            result = client.snapshot(args.repo, args.branch)
        print(json.dumps(result, ensure_ascii=False, indent=2))
    except MemoryError as exc:
        print(json.dumps({"error": str(exc)}), file=sys.stderr)
        raise SystemExit(1) from None


if __name__ == "__main__":
    app()
