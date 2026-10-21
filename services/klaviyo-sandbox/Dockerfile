# syntax=docker/dockerfile:1.7

ARG CENTAUR_IMAGE=klaviyo-centaur-agent:local
ARG GO_VERSION=1.25.3
ARG UV_VERSION=0.10.2

FROM golang:${GO_VERSION}-bookworm AS go-toolchain
FROM ghcr.io/astral-sh/uv:${UV_VERSION} AS uv-toolchain
FROM ${CENTAUR_IMAGE}

USER root
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]

COPY --from=go-toolchain /usr/local/go/ /usr/local/go/
COPY --from=uv-toolchain /uv /uvx /usr/local/bin/

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      autoconf automake bison build-essential ca-certificates cmake curl flex \
      git jq libbrotli-dev libbz2-dev libffi-dev libfreetype6-dev libgdbm-dev \
      libjpeg-dev liblz4-dev liblzma-dev libmemcached-dev \
      default-libmysqlclient-dev libncurses-dev libpng-dev libpq-dev \
      libreadline-dev libsnappy-dev libsqlite3-dev libssl-dev libtiff-dev \
      libtool libxml2-dev libxmlsec1-dev libxmlsec1-openssl libxslt1-dev \
      libzstd-dev locales ninja-build pbzip2 pkg-config ripgrep shellcheck \
      unixodbc-dev unzip xz-utils zlib1g-dev \
    && locale-gen en_US.UTF-8 \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf /usr/local/bin/uv /home/agent/.local/bin/uv \
    && ln -sf /usr/local/bin/uvx /home/agent/.local/bin/uvx

ENV LANG=en_US.UTF-8 \
    LC_ALL=en_US.UTF-8 \
    UV_PYTHON_INSTALL_DIR=/opt/krepo/python \
    PANTS_CA_CERTS_PATH=/firewall-certs/ca-cert.pem \
    UV_NATIVE_TLS=true
ENV PATH="/usr/local/go/bin:/home/agent/go/bin:${PATH}"

ARG PYTHON_310=3.10.18
ARG PYTHON_311=3.11.12
ARG PYTHON_312=3.12.9
ARG PYTHON_314=3.14.3

RUN uv python install "${PYTHON_310}" "${PYTHON_311}" "${PYTHON_312}" "${PYTHON_314}" \
    && for version in "${PYTHON_310}" "${PYTHON_311}" "${PYTHON_312}" "${PYTHON_314}"; do \
         minor="${version%.*}"; \
         installations=(/opt/krepo/python/cpython-${version}-linux-*); \
         test "${#installations[@]}" -eq 1; \
         test -x "${installations[0]}/bin/python${minor}"; \
         ln -s "${installations[0]}" "/opt/krepo/python/${minor}"; \
         ln -s "/opt/krepo/python/${minor}/bin/python${minor}" "/usr/local/bin/python${minor}"; \
       done \
    && chmod -R a+rX /opt/krepo/python

# Keep Centaur's system python3 and its installed tool libraries intact.
ENV PANTS_PYTHON_BOOTSTRAP_SEARCH_PATH="['/opt/krepo/python/3.10/bin', '/opt/krepo/python/3.11/bin', '/opt/krepo/python/3.12/bin', '/opt/krepo/python/3.14/bin', '<PATH>']"

ENV PANTS_DOCKER_EXECUTION=false

ARG PROTOC_VERSION=33.1
ARG BUF_VERSION=1.3.0
ARG PANTS_LAUNCHER_VERSION=0.13.2

