//! Roles and edits in a server App (T-3825, API/07): who may change what is decided from
//! [`identity::caller`](crate::identity::caller) alone, rows a person writes carry their
//! `owner_id`, and every write leaves a row in the App's `app_audit` table in the same statement.
//!
//! ```no_run
//! use jc_app_sdk::access::Owned;
//! use jc_app_sdk::http::{Params, Request, Response};
//! use jc_app_sdk::identity::Caller;
//!
//! const NOTES: Owned = Owned::new("notes", "editor");
//!
//! fn change(request: &Request, params: &Params) -> Response {
//!     let caller = match Caller::require() {
//!         Ok(caller) => caller,
//!         Err(answer) => return answer,
//!     };
//!     let id: i64 = params["id"].parse().unwrap_or(0);
//!     let body = String::from_utf8_lossy(&request.body).into_owned();
//!     match NOTES.update(&caller, id, &["body"], vec![body.into()], "changed a note") {
//!         Ok(()) => Response::no_content(),
//!         Err(answer) => answer,
//!     }
//! }
//! ```

use crate::http::Response;
use crate::identity::Caller;
use crate::sql::{self, Rows, Value};

/// The App's history table; put this text into a migration of the App's own (AP-149).
pub const AUDIT_TABLE: &str = "create table app_audit (
    id bigserial primary key,
    at timestamptz not null default now(),
    actor_id text not null,
    actor_name text not null,
    action text not null check (action in ('insert', 'update', 'delete', 'edit')),
    target text not null,
    summary text not null check (length(summary) <= 500)
);
";

/// The longest summary a history row keeps; a longer one is cut.
pub const SUMMARY_CHARS: usize = 500;

/// The most history rows one [`history`] call reads.
pub const HISTORY_ROWS: u32 = 200;

impl Caller {
    /// The signed-in caller, or a `401` problem. An anonymous visitor and a job have none.
    pub fn require() -> Result<Caller, Response> {
        Self::required(crate::identity::caller())
    }

    /// [`Caller::require`] on a caller the App already holds.
    pub fn required(caller: Option<Caller>) -> Result<Caller, Response> {
        caller.ok_or_else(|| Response::problem(401, "Unauthorized", "sign in to do this"))
    }

    /// Whether the caller holds the App role `role`.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|held| held == role)
    }

    /// `Ok` when the caller holds one of `roles`, else a `403` problem naming the roles this
    /// needs, never the caller's own.
    pub fn require_role(&self, roles: &[&str]) -> Result<(), Response> {
        if roles.iter().any(|role| self.has_role(role)) {
            return Ok(());
        }
        let needs = match roles {
            [one] => format!("this needs the role {one}"),
            _ => format!("this needs one of the roles {}", roles.join(", ")),
        };
        Err(Response::problem(403, "Forbidden", &needs))
    }
}

/// A table whose rows belong to the person who wrote them (API/07 §2): `id bigserial primary
/// key` and `owner_id text not null`. The owner, and a caller with the `editor` role, may change
/// or delete a row; nobody else learns it exists.
#[derive(Debug, Clone, Copy)]
pub struct Owned {
    table: &'static str,
    editor: &'static str,
}

impl Owned {
    /// The table `table`, whose every row the App role `editor` may change.
    pub const fn new(table: &'static str, editor: &'static str) -> Self {
        Self { table, editor }
    }

    /// A new row of `columns` = `values`, `owner_id` the caller's id; the new row.
    pub fn insert(
        &self,
        caller: &Caller,
        columns: &[&str],
        values: Vec<Value>,
        summary: &str,
    ) -> Result<Rows, Response> {
        let (statement, params) = self.insert_plan(caller, columns, values, summary)?;
        sql::query(&statement, &params).map_err(Response::from_sql)
    }

    /// Row `id`'s `columns` set to `values`, when the caller owns it or holds the editor role;
    /// `404` when no row changed.
    pub fn update(
        &self,
        caller: &Caller,
        id: i64,
        columns: &[&str],
        values: Vec<Value>,
        summary: &str,
    ) -> Result<(), Response> {
        let (statement, params) = self.update_plan(caller, id, columns, values, summary)?;
        applied(sql::execute(&statement, &params))
    }

