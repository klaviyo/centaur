-- no-transaction

-- GET /api/sessions lists sessions most recently updated first.
create index concurrently if not exists sessions_updated_at_idx
    on sessions (updated_at desc, thread_key desc);
