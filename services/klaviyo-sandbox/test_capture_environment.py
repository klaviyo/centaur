"""Verify captured settings work in a fresh shell without leaking build state."""

import hashlib
import json
import shlex
import subprocess
import tempfile
import unittest
from pathlib import Path

from capture_environment import render

ASSETS = Path(__file__).resolve().parent


class CapturedEnvironmentTest(unittest.TestCase):
    def environment(self):
        return {
            "PATH": "/opt/krepo/dev/.venv/bin:/nix/store/toolchain/bin:/tmp/native-go/bin:/usr/bin",
            "CFLAGS": "-I/nix/store/library/include",
            "LDFLAGS": "-L/nix/store/library/lib",
            "MAINLINE_PYTHON": "/opt/krepo/dev/.venv/bin/python",
            "PANTS_PYTHON_BOOTSTRAP_SEARCH_PATH": "['/opt/krepo/dev/.venv/bin']",
            "CGO_CFLAGS": "-I/nix/store/library/include",
            "CC": "gcc",
            "IS_NIX_INITIALIZED": "true",
            "HOME": "/root",
            "GOPATH": "/tmp/native-go",
            "VIRTUAL_ENV": "/opt/krepo/dev/.venv",
            "UV_PYTHON": "3.10.18",
            "UV_CACHE_DIR": "/opt/krepo/dev/.direnv/uv",
            "NIX_BUILD_TOP": "/tmp/nix-shell.build",
            "GITHUB_TOKEN": "fixture-not-a-credential",
        }

    def test_fresh_shell_receives_tools_and_flags_without_builder_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            script = Path(temporary) / "environment.sh"
            script.write_text(render(self.environment()))
            result = subprocess.run(
                ["/bin/bash", "-c", 'source "$1"; /usr/bin/env', "bash", str(script)],
                check=True,
                capture_output=True,
                text=True,
                env={"PATH": "/usr/bin:/bin", "HOME": temporary},
            )
            values = dict(
                line.split("=", 1) for line in result.stdout.splitlines() if "=" in line
            )
            self.assertEqual(values["HOME"], temporary)
            self.assertEqual(values["CFLAGS"], self.environment()["CFLAGS"])
            self.assertEqual(values["CGO_CFLAGS"], self.environment()["CGO_CFLAGS"])
            self.assertEqual(
                values["PATH"], "/opt/krepo/bin:/nix/store/toolchain/bin:/usr/bin:/bin"
            )
            for name in (
                "GITHUB_TOKEN",
                "VIRTUAL_ENV",
                "UV_PYTHON",
                "UV_CACHE_DIR",
                "GOPATH",
                "NIX_BUILD_TOP",
            ):
                self.assertNotIn(name, values)

    def test_shell_metacharacters_are_literal(self):
        with tempfile.TemporaryDirectory() as temporary:
            marker = Path(temporary) / "must-not-exist"
            environment = self.environment()
            environment["CFLAGS"] = "-DVALUE='$(touch must-not-exist)'"
            subprocess.run(
                ["/bin/bash", "-c", render(environment)], check=True, cwd=temporary
            )
            self.assertFalse(marker.exists())

    def test_partial_or_build_only_environments_fail(self):
        for name in (
            "CFLAGS",
            "LDFLAGS",
            "MAINLINE_PYTHON",
            "PANTS_PYTHON_BOOTSTRAP_SEARCH_PATH",
        ):
            environment = self.environment()
            environment.pop(name)
            with self.subTest(missing=name), self.assertRaises(ValueError):
                render(environment)
        for value in ("/tmp/build/include", "/root/private/include"):
            environment = self.environment()
            environment["CFLAGS"] = value
            with self.subTest(value=value), self.assertRaises(ValueError):
                render(environment)
        environment = self.environment()
        environment["PATH"] = "/usr/bin"
        with self.assertRaises(ValueError):
            render(environment)

    def test_snapshot_matches_recorded_hashes_and_locked_inputs(self):
        metadata = json.loads((ASSETS / "nix/source.json").read_text())
        self.assertEqual(len(metadata["revision"]), 40)
        for name, expected in metadata["sha256"].items():
            self.assertEqual(
                hashlib.sha256((ASSETS / "nix" / name).read_bytes()).hexdigest(),
                expected,
            )
        lock = json.loads((ASSETS / "nix/flake.lock").read_text())
        for node in lock["nodes"].values():
            if "locked" in node:
                self.assertIn("narHash", node["locked"])
                self.assertIn("rev", node["locked"])

    def test_startup_preserves_arguments_and_certificate_overrides(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            settings = root / "environment.sh"
            settings.write_text(render(self.environment()))
            parent = root / "parent.sh"
            parent.write_text(
                '#!/bin/bash\nprintf "arg=%s\\n" "$@"\n'
                'printf "ca=%s\\nflags=%s\\n" "$PANTS_CA_CERTS_PATH" "$CFLAGS"\n'
            )
            parent.chmod(0o755)
            script = (ASSETS / "native-entrypoint.sh").read_text()
            script = script.replace(
                "source /opt/krepo/environment.sh",
                f"source {shlex.quote(str(settings))}",
            )
            script = script.replace(
                "exec /entrypoint.sh", f"exec {shlex.quote(str(parent))}"
            )
            certificate = root / "certificate.pem"
            certificate.write_text("fixture certificate")
            for explicit in (None, "/explicit/certificate.pem"):
                environment = {
                    "PATH": "/usr/bin:/bin",
                    "SSL_CERT_FILE": str(certificate),
                }
                if explicit:
                    environment["PANTS_CA_CERTS_PATH"] = explicit
                result = subprocess.run(
                    [
                        "/bin/bash",
                        "-c",
                        script,
                        "startup",
                        "first argument",
                        "--model=value",
                    ],
                    env=environment,
                    check=True,
                    capture_output=True,
                    text=True,
                )
                with self.subTest(explicit=explicit):
                    self.assertEqual(
                        result.stdout.splitlines(),
                        [
                            "arg=first argument",
                            "arg=--model=value",
                            f"ca={explicit or certificate}",
                            f"flags={self.environment()['CFLAGS']}",
                        ],
                    )


if __name__ == "__main__":
    unittest.main()
