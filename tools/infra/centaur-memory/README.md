# centaur-memory

Personal memory stored in a private GitHub repository. `memory/state.json` is
canonical; each successful consolidation creates one Git commit containing the
state, a bounded evidence episode, and readable `MEMORY.md`, `WORKING_SET.md`, and
`ALIGNMENT.md` views. Unrelated files are retained. No local clone is required.

```sh
centaur-memory snapshot --repo example/personal-memory
centaur-memory apply --repo example/personal-memory < proposal.json
centaur-memory search --repo example/personal-memory 'deferred migration'
centaur-memory explain --repo example/personal-memory mem_RECORD_ID
```

`apply` reads JSON from stdin with `expected_head` (the snapshot SHA or null),
`window_id`, `sources`, `claims`, and optional `processed_through`. Each source
contains `id`, `role` (`user` or `assistant`), `text` (at most 2,000 characters),
and `occurred_at`. Sources are selected evidence excerpts, not full transcripts.
Each claim contains `key`, `category`, `text`, `status`, `evidence`, optional
`project`, and `expected_revision` (zero for new records). Evidence entries contain
`source_id` and an exact `quote` from that source. Reuse the same category, key,
and project to update a record; its ID is deterministic.

Categories are `current_work`, `deferred_step`, `project_decision`,
`project_constraint`, `work_preference`, `user_fact`, and `lesson`. Status is
`active`, `deferred`, `completed`, or `retracted`. User facts, preferences,
decisions, and constraints require direct user evidence. Other categories may
include assistant evidence, which never grants permissions. Corrections update
record revisions; completed and retracted records remain searchable and in Git
history but leave the injected context. Memory always remains untrusted data.

The client requires a private repository for reads and writes and uses the SDK's
`GITHUB_TOKEN` placeholder through the credential proxy. It does not load dotenv
files, copy tokens into Git, or follow HTTP redirects. Obvious credential patterns
are rejected; callers must still select and redact evidence responsibly.

Writes require the exact snapshot head and record revisions. Concurrent commits
return `status: conflict` with a fresh snapshot without force updates; reload and
review before resubmitting. CLI conflict output is valid JSON with exit status
zero so durable workflows can checkpoint the outcome. A
repeated window is a no-op only if its full payload hash matches. An empty repo
is bootstrapped with a README commit because GitHub cannot create Git objects in
an empty repository; interruption before consolidation leaves only that harmless
bootstrap, and a fresh snapshot permits retry. Missing branches in nonempty
repositories must be created explicitly.

Limits: 50 source excerpts, 20 claims and five quotes per claim per write; 1,000
records, 2,000 run entries, one megabyte per artifact, 12,000 UTF-8 bytes of active
context. Hitting a state limit stops writes for explicit review/archival rather
than silently dropping evidence or history. Search is deterministic lexical
matching, not semantic retrieval. The tool does not extract claims itself; a
durable workflow proposes them and the client validates and persists them.

Offline validation: `python -m pytest tools/infra/centaur-memory/test_client.py`.
