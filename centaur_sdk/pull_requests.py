"""Agent-facing CLI for opening pull requests bound to this session.

Run inside a sandbox as ``python -m centaur_sdk.pull_requests create
<owner/repo> --head <branch> --base <branch> --title <title>``. The pull
request is opened as the requesting human, and failed CI is reported back to
this session's thread. Use it instead of ``gh pr create``.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error

from centaur_sdk.tool_sdk import create_pull_request

__all__ = ["create_pull_request", "main"]

_ERROR_BODY_LIMIT = 500
# Codes that mean this session cannot open a bound PR; the agent may fall back.
_FALLBACK_CODES = (
    "slack_session_required",
    "dm_not_supported",
    "github_oauth_required",
    "sandbox_mismatch",
    "sandbox_proxy_unavailable",
    "pull_requests_unsupported",
    "sandbox_claim_required",
)
_FALLBACK_HINT = (
    "This session cannot open a session-bound pull request; "
    "open the pull request as you normally would (for example with `gh pr create`)."
)
# Not fallbacks: the session is catching up, so the same call will work shortly.
_RETRY_HINTS = {
    "requester_not_applied": (
        "The session's GitHub identity is still switching to the current speaker; "
        "retry in a few seconds."
    ),
    "no_active_execution": "The session's turn is still starting; retry in a few seconds.",
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python -m centaur_sdk.pull_requests",
        description="Open a pull request bound to the current session.",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("create", help="open a pull request")
    create.add_argument("repository", help="owner/repo")
    create.add_argument("--head", required=True, help="branch with the changes")
    create.add_argument("--base", required=True, help="branch to merge into")
    create.add_argument("--title", required=True)
    create.add_argument("--body", default="")
    create.add_argument("--draft", action="store_true", help="open as a draft")
    args = parser.parse_args(argv)

    try:
        result = create_pull_request(
            args.repository,
            args.head,
            args.base,
            args.title,
            body=args.body,
            draft=args.draft,
        )
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:_ERROR_BODY_LIMIT]
        sys.stderr.write(f"create failed: HTTP {error.code} {detail}\n")
        retry_hint = next((hint for code, hint in _RETRY_HINTS.items() if code in detail), None)
        if retry_hint:
            sys.stderr.write(f"{retry_hint}\n")
        elif any(code in detail for code in _FALLBACK_CODES):
            sys.stderr.write(f"{_FALLBACK_HINT}\n")
        return 1
    except (OSError, RuntimeError, ValueError) as error:
        sys.stderr.write(f"create failed: {error}\n")
        return 1
    sys.stdout.write(json.dumps(result, separators=(",", ":")) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
