# Native development environment

The image prebuilds the Linux variant of the existing cross-platform Nix development shell. `nix/flake.nix` and `nix/flake.lock` are unchanged snapshots; `nix/source.json` records their source revision and SHA-256 hashes. Update both files together from a reviewed source revision and refresh the metadata when changing the development environment. No repository PR is required to use this image.

The builder realizes the locked shell and runs its existing shell hook in a dedicated image directory. This prepares the four Python interpreter environments, Go, Node, protobuf tools, native libraries and compiler/linker flags. The final image contains those environments and their Nix store closure. Runtime startup sources only selected, build-time captured toolchain settings before invoking the original agent entrypoint. It runs no Nix evaluation or shell hook and performs no package downloads or checkout mutation. The agent's system Python remains available for its installed tool libraries.

Pants uses the unmodified upstream launcher and its normal native execution path. There is no environment-alias override, custom Pants wrapper, generated BUILD file or checked-in sandbox profile. Ordinary targets execute locally using their interpreter constraints. A target that explicitly selects a Docker environment still requires Docker; its existing repository semantics are preserved. Service integration tests still need their databases and other declared dependencies.

Remote-cache reads and writes default off to avoid requiring a developer VPN; explicit runtime settings or command-line options can override them. Startup selects an existing proxy certificate bundle, with the system bundle as a fallback, unless the caller supplies a certificate path. Private package access continues through the existing proxy-backed configuration. Real credentials are not included in the image.

The build runs `smoke-test.sh` as the runtime user after copying the realized environment into the final image. It checks Python standard-library extensions, the native tools, C and Go/CGO compilation, and three native Pants tests on Python 3.10 and 3.12 with a fresh JUnit report. The fixture retains Docker environment definitions but does not select them. It runs before private package configuration is installed. This gate must pass before publishing a usable image; it does not verify private package authorization or service integration tests.

Focused local checks:

```bash
uv run --no-project python -m unittest discover -s services/klaviyo-sandbox -p 'test_*.py'
shellcheck services/klaviyo-sandbox/*.sh
```
