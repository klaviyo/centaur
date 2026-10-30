# centaur-memory

Personal memory stored in a private GitHub repository. `memory/state.json` is
canonical; each successful consolidation creates one Git commit containing the
state, a bounded evidence episode, and readable `MEMORY.md`, `WORKING_SET.md`,
`ALIGNMENT.md`, and `ALIGNMENT_SYNTHESIS.md` views. Unrelated files are retained.
No local clone is required.

```sh
centaur-memory snapshot --repo example/personal-memory
centaur-memory history --repo example/personal-memory --limit 12 --source-limit 24
centaur-memory apply --repo example/personal-memory < proposal.json
centaur-memory search --repo example/personal-memory 'deferred migration'
centaur-memory explain --repo example/personal-memory mem_RECORD_ID
```

`apply` reads JSON from stdin with `expected_head` (the snapshot SHA or null),
`window_id`, `sources`, `claims`, and optional `processed_through` and
`checkpoint_source_ids`. Each source
contains `id`, `role` (`user` or `assistant`), `text` (at most 2,000 characters),
and `occurred_at`. Sources are selected evidence excerpts, not full transcripts.
Each claim contains `key`, `category`, `text`, `status`, `evidence`, optional
`project`, and `expected_revision` (zero for new records). Evidence entries contain
`source_id` and an exact `quote` from that source. Reuse the same category, key,
and project to update a record; its ID is deterministic.

By default all supplied source IDs are marked processed in the run. When a loop
borrows older evidence for comparison, supply `checkpoint_source_ids` as a
nonempty, unique subset containing only its newly processed event IDs. The full
evidence remains in the episode, but borrowed sources do not advance that loop's
checkpoint. Omitting this option preserves the original episode payload/hash.
Reflection run checkpoints add `reflection:` to these IDs in `run.source_ids`
only, so an older ingestion loop cannot mistake reflected events for ingested
ones after rollback. The prefixed ID must fit the existing 160-character limit.
Episode sources, evidence references, and `checkpoint_source_ids` retain raw
IDs; history checks the namespaced checkpoint separately from the raw evidence.

`history` returns `{head, sources, window_ids}` from the most recent 12 runs by
default, including episodes with no approved claims. `--head COMMIT_SHA` pins
the read; otherwise the branch head is read once. Each episode's schema,
provenance, payload hash, and exact raw user/assistant sources are checked.
Generated claims and Markdown projections never become source evidence.
Source IDs are deduplicated; conflicting content for the same ID fails closed.
The run limit is 1–24, source limit 1–50 (default 24), and the returned compact
JSON is capped at 60,000 UTF-8 bytes by omitting whole source excerpts. Window
IDs identify all selected validated episodes, even when the source cap omits
some excerpts. No source text is shortened or rewritten.

Categories are `current_work`, `deferred_step`, `project_decision`,
`project_constraint`, `work_preference`, `user_fact`, and `lesson`. Status is
`active`, `deferred`, `completed`, or `retracted`. User facts, preferences,
decisions, and constraints require direct user evidence. Other categories may
include assistant evidence, which never grants permissions. Corrections update
record revisions; completed and retracted records remain searchable and in Git
history but leave the injected context. Memory always remains untrusted data.

Reflection writes use a `personal-reflection:` window ID and only `lesson`
claims whose stable keys start with `reflection:` and text starts with
`Derived guidance: `. This labels interpretations separately from direct user
preferences or authorization. Every apply regenerates `ALIGNMENT_SYNTHESIS.md`
from active reflection lessons. A reflection also atomically writes a dated
`reflections/<window-hash>.md` report containing approved changes and source
citations, or an explicit no-change result. Up to 2,500 of the existing 12,000
context bytes are reserved for active reflection lessons; the remaining budget
prioritizes working facts. These are projections of existing version-1 records,
not a new canonical schema. Older readers can still read the state and episodes.

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
