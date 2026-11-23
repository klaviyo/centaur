"""Agent-facing CLI for pull-request session bindings.

Run inside a sandbox as ``python -m centaur_sdk.pull_requests register
<owner/repo> <number>`` right after opening a pull request, so GitHub
follow-ups (reviews, mentions, red CI) route back to this same session.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error

from centaur_sdk.tool_sdk import register_pull_request


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python -m centaur_sdk.pull_requests",
        description="Register a pull request this session opened for same-session follow-ups.",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    register = commands.add_parser("register", help="bind a PR to the current session")
    register.add_argument("repository", help="owner/repo")
    register.add_argument("number", type=int, help="pull request number")
    args = parser.parse_args(argv)

    try:
        binding = register_pull_request(args.repository, args.number)
    except urllib.error.HTTPError as error:
        sys.stderr.write(f"register failed: HTTP {error.code}\n")
        return 1
    except (OSError, RuntimeError, ValueError) as error:
        sys.stderr.write(f"register failed: {error}\n")
        return 1
    sys.stdout.write(json.dumps(binding, separators=(",", ":")) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
