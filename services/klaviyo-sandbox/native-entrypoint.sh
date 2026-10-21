#!/usr/bin/env bash
set -euo pipefail
# This file contains only the build-time captured compiler/toolchain settings.
# It performs no Nix evaluation, downloads or checkout mutation.
# shellcheck disable=SC1091
source /opt/krepo/environment.sh
if [[ -z "${PANTS_CA_CERTS_PATH:-}" ]]; then
  for certificate in "${SSL_CERT_FILE:-}" /firewall-certs/ca-cert.pem /etc/ssl/certs/ca-certificates.crt; do
    if [[ -n "$certificate" && -f "$certificate" ]]; then
      export PANTS_CA_CERTS_PATH="$certificate"
      break
    fi
  done
fi
exec /entrypoint.sh "$@"
