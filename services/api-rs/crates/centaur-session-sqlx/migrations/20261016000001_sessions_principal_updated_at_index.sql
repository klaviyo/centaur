-- no-transaction

-- GET /api/sessions for a user caller lists the sessions bound to its principal,
-- most recently updated first.
create index concurrently if not exists sessions_principal_updated_at_idx
    on sessions (iron_control_principal, updated_at desc, thread_key desc)
    where iron_control_principal is not null;