    /// Row `id` deleted under the rule of [`Owned::update`].
    pub fn delete(&self, caller: &Caller, id: i64, summary: &str) -> Result<(), Response> {
        let (statement, params) = self.delete_plan(caller, id, summary)?;
        applied(sql::execute(&statement, &params))
    }

    /// The statement and parameters of [`Owned::insert`].
    pub fn insert_plan(
        &self,
        caller: &Caller,
        columns: &[&str],
        values: Vec<Value>,
        summary: &str,
    ) -> Result<(String, Vec<Value>), Response> {
        let table = self.checked_table()?;
        let columns = settable(columns, &values)?;
        let mut params = head(caller, table, "insert", summary);
        let first = params.len() + 1;
        let placeholders: Vec<String> = (first..first + values.len())
            .map(|n| format!("${n}"))
            .collect();
        params.extend(values);
        let statement = format!(
            "with changed as (insert into {table} (owner_id, {}) values ($1, {}) returning *), \
             audit as (insert into app_audit (actor_id, actor_name, action, target, summary) \
             select $1, $2, $3, $4::text || '/' || changed.id::text, $5 from changed) select * from changed",
            columns.join(", "),
            placeholders.join(", "),
        );
        Ok((statement, params))
    }

    /// The statement and parameters of [`Owned::update`].
    pub fn update_plan(
        &self,
        caller: &Caller,
        id: i64,
        columns: &[&str],
        values: Vec<Value>,
        summary: &str,
    ) -> Result<(String, Vec<Value>), Response> {
        let table = self.checked_table()?;
        let columns = settable(columns, &values)?;
        let mut params = head(caller, table, "update", summary);
        params.push(Value::Boolean(caller.has_role(self.editor)));
        params.push(Value::Int(id));
        let first = params.len() + 1;
        let sets: Vec<String> = columns
            .iter()
            .zip(first..)
            .map(|(column, n)| format!("{column} = ${n}"))
            .collect();
        params.extend(values);
        let statement = format!(
            "with changed as (update {table} set {} where id = $7 and (owner_id = $1 or $6::boolean) \
             returning id) {}",
            sets.join(", "),
            AUDIT_FROM_ROW,
        );
        Ok((statement, params))
    }

    /// The statement and parameters of [`Owned::delete`].
    pub fn delete_plan(
        &self,
        caller: &Caller,
        id: i64,
        summary: &str,
    ) -> Result<(String, Vec<Value>), Response> {
        let table = self.checked_table()?;
        let mut params = head(caller, table, "delete", summary);
        params.push(Value::Boolean(caller.has_role(self.editor)));
        params.push(Value::Int(id));
        let statement = format!(
            "with changed as (delete from {table} where id = $7 and (owner_id = $1 or $6::boolean) \
             returning id) {AUDIT_FROM_ROW}"
        );
        Ok((statement, params))
    }

    fn checked_table(&self) -> Result<&'static str, Response> {
        name(self.table).map(|()| self.table)
    }
}

/// One history row per row the `changed` CTE returned; the statement's count is the rows changed.
const AUDIT_FROM_ROW: &str = "insert into app_audit (actor_id, actor_name, action, target, \
                              summary) select $1, $2, $3, $4::text || '/' || changed.id::text, $5 from changed";

/// A history row for a change outside the App's database, such as a context-data edit
/// ([`crate::gateway::patch_entity`]): `action` is `insert`, `update`, `delete` or `edit`.
pub fn record(caller: &Caller, action: &str, target: &str, summary: &str) -> Result<(), Response> {
    let (statement, params) = record_plan(caller, action, target, summary)?;
    sql::execute(&statement, &params)
        .map(|_| ())
        .map_err(Response::from_sql)
}

/// The statement and parameters of [`record`].
pub fn record_plan(
    caller: &Caller,
    action: &str,
    target: &str,
    summary: &str,
) -> Result<(String, Vec<Value>), Response> {
    if !matches!(action, "insert" | "update" | "delete" | "edit") {
        return Err(misnamed(
            "a history action is insert, update, delete or edit",
        ));
    }
    let mut params = head(caller, "", action, summary);
    params[3] = Value::from(target);
    Ok((
        "insert into app_audit (actor_id, actor_name, action, target, summary) \
         values ($1, $2, $3, $4, $5)"
            .into(),
        params,
    ))
}

