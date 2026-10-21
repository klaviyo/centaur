"""Exercise image defaults against real temporary Git checkouts."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import pants_native

CONFIG = """[environments-preview.names]
default = "//:python310"
python_3_10 = "//:python310"
python_3_12 = "//:python312"
"""


class NativePantsTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.git("init", "--quiet")
        (self.root / "pants.toml").write_text(CONFIG)
        (self.root / "BUILD").write_text("# Existing repository targets.\n")
        self.git("add", "pants.toml", "BUILD")

    def git(self, *args):
        return subprocess.run(
            ["git", "-C", str(self.root), *args],
            check=True,
            capture_output=True,
            text=True,
            env={**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null"},
        ).stdout

    def test_ordinary_checkout_needs_no_repository_profile(self):
        before = self.git("diff", "--cached")
        env = pants_native.environment(self.root, ["test", "::"], {})
        self.assertEqual(
            json.loads(env["PANTS_CONFIG_FILES"]),
            [
                str(self.root / "pants.toml"),
                str(pants_native.ASSETS / "pants.native.toml"),
            ],
        )
        for key in (
            "PANTS_DOCKER_EXECUTION",
            "PANTS_REMOTE_CACHE_READ",
            "PANTS_REMOTE_CACHE_WRITE",
        ):
            self.assertEqual(env[key], "false")
        target = self.root / pants_native.GENERATED / "BUILD"
        self.assertEqual(
            target.read_bytes(), (pants_native.ASSETS / "native.BUILD").read_bytes()
        )
        self.assertEqual(before, self.git("diff", "--cached"))
        self.assertEqual(self.git("ls-files", "--others", "--exclude-standard"), "")
        self.assertEqual(self.git("diff"), "")
        self.assertFalse((self.root / "pants.sandbox.toml").exists())

    def test_repeated_setup_is_idempotent(self):
        pants_native.environment(self.root, [], {})
        target = self.root / pants_native.GENERATED / "BUILD"
        first_mtime = target.stat().st_mtime_ns
        pants_native.environment(self.root, [], {})
        self.assertEqual(target.stat().st_mtime_ns, first_mtime)
        exclude = (self.root / ".git/info/exclude").read_text()
        self.assertEqual(exclude.count(f"/{pants_native.GENERATED}/"), 1)

    def test_explicit_configuration_and_other_repositories_are_unchanged(self):
        for env, args in (
            ({"PANTS_CONFIG_FILES": '["custom.toml"]'}, []),
            ({"PANTS_DOCKER_EXECUTION": "true"}, []),
            ({}, ["--pants-config-files=custom.toml"]),
            ({}, ["--config-files", "custom.toml"]),
            ({}, ["--docker-execution"]),
            ({}, ["--docker-execution=true"]),
        ):
            with self.subTest(env=env, args=args):
                self.assertEqual(pants_native.environment(self.root, args, env), env)
                self.assertFalse((self.root / pants_native.GENERATED).exists())
        (self.root / "pants.toml").write_text("[GLOBAL]\npants_version = '2.33.0'\n")
        self.assertEqual(pants_native.environment(self.root, [], {}), {})
        self.assertEqual(pants_native.environment(None, [], {}), {})

    def test_explicit_cache_and_certificate_settings_survive(self):
        original = {
            "PANTS_REMOTE_CACHE_READ": "true",
            "PANTS_REMOTE_CACHE_WRITE": "true",
            "PANTS_CA_CERTS_PATH": "/explicit/ca.pem",
            "PANTS_ENVIRONMENTS_PREVIEW_NAMES": "obsolete mapping",
        }
        env = pants_native.environment(self.root, [], original)
        for key in (
            "PANTS_REMOTE_CACHE_READ",
            "PANTS_REMOTE_CACHE_WRITE",
            "PANTS_CA_CERTS_PATH",
        ):
            self.assertEqual(env[key], original[key])
        self.assertNotIn("PANTS_ENVIRONMENTS_PREVIEW_NAMES", env)
        self.assertIn("PANTS_ENVIRONMENTS_PREVIEW_NAMES", original)

    def test_proxy_certificate_is_selected_when_present(self):
        certificate = self.root / "proxy.pem"
        certificate.write_text("fixture certificate")
        env = pants_native.environment(
            self.root, [], {"SSL_CERT_FILE": str(certificate)}
        )
        self.assertEqual(env["PANTS_CA_CERTS_PATH"], str(certificate))

    def test_existing_unowned_files_are_preserved(self):
        directory = self.root / pants_native.GENERATED
        directory.mkdir()
        target = directory / "BUILD"
        target.write_text("# User-owned file.\n")
        with self.assertRaisesRegex(RuntimeError, "existing files"):
            pants_native.environment(self.root, [], {})
        self.assertEqual(target.read_text(), "# User-owned file.\n")

    def test_tracked_generated_files_are_preserved(self):
        directory = self.root / pants_native.GENERATED
        directory.mkdir()
        target = directory / "BUILD"
        target.write_text("# Tracked file.\n")
        self.git("add", str(target))
        with self.assertRaisesRegex(RuntimeError, "tracked files"):
            pants_native.environment(self.root, [], {})
        self.assertEqual(target.read_text(), "# Tracked file.\n")

    def test_broken_marker_symlink_is_not_followed(self):
        directory = self.root / pants_native.GENERATED
        directory.mkdir()
        outside = self.root / "outside"
        (directory / ".image-owned").symlink_to(outside)
        with self.assertRaisesRegex(RuntimeError, "symlink"):
            pants_native.environment(self.root, [], {})
        self.assertFalse(outside.exists())

    def test_subdirectory_and_test_arguments(self):
        directory = self.root / "src/package"
        directory.mkdir(parents=True)
        self.assertEqual(pants_native.buildroot(directory), self.root)
        env = pants_native.environment(
            self.root, ["test", "::", "--", "--config-files"], {}
        )
        self.assertEqual(env["PANTS_DOCKER_EXECUTION"], "false")


if __name__ == "__main__":
    unittest.main()
