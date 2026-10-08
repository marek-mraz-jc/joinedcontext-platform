//! What the apps database holds for a shard and an App (ADR-N-044 §2.5, AP-144): the statements
//! the reconciler runs, as the database's owner role, when a shard starts and when an App is
//! placed. They are here, beside the host that relies on them, and the tests run them as written.

use crate::placement::is_id;
use crate::sql::role_of;

/// The database as a whole, once: nobody but an App's own role creates in its schema, and the
/// functions that would reach outside the database are no one's.
pub fn database(database: &str) -> Vec<String> {
    vec![
        format!("revoke create, temporary on database \"{database}\" from public"),
        "revoke all on schema public from public".into(),
        "revoke execute on function pg_catalog.pg_read_file(text, bigint, bigint, boolean) from public".into(),
        "revoke execute on function pg_catalog.pg_ls_dir(text, boolean, boolean) from public".into(),
    ]
}

/// The shard's login role: it owns nothing, has no table right of its own, and becomes an App
/// only by the per-transaction role switch the host makes (NOINHERIT). `password` comes from the
/// secret store; it is a parameter of the statement the reconciler sends, never logged.
pub fn shard(shard: &str) -> Result<Vec<String>, String> {
    if !is_id(shard) {
        return Err("a shard id is lowercase letters, digits and `_`".into());
    }
    Ok(vec![format!(
        "do $$ begin if not exists (select from pg_roles where rolname = 'wasm_host_{shard}') then \
         create role wasm_host_{shard} login noinherit nocreatedb nocreaterole; end if; end $$"
    )])
}

/// The role that owns an App's schema and runs its migrations: `app_<id>_owner`. Each App has its
/// own, so a view or a function an App's migration defines runs with that App's rights alone: an
/// owner shared by every App would let one App's view read another App's tables (T-3342).
pub fn owner_of(id: &str) -> String {
    format!("{}_owner", role_of(id))
}

/// One App placed on `shard`: its NOLOGIN role and its own NOLOGIN owner, its schema (owned by
/// the owner, so the App's role cannot change it), the App's rights on its schema alone, the
/// shard's membership of the App's role, and `migrator`'s of the owner, so the reconciler can run
/// the App's migrations as it.
pub fn app(shard: &str, id: &str, migrator: &str) -> Result<Vec<String>, String> {
    if !is_id(shard) || !is_id(id) || !is_id(migrator) {
        return Err(
            "shard and App ids and the migrator are lowercase letters, digits and `_`".into(),
        );
    }
    let role = role_of(id);
    let owner = owner_of(id);
    let create = |name: &str| {
        format!("do $$ begin if not exists (select from pg_roles where rolname = '{name}') then create role {name} nologin noinherit; end if; end $$")
    };
    Ok(vec![
        create(&role),
        create(&owner),
        format!("grant {owner} to {migrator}"),
        format!("create schema if not exists {role} authorization {owner}"),
        format!("revoke all on schema {role} from public"),
        format!("grant usage on schema {role} to {role}"),
        format!("grant select, insert, update, delete on all tables in schema {role} to {role}"),
        format!("grant usage, select, update on all sequences in schema {role} to {role}"),
        format!("alter default privileges for role {owner} in schema {role} grant select, insert, update, delete on tables to {role}"),
        format!("alter default privileges for role {owner} in schema {role} grant usage, select, update on sequences to {role}"),
        format!("grant {role} to wasm_host_{shard} with inherit false, set true"),
    ])
}
