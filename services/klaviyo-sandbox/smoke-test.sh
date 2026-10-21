#!/usr/bin/env bash
set -euo pipefail

pants_version="${1:?Pass the image Pants version}"
smoke_root="$(mktemp -d)"
trap 'rm -rf "$smoke_root"' EXIT
cd "$smoke_root"

cat > pants.toml <<TOML
[GLOBAL]
pants_version = "$pants_version"
backend_packages = ["pants.backend.python"]
pantsd = false
docker_execution = false

[anonymous-telemetry]
enabled = false

[python]
enable_resolves = false

[python-repos]
indexes = ["https://pypi.org/simple/"]

[environments-preview.names]
native = "//:build_machine"
default = "//:python_3_10"
python_3_10 = "//:python_3_10"
python_3_12 = "//:python_3_12"

[test]
report = true
TOML

cat > BUILD <<'BUILD'
local_environment(name="build_machine", compatible_platforms=["linux_x86_64", "linux_arm64"])
docker_environment(name="python_3_10", image="python:3.10", fallback_environment="native")
docker_environment(name="python_3_12", image="python:3.12", fallback_environment="native")
python_tests(name="default", sources=["test_default.py"], interpreter_constraints=["==3.10.*"])
python_tests(name="py310", sources=["test_python310.py"], interpreter_constraints=["==3.10.*"], environment="python_3_10")
python_tests(name="py312", sources=["test_python312.py"], interpreter_constraints=["==3.12.*"], environment="python_3_12")
BUILD

for target in default python310 python312; do
  minor=10
  if [[ "$target" == python312 ]]; then minor=12; fi
  cat > "test_${target}.py" <<PYTEST
import sys


def test_interpreter():
    assert sys.version_info[:2] == (3, $minor)
PYTEST
done

pants test :default :py310 :py312

uv run --no-project python - <<'PY'
from pathlib import Path
from xml.etree import ElementTree

reports = list(Path("dist/test/reports").rglob("*.xml"))
cases = [case for report in reports for case in ElementTree.parse(report).iter("testcase")]
assert len(cases) == 3, f"Expected three test results, found {len(cases)}"
assert not any(
    case.find(result) is not None
    for case in cases
    for result in ("failure", "error", "skipped")
), "A smoke test failed or was skipped"
print("Pants native environments: three passing tests on Python 3.10 and 3.12")
PY
