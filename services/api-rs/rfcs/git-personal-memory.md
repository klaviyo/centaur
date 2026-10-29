# Git-backed personal memory for a continuing DM

This opt-in experiment keeps a compact working set and evidence-backed durable
memory outside the harness transcript. It uses the existing durable workflow
engine and session metadata; there is no database migration. It is disabled by
default and binds one canonical Slack DM, one principal, and one private GitHub
repository. Channel conversations and other DMs are not ingested or injected.

## Data and context

The private Git repository is authoritative:

- `memory/state.json`: versioned records and processed source IDs.
- `episodes/*.json`: bounded, redacted evidence excerpts and applied proposals.
- `MEMORY.md`: reviewable projection of active and closed records.
- `WORKING_SET.md`: current work and deferred steps that remain open.
- `ALIGNMENT.md`: durable user preferences, decisions, constraints, and facts.

Records have a stable semantic identity, revision, status, exact evidence quotes,
source IDs, and an update timestamp. Categories cover current work, work
preferences, deferred steps, project decisions, project constraints, user facts,
and lessons. A completion or retraction preserves Git history while removing the
record from the active context. Deferral never creates a job or grants permission
to execute it. Quoted documents and assistant suggestions are not user decisions.

Before an opted-in normal turn or steering message, the API prepends at most
12,000 UTF-8 bytes of curated context from `sessions.metadata.centaur_memory`.
It validates the exact session/principal/repository/branch binding and Git commit.
The cache is explicitly untrusted evidence, with its last refresh time. The
current user request and higher-priority instructions retain precedence.
Invalid caches are omitted. Disabling the feature bypasses even the cache query.

Native harness compaction remains responsible for the live conversation window.
This feature does not reset the harness, delete its sandbox, or replace older
conversation with a memory summary. Detailed recall uses `centaur-memory search`,
`get`, and `explain`; exact original chat records remain in Centaur's existing
session store. The Git evidence excerpt is not a full transcript backup.

## Maintenance and recovery

The `personal_memory` workflow runs on a configurable schedule and can also be
started by an administrator. Its target comes exclusively from deployment
configuration; workflow input cannot choose another user or repository. It uses
the configured principal's credential proxy in a workflow sandbox. Both the
memory CLI and catalog shim must be present; unscoped tool-server fallback is
rejected.

Each run takes a session-scoped advisory lock and processes up to six previously
unprocessed sources from that DM's durable user/assistant messages and completed
execution results. Exact source IDs are retained in Git, so a late-committing
message cannot disappear behind a timestamp cursor. Attachments and internal
reasoning/tool traces are not ingested. Excerpts are capped at 2,000 characters;
long text includes its beginning and end with an explicit truncation notice.
Recognizable credentials are redacted before model requests or checkpoints, and
the Git writer independently rejects recognizable secrets. These checks cannot
guarantee detection of every possible secret format.

The durable steps are:

1. Read a pinned Git snapshot and a bounded source window.
2. Extract candidate claims using a structured model response with no tools.
3. Independently verify each proposal against the original evidence. Empty and
   rejected proposals are valid; malformed or incomplete reviews fail the run.
4. Validate exact quotes, direct-user evidence for protected categories, stable
   identities, and expected revisions in deterministic code.
5. Commit the records, evidence, and readable projections together. Git's
   non-force reference update rejects a concurrent writer; it never overwrites
   or auto-merges a stale proposal.
6. Reread the latest Git state and update the bounded session cache. This read is
   inside the retryable projection step, so recovery does not publish an old
   checkpoint over newer memory.

Model calls use the existing configured provider credentials, `store=false`,
strict JSON schemas, and no tools. Verification is batched within the input
budget. Model output never directly writes files or executes commands. Durable
steps checkpoint successful results. Replaying an already committed window is
idempotent; reusing a window ID with different content is rejected.

A Git conflict leaves sources unprocessed for a fresh scheduled run. A model or
Git failure does not advance the checkpoint. Cache publication is retried after
a successful Git commit. If upkeep is unavailable, the last known valid memory
can still appear with its timestamp and stale-data warning; it does not expire
automatically. Freshness-sensitive facts must be verified against their sources.
Nothing is posted to Slack by the maintenance workflow.

## Configuration and rollout

```yaml
apiRs:
  workflowHostSandbox: true
  # If workflowEnableMode is allowlist, retain existing names and add:
  workflowAllowedNames: "console_workflow,personal_memory"
  memory:
    enabled: true
    threadKey: "slack:DEXAMPLE:"
    principal: "prn_example"
    repository: "example/private-memory"
    branch: "main"
    model: "YOUR_CONFIGURED_MODEL"
    intervalSeconds: 3600
```

The principal must exactly equal the session's stored `iron_control_principal`.
Give it a GitHub credential grant with write access to the selected private
repository. The actual credential stays in the proxy; the sandbox receives a
placeholder. Use a private empty repository or a branch that already exists.
The first write initializes an empty repository, then commits the first memory
window. A missing branch in a nonempty repository is an error.

The chart supplies `CENTAUR_MEMORY_*` settings to the API and workflow host.
Ship the new API/workflow code and sandbox tool together. Updating the configured
sandbox image affects future sandboxes; a running sandbox needs its tool catalog
updated separately without resetting its harness. Keep existing deployment
identity, tool grants, and image configuration intact when enabling this option.

Roll back by setting `apiRs.memory.enabled: false`. New maintenance runs become
no-ops and new turns stop receiving injected memory. An already-running run can
finish; interrupt it explicitly if immediate cessation is required. The private
Git repository, chat history, and existing sandbox are preserved. Re-enabling
resumes from committed source IDs. Correct memory with a compensating edit or
retraction that retains the processed source IDs and provenance in
`memory/state.json`. Prefer a new evidence-backed revision through the memory
tool; reviewed direct Git edits must preserve the state schema and checkpoints.
A plain Git revert of a consolidation also removes its source checkpoints, so
the next maintenance run can replay that evidence and recreate the unwanted
claim. Changing generated Markdown alone does not alter canonical state.

## Bounds and current limits

Each scheduled run processes at most six sources and then stops; it does not
drain a backlog. The default one-hour interval therefore processes at most six
sources per hour, and one conversation turn can produce multiple sources. Use
a shorter interval for interactive testing or backlog workloads, such as
`intervalSeconds: 300`, and check run history and processed source IDs to confirm
that maintenance is catching up. Model latency, retries, and lock contention can
reduce throughput further; shortening the interval does not guarantee freshness.

The canonical store caps records at 1,000, processed windows at 2,000, and each
owned artifact at 1 MB. Reaching a bound fails visibly without deleting history
or silently advancing source IDs. Archive/migration should preserve deduplication
and provenance before those limits are increased. Only a bounded, relevance-
selected set of prior records is sent to the model; lexical search is available
for additional recall. This is not a vector database.

This increment does not add relationship graphs, nightly persona rewriting,
automatic skill installation, proactive messages, or autonomous deferred-task
execution. It separates durable memory from native compaction and gives each
memory change evidence, review, a revision check, and a Git audit trail.

## Validation

Run the memory tool tests, workflow/model tests, runtime injection tests, SQLx
ownership/size integration test against a disposable database, and Helm render
checks. Then exercise a real local workflow and follow-up chat: verify the Git
commit, source checkpoint, correctly scoped cache, recalled fact, no Slack
maintenance message, and placeholder credentials inside the sandbox. Repeat a
window to prove idempotency and disable the flag to prove rollback.
