# Private DM continuity: opt-in rollout and follow-on design

## Decision and scope

Make the private DM the stable conversation with Centaur. A future coordinator
can delegate project work to several sessions and sandboxes while retaining one
place for the user to talk, review progress, and resume deferred work.

The implementation provides conversation continuity and separately gated Git
memory. It routes selected Slack DM users to the existing conversation-wide
session and posts replies at the top level. An optional durable workflow extracts
and verifies bounded memory for one configured DM. It adds no database migration, persistent
project-session mapping, or automatic import of historical threads.

| Capability | Implemented | Follow-on work |
| --- | --- | --- |
| One continuing private DM | Opt-in canonical DM session | Product controls and recovery UX |
| Existing conversation context | Native harness history and optional bounded memory injection | Richer retrieval and reconciliation |
| Multiple project sandboxes | Existing single session assignment | Owner-bound child sessions |
| Current work, preferences, deferred actions, decisions | Evidence-backed records in a private Git repository | User-facing correction and forget controls |
| Revert experiment | Independently disable ingress and memory configuration | Child dispatch and cancellation controls |

## Implemented configuration and behavior

Both the global flag and an exact `TEAM_ID:USER_ID` allowlist match are required.
The empty allowlist enables nobody; enabling the flag alone is insufficient.

```yaml
slackbotv2:
  unifiedDmEnabled: true
  unifiedDmUsers:
    - "TEXAMPLE:UEXAMPLE"
```

The corresponding environment variables are `SLACKBOTV2_UNIFIED_DM_ENABLED`
(default `false`) and `SLACKBOTV2_UNIFIED_DM_USERS` (comma- or whitespace-separated
team/user pairs). These settings are deployment configuration, not a per-user
database preference or a conversational opt-in command.

For an eligible private DM, ingress uses the canonical Chat SDK `Thread` with
ID `slack:<DM_CHANNEL_ID>:`: its thread timestamp is deliberately blank. This
keeps the complete SDK thread interface for posting, state, subscription, and
render recovery. Responses are delivered into the main DM conversation.
Messages outside the gate continue through the existing routing behavior.

The canonical identity is already supported: `e2e/tests/context.test.ts` has a
conversation-wide DM test, and DM principals are derived from Slack team and
user in `crates/centaur-iron-control/src/principal.rs`. This is an ingress
routing change, not a new identity or permission model. Existing access gates
still apply; the allowlist is not a permission grant.

Canonical handoffs use the existing Postgres-backed SDK lock table. Contention
returns a retryable response before SDK deduplication. Lease ownership is checked
before new state and API mutations, and response cleanup verifies the execution
it belongs to. Already-dispatched API requests rely on the control plane's
existing execution serialization and idempotency; this adds no server-side
fencing token. The lost-lease retry path is pinned to both SDK 4.40.0 deduplication
keys and must be retested when upgrading the SDK.

If a canonical DM session already exists, enabling the mode reuses it. Existing
timestamped DM sessions remain intact and are not merged into it. Conversation
continuity starts from the canonical session's existing history, not all of the
user's old threads. Preserve original Slack message IDs for retry deduplication.
Replying in an old Slack thread can still include its normal thread context for
that turn; the old durable session records are not merged.

### User-visible tradeoffs

- The main DM stream receives replies even when incoming messages originated
  from a Slack thread. The old thread is not the active conversation boundary.
- Slack assistant title and status calls need a nonempty thread timestamp;
  `slackAssistantTarget` in `services/slackbotv2/src/index.ts` skips those calls
  for the canonical DM. Validate progress visibility with the actual Slack UI.
- Streaming and fallback replies use the canonical destination. Send `stop` in
  the DM to interrupt its active run. Slack's native thread Stop button still
  targets that original timestamped thread; it is not a control for the new
  conversation-wide session. Attachment content remains per message; historical
  attachments are not bulk imported by enabling the mode. The canonical key has
  no thread upload context, so sandbox upload tooling also needs live validation.
- A single session serializes its work and shares context across projects.
  Rapid messages may steer an active execution. This version does not provide
  persistent parallel project sandboxes or unlimited context retention. Manual
  Console tasks can run temporary jobs independently; stopping the main chat
  does not cancel those tasks.

## Target coordinator and project sessions

Console already supports owner-scoped manual tasks through
`centaur-console create-task` and `run-task`. Console verifies the linked active
user and task ownership, then starts its registered `console_workflow` as that
owner. This does not require generic workflow API permissions. Each run owns a
temporary sandbox and posts its result to the DM before cleanup. Child task
agents are instructed to execute directly, without recursively creating or
rerunning Console tasks. This existing delegation mechanism does not provide
persistent project sessions or aggregate cancellation.

Keep one persistent coordinator session per verified private DM owner. It owns
conversation state and a bounded mapping of project IDs to owner-bound child
session references. A project session owns its sandbox and repository context;
the coordinator should not switch one sandbox among unrelated projects.

