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
        return 1
    except (OSError, RuntimeError, ValueError) as error:
        sys.stderr.write(f"create failed: {error}\n")
        return 1
    sys.stdout.write(json.dumps(result, separators=(",", ":")) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
