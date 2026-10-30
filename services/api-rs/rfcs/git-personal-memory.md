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
- `ALIGNMENT_SYNTHESIS.md`: tentative derived guidance from reflection.
- `reflections/*.md`: reviewable reports of individual reflection windows.

Records have a stable semantic identity, revision, status, exact evidence quotes,
source IDs, and an update timestamp. Categories cover current work, work
preferences, deferred steps, project decisions, project constraints, user facts,
and lessons. A completion or retraction preserves Git history while removing the
record from the active context. Deferral never creates a job or grants permission
to execute it. Quoted documents and assistant suggestions are not user decisions.

Derived guidance uses the existing `lesson` category, a stable `reflection:` key,
and text beginning `Derived guidance: `. These labels distinguish interpretation
from facts and preserve the version 1 canonical state contract. A reflection is
a fallible prior that the current request overrides, never new evidence or an
authority grant. Raw historical evidence may corroborate a pattern; an old lesson,
generated projection, or repeated assistant assertion cannot. Reflection context
has a separate bound within the total context budget so it does not displace the
entire working set.

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
session store. `centaur-memory history` returns bounded raw episode evidence from
a pinned Git revision, after validating episode provenance and integrity. It
does not turn generated records or summaries into source evidence. The Git
evidence excerpt is not a full transcript backup.

## Maintenance and recovery

There are two configurable schedules; administrators can also run either workflow
manually:

- `personal_memory` consolidates evidence hourly by default. New events are
  compared with accumulated records and bounded historical raw evidence to add
  useful facts and reconcile explicit corrections, completions, or retractions.
- `personal_reflection` performs a daily reflection by default. It looks for
  recurring work patterns, friction, corrections, and boundaries, producing at
  most four tentative derived lessons per batch. Every lesson needs fresh
  evidence and direct user evidence. Repeated patterns need at least two distinct
  user source IDs; explicit corrections, preferences, or boundaries may use one.
  It cannot infer personality traits, permission, or commitments.

Each loop has an independent exact source-ID checkpoint in Git. Daily reflection
can consider events already consolidated hourly; neither loop consumes the
other's checkpoint. An idle loop skips model calls and Git writes. Exact IDs also
ensure a late-committing message cannot disappear behind a timestamp cursor.
Reflection checkpoint IDs use a `reflection:` prefix, so an older ingestion
workflow cannot mistake them for already-ingested raw event IDs. The canonical
state remains schema version 1 without a database or memory-state migration.

Targets come exclusively from deployment configuration; workflow input cannot
choose another user or repository. Both workflows use the configured principal's
credential proxy in a workflow sandbox. The memory CLI and catalog shim must be
present; unscoped tool-server fallback is rejected. They share a session-scoped
advisory lock. Lock contention retries durably every 30 seconds, up to 60 times,
then fails visibly rather than marking the work complete.

Each batch contains up to six previously unprocessed sources from that DM's
durable user/assistant messages and completed execution results, plus bounded
historical evidence selected for comparison. Attachments and internal
reasoning/tool traces are not ingested. Excerpts are capped at 2,000 characters;
long text includes its beginning and end with an explicit truncation notice.
Recognizable credentials are redacted before model requests or checkpoints, and
the Git writer independently rejects recognizable secrets. These checks cannot
guarantee detection of every possible secret format.

Historical retrieval examines the latest 24 evidence windows and at most 50
original excerpts, then selects whole excerpts by relevance and recency within
the model budget. This is bounded recall, not an exhaustive search of every past
project. Older records can provide context, but their generated text is never
substituted for original evidence.

The durable steps are:

1. Read a pinned Git snapshot and a bounded source window. Skip model work and
   writes if there are no new events for this loop.
2. Read bounded raw history and select relevant existing records. Propose up to
   four reconciliations or derived lessons using structured responses without
   tools. Each proposal must cite fresh evidence; historical evidence alone does
   not justify a new change.
3. Independently verify each proposal against the original evidence. Reflection
   uses a distinct review that distinguishes explicit corrections from inferred
   patterns. Empty and rejected proposals are valid; malformed or incomplete
   reviews fail the run.
