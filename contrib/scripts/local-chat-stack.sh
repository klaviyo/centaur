#!/usr/bin/env bash
# Persistent local DM playground. Uses prebuilt local-dm images and the e2e
# Slack fixture; does not invoke the destructive e2e test/reset lifecycle.
set +x
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
CLUSTER="${CENTAUR_LOCAL_CLUSTER:-centaur-local-dm}"
STATE_DIR="${CENTAUR_LOCAL_STATE_DIR:-$ROOT/../local-playground}"
MODE="${CENTAUR_LOCAL_MODEL_MODE:-}"
MODEL_ENV_FILE=""
RELEASE=centaur
NAMESPACE=centaur
SLACK_NAMESPACE=centaur-e2e-slack
MODEL_NAMESPACE=centaur-e2e-model
API_ADMIN_KEY=centaur-e2e-api-admin-key
CHATGPT_ACCOUNT_ID=e2e00000-0000-4000-8000-00000000c0de
IMAGES=(api-rs:apiRs proxy-sync:proxySync token-broker:tokenBroker iron-proxy:ironProxy slackbotv2:slackbotv2 agent:sandbox console:console)
WORK=""

usage() {
  cat <<'HELP'
Usage: contrib/scripts/local-chat-stack.sh up|status|down [options]

  --mode mock|real         Mock on first up; subsequent up preserves last mode.
  --model-env-file PATH    Read provider keys from a dotenv file, never source it.
                          Real mode also accepts existing Kubernetes Secret
                          centaur/centaur-model-credentials containing
                          nonempty OPENAI_API_KEY (required for Codex),
                          with optional ANTHROPIC_API_KEY.
  --state-dir PATH         Persistent kubeconfig and mode; default ../local-playground.
  --cluster NAME          Dedicated Kind name; default centaur-local-dm.
                          Custom names must begin with centaur-local-.

Requires Docker, Kind, kubectl, Helm, jq, OpenSSL, curl and these prebuilt images:
  centaur-{api-rs,proxy-sync,token-broker,iron-proxy,slackbotv2,agent,console}:local-dm
Run normal Dockerfile recipes separately to rebuild them. Up reloads images and
uses content tags for control-plane rollouts. It preserves the database and
sandboxes. Down explicitly deletes this local cluster and its data.

The fake Slack API is http://127.0.0.1:30443. The opted-in identity is
TE2E00001:UE2EUSER1 and its DM is DE2EUSER1. API: :30081; Console: :30082;
Postgres: :30432; mock model control API: :30080 (mock mode only).

Real mode sends prompts to a real provider and can incur model charges. Provider
keys are copied only from the supplied file or local credentials Secret, never
from ambient shell variables. Switching modes with existing sandbox proxies is
rejected. Changed provider keys are also rejected while a Helm release or sandbox
proxy exists, before updating either Secret. Use down explicitly to recreate the
playground, or perform a separate deliberate credential update and restart of
all affected pods with the private kubeconfig and context.
HELP
}
fail() { echo "Error: $*" >&2; exit 1; }
COMMAND="${1:-help}"
[[ $# -eq 0 ]] || shift
while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode) MODE="${2:?--mode requires mock or real}"; shift 2 ;;
    --model-env-file) MODEL_ENV_FILE="${2:?--model-env-file requires a path}"; shift 2 ;;
    --state-dir) STATE_DIR="${2:?--state-dir requires a path}"; shift 2 ;;
    --cluster) CLUSTER="${2:?--cluster requires a name}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) fail "unknown option: $1" ;;
  esac
done
case "$COMMAND" in up|status|down) ;; help|-h|--help) usage; exit 0 ;; *) usage >&2; exit 2 ;; esac
[[ "$CLUSTER" =~ ^centaur-local-[a-z0-9][a-z0-9-]*$ ]] || fail "cluster name must begin with centaur-local-"
[[ -z "$MODE" || "$MODE" == mock || "$MODE" == real ]] || fail "mode must be mock or real"
for command in kind kubectl docker jq; do command -v "$command" >/dev/null || fail "$command is required"; done
mkdir -p "$STATE_DIR"
STATE_DIR="$(cd "$STATE_DIR" && pwd)"
if [[ -f "$STATE_DIR/cluster" && "$(cat "$STATE_DIR/cluster")" != "$CLUSTER" ]]; then
  fail "state directory belongs to a different cluster; choose another --state-dir"
