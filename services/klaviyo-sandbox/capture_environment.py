"""Export the realized developer toolchain without build-session state."""

from __future__ import annotations

import os
import shlex
import sys
from pathlib import Path

# Keep the list explicit: never copy the builder's general environment or auth.
VARIABLES = (
    "AR",
    "AS",
    "CC",
    "CXX",
    "LD",
    "NM",
    "RANLIB",
    "STRIP",
    "CFLAGS",
    "CPPFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "CGO_CFLAGS",
    "CGO_CPPFLAGS",
    "CGO_CXXFLAGS",
    "CGO_LDFLAGS",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_FOR_BUILD",
    "CMAKE_PREFIX_PATH",
    "NIX_CC",
    "NIX_BINTOOLS",
    "NIX_CFLAGS_COMPILE",
    "NIX_LDFLAGS",
    "NIX_HARDENING_ENABLE",
    "NIX_DONT_SET_RPATH",
    "MAINLINE_PYTHON",
    "PANTS_PYTHON_BOOTSTRAP_SEARCH_PATH",
    "IS_NIX_INITIALIZED",
)


def render(environment: dict[str, str]) -> str:
    for name in (
        "CFLAGS",
        "LDFLAGS",
        "MAINLINE_PYTHON",
        "PANTS_PYTHON_BOOTSTRAP_SEARCH_PATH",
    ):
        if not environment.get(name):
            raise ValueError(f"Development environment is missing {name}")
    paths = list(
        dict.fromkeys(
            path
            for path in environment["PATH"].split(":")
            if path.startswith("/nix/store/")
        )
    )
    if not paths:
        raise ValueError("Development environment has no realized Nix tool paths")
    lines = ["# Generated at image build time; no shell hooks or downloads."]
    for name in VARIABLES:
        if name in environment:
            value = environment[name]
            if "/tmp/" in value or "/root/" in value:
                raise ValueError(
                    f"Development environment contains a build-only path in {name}"
                )
            lines.append(f"export {name}={shlex.quote(value)}")
    prefix = "/opt/krepo/bin:" + ":".join(paths)
    lines.append(f'export PATH={shlex.quote(prefix)}:"$PATH"')
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    Path(sys.argv[1]).write_text(render(dict(os.environ)))
