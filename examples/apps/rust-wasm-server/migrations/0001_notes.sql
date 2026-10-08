-- The notes of the example (T-3341). The reconciler runs this as the schema's owner, in the App's
-- own schema; the App itself never runs DDL (ADR-N-044 §2.3).
create table notes (
    id bigserial primary key,
    body text not null check (length(body) between 1 and 2000),
    file text,
    created_at timestamptz not null default now()
);
