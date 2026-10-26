from __future__ import annotations

import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SANDBOX = Path(__file__).resolve().parent
REPO_ROOT = SANDBOX.parents[1]


class CodexRuntimeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.codex_home = self.home / "codex-state"
        self.codex_home.mkdir()
        shutil.copyfile(
            REPO_ROOT / "harness/codex/config.toml", self.codex_home / "config.toml"
        )
        (self.home / "AGENTS.md").write_text("CENTAUR_BASE_SENTINEL\n")
        (self.home / "AGENTS_OVERLAY.md").write_text("CENTAUR_OVERLAY_SENTINEL\n")
        (self.home / "AGENTS_PERSONA.md").write_text("CENTAUR_PERSONA_SENTINEL\n")
        self._skill(self.home / "centaur-skills", "centaur-runtime-sentinel")
        self._skill(self.home / "centaur-overlay-skills", "overlay-runtime-sentinel")
        self.source = self.home / "github/acme/example"
        self.source.mkdir(parents=True)
        subprocess.run(["git", "init", "-q", str(self.source)], check=True)
        (self.source / "AGENTS.md").write_text("REPO_INSTRUCTION_SENTINEL\n")
        self._skill(self.source / ".agents/skills", "repo-runtime-sentinel")
        subprocess.run(["git", "-C", str(self.source), "add", "."], check=True)
        subprocess.run(
            [
                "git",
                "-C",
                str(self.source),
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "test: add fixture",
            ],
            check=True,
        )
        bin_dir = self.home / "bin"
        bin_dir.mkdir()
        installer = bin_dir / "installer.py"
        installer.write_text(
            "import os, sys\nfrom pathlib import Path\n"
            f"sys.path.insert(0, {str(SANDBOX)!r})\n"
            "import install_tool_shims\n"
            "install_tool_shims._home_dir = lambda: Path(os.environ['HOME_DIR'])\n"
            "sys.exit(install_tool_shims.main(sys.argv))\n"
        )
        for name, script in (
            ("install-tool-shims", installer),
            ("compose-system-prompt", SANDBOX / "compose_system_prompt.py"),
        ):
            shim = bin_dir / name
            shim.write_text(
                f"#!/bin/sh\nexec {shlex.quote(sys.executable)} "
                f'{shlex.quote(str(script))} "$@"\n'
            )
            shim.chmod(0o755)
        self.env = {
            **os.environ,
            "HOME_DIR": str(self.home),
            "STATE_DIR": str(self.home / "state"),
            "CODEX_HOME": str(self.codex_home),
            "CENTAUR_PERSISTENT_STATE": "0",
            "CENTAUR_TOOLS_AUTO_RELOAD": "false",
            "CENTAUR_SANDBOX_OBSERVABILITY_ENABLED": "true",
            "CENTAUR_SKILL_DIRS": "",
            "CENTAUR_OVERLAY_DIR": "",
            "CENTAUR_MANAGED_SKILLS_ONLY": "0",
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "AGENT_REPO": "acme/example",
        }

    def _skill(self, root: Path, name: str) -> None:
        path = root / name
        path.mkdir(parents=True)
        (path / "SKILL.md").write_text(
            f"---\nname: {name}\ndescription: Runtime discovery fixture.\n---\n"
            "Fixture instructions.\n"
        )

    def _startup(self, harness: str = "codex", *, check: bool = True):
        entrypoint = (SANDBOX / "entrypoint.sh").read_text()
        start = entrypoint.index("# ── Per-session workspace clone")
        end = entrypoint.index("# Codex reads its auth file")
        return subprocess.run(
            [
                "bash",
                "-ec",
                entrypoint[start:end] + "\npwd\n",
                "entrypoint-probe",
                "harness-server",
                harness,
            ],
            cwd=self.home,
            env=self.env,
            check=check,
            text=True,
            capture_output=True,
        )

    def test_codex_keeps_checkout_separate_and_rebuilds_managed_skills(self) -> None:
        runtime = self.home / "codex-workspace"
        self._skill(runtime / ".agents/skills", "stale-runtime-sentinel")
        result = self._startup()
        self.assertEqual(Path(result.stdout.strip()), runtime)
        checkout = self.home / "branches/acme/example"
        self.assertTrue((checkout / ".git").exists())
        self.assertEqual(
            (checkout / "AGENTS.md").read_text(), "REPO_INSTRUCTION_SENTINEL\n"
        )
        self.assertEqual(
            {p.name for p in (runtime / ".agents/skills").iterdir()},
            {"centaur-runtime-sentinel", "overlay-runtime-sentinel"},
        )
        self.assertEqual(
            (self.codex_home / "AGENTS.md").read_text(),
            (runtime / "AGENTS.md").read_text(),
        )
        self._startup()
        self.assertTrue((checkout / ".git").exists())

    def test_persistent_codex_ignores_legacy_workspace_clone(self) -> None:
        state = self.home / "state"
        state.mkdir()
        subprocess.run(
            ["git", "clone", "-q", str(self.source), str(state / "workspace")],
            check=True,
        )
        self.env["CENTAUR_PERSISTENT_STATE"] = "1"
        result = self._startup()
        self.assertEqual(Path(result.stdout.strip()), state / "codex-workspace")
        self.assertEqual(
            (state / "workspace/AGENTS.md").read_text(),
            "REPO_INSTRUCTION_SENTINEL\n",
        )

    def test_rejects_repository_as_runtime_directory(self) -> None:
        runtime = self.home / "codex-workspace"
        subprocess.run(["git", "init", "-q", str(runtime)], check=True)
        result = self._startup(check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be outside a repository", result.stderr)

    def test_codex_without_assigned_repo_uses_neutral_workspace(self) -> None:
        self.env["AGENT_REPO"] = ""
        result = self._startup()
        self.assertEqual(Path(result.stdout.strip()), self.home / "codex-workspace")
        self.assertFalse((self.home / "branches/acme/example").exists())

    def test_other_harness_keeps_existing_workspace_clone(self) -> None:
        result = self._startup("claude")
        self.assertEqual(Path(result.stdout.strip()), self.home / "workspace")
        self.assertTrue((self.home / "workspace/.git").exists())
        self.assertTrue(
            (self.home / "workspace/.agents/skills/repo-runtime-sentinel").is_dir()
        )

    @unittest.skipUnless(shutil.which("codex"), "Codex CLI is not installed")
    def test_native_codex_prompt_excludes_checkout_content(self) -> None:
        self._startup()
        result = subprocess.run(
            ["codex", "debug", "prompt-input", "runtime probe"],
            cwd=self.home / "codex-workspace",
            env=self.env,
            check=True,
            text=True,
            capture_output=True,
            timeout=30,
        )
        prompt = json.dumps(json.loads(result.stdout))
        for marker in (
            "CENTAUR_BASE_SENTINEL",
            "CENTAUR_OVERLAY_SENTINEL",
            "CENTAUR_PERSONA_SENTINEL",
            "centaur-runtime-sentinel",
            "overlay-runtime-sentinel",
        ):
            self.assertIn(marker, prompt)
        self.assertNotIn("REPO_INSTRUCTION_SENTINEL", prompt)
        self.assertNotIn("repo-runtime-sentinel", prompt)


if __name__ == "__main__":
    unittest.main()
