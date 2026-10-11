-- Roles and edits (T-3825, API/07): a note belongs to the person who wrote it, the shared notice
-- is changed by a steward alone, and every change leaves a history row in app_audit.
alter table notes add column owner_id text not null default '';

create table notice (
    id bigint primary key,
    owner_id text not null default '',
    body text not null check (length(body) between 1 and 2000)
);
insert into notice (id, body) values (1, 'Welcome. A steward keeps this notice up to date.');

-- jc_app_sdk::access::AUDIT_TABLE
create table app_audit (
    id bigserial primary key,
    at timestamptz not null default now(),
    actor_id text not null,
    actor_name text not null,
    action text not null check (action in ('insert', 'update', 'delete', 'edit')),
    target text not null,
    summary text not null check (length(summary) <= 500)
);
