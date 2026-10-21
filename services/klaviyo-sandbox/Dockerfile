# syntax=docker/dockerfile:1.7

ARG CENTAUR_IMAGE=klaviyo-centaur-agent:local
ARG NIX_IMAGE=nixos/nix:2.31.2

FROM ${NIX_IMAGE} AS nix-bootstrap
FROM ${CENTAUR_IMAGE} AS native-environment

USER root
SHELL ["/bin/bash", "-eo", "pipefail", "-c"]
COPY --from=nix-bootstrap /nix/ /nix/
ENV PATH="/nix/var/nix/profiles/default/bin:${PATH}" \
    NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    GOPATH=/tmp/native-go \
    UV_NATIVE_TLS=true \
    UV_LINK_MODE=copy
RUN mkdir -p /etc/nix \
    && printf '%s\n' 'experimental-features = nix-command flakes' \
         'sandbox = false' 'build-users-group =' > /etc/nix/nix.conf

COPY services/klaviyo-sandbox/nix/ /opt/krepo/source/
COPY services/klaviyo-sandbox/build-native-environment.sh services/klaviyo-sandbox/capture_environment.py /opt/image-build/
ARG GO_COMPAT_VERSION=1.25.0
RUN bash /opt/image-build/build-native-environment.sh "${GO_COMPAT_VERSION}"

FROM ${CENTAUR_IMAGE}
USER root
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]

# Nix binaries embed absolute references; retain the complete runtime closure.
# Package evaluation, the Nix database and build caches stay in the build stage.
COPY --from=native-environment /export/nix/store/ /nix/store/
COPY --from=native-environment /opt/krepo/ /opt/krepo/
COPY --from=native-environment --chown=1001:1001 /opt/go-compat-cache/ /home/agent/go/pkg/mod/

ARG PANTS_LAUNCHER_VERSION=0.13.2
RUN architecture="$(uname -m)"; \
    launcher="scie-pants-linux-${architecture}"; \
    url="https://github.com/pantsbuild/scie-pants/releases/download/v${PANTS_LAUNCHER_VERSION}/${launcher}"; \
    curl -fsSL --retry 3 "${url}" -o "/tmp/${launcher}"; \
    curl -fsSL --retry 3 "${url}.sha256" -o "/tmp/${launcher}.sha256"; \
    (cd /tmp && sha256sum -c "${launcher}.sha256"); \
    install -m 0755 "/tmp/${launcher}" /usr/local/bin/pants; \
    rm "/tmp/${launcher}" "/tmp/${launcher}.sha256"

COPY services/klaviyo-sandbox/native-entrypoint.sh /usr/local/bin/native-entrypoint
COPY services/klaviyo-sandbox/smoke-test.sh /usr/local/share/klaviyo-sandbox/smoke-test.sh
RUN chmod 0755 /usr/local/bin/native-entrypoint \
    && mkdir -p /home/agent/go /home/agent/.cache/go-build \
    && chown -R 1001:1001 /home/agent/go /home/agent/.cache/go-build

ENV GOPATH=/home/agent/go \
    GOCACHE=/home/agent/.cache/go-build \
    GOMODCACHE=/home/agent/go/pkg/mod \
    GOPRIVATE=github.com/klaviyo \
    UV_NATIVE_TLS=true \
    UV_PYTHON_INSTALL_DIR=/opt/krepo/dev/.direnv/share/uv/python \
    PANTS_REMOTE_CACHE_READ=false \
    PANTS_REMOTE_CACHE_WRITE=false

USER agent
WORKDIR /home/agent
ARG PANTS_VERSION=2.33.0
ARG GO_COMPAT_VERSION=1.25.0
RUN source /opt/krepo/environment.sh \
    && GOPROXY=off GOTOOLCHAIN="go${GO_COMPAT_VERSION}" go version \
    && PANTS_CA_CERTS_PATH=/etc/ssl/certs/ca-certificates.crt \
         timeout --kill-after=15 600 bash /usr/local/share/klaviyo-sandbox/smoke-test.sh "${PANTS_VERSION}"

# Keep private package configuration out of the public-dependency build fixture.
# The proxy replaces these placeholders only on authorized outbound requests.
COPY --chown=1001:1001 services/klaviyo-sandbox/pip.conf /home/agent/.pip/pip.conf

# Activate the prebuilt environment before the original agent entrypoint.
# Pants itself is the unmodified upstream launcher.
ENTRYPOINT ["/usr/local/bin/native-entrypoint"]
# Setting ENTRYPOINT resets an inherited CMD; preserve the base image default.
CMD ["harness-server", "codex"]