/// The newest `limit` history rows (at most [`HISTORY_ROWS`]), newest first.
pub fn history(limit: u32) -> Result<Rows, Response> {
    sql::query(
        "select id, at, actor_id, actor_name, action, target, summary from app_audit \
         order by at desc, id desc limit $1",
        &[Value::Int(i64::from(limit.clamp(1, HISTORY_ROWS)))],
    )
    .map_err(Response::from_sql)
}

/// `$1` actor id, `$2` actor name, `$3` action, `$4` table (or target), `$5` summary.
fn head(caller: &Caller, table: &str, action: &str, summary: &str) -> Vec<Value> {
    vec![
        Value::from(caller.id.as_str()),
        Value::from(caller.name.as_str()),
        Value::from(action),
        Value::from(table),
        Value::from(summary.chars().take(SUMMARY_CHARS).collect::<String>()),
    ]
}

/// `columns` the caller may set: valid names, not `id` or `owner_id`, as many as `values`.
fn settable<'a>(columns: &[&'a str], values: &[Value]) -> Result<Vec<&'a str>, Response> {
    if columns.is_empty() || columns.len() != values.len() {
        return Err(misnamed("one value for each named column, at least one"));
    }
    for column in columns {
        name(column)?;
        if matches!(*column, "id" | "owner_id") {
            return Err(misnamed(
                "id and owner_id are set by the SDK, not by the App",
            ));
        }
    }
    Ok(columns.to_vec())
}

/// A table or column name the SDK writes into a statement: `[a-z_][a-z0-9_]*`, at most 63.
fn name(name: &str) -> Result<(), Response> {
    let mut chars = name.chars();
    let fine = name.len() <= 63
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if fine {
        Ok(())
    } else {
        Err(misnamed(
            "a table or column name is [a-z_][a-z0-9_]*, at most 63 characters",
        ))
    }
}

/// The App's own mistake, before any statement ran.
fn misnamed(why: &str) -> Response {
    Response::problem(500, "Internal Server Error", why)
}