fi
printf '%s\n' "$CLUSTER" > "$STATE_DIR/cluster"
export KUBECONFIG="$STATE_DIR/kubeconfig"
CONTEXT="kind-$CLUSTER"
trap '[[ -z "$WORK" ]] || rm -rf "$WORK"' EXIT
k() { kubectl --kubeconfig "$KUBECONFIG" --context "$CONTEXT" "$@"; }
h() { helm --kubeconfig "$KUBECONFIG" --kube-context "$CONTEXT" "$@"; }
apply_stdin() { k apply -f - >/dev/null; }
cluster_exists() { kind get clusters | grep -Fxq "$CLUSTER"; }
verify_context() {
  [[ "$(kubectl --kubeconfig "$KUBECONFIG" config current-context)" == "$CONTEXT" ]] || fail "unexpected Kubernetes context"
  local server
  server="$(kubectl --kubeconfig "$KUBECONFIG" config view --minify -o jsonpath='{.clusters[0].cluster.server}')"
  [[ "$server" == https://127.0.0.1:* || "$server" == https://localhost:* ]] || fail "cluster API is not on localhost"
}
use_cluster() {
  kind get kubeconfig --name "$CLUSTER" > "$KUBECONFIG.tmp"
  chmod 600 "$KUBECONFIG.tmp"
  mv "$KUBECONFIG.tmp" "$KUBECONFIG"
  verify_context
}
rand_hex() { openssl rand -hex 32 | tr -d '\n'; }

bootstrap() {
  k create namespace "$NAMESPACE" --dry-run=client -o yaml | apply_stdin
  # Same key contract as bootstrap-k8s-secrets.sh, but use a private env file
  # so generated local credentials never occur in subprocess arguments.
  if ! k -n "$NAMESPACE" get secret centaur-infra-env >/dev/null 2>&1; then
    local password
    password="$(rand_hex)"
    {
      printf 'IRON_MANAGEMENT_API_KEY=%s\n' "$(rand_hex)"
      printf 'OP_SERVICE_ACCOUNT_TOKEN=unused\nOP_VAULT=unused\n'
      printf 'SLACK_BOT_TOKEN=xoxb-centaur-e2e\nSLACK_SIGNING_SECRET=centaur-e2e-signing-secret\n'
      printf 'SLACKBOT_API_KEY=%s\nPOSTGRES_PASSWORD=%s\n' "$(rand_hex)" "$password"
      printf 'DATABASE_URL=postgresql://tempo:%s@centaur-centaur-postgres:5432/ai_v2\n' "$password"
      printf 'IRON_CONTROL_DATABASE_URL=postgresql://tempo:%s@centaur-centaur-postgres:5432\n' "$password"
      printf 'IRON_CONTROL_INITIAL_USER_EMAIL=admin@centaur.local\n'
      printf 'IRON_CONTROL_INITIAL_USER_PASSWORD=%s\nIRON_CONTROL_INITIAL_API_KEY=iak_%s\n' "$(rand_hex)" "$(rand_hex)"
      printf 'IRON_CONTROL_AR_ENCRYPTION_PRIMARY_KEY=%s\n' "$(rand_hex)"
      printf 'IRON_CONTROL_AR_ENCRYPTION_DETERMINISTIC_KEY=%s\n' "$(rand_hex)"
      printf 'IRON_CONTROL_AR_ENCRYPTION_KEY_DERIVATION_SALT=%s\n' "$(rand_hex)"
      printf 'IRON_CONTROL_SECRET_KEY_BASE=%s%s\n' "$(rand_hex)" "$(rand_hex)"
      printf 'CENTAUR_JWT_SIGNING_SECRET=%s%s\n' "$(rand_hex)" "$(rand_hex)"
      printf 'CENTAUR_APIRS_ADMIN_API_KEY=%s\n' "$API_ADMIN_KEY"
    } > "$WORK/infra.env"
    k -n "$NAMESPACE" create secret generic centaur-infra-env --from-env-file="$WORK/infra.env" >/dev/null
    rm "$WORK/infra.env"
  fi
  if k -n "$NAMESPACE" get secret centaur-firewall-ca centaur-firewall-ca-key >/dev/null 2>&1; then
    return
  fi
  if k -n "$NAMESPACE" get secret centaur-firewall-ca >/dev/null 2>&1 || k -n "$NAMESPACE" get secret centaur-firewall-ca-key >/dev/null 2>&1; then
    fail "incomplete local firewall CA secrets; refusing to rotate an existing CA"
  fi
  openssl genrsa -out "$WORK/ca-key.pem" 4096 >/dev/null 2>&1
  openssl req -x509 -new -nodes -key "$WORK/ca-key.pem" -sha256 -days 3650 \
    -subj '/CN=centaur iron-proxy CA' -addext basicConstraints=critical,CA:TRUE \
    -addext keyUsage=critical,keyCertSign -out "$WORK/ca.pem" >/dev/null 2>&1
  k -n "$NAMESPACE" create secret generic centaur-firewall-ca --from-file=ca-cert.pem="$WORK/ca.pem" >/dev/null
  k -n "$NAMESPACE" create secret generic centaur-firewall-ca-key \
    --from-file=ca-cert.pem="$WORK/ca.pem" --from-file=ca-key.pem="$WORK/ca-key.pem" >/dev/null
}

validate_provider() {
  jq -ce 'if (.data | keys | all(. == "OPENAI_API_KEY" or . == "ANTHROPIC_API_KEY")) and
      (.data.OPENAI_API_KEY != null and .data.OPENAI_API_KEY != "") and
      (.data.ANTHROPIC_API_KEY == null or .data.ANTHROPIC_API_KEY != "")
    then . else error("Codex playground requires nonempty OPENAI_API_KEY; only optional ANTHROPIC_API_KEY is also allowed") end'
}

validate_model_env_file() {
  [[ -r "$MODEL_ENV_FILE" ]] || fail "model env file is not readable"
  # kubectl accepts a key-only line by reading that variable from its own
  # environment. Reject it, shell assignments, and quoted values explicitly.
  awk '
    /^[[:space:]]*($|#)/ { next }
    {
      if ($0 !~ /^(OPENAI_API_KEY|ANTHROPIC_API_KEY)=.+$/) exit 1
      value = substr($0, index($0, "=") + 1)
      if (value ~ /[[:space:]]/ || index(value, sprintf("%c", 34)) ||
          index(value, sprintf("%c", 39)) || index(value, sprintf("%c", 96)) || index(value, "$")) exit 1
    }
  ' "$MODEL_ENV_FILE" || fail "model env file must use explicit, unquoted OPENAI_API_KEY=value and optional ANTHROPIC_API_KEY=value lines"
}

provider_document() {
  if [[ "$MODE" == mock ]]; then
    jq -cn '{data:{OPENAI_API_KEY:("sk-e2e-openai-test-key" | @base64),ANTHROPIC_API_KEY:("sk-ant-e2e-anthropic-test-key" | @base64)}}'
  elif [[ -n "$MODEL_ENV_FILE" ]]; then
    validate_model_env_file
    # Remove these variables as defense in depth against ambient fallback.
    env -u OPENAI_API_KEY -u ANTHROPIC_API_KEY kubectl --kubeconfig "$KUBECONFIG" --context "$CONTEXT" \
      -n "$NAMESPACE" create secret generic centaur-model-credentials \
      --from-env-file="$MODEL_ENV_FILE" --dry-run=client -o json | validate_provider
  else
    k -n "$NAMESPACE" get secret centaur-model-credentials -o json | validate_provider
  fi
}

provider_fingerprint() {
  # Compare only the copied fields. Digests are held in memory and never shown.
  jq -Sc '{OPENAI_API_KEY:(.data.OPENAI_API_KEY // null),ANTHROPIC_API_KEY:(.data.ANTHROPIC_API_KEY // null)}' |
    openssl dgst -sha256
}

check_provider_change() {
  local requested existing release proxies
  requested="$(provider_document | provider_fingerprint)"
  existing="$(k -n "$NAMESPACE" get secret centaur-infra-env -o json | provider_fingerprint)"
  [[ "$requested" != "$existing" ]] || return 0
  # Check before applying the source Secret or infra Secret: live pods snapshot
  # env credentials and would otherwise continue using the previous values.
  release="$(h list -n "$NAMESPACE" --all --filter "^${RELEASE}$" -q)"
  proxies="$(k -n "$NAMESPACE" get pods -l centaur.ai/iron-proxy=true -o name)"
  if [[ -n "$release$proxies" ]]; then
    fail "provider keys differ from the running playground; no credentials were changed. Use down explicitly and recreate, or separately update credentials and restart every affected pod using kubeconfig $KUBECONFIG and context $CONTEXT"
  fi
}

configure_provider() {
  if [[ "$MODE" == real && -z "$MODEL_ENV_FILE" ]]; then
    k -n "$NAMESPACE" get secret centaur-model-credentials >/dev/null 2>&1 ||
      fail "real mode requires --model-env-file or local Secret centaur/centaur-model-credentials with OPENAI_API_KEY"
  elif [[ "$MODE" == mock && -n "$MODEL_ENV_FILE" ]]; then
    fail "--model-env-file is only valid with --mode real"
  fi
  check_provider_change
  if [[ "$MODE" == real ]]; then
    if [[ -n "$MODEL_ENV_FILE" ]]; then
      # Server-side apply avoids a last-applied Secret annotation.
      provider_document | k apply --server-side --field-manager=centaur-local-chat -f - >/dev/null
    fi
    # No provider key is put in an argument, log, shell variable, or scratch file.
    provider_document |
      jq --arg admin "$API_ADMIN_KEY" '{data:{OPENAI_API_KEY:.data.OPENAI_API_KEY,ANTHROPIC_API_KEY:.data.ANTHROPIC_API_KEY},stringData:{CENTAUR_APIRS_ADMIN_API_KEY:$admin}}' |
      k -n "$NAMESPACE" patch secret centaur-infra-env --type merge --patch-file=/dev/stdin >/dev/null
    printf '{"apiVersion":"v1","kind":"Secret","metadata":{"name":"centaur-local-iron-proxy-env","namespace":"centaur"},"data":{}}\n' | apply_stdin
  else
    printf '{"stringData":{"OPENAI_API_KEY":"sk-e2e-openai-test-key","ANTHROPIC_API_KEY":"sk-ant-e2e-anthropic-test-key","OPENAI_CODEX_ACCOUNT_ID":"%s","CENTAUR_APIRS_ADMIN_API_KEY":"%s"}}\n' "$CHATGPT_ACCOUNT_ID" "$API_ADMIN_KEY" |
      k -n "$NAMESPACE" patch secret centaur-infra-env --type merge --patch-file=/dev/stdin >/dev/null
    k -n "$NAMESPACE" create secret generic centaur-local-iron-proxy-env \
      --from-literal=SSL_CERT_FILE=/etc/iron-proxy-ca/ca-cert.pem --dry-run=client -o yaml | apply_stdin
  fi
}

configure_dns() {
  local corefile=e2e/infra/local-chat/Corefile
  [[ "$MODE" != mock ]] || corefile=e2e/infra/Corefile
  # Only our verified dedicated node is changed; never touch the host resolver.
  docker exec "${CLUSTER}-control-plane" sh -c \
    "grep -v '^search ' /etc/resolv.conf > /tmp/resolv.conf && cat /tmp/resolv.conf > /etc/resolv.conf"
  k -n kube-system create configmap coredns --from-file="Corefile=$corefile" --dry-run=client -o yaml | apply_stdin
  k -n kube-system rollout restart deployment/coredns >/dev/null
  k -n kube-system rollout status deployment/coredns --timeout=180s
}

deploy_model_server() {
  k -n "$NAMESPACE" get secret centaur-firewall-ca-key -o jsonpath='{.data.ca-cert\.pem}' | openssl base64 -d -A > "$WORK/ca.pem"
  k -n "$NAMESPACE" get secret centaur-firewall-ca-key -o jsonpath='{.data.ca-key\.pem}' | openssl base64 -d -A > "$WORK/ca-key.pem"
  openssl req -new -newkey rsa:2048 -nodes -subj /CN=api.openai.com -keyout "$WORK/tls.key" -out "$WORK/tls.csr" 2>/dev/null
  printf 'subjectAltName=DNS:api.openai.com,DNS:chatgpt.com,DNS:api.anthropic.com\nextendedKeyUsage=serverAuth\n' > "$WORK/ext.cnf"
  openssl x509 -req -in "$WORK/tls.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca-key.pem" \
    -CAcreateserial -days 30 -sha256 -extfile "$WORK/ext.cnf" -out "$WORK/tls.crt" 2>/dev/null
  k create namespace "$MODEL_NAMESPACE" --dry-run=client -o yaml | apply_stdin
  k -n "$MODEL_NAMESPACE" create secret tls model-server-tls --cert="$WORK/tls.crt" --key="$WORK/tls.key" --dry-run=client -o yaml | apply_stdin
  k -n "$MODEL_NAMESPACE" create secret generic model-server --from-literal=CHATGPT_ACCOUNT_ID="$CHATGPT_ACCOUNT_ID" --dry-run=client -o yaml | apply_stdin
  k -n "$MODEL_NAMESPACE" create configmap model-server --from-file=e2e/fakes/model-server.ts --dry-run=client -o yaml | apply_stdin
  k apply -f e2e/infra/model-server.yaml >/dev/null
  k -n "$MODEL_NAMESPACE" rollout restart deployment/model-server >/dev/null
  k -n "$MODEL_NAMESPACE" rollout status deployment/model-server --timeout=180s
}

deploy_fake_slack() {
  k create namespace "$SLACK_NAMESPACE" --dry-run=client -o yaml | apply_stdin
  k -n "$SLACK_NAMESPACE" create configmap fake-slack --from-file=e2e/fakes/fake-slack.ts \
    --from-file=e2e/fakes/slack-fixture.ts --dry-run=client -o yaml | apply_stdin
  k -n "$SLACK_NAMESPACE" create secret generic fake-slack \
    --from-literal=SLACK_BOT_TOKEN=xoxb-centaur-e2e \
    --from-literal=SLACK_SIGNING_SECRET=centaur-e2e-signing-secret \
    --from-literal=SLACK_EVENTS_URL="http://${RELEASE}-centaur-slackbotv2.${NAMESPACE}.svc.cluster.local:3001/api/webhooks/slack" \
    --dry-run=client -o yaml | apply_stdin
  k -n "$SLACK_NAMESPACE" apply -f e2e/infra/fake-slack.yaml >/dev/null
  # The fixture stores its messages in memory. Preserve them on ordinary up;
  # a source change updates the pod template to load the new code.
  local digest
  digest="$(cat e2e/fakes/fake-slack.ts e2e/fakes/slack-fixture.ts | openssl dgst -sha256 | awk '{print $NF}')"
  k -n "$SLACK_NAMESPACE" patch deployment fake-slack --type merge \
    -p "{\"spec\":{\"template\":{\"metadata\":{\"annotations\":{\"centaur.local/fixture-sha\":\"$digest\"}}}}}" >/dev/null
  k -n "$SLACK_NAMESPACE" rollout status deployment/fake-slack --timeout=180s
}

up() {
  for command in helm openssl curl; do command -v "$command" >/dev/null || fail "$command is required"; done
  MODE="${MODE:-$(cat "$STATE_DIR/model-mode" 2>/dev/null || printf mock)}"
  [[ "$MODE" == mock || "$MODE" == real ]] || fail "invalid stored model mode"
  local entry service key tag image_id
  local image_args=() load_images=()
  for entry in "${IMAGES[@]}"; do
    service="${entry%%:*}"
    docker image inspect "centaur-$service:local-dm" >/dev/null 2>&1 || fail "missing centaur-$service:local-dm; build/pull it first"
  done
  if cluster_exists; then
    use_cluster
    docker port "${CLUSTER}-control-plane" | grep -q '^30082/tcp' || fail "cluster lacks local-chat port mappings; use down explicitly before recreating"
  else
    kind create cluster --name "$CLUSTER" --config e2e/infra/kind.yaml --kubeconfig "$KUBECONFIG" --wait 120s
    verify_context
  fi
  local previous_mode proxies
  previous_mode="$(cat "$STATE_DIR/model-mode" 2>/dev/null || printf unknown)"
  proxies="$(k -n "$NAMESPACE" get pods -l centaur.ai/iron-proxy=true -o name 2>/dev/null || true)"
  if [[ "$MODE" != "$previous_mode" && -n "$proxies" ]]; then
    fail "mode change with existing sandbox proxies would leave stale credentials; use down explicitly first"
  fi
  WORK="$(mktemp -d "$STATE_DIR/.setup.XXXXXX")"
  bootstrap
  configure_provider
  # Record before DNS/Helm operations so an interrupted up has a safe retry mode.
  printf '%s\n' "$MODE" > "$STATE_DIR/model-mode"
  configure_dns
  [[ "$MODE" != mock ]] || deploy_model_server
  deploy_fake_slack
  for entry in "${IMAGES[@]}"; do
    service="${entry%%:*}" key="${entry#*:}"
    image_id="$(docker image inspect -f '{{.Id}}' "centaur-$service:local-dm")"
    tag="local-dm-$(printf '%s' "${image_id#sha256:}" | cut -c1-16)"
    # Keep the multi-GB sandbox's already-loaded tag; new sandboxes use it.
    [[ "$service" != agent ]] || tag=local-dm
    docker tag "centaur-$service:local-dm" "centaur-$service:$tag"
    load_images+=("centaur-$service:$tag")
    image_args+=(--set "${key}.image.repository=centaur-$service" --set "${key}.image.tag=$tag")
  done
  # Docker can retain a multi-platform index after pulling only this machine's
  # platform. Kind's --all-platforms import then requests nonexistent layers.
  # Select the verified node platform explicitly for both export and import.
  local node_arch host_arch platform
  node_arch="$(k get node "${CLUSTER}-control-plane" -o jsonpath='{.status.nodeInfo.architecture}')"
  host_arch="$(docker info --format '{{.Architecture}}')"
  case "$host_arch" in aarch64) host_arch=arm64 ;; x86_64) host_arch=amd64 ;; esac
  [[ "$node_arch" == "$host_arch" ]] || fail "Docker and Kind node architectures differ"
  platform="linux/$node_arch"
  docker image save --platform "$platform" "${load_images[@]}" |
    docker exec --privileged -i "${CLUSTER}-control-plane" ctr --namespace=k8s.io images import \
      --platform "$platform" --digests --snapshotter=overlayfs -
  if [[ ! -f contrib/chart/charts/connect-2.4.1.tgz ]]; then
    h repo add onepassword https://1password.github.io/connect-helm-charts --force-update >/dev/null
    h dependency build contrib/chart >/dev/null
  fi
  local release_status
  release_status="$(h status "$RELEASE" -n "$NAMESPACE" -o json 2>/dev/null | jq -r '.info.status' || true)"
  [[ "$release_status" != pending-* ]] || fail "Helm release is pending; inspect it before recovery (up will not uninstall your data)"
  h upgrade --install "$RELEASE" contrib/chart -n "$NAMESPACE" \
    -f contrib/chart/values.dev.yaml -f e2e/infra/values.yaml -f e2e/infra/local-chat/values.yaml \
    "${image_args[@]}" --wait --timeout 20m
  k apply -f e2e/infra/test-access.yaml >/dev/null
  local ready=false
  for ((attempt=0; attempt<90; attempt++)); do
    if curl -fsS -o /dev/null --max-time 2 http://127.0.0.1:30081/readyz 2>/dev/null && \
      curl -fsS -o /dev/null --max-time 2 http://127.0.0.1:30082/up 2>/dev/null; then ready=true; break; fi
    sleep 1
  done
  [[ "$ready" == true ]] || fail "API or Console NodePort did not become reachable"
  # Match the fake Slack identity to a non-admin Console user so the normal
  # owner-scoped manual-task commands work without broad workflow permissions.
  k -n "$NAMESPACE" exec -i "deployment/${RELEASE}-centaur-console" -- \
    bin/rails runner - < e2e/infra/local-chat/seed-user.rb
  status
}

status() {
  if ! cluster_exists; then printf 'Local cluster %s is not running.\n' "$CLUSTER"; return; fi
  use_cluster
  printf 'Cluster: %s\nKubeconfig: %s\nModel mode: %s\n' "$CLUSTER" "$KUBECONFIG" "$(cat "$STATE_DIR/model-mode" 2>/dev/null || printf unconfigured)"
  k get pods -A
  printf '\nFake Slack: http://127.0.0.1:30443\nAPI: http://127.0.0.1:30081\nConsole: http://127.0.0.1:30082\n'
  printf 'Unified DM: TE2E00001:UE2EUSER1 / DE2EUSER1\n'
}

down() {
  if cluster_exists; then use_cluster; kind delete cluster --name "$CLUSTER"; fi
  rm -f "$KUBECONFIG" "$STATE_DIR/model-mode"
}
"$COMMAND"
