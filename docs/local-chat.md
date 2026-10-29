# Local single-chat playground

Run Centaur's control plane, database, credential proxy, Console, Slackbot, and
agent sandboxes in a dedicated local Kind cluster. A terminal client replaces
the Slack UI: fake Slack signs each fixture-user message and delivers it to the
real Slackbot webhook. Sessions, executions, sandbox work, and reply rendering
follow their normal paths. No Slack app, tunnel, or real Slack token is needed.

This first version provides an opted-in continuing DM, including steering and
stop, and can delegate manual jobs to separate temporary sandboxes through
Centaur's existing Console task API. It adds no database migration. It does not
yet extract memory objects, maintain persistent child chats, or select and
coordinate persistent project sandboxes. The scope and follow-on design are in
[Private DM continuity](private-dm-continuity.md).

## Prepare the images once

Install Docker, Kind, kubectl, Helm, jq, OpenSSL, curl, and Bun. The setup script
uses existing Docker images; it does not build them. It expects
`centaur-{api-rs,proxy-sync,token-broker,iron-proxy,slackbotv2,agent,console}:local-dm`.
Build the control-plane images from this checkout so Slackbot includes the DM
experiment. The recipes are in the root `justfile`; with `just` installed:

```sh
for service in api-rs proxy-sync token-broker iron-proxy slackbotv2 console; do
  just build-one "$service"
  docker tag "centaur-$service:latest" "centaur-$service:local-dm"
done
```

The standard public sandbox image supports native Apple Silicon. This pinned
multi-platform image was verified to include `linux/arm64`:

```sh
local_sandbox_image=ghcr.io/paradigmxyz/centaur/centaur-agent@sha256:ff4b51af8d11ce0cf673dc4013e0041c515d5a25595e26032eb00538f456f99d
docker pull --platform linux/arm64 "$local_sandbox_image"
docker tag "$local_sandbox_image" centaur-agent:local-dm
```

Use `linux/amd64` on an Intel host. This is the standard upstream image, with no
private overlay. Its source revision is
`77f6f23aec5f1ed6d0336bd51dcd01657c7a34cb`. To test sandbox runtime changes from
this checkout instead, build the normal image locally:

```sh
docker build --target sandbox -f services/sandbox/Dockerfile -t centaur-agent:local-dm .
```

## Start the playground

From the repository root, start with the mock provider:

```sh
contrib/scripts/local-chat-stack.sh up --mode mock
bun scripts/local-chat.ts
```

On first startup, plain `up` defaults to mock mode. Later `up` calls preserve
the previous mode. Mock mode runs the real harness and sandbox against the
scripted model fixture, which answers `ok` unless a response has been registered.
It is useful for testing routing, rendering, and cancellation; it does not
provide open-ended reasoning. For a recognizable mock reply, register one before
sending a message containing `local-chat-example`:

```sh
curl --fail-with-body http://127.0.0.1:30080/_e2e/replies \
  -H 'content-type: application/json' \
  --data '{"match":"local-chat-example","text":"The local DM path is working."}'
```

For an open-ended conversation, use real mode. Provide an existing private
dotenv file containing a nonempty `OPENAI_API_KEY` and optionally
`ANTHROPIC_API_KEY`, with no other fields. Use explicit unquoted `KEY=value`
lines, not shell `export` statements or key-only lines. Keep the file outside
the checkout and restrict its permissions. The default DM harness is Codex;
an Anthropic key alone cannot run this profile.

```sh
contrib/scripts/local-chat-stack.sh up --mode real \
  --model-env-file ../local-playground/provider.env
bun scripts/local-chat.ts
```

Alternatively, if the dedicated local cluster already contains the Secret
`centaur/centaur-model-credentials` with those provider-key fields:

```sh
contrib/scripts/local-chat-stack.sh up --mode real
```

Real mode resolves the actual provider endpoints and sends model requests through
Centaur's credential proxy. Provider usage is billed by the configured provider.
The setup script treats the env file as data, never sources it, and does not read
provider keys from ambient shell variables. The CLI does not handle model
credentials. No remote cluster is needed for credential setup.

Changing provider mode while sandbox proxies exist is rejected to avoid leaving
those proxies with credentials or routing from the previous mode. Credential
rotation on an existing deployment is also rejected because running processes
would retain their previous environment. Choose the mode and credentials before
starting conversations, or explicitly delete and recreate the local cluster
with `down` before changing them. Deleting the cluster loses its database.

## Use the chat

Type a message and press Enter. Replies are polled and shown again when Slackbot
edits them. Send another message while a turn runs to exercise steering.

| Command | Behavior |
| --- | --- |
| `/stop` | Send the literal `stop` through Slackbot to interrupt the active run |
| `/status` | Show connection details and Slack-visible activity |
| `/history` | Replay the visible chat |
| `/help` | Show commands |
| `/quit` | Disconnect while any running execution continues |
| `//text` | Send a message beginning with `/` |