RUN architecture="$(uname -m)"; \
    case "${architecture}" in \
      x86_64) protoc_arch=x86_64 ;; \
      aarch64) protoc_arch=aarch_64 ;; \
      *) echo "Unsupported architecture: ${architecture}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL --retry 3 \
      "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-${protoc_arch}.zip" \
      -o /tmp/krepo-protoc.zip; \
    unzip -qo /tmp/krepo-protoc.zip -d /usr/local; \
    rm /tmp/krepo-protoc.zip; \
    curl -fsSL --retry 3 \
      "https://github.com/bufbuild/buf/releases/download/v${BUF_VERSION}/buf-Linux-${architecture}.tar.gz" \
      -o /tmp/krepo-buf.tar.gz; \
    tar -xzf /tmp/krepo-buf.tar.gz -C /tmp; \
    install -m 0755 /tmp/buf/bin/buf /usr/local/bin/buf; \
    rm -rf /tmp/buf /tmp/krepo-buf.tar.gz; \
    launcher="scie-pants-linux-${architecture}"; \
    url="https://github.com/pantsbuild/scie-pants/releases/download/v${PANTS_LAUNCHER_VERSION}/${launcher}"; \
    curl -fsSL --retry 3 "${url}" -o "/tmp/${launcher}"; \
    curl -fsSL --retry 3 "${url}.sha256" -o "/tmp/${launcher}.sha256"; \
    (cd /tmp && sha256sum -c "${launcher}.sha256"); \
    install -m 0755 "/tmp/${launcher}" /usr/local/bin/pants; \
    rm "/tmp/${launcher}" "/tmp/${launcher}.sha256"

RUN --mount=type=cache,target=/root/go/pkg/mod \
    --mount=type=cache,target=/root/.cache/go-build \
    GOBIN=/usr/local/bin go install google.golang.org/protobuf/cmd/protoc-gen-go@v1.36.11 \
    && GOBIN=/usr/local/bin go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@v1.6.1 \
    && GOBIN=/usr/local/bin go install github.com/favadi/protoc-go-inject-tag@v1.4.0 \
    && GOBIN=/usr/local/bin go install honnef.co/go/tools/cmd/staticcheck@2025.1.1 \
    && GOBIN=/usr/local/bin go install gotest.tools/gotestsum@v1.13.0

USER agent
WORKDIR /home/agent

ENV GOPATH=/home/agent/go \
    GOCACHE=/home/agent/.cache/go-build \
    GOMODCACHE=/home/agent/go/pkg/mod \
    GOPRIVATE=github.com/klaviyo

ARG PANTS_VERSION=2.33.0
RUN mkdir -p /home/agent/go /home/agent/.cache/go-build \
    && bootstrap="$(mktemp -d)" \
    && printf '[GLOBAL]\npants_version = "%s"\n[anonymous-telemetry]\nenabled = false\n' \
         "${PANTS_VERSION}" > "${bootstrap}/pants.toml" \
    && (cd "${bootstrap}" && \
         PANTS_CA_CERTS_PATH=/etc/ssl/certs/ca-certificates.crt pants --version) \
    && rm -rf "${bootstrap}"

RUN <<'SMOKE'
for minor in 3.10 3.11 3.12 3.14; do
  "python${minor}" -c 'import bz2, ctypes, lzma, sqlite3, ssl, venv; print(ssl.OPENSSL_VERSION)'
done
go version
protoc --version
protoc-gen-go --version
protoc-gen-go-grpc --version
buf --version
staticcheck -version
gotestsum --version
uv --version
pkg-config --exists libmemcached libxml-2.0 xmlsec1 openssl zlib
smoke="$(mktemp -d)"
trap 'rm -rf "$smoke"' EXIT
printf 'package main\n/*\n#include <stdlib.h>\n*/\nimport "C"\nfunc main() { _ = C.abs(-1) }\n' > "$smoke/main.go"
CGO_ENABLED=1 go build -o "$smoke/cgo-smoke" "$smoke/main.go"
"$smoke/cgo-smoke"
python3.10 -m venv "$smoke/venv"
"$smoke/venv/bin/python" -m pip --version
SMOKE

COPY --chown=1001:1001 services/klaviyo-sandbox/smoke-test.sh /usr/local/share/klaviyo-sandbox/smoke-test.sh
RUN PANTS_CA_CERTS_PATH=/etc/ssl/certs/ca-certificates.crt \
    timeout 600 bash /usr/local/share/klaviyo-sandbox/smoke-test.sh "${PANTS_VERSION}"

COPY --chown=1001:1001 services/klaviyo-sandbox/pip.conf /home/agent/.pip/pip.conf

# Iron-proxy replaces the pip.conf placeholders on JFrog package requests.
# Docker-dependent tests still need a separately provided Docker daemon;
# service integration tests need their databases and other dependencies.
# ENTRYPOINT and CMD are inherited from the Centaur image.
