-- Binds a GitHub pull request to the exact Centaur session that opened it, so
-- follow-up work on the PR (reviews, mentions, CI, conflicts) is routed back
-- into that same session. One row per PR; `repository` is lowercased
-- `owner/repo`. The sandbox registers a row through the principal-owned
-- session route; githubbot verifies it from a signed webhook and drives it.
create table pr_session_bindings (
    repository text not null,
    number bigint not null,
    thread_key text not null references sessions(thread_key) on delete cascade,
    state text not null default 'registered',
    pr_node_id text,
    pr_author_login text,
    pr_created_at timestamptz,
    head_ref text,
    managed boolean not null default false,
    head_sha text,
    automation_paused boolean not null default false,
    pause_reason text,
    -- Recorded "manage pushes as me" instructions confer nothing in v1, so
    -- a stored grant is always inactive.
    push_grant jsonb,
    blocker text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    primary key (repository, number),
    constraint pr_session_bindings_repository_lowercase check (repository = lower(repository)),
    constraint pr_session_bindings_repository_shape check (repository ~ '^[^/\s]+/[^/\s]+$'),
    constraint pr_session_bindings_number_positive check (number > 0),
    constraint pr_session_bindings_state check (state in ('registered', 'verified', 'rejected')),
    constraint pr_session_bindings_push_grant_inactive check (
        push_grant is null
        or (jsonb_typeof(push_grant) = 'object' and coalesce(push_grant -> 'active', 'false'::jsonb) = 'false'::jsonb)
    )
);

create index pr_session_bindings_thread_key_idx on pr_session_bindings (thread_key);

-- The durable FIFO of follow-up work per binding. `dedupe_key` names the
-- logical item (a review, a comment, a CI conclusion for a head), so repeat
-- webhook deliveries are no-ops while distinct reviews stay distinct.
create table pr_session_followups (
    id text primary key,
    binding_repository text not null,
    binding_number bigint not null,
    kind text not null,
    dedupe_key text not null,
    head_sha text not null,
    actor jsonb not null,
    prompt jsonb not null,
    reply_target jsonb,
    status text not null default 'queued',
    drop_reason text,
    execution_id text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    foreign key (binding_repository, binding_number)
        references pr_session_bindings (repository, number) on delete cascade,
    constraint pr_session_followups_kind check (
        kind in ('review', 'review_comment', 'issue_comment', 'mention', 'ci_failure', 'conflict')
    ),
    constraint pr_session_followups_status check (status in ('queued', 'dispatched', 'dropped')),
    constraint pr_session_followups_dedupe unique (binding_repository, binding_number, dedupe_key)
);

create index pr_session_followups_queue_idx
    on pr_session_followups (binding_repository, binding_number, status, created_at);