Ctrl-C and EOF also disconnect. Reconnecting shows the latest ten visible
messages. Fake Slack keeps its display history in memory, so restarting that
fixture loses the visible transcript; durable sessions and executions remain in
Postgres. A send is accepted by fake Slack before signed webhook delivery
finishes. `/status` does not certify execution completion, and canonical DMs do
not use Slack's native per-thread status. Check service logs and durable execution
state if no reply appears.

For a different local port-forward, use
`bun scripts/local-chat.ts --url http://127.0.0.1:30444` or set
`CENTAUR_LOCAL_SLACK_URL`. Only loopback URLs are accepted. The setup script's
default port mappings are fixed in `e2e/infra/kind.yaml`.

## Run jobs in separate sandboxes

In real mode, ask the chat to use Centaur's manual tasks. For example:

> Create two manual Centaur tasks with no recurring schedule. In one, write and
> run a Python script that sums 1 through 10. In the other, write and run a Python
> script that calculates 6 factorial. Run each task in its own sandbox and report
> its task ID, hostname, and result back here.

The agent discovers installed shell tools with `centaur-tools list`, creates
each job with `centaur-console create-task NAME --prompt PROMPT` without
`--cron`, and starts it with `centaur-console run-task TASK_ID`. The `tasks` and
`task` commands inspect the owner's tasks. Each run executes in its own
temporary Centaur sandbox, delivers its result into the same fake Slack DM, and
cleans up that sandbox after completion. Native harness subagents share their
parent sandbox; Console task runs provide the separate instances in this flow.

Setup links the fixture DM identity to an active, non-admin Console user. These
task commands use the owner-scoped Console API, which checks that linked user
and task ownership. Generic workflow API permissions
`sandbox_workflows_read_enabled` and `sandbox_workflows_write_enabled` remain
false and are not required for these commands. Queuing a task does not mean it
has completed; its delivered reply supplies the result. This mechanism does not
provide persistent child chats or project sandbox selection.

## Local state and lifecycle

The setup script creates `centaur-local-dm`, namespace `centaur`, and a separate
kubeconfig at `../local-playground/kubeconfig`. It verifies that the cluster API
is on loopback and explicitly selects this context for Kubernetes and Helm
operations; it does not change the usual kubeconfig. The state directory also
records the cluster name and model mode. Use `--state-dir` and `--cluster` on
every invocation if overriding these defaults; custom names must start with
`centaur-local-`.

```sh
contrib/scripts/local-chat-stack.sh status
# After rebuilding a local-dm image, import it and update the running stack:
contrib/scripts/local-chat-stack.sh up
```

`up` preserves the database and existing sandboxes, imports the local images,
and rolls out changed control-plane images with content-based tags. Sandbox
image changes apply to newly created sandboxes. Ordinary `up` also preserves
fake Slack's display history unless its fixture source changes.

| Service | Local address |
| --- | --- |
| Fake Slack, used by the CLI | `http://127.0.0.1:30443` |
| Centaur API | `http://127.0.0.1:30081` |
| Console | `http://127.0.0.1:30082` |
| Postgres | `127.0.0.1:30432` |
| Scripted model control, mock mode only | `http://127.0.0.1:30080` |

To inspect Slackbot logs with the isolated configuration:

```sh
kubectl --kubeconfig ../local-playground/kubeconfig --context kind-centaur-local-dm \
  -n centaur logs deployment/centaur-centaur-slackbotv2 --tail=100
```

**`down` deletes the dedicated local cluster, its database, and its sandboxes.**
Use it only when that local state can be discarded:

```sh
contrib/scripts/local-chat-stack.sh down
```

The script does not invoke the e2e test suite or its session-reset lifecycle.
Keep this persistent playground separate from disposable e2e runs.

## Experiment configuration

The profile in `e2e/infra/local-chat/values.yaml` enables agent view and the
unified-DM experiment only for fixture identity `TE2E00001:UE2EUSER1`. Its DM is
`DE2EUSER1`; the expected canonical session key is `slack:DE2EUSER1:`. Both the
global flag and the exact team/user allowlist entry are required:

```yaml
slackbotv2:
  unifiedDmEnabled: true
  unifiedDmUsers:
    - "TE2E00001:UE2EUSER1"
```

The profile selects the Codex harness for that DM, disables the warm sandbox
pool, optional tool server, and repository cache, and uses API-key provider
authentication. `sandbox.extraEnv.TOOL_DIRS=/opt/centaur/tools` exposes the tools
baked into the standard sandbox image even with the optional tool server
disabled. A local Codex instruction overlay explains manual task delegation.
The setup script wires the fixture signing secret and Slack API URL, including
Console's Slack API calls, and seeds the linked non-admin fixture user. Other
fixture users retain normal routing.

To revert the experiment locally, change `unifiedDmEnabled` to `false` in that
profile and rerun `up`. This changes ingress routing without deleting existing
canonical or timestamped sessions. Re-enabling the same allowlist resumes the
canonical session; historical threaded sessions are not merged into it.

Focused CLI transport validation:

```sh
bun test scripts/local-chat.test.ts
```
