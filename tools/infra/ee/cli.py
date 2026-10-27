"""Run remote workspace operations through the sandbox's paired SSH proxy."""
import argparse
import contextlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile


@contextlib.contextmanager
def connection():
    """Materialize only the disposable client key and pinned proxy identity."""
    config = json.loads(os.environ["CENTAUR_EE_SSH"])
    with tempfile.TemporaryDirectory(prefix="centaur-ee-") as directory:
        root = Path(directory)
        key = root / "key"
        key.write_text(config["private_key"])
        key.chmod(0o600)
        hosts = root / "known_hosts"
        hosts.write_text(f'[{config["host"]}]:{config["port"]} {config["host_key"]}\n')
        yield config, [
            "ssh", "-F", "/dev/null", "-T", "-p", str(config["port"]),
            "-i", str(key), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=yes", "-o", f"UserKnownHostsFile={hosts}",
            "-o", "GlobalKnownHostsFile=/dev/null", "-o", "ForwardAgent=no",
            "-o", "ClearAllForwardings=yes", "-o", "ConnectTimeout=10",
            "-o", "LogLevel=ERROR", "-l", config["route"], config["host"],
        ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    run = sub.add_parser("exec", help="Run a command in the remote workspace")
    run.add_argument("--timeout", type=float, default=300)
    run.add_argument("command", nargs=argparse.REMAINDER)
    read = sub.add_parser("read", help="Read a remote file")
    read.add_argument("path")
    write = sub.add_parser("write", help="Write stdin to a remote file")
    write.add_argument("path")
    args = parser.parse_args()
    try:
        with connection() as (config, ssh):
            if args.operation == "exec":
                command = args.command
                if command[:1] == ["--"]:
                    command = command[1:]
                if not command:
                    parser.error("exec requires a command")
                command = shlex.join(command)
            elif args.operation == "read":
                command = "cat -- " + shlex.quote(args.path)
            else:
                command = "cat > " + shlex.quote(args.path)
            script = "cd -- " + shlex.quote(config["workspace"]) + " && " + command
            result = subprocess.run(ssh + [script], timeout=getattr(args, "timeout", 300), check=False)
            return result.returncode
    except subprocess.TimeoutExpired:
        print("Remote command timed out; its remote completion is unknown. Do not replay automatically.", file=sys.stderr)
        return 124
    except (KeyError, ValueError, OSError):
        print("EE SSH configuration unavailable or invalid", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