fn applied(result: Result<u64, sql::Error>) -> Result<(), Response> {
    match result {
        Ok(0) => Err(Response::problem(
            404,
            "Not Found",
            "no such record, or not yours to change",
        )),
        Ok(_) => Ok(()),
        Err(err) => Err(Response::from_sql(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTES: Owned = Owned::new("notes", "editor");

    fn person(id: &str, roles: &[&str]) -> Caller {
        Caller {
            id: id.into(),
            name: format!("Person {id}"),
            email: None,
            roles: roles.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    fn detail(answer: &Response) -> serde_json::Value {
        serde_json::from_slice(&answer.body).expect("a problem body")
    }

    fn params(values: &[Value]) -> String {
        format!("{values:?}")
    }

    #[test]
    fn an_anonymous_caller_is_401() {
        let answer = Caller::required(None).expect_err("anonymous");
        assert_eq!(answer.status, 401);
        assert_eq!(
            Caller::required(Some(person("u1", &[]))).map(|c| c.id).ok(),
            Some("u1".into())
        );
    }

    #[test]
    fn a_missing_role_is_403_naming_only_what_is_needed() {
        let viewer = person("u1", &["viewer", "secret-club"]);
        assert!(viewer.has_role("viewer"));
        assert!(!viewer.has_role("steward"));
        assert!(!viewer.has_role("view"), "a prefix is no role");
        let answer = viewer.require_role(&["steward"]).expect_err("viewer");
        assert_eq!(answer.status, 403);
        let body = detail(&answer).to_string();
        assert!(body.contains("this needs the role steward"), "{body}");
        assert!(
            !body.contains("viewer") && !body.contains("secret-club"),
            "{body}"
        );
        let answer = viewer
            .require_role(&["steward", "editor"])
            .expect_err("viewer");
        assert!(detail(&answer)["detail"]
            .as_str()
            .is_some_and(|d| d == "this needs one of the roles steward, editor"));
        assert!(person("u2", &["editor"])
            .require_role(&["steward", "editor"])
            .is_ok());
    }

    #[test]
    fn an_insert_owns_its_row_and_writes_history_in_the_same_statement() {
        let (statement, values) = NOTES
            .insert_plan(
                &person("u1", &[]),
                &["body", "tag"],
                vec!["hi".into(), "x".into()],
                "added a note",
            )
            .expect("plan");
        assert_eq!(
            statement,
            "with changed as (insert into notes (owner_id, body, tag) values ($1, $6, $7) returning *), \
             audit as (insert into app_audit (actor_id, actor_name, action, target, summary) \
             select $1, $2, $3, $4::text || '/' || changed.id::text, $5 from changed) select * from changed"
        );
        assert_eq!(
            params(&values),
            params(&[
                "u1".into(),
                "Person u1".into(),
                "insert".into(),
                "notes".into(),
                "added a note".into(),
                "hi".into(),
                "x".into()
            ])
        );
    }

    #[test]
    fn an_update_or_delete_is_the_owners_or_the_editors_alone() {
        let (statement, values) = NOTES
            .update_plan(
                &person("u1", &["viewer"]),
                7,
                &["body"],
                vec!["new".into()],
                "changed",
            )
            .expect("plan");
        assert!(statement.starts_with(
            "with changed as (update notes set body = $8 where id = $7 and (owner_id = $1 or $6::boolean) returning id) insert into app_audit"
        ), "{statement}");
        assert!(statement.ends_with("from changed"), "{statement}");
        assert_eq!(
            params(&values[5..7]),
            params(&[Value::Boolean(false), Value::Int(7)])
        );
        let (_, values) = NOTES
            .update_plan(
                &person("u2", &["editor"]),
                7,
                &["body"],
                vec!["new".into()],
                "changed",
            )
            .expect("plan");
        assert_eq!(params(&values[5..6]), params(&[Value::Boolean(true)]));
        let (statement, values) = NOTES
            .delete_plan(&person("u1", &[]), 9, "removed")
            .expect("plan");
        assert!(statement.starts_with(
            "with changed as (delete from notes where id = $7 and (owner_id = $1 or $6::boolean) returning id) insert into app_audit"
        ), "{statement}");
        assert_eq!(params(&values[2..3]), params(&["delete".into()]));
        assert_eq!(
            params(&values[5..7]),
            params(&[Value::Boolean(false), Value::Int(9)])
        );
    }

    #[test]
    fn a_name_from_anywhere_but_code_shape_is_refused_before_any_statement() {
        let caller = person("u1", &[]);
        for bad in [
            "Body",
            "body; drop table x",
            "1body",
            "",
            "b\u{e9}",
            "body\"",
            &"a".repeat(64),
        ] {
            let answer = NOTES
                .insert_plan(&caller, &[bad], vec!["v".into()], "s")
                .expect_err(bad);
            assert_eq!(answer.status, 500, "{bad}");
        }
        for reserved in ["id", "owner_id"] {
            assert!(NOTES
                .update_plan(&caller, 1, &[reserved], vec!["v".into()], "s")
                .is_err());
        }
        assert!(NOTES
            .insert_plan(&caller, &["a", "b"], vec!["v".into()], "s")
            .is_err());
        assert!(NOTES.insert_plan(&caller, &[], vec![], "s").is_err());
        let evil = Owned::new("notes where true", "editor");
        assert!(evil.delete_plan(&caller, 1, "s").is_err());
    }

    #[test]
    fn a_summary_is_cut_and_a_record_names_its_target() {
        let long = "é".repeat(SUMMARY_CHARS + 20);
        let (_, values) = NOTES
            .delete_plan(&person("u1", &[]), 1, &long)
            .expect("plan");
        assert_eq!(
            params(&values[4..5]),
            params(&["é".repeat(SUMMARY_CHARS).into()])
        );
        let (statement, values) = record_plan(
            &person("u1", &[]),
            "edit",
            "urn:ngsi-ld:Station:7",
            "target level 12",
        )
        .expect("plan");
        assert!(statement.starts_with("insert into app_audit"));
        assert_eq!(
            params(&values[2..4]),
            params(&["edit".into(), "urn:ngsi-ld:Station:7".into()])
        );
        assert!(record_plan(&person("u1", &[]), "drop", "x", "s").is_err());
    }

    #[test]
    fn the_audit_table_matches_what_the_statements_write() {
        for column in [
            "actor_id",
            "actor_name",
            "action",
            "target",
            "summary",
            "at",
        ] {
            assert!(AUDIT_TABLE.contains(&format!("    {column} ")), "{column}");
        }
        assert!(AUDIT_TABLE.contains(&format!("length(summary) <= {SUMMARY_CHARS}")));
    }
}