Dispatch through a registered workflow with an explicit authorization contract.
The ingress currently lacks workflow-write capability (`auth.rs`); do not grant
broad workflow access simply to let it dispatch. Introduce a narrow control-plane
operation that derives the owner from the authenticated DM session, checks the
feature gate, and starts only the registered coordinator workflow. The model
must not choose an arbitrary principal, thread key, or workflow name.

`services/workflow-python/api/workflow_engine.py` and
`crates/centaur-workflows/src/lib.rs` already provide durable `ctx.agent_turn`
and bounded `ctx.run_agents`. They are workflow primitives, not a generic
fan-out tool currently available to the DM agent. A workflow must be added and
registered before the coordinator can use them.

`ctx.run_agents` creates distinct `wf:*` sessions, rejects caller-supplied thread
keys, preserves input order, and reports individual failures. Current bounds
are four concurrent agents by default, sixteen maximum, and thirty-two items
per batch. Workflow-owned sessions are cleaned up after the workflow completes.
Use this primitive for ephemeral parallel jobs with every child bound to the
verified DM owner.

Persistent project work needs the owner-checked project mapping and explicit
`ctx.agent_turn` thread keys; explicit keys opt out of workflow-owned cleanup.
Reusing project sessions through `ctx.run_agents` would require a deliberate
API extension. Do not assume the existing batch primitive preserves project
sandboxes across runs. Each dispatch must carry a stable idempotency key and
return an owner-checked session reference and bounded status to the coordinator.

The coordinator remains responsible for presenting combined progress and
results. A child failure must be visible without discarding successful siblings.
Cancellation needs an explicit contract for the coordinator and its children;
changing the opt-in flag must not silently kill already authorized project work.

## Implemented opt-in memory

[Git-backed personal memory](../services/api-rs/rfcs/git-personal-memory.md)
describes the configuration, data contract, recovery behavior, and limits.
`apiRs.memory.enabled` defaults to false and is independent of DM routing.
Enabling it requires an exact canonical DM, its existing principal, a private
Git repository and branch, a model, and sandboxed workflow hosting.

The repository's `memory/state.json` holds versioned current work, preferences,
deferred steps, decisions, constraints, user facts, and lessons. Bounded evidence
episodes preserve provenance; generated Markdown provides readable views.
`sessions.metadata.centaur_memory` holds only a derived context cache. The API
injects at most 12,000 UTF-8 bytes from that cache into the configured session's
normal and steering inputs, framed as untrusted evidence. Native harness
compaction remains responsible for the conversation window.

The scheduled `personal_memory` workflow reads bounded sources from the exact
DM/principal, extracts proposals, independently verifies them, then applies
revision-checked Git commits. Protected categories require direct user evidence.
Exact source IDs and window hashes prevent duplicate application; conflicts
leave evidence unprocessed for a fresh run. The workflow uses the principal's
credential proxy and does not post maintenance messages to Slack. Its public
run input/result contain status metadata, counts, and commit IDs rather than
private excerpts; redacted evidence and proposals remain in durable checkpoints
and the private repository.

Disabling memory stops new maintenance and injection without deleting data; an
already-running maintenance run can finish. Correct records with compensating
edits or retractions that retain processed source IDs. A plain Git revert can
remove checkpoints and cause evidence to be processed again. User-facing forget
and clear-memory controls, broader reconciliation, and project dispatch remain
follow-on work. Deferred steps never authorize execution or create schedules.

## Canary validation, rollout, and rollback

1. Validate service type checks, focused routing/render tests, and chart wiring.
   Prove default-off, empty allowlist, wrong team/user, non-DM conversations,
   top-level messages, thread replies, and existing-session reuse.
2. On an explicitly selected local stack, send two separate DM messages and
   confirm the same durable session and sandbox, context continuity, and
   top-level responses. Exercise attachments, rapid steering, stop controls,
   webhook retry, and restart/render recovery. Check a non-opted-in user too.
3. Deploy the flag off, then enable one team/user canary. Inspect durable
   message/execution records and final Slack delivery, not just healthy pods.
   Watch duplicate replies, missing deliveries, access failures, unexpected
   session churn, latency, and context confusion before expanding the allowlist.
4. To roll back, set `unifiedDmEnabled: false` or remove the selected user and
   roll out the configuration. New ingress resumes its prior routing. Existing
   in-flight executions and render obligations drain; the flag does not cancel
   them. Old binaries may also recover obligations left by the newer version.
5. Preserve the canonical session, its messages, and older timestamped threads.
   No database rollback or deletion is needed. Re-enabling reuses the canonical
   session; it does not create a clean context. Test disable/re-enable explicitly.

For memory canaries, additionally verify the Git source checkpoint, bounded
context in actual harness input, and a follow-up recall. Disable the memory flag
and confirm new inputs omit the context and new workflow runs are no-ops. Memory
can be disabled without changing DM routing; neither rollback needs a database
migration.

Scope future user-facing memory controls and persistent project dispatch
independently, including explicit child cancellation behavior. The current
change has performed no production deployment; rollout and its live verification
remain separate work.
