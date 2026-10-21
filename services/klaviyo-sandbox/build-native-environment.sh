#!/usr/bin/env bash
set -eo pipefail

compat_version="${1:?Pass the compatibility Go version}"
mkdir -p /opt/krepo/bin /opt/krepo/dev
# Keep the agent's system Python and installed tool libraries available even if
# a development dependency contributes another unversioned python to PATH.
ln -s "$(command -v python3)" /opt/krepo/bin/python3

nix print-dev-env --no-update-lock-file --profile /opt/krepo/profile \
  path:/opt/krepo/source > /tmp/native-dev-env.sh
cd /opt/krepo/dev
# print-dev-env includes shellHook evaluation. Run the unchanged upstream hook
# here, once, so Python installations and virtual environments are image layers.
# shellcheck disable=SC1091
source /tmp/native-dev-env.sh
if [[ "${NIX_FLAKE_SHELLHOOK_FAILED:-0}" == 1 ]]; then
  echo 'Development shell initialization failed' >&2
  exit 1
fi

cp "$GOPATH/bin/protoc-gen-go" "$GOPATH/bin/protoc-gen-go-grpc" /opt/krepo/bin/
GOBIN=/opt/krepo/bin go install honnef.co/go/tools/cmd/staticcheck@2025.1.1
GOBIN=/opt/krepo/bin go install gotest.tools/gotestsum@v1.13.0
mkdir -p /opt/go-compat-cache
GOMODCACHE=/opt/go-compat-cache GOTOOLCHAIN="go${compat_version}" go version

uv run --no-project --python "$PWD/.plugins-venv/bin/python" \
  python /opt/image-build/capture_environment.py /opt/krepo/environment.sh

# Copy only the realized development environment and its transitive store
# references. Do not copy the package manager's mutable store database.
mkdir -p /export
nix-store --query --requisites /opt/krepo/profile > /tmp/native-store-paths
while IFS= read -r store_path; do
  cp -a --parents "$store_path" /export/
done < /tmp/native-store-paths
rm -rf /opt/krepo/dev/.direnv/uv
