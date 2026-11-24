-- Binds a GitHub pull request to the exact Centaur Slack session whose sandbox
-- created it through the authorized human's OAuth identity. api-rs inserts a
-- row only after GitHub's own 201 response proves the repository and author;
-- githubbot then drives `managed`, `closed` and `head_sha` from signed
-- webhooks. `repository` is lowercased `owner/repo`.
create table pr_session_bindings (
    repository text not null,               -- lowercased owner/repo
    number bigint not null,
    thread_key text not null references sessions(thread_key) on delete cascade,
    pr_id bigint not null,                  -- GitHub REST id
    pr_node_id text not null,
    author_github_id bigint not null,
    author_login text not null,
    sandbox_id text not null,               -- the sandbox whose proxy made the create call
    head_ref text not null,
    base_ref text not null,
    head_sha text not null,                 -- latest head githubbot has seen; starts at the created head
    managed boolean not null default false, -- set only by an explicit assignment of the bot
    closed boolean not null default false,
    -- The `updated_at` of the fetched PR behind the latest lifecycle write
    -- (managed, closed, head_sha). Writes observed earlier are ignored, so
    -- reordered webhook handlers converge on the newest observation.
    github_updated_at timestamptz,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    primary key (repository, number),
    unique (pr_node_id),
    check (repository = lower(repository) and repository ~ '^[^/\s]+/[^/\s]+$' and number > 0)
);

create index pr_session_bindings_thread_key_idx on pr_session_bindings (thread_key);

-- One row per failed-CI notice posted (or being posted) for a pull request at
-- a head, so repeat deliveries never notify the thread twice. Keyed by the PR's
-- node id, not its number, so a reused repository and number is a new PR.
create table pr_ci_notices (
    pr_node_id text not null,
    head_sha text not null,
    thread_key text not null references sessions(thread_key) on delete cascade,
    created_at timestamptz not null default now(),
    primary key (pr_node_id, head_sha)
);
