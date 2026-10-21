"""Apply the image's native environment defaults before starting Pants."""

from __future__ import annotations

import fcntl
import json
import os
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ASSETS = Path(__file__).resolve().parent
LAUNCHER = "/usr/local/libexec/pants"
GENERATED = ".centaur-native-pants"
OWNER = "Managed by the sandbox image.\n"


def buildroot(start: Path) -> Path | None:
    for path in (start, *start.parents):
        if (path / "pants.toml").is_file():
            return path
    return None


def native_defaults(root: Path, argv: list[str], env: dict[str, str]) -> bool:
    argv = argv[: argv.index("--")] if "--" in argv else argv
    if env.get("PANTS_DOCKER_EXECUTION", "").lower() == "true":
        return False
    if "--docker-execution" in argv or "--docker-execution=true" in argv:
        return False
    if env.get("PANTS_CONFIG_FILES") or any(
        arg.split("=", 1)[0] in ("--pants-config-files", "--config-files")
        for arg in argv
    ):
        return False
    config = tomllib.loads((root / "pants.toml").read_text())
    names = config.get("environments-preview", {}).get("names", {})
    return {"default", "python_3_10", "python_3_12"}.issubset(names)


def prepare_targets(root: Path) -> None:
    tracked = subprocess.run(
        ["git", "-C", str(root), "ls-files", "--", GENERATED],
        check=True,
        capture_output=True,
        text=True,
    )
    if tracked.stdout:
        raise RuntimeError(f"Refusing to replace tracked files in {GENERATED}")
    destination = root / GENERATED
    if destination.is_symlink():
        raise RuntimeError(f"Refusing to follow a symlink at {GENERATED}")
    destination.mkdir(exist_ok=True)
    if (destination / ".lock").is_symlink():
        raise RuntimeError(f"Refusing to follow a symlink at {GENERATED}/.lock")
    with (destination / ".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        marker = destination / ".image-owned"
        if marker.is_symlink():
            raise RuntimeError(
                f"Refusing to follow a symlink at {GENERATED}/.image-owned"
            )
        if marker.exists():
            if marker.read_text() != OWNER:
                raise RuntimeError(f"Unrecognized owner for {GENERATED}")
        elif any(path.name != ".lock" for path in destination.iterdir()):
            raise RuntimeError(f"Refusing to replace existing files in {GENERATED}")
        marker.write_text(OWNER)
        contents = (ASSETS / "native.BUILD").read_bytes()
        target = destination / "BUILD"
        if target.is_symlink():
            raise RuntimeError(f"Refusing to follow a symlink at {GENERATED}/BUILD")
        if not target.exists() or target.read_bytes() != contents:
            with tempfile.NamedTemporaryFile(
                dir=destination, delete=False
            ) as temporary:
                temporary.write(contents)
            os.replace(temporary.name, target)

    result = subprocess.run(
        [
            "git",
            "-C",
            str(root),
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    exclude = Path(result.stdout.strip())
    exclude.parent.mkdir(parents=True, exist_ok=True)
    with exclude.open("a+") as stream:
        fcntl.flock(stream, fcntl.LOCK_EX)
        stream.seek(0)
        if f"/{GENERATED}/" not in stream.read().splitlines():
            stream.write(f"\n/{GENERATED}/\n")


def environment(
    root: Path | None, argv: list[str], original: dict[str, str]
) -> dict[str, str]:
    env = original.copy()
    if root is None or not native_defaults(root, argv, env):
        return env
    prepare_targets(root)
    env["PANTS_CONFIG_FILES"] = json.dumps(
        [str(root / "pants.toml"), str(ASSETS / "pants.native.toml")]
    )
    env.pop("PANTS_ENVIRONMENTS_PREVIEW_NAMES", None)
    env["PANTS_DOCKER_EXECUTION"] = "false"
    env.setdefault("PANTS_REMOTE_CACHE_READ", "false")
    env.setdefault("PANTS_REMOTE_CACHE_WRITE", "false")
    for candidate in (
        env.get("SSL_CERT_FILE", ""),
        "/firewall-certs/ca-cert.pem",
        "/etc/ssl/certs/ca-certificates.crt",
    ):
        if candidate and Path(candidate).is_file():
            env.setdefault("PANTS_CA_CERTS_PATH", candidate)
            break
    return env


def main() -> None:
    args = sys.argv[1:]
    try:
        env = environment(buildroot(Path.cwd()), args, dict(os.environ))
    except (
        OSError,
        RuntimeError,
        subprocess.CalledProcessError,
        tomllib.TOMLDecodeError,
    ) as error:
        print(f"Native Pants setup failed: {error}", file=sys.stderr)
        sys.exit(1)
    os.execve(LAUNCHER, [LAUNCHER, *args], env)


if __name__ == "__main__":
    main()