4. Validate exact quotes, direct-user evidence for protected categories, stable
   identities, and expected revisions in deterministic code. An independent
   preservation review then approves or rejects the entire plan, checking that
   still-supported constraints, uncertainty, boundaries, and unrelated meaning
   survive every update. It receives complete affected records and sources. If
   a plan exceeds its input budget, discard that entire plan and independently
   propose and review one change for one fresh event; the remaining events stay
   pending. A single change that still cannot fit fails closed. A valid rejection checkpoints
   the examined window without applying any of its record changes; malformed
   reviews or model failures do not advance the checkpoint.
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
Nothing is posted to Slack by either memory workflow. Memory does not authorize
new tools, tasks, schedules, or messages.

## Configuration and rollout

```yaml
apiRs:
  workflowHostSandbox: true
  # If workflowEnableMode is allowlist, retain existing names and add:
  workflowAllowedNames: "console_workflow,personal_memory,personal_reflection"
  memory:
    enabled: true
    threadKey: "slack:DEXAMPLE:"
    principal: "prn_example"
    repository: "example/private-memory"
    branch: "main"
    model: "YOUR_CONFIGURED_MODEL"
    intervalSeconds: 3600
    reflectionEnabled: true
    reflectionIntervalSeconds: 86400
```

The principal must exactly equal the session's stored `iron_control_principal`.
Give it a GitHub credential grant with write access to the selected private
repository. The actual credential stays in the proxy; the sandbox receives a
placeholder. Use a private empty repository or a branch that already exists.
The first write initializes an empty repository, then commits the first memory
window. A missing branch in a nonempty repository is an error.

`reflectionEnabled` defaults to true but is gated by the overall `enabled`
setting, which defaults to false. Disabling reflection alone leaves hourly
consolidation and memory injection enabled. The schedules use intervals, so
daily means every 86,400 seconds rather than a particular wall-clock time.

The chart supplies `CENTAUR_MEMORY_*` settings to the API and workflow host,
including `CENTAUR_MEMORY_REFLECTION_ENABLED` and
`CENTAUR_MEMORY_REFLECTION_INTERVAL_SECONDS`.
Ship the new API/workflow code and sandbox tool together. Updating the configured
sandbox image affects future sandboxes; a running sandbox needs its tool catalog
updated separately without resetting its harness. Keep existing deployment
identity, tool grants, and image configuration intact when enabling this option.

Roll back both loops by setting `apiRs.memory.enabled: false`. New maintenance runs become
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

Each scheduled run drains a bounded number of batches: at most eight for hourly
consolidation and 32 for daily reflection, each with up to six fresh sources.
That is at most 48 hourly sources or 192 daily sources per run; one conversation
turn can produce multiple sources. Any remaining backlog is left for another
run. Check run history and independent processed source IDs to confirm both loops
are catching up. Model latency, retries, and lock contention can reduce throughput
further. Manual administrator runs are available for interactive testing without
shortening the normal schedules.

The canonical store caps records at 1,000, processed windows at 2,000, and each
owned artifact at 1 MB. Reaching a bound fails visibly without deleting history
or silently advancing source IDs. Archive/migration should preserve deduplication
and provenance before those limits are increased. Only a bounded, relevance-
selected set of prior records is sent to the model; lexical search is available
for additional recall. This is not a vector database.

This increment does not add relationship graphs, persona rewriting,
automatic skill installation, proactive messages, or autonomous deferred-task
execution. It separates durable memory from native compaction and gives each
memory change evidence, review, a revision check, and a Git audit trail.

## Validation

Run the memory tool tests, workflow/model tests, runtime injection tests, SQLx
ownership/size integration test against a disposable database, and Helm render
checks. Then exercise a real local workflow and follow-up chat: verify the Git
commit, independent source checkpoints, correctly scoped cache, recalled fact,
derived guidance with real evidence, no Slack maintenance message, and placeholder
credentials inside the sandbox. Repeat a window to prove idempotency; run idle
loops to prove zero model calls and writes. Test a rejected preservation plan,
lock contention, and Git conflicts. Disable reflection separately, then disable
the overall flag to prove rollback.
