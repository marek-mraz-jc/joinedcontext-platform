//! `jc:app/sql` on the apps database (ADR-N-044 §2.3, §2.5, AP-144, AP-146). One pool per shard,
//! logged in as `wasm_host_<shard>`, and every call one transaction that first takes the App's own
//! role, `jc.app_id`, `search_path` and `statement_timeout` with transaction-local `set_config`, so
//! nothing outlives it on the pooled connection. Before Postgres sees a statement the host reads it
//! with PostgreSQL's own parser (libpg_query, T-3364): exactly one statement, a top-level
//! `SELECT`, `INSERT`, `UPDATE`, `DELETE` or `VALUES`, naming none of the refused functions
//! anywhere in its tree. A word scanner checks the same text a second time. So an App never runs
//! DDL, `SET`, role and setting changes (`SET ROLE` cannot be refused by a grant while the shard
//! login may take every App role of its shard), advisory locks, listening, large objects, file
//! access, or the built-ins that run a query given as text. Postgres refuses the rest: the App's
//! role has rights on its own schema alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use sqlx::postgres::{PgArguments, PgPoolOptions, PgRow};
use sqlx::{Column, PgPool, Postgres, Row, TypeInfo};
use tokio::sync::Semaphore;

use crate::placement::Placed;
use crate::storage::{Rows, SqlError, Value};

/// What the shard allows each App.
#[derive(Debug, Clone)]
pub struct SqlLimits {
    pub statement_timeout: Duration,
    pub row_cap: usize,
    /// The bytes of one result, so a thousand large rows cannot fill the host (T-3342).
    pub result_bytes: usize,
    /// Statements of one App running at once, so one App cannot hold the shard's whole pool.
    pub per_app_statements: usize,
    /// The bytes an App's schema may hold before writes are refused.
    pub quota_bytes: u64,
}

impl Default for SqlLimits {
    fn default() -> Self {
        Self {
            statement_timeout: Duration::from_secs(2),
            row_cap: 1_000,
            result_bytes: 8 << 20,
            per_app_statements: 4,
            quota_bytes: 100 << 20,
        }
    }
}

/// The kind of statement an App sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
}

/// Words that may begin an App's statement.
const FIRST: &[&str] = &["select", "with", "insert", "update", "delete", "values"];

/// Function names an App's statement may not name, anywhere, quoted or not: role and setting
/// changes, locks held past the transaction, notifications, large objects, server files, and the
/// built-ins that run a query given as text (`query_to_xml`, `ts_stat`, `ts_rewrite`: they would
/// run a `set_config` this guard never saw).
fn refused_word(word: &str) -> bool {
    matches!(
        word,
        "set_config"
            | "pg_notify"
            | "pg_read_file"
            | "pg_read_binary_file"
            | "pg_ls_dir"
            | "pg_stat_file"
            | "pg_reload_conf"
            | "pg_terminate_backend"
            | "pg_cancel_backend"
            | "query_to_xml"
            | "query_to_xmlschema"
            | "query_to_xml_and_xmlschema"
            | "cursor_to_xml"
            | "cursor_to_xmlschema"
            | "dblink"
            | "ts_stat"
            | "ts_rewrite"
    ) || word.starts_with("pg_advisory")
        || word.starts_with("pg_try_advisory")
        || word.starts_with("lo_")
        || word.starts_with("dblink_")
}

/// The words of a statement outside its string literals, quoted identifiers and comments, and
/// whether it holds a `;` outside them.
fn words(statement: &str) -> Result<(Vec<String>, bool), String> {
    let chars: Vec<char> = statement.chars().collect();
    let (mut words, mut semicolon, mut i) = (Vec::new(), false, 0);
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // An `E'...'` string reads a backslash as an escape, so `E'\' '` is one string to
            // Postgres; read it the same way or a call after it hides inside a "string".
            let escapes = i > 0
                && matches!(chars[i - 1], 'e' | 'E')
                && (i < 2 || !(chars[i - 2].is_alphanumeric() || chars[i - 2] == '_'));
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("a string literal is not closed".into()),
                    Some('\\') if escapes => i += 2,
                    Some('\'') if chars.get(i + 1) == Some(&'\'') => i += 2,
                    Some('\'') => break,
                    Some(_) => i += 1,
                }
            }
            i += 1;
        } else if c == '"' {
            // A quoted name is still a name: `"set_config"` calls set_config (T-3342). Its text is
            // a word as written, `""` an escaped quote inside it.
            let mut name = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("a quoted name is not closed".into()),
                    Some('"') if chars.get(i + 1) == Some(&'"') => {
                        name.push('"');
                        i += 2;
                    }
                    Some('"') => break,
                    Some(c) => {
                        name.push(*c);
                        i += 1;
                    }
                }
            }
            i += 1;
            words.push(name);
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while chars.get(i).is_some_and(|c| *c != '\n') {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 0;
            loop {
                match (chars.get(i), chars.get(i + 1)) {
                    (None, _) => return Err("a comment is not closed".into()),
                    (Some('/'), Some('*')) => {
                        depth += 1;
                        i += 2;
                    }
                    (Some('*'), Some('/')) => {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => i += 1,
                }
            }
        } else if c == '$' && !chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            // A dollar-quoted string, `$tag$ … $tag$`; `$1` is a parameter.
            let start = i;
            i += 1;
            while chars
                .get(i)
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                i += 1;
            }
            if chars.get(i) != Some(&'$') {
                return Err("a `$` that opens no quoted string".into());
            }
            let tag: String = chars[start..=i].iter().collect();
            let rest: String = chars[i + 1..].iter().collect();
            let Some(end) = rest.find(&tag) else {
                return Err("a dollar-quoted string is not closed".into());
            };
            i += 1 + rest[..end].chars().count() + tag.chars().count();
        } else if c == ';' {
            semicolon = true;
            i += 1;
        } else if (c == 'u' || c == 'U')
            && chars.get(i + 1) == Some(&'&')
            && matches!(chars.get(i + 2), Some('"' | '\''))
        {
            // `U&"\0073et_config"` spells a name the guard could not read (T-3342).
            return Err("unicode-escaped names and strings are not taken".into());
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while chars
                .get(i)
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                i += 1;
            }
            words.push(chars[start..i].iter().collect::<String>().to_lowercase());
        } else {
            i += 1;
        }
    }
    Ok((words, semicolon))
}

/// The function names a parse tree calls, anywhere in it: every `FuncCall` node's last name, in
/// the case Postgres resolves it (`pg_catalog.set_config` and `"set_config"` are `set_config`).
/// The tree is walked as a whole, not through the crate's own iterator, which visits a subset.
fn called(tree: &serde_json::Value, names: &mut Vec<String>) {
    match tree {
        serde_json::Value::Object(fields) => {
            if let Some(call) = fields.get("FuncCall") {
                if let Some(name) = call
                    .get("funcname")
                    .and_then(|parts| parts.as_array())
                    .and_then(|parts| parts.last())
                    .and_then(|last| last.pointer("/node/String/sval"))
                    .and_then(|name| name.as_str())
                {
                    names.push(name.to_lowercase());
                }
            }
            for value in fields.values() {
                called(value, names);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                called(item, names);
            }
        }
        _ => {}
    }
}

/// Whether any node of the tree is a statement that writes: a data-modifying `WITH` makes a
/// `SELECT` a write.
fn writes(tree: &serde_json::Value) -> bool {
    match tree {
        serde_json::Value::Object(fields) => fields.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "InsertStmt" | "UpdateStmt" | "DeleteStmt" | "MergeStmt"
            ) || writes(value)
        }),
        serde_json::Value::Array(items) => items.iter().any(writes),
        _ => false,
    }
}

/// The verdict of PostgreSQL's own parser (T-3364): one top-level statement of the kinds an App
/// runs, calling no refused function anywhere. A statement it cannot read is refused.
fn parsed(statement: &str) -> Result<Kind, String> {
    use pg_query::NodeEnum;
    let tree = pg_query::parse(statement)
        .map_err(|_| "the statement is not one PostgreSQL reads".to_string())?;
    let statements = &tree.protobuf.stmts;
    let node = match statements.as_slice() {
        [] => return Err("the statement is empty".into()),
        [one] => one.stmt.as_ref().and_then(|stmt| stmt.node.as_ref()),
        _ => return Err("one statement per call".into()),
    };
    let kind = match node {
        Some(NodeEnum::SelectStmt(select)) => {
            if select.into_clause.is_some() {
                return Err(
                    "`select … into` creates a table, which an application does not".into(),
                );
            }
            Kind::Read
        }
        Some(NodeEnum::InsertStmt(_) | NodeEnum::UpdateStmt(_) | NodeEnum::DeleteStmt(_)) => {
            Kind::Write
        }
        _ => return Err(
            "only select, insert, update, delete and values statements are run for an application"
                .into(),
        ),
    };
    let json = serde_json::to_value(&tree.protobuf)
        .map_err(|_| "the statement is not one PostgreSQL reads".to_string())?;
    let mut names = Vec::new();
    called(&json, &mut names);
    if let Some(name) = names.iter().find(|name| refused_word(name)) {
        return Err(format!("`{name}` is not available to an application"));
    }
    Ok(if kind == Kind::Read && writes(&json) {
        Kind::Write
    } else {
        kind
    })
}

/// Whether the host lets a statement reach Postgres, and what kind it is (AP-144). The parser
/// decides (T-3364); the word scanner below must agree as a second check. The reason of a
/// refusal names the rule, never the statement.
pub fn check(statement: &str) -> Result<Kind, String> {
    let kind = parsed(statement)?;
    scanned(statement)?;
    Ok(kind)
}

/// The word scanner's verdict, kept as a second check behind the parser's.
fn scanned(statement: &str) -> Result<Kind, String> {
    let (words, semicolon) = words(statement)?;
    let trailing = statement
        .trim_end()
        .strip_suffix(';')
        .map(|s| !s.contains(';'))
        .unwrap_or(false);
    if semicolon && !trailing {
        return Err("one statement per call".into());
    }
    let Some(first) = words.first() else {
        return Err("the statement is empty".into());
    };
    if !FIRST.contains(&first.as_str()) {
        return Err(format!("`{first}` statements are not run for an application: only select, insert, update, delete and values"));
    }
    if let Some(word) = words.iter().find(|w| refused_word(w)) {
        return Err(format!("`{word}` is not available to an application"));
    }
    // `select … into` creates a table; `insert into` is a write.
    if first == "select" && words.iter().any(|w| w == "into") {
        return Err("`select … into` creates a table, which an application does not".into());
    }
    Ok(match first.as_str() {
        "select" | "values" => Kind::Read,
        "with"
            if !words
                .iter()
                .any(|w| matches!(w.as_str(), "insert" | "update" | "delete")) =>
        {
            Kind::Read
        }
        _ => Kind::Write,
    })
}

/// The App's role and schema name, `app_<id>` (an id is `[a-z0-9_]`, so no two ids share one).
pub fn role_of(id: &str) -> String {
    format!("app_{id}")
}

fn bind<'q>(
    mut query: sqlx::query::Query<'q, Postgres, PgArguments>,
    params: Vec<Value>,
) -> sqlx::query::Query<'q, Postgres, PgArguments> {
    for value in params {
        query = match value {
            Value::Null => query.bind(None::<String>),
            Value::Boolean(v) => query.bind(v),
            Value::Int(v) => query.bind(v),
            Value::Float(v) => query.bind(v),
            Value::Text(v) => query.bind(v),
            Value::Bytes(v) => query.bind(v),
            Value::Json(v) => match serde_json::from_str::<serde_json::Value>(&v) {
                Ok(json) => query.bind(sqlx::types::Json(json)),
                Err(_) => query.bind(v),
            },
        };
    }
    query
}

fn value_of(row: &PgRow, index: usize) -> Result<Value, SqlError> {
    let column = &row.columns()[index];
    let name = column.type_info().name().to_owned();
    let read = |err: sqlx::Error| SqlError::Invalid(format!("column {}: {err}", column.name()));
    Ok(match name.as_str() {
        "BOOL" => row
            .try_get::<Option<bool>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, Value::Boolean),
        "INT2" => row
            .try_get::<Option<i16>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, |v| Value::Int(v.into())),
        "INT4" => row
            .try_get::<Option<i32>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, |v| Value::Int(v.into())),
        "INT8" => row
            .try_get::<Option<i64>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, Value::Int),
        "FLOAT4" => row
            .try_get::<Option<f32>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, |v| Value::Float(v.into())),
        "FLOAT8" => row
            .try_get::<Option<f64>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, Value::Float),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => row
            .try_get::<Option<String>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, Value::Text),
        "BYTEA" => row
            .try_get::<Option<Vec<u8>>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, Value::Bytes),
        "JSON" | "JSONB" => row
            .try_get::<Option<sqlx::types::Json<serde_json::Value>>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, |v| Value::Json(v.0.to_string())),
        "UUID" => row
            .try_get::<Option<sqlx::types::Uuid>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, |v| Value::Text(v.to_string())),
        "TIMESTAMPTZ" => row
            .try_get::<Option<time::OffsetDateTime>, _>(index)
            .map_err(read)?
            .map_or(Ok(Value::Null), |v| {
                v.format(&time::format_description::well_known::Rfc3339)
                    .map(Value::Text)
            })
            .map_err(|err| SqlError::Invalid(err.to_string()))?,
        "TIMESTAMP" => row
            .try_get::<Option<time::PrimitiveDateTime>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, |v| Value::Text(v.to_string())),
        "DATE" => row
            .try_get::<Option<time::Date>, _>(index)
            .map_err(read)?
            .map_or(Value::Null, |v| Value::Text(v.to_string())),
        other => {
            return Err(SqlError::Invalid(format!(
                "column {} is {other}; cast it in the query (`::text`, `::float8`)",
                column.name()
            )))
        }
    })
}

/// About what a value weighs in the host's memory.
fn weight(value: &Value) -> usize {
    match value {
        Value::Text(v) | Value::Json(v) => v.len() + 16,
        Value::Bytes(v) => v.len() + 16,
        _ => 16,
    }
}

/// A Postgres error as the App reads it: a refusal for a missing right, a timeout for a cancelled
/// statement, else the database's own message. Never the statement, never another App's name.
fn from_db(err: sqlx::Error) -> SqlError {
    match &err {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some("42501") => SqlError::Refused("permission denied".into()),
            Some("57014") => SqlError::Timeout,
            _ => SqlError::Invalid(db.message().to_owned()),
        },
        sqlx::Error::PoolTimedOut => SqlError::Unavailable("the database is busy".into()),
        _ => SqlError::Unavailable("the database cannot be reached".into()),
    }
}

/// The apps database of one shard.
pub struct PgStore {
    pool: PgPool,
    limits: SqlLimits,
    statements: Mutex<HashMap<String, Arc<Semaphore>>>,
    sizes: Mutex<HashMap<String, (Instant, u64)>>,
}

impl PgStore {
    /// A pool on `url`, the shard's login role (`wasm_host_<shard>`).
    pub async fn connect(url: &str, size: u32, limits: SqlLimits) -> Result<Self, String> {
        let pool = PgPoolOptions::new()
            .max_connections(size)
            .acquire_timeout(Duration::from_secs(3))
            .connect(url)
            .await
            .map_err(|err| format!("the apps database: {err}"))?;
        Ok(Self::with_pool(pool, limits))
    }

    pub fn with_pool(pool: PgPool, limits: SqlLimits) -> Self {
        Self {
            pool,
            limits,
            statements: Mutex::new(HashMap::new()),
            sizes: Mutex::new(HashMap::new()),
        }
    }

    /// The bytes the App's schema holds, read at most every 30 seconds.
    async fn size(&self, role: &str) -> Result<u64, SqlError> {
        if let Some((at, bytes)) = self.sizes.lock().ok().and_then(|s| s.get(role).copied()) {
            if at.elapsed() < Duration::from_secs(30) {
                return Ok(bytes);
            }
        }
        let bytes: i64 = sqlx::query_scalar(
            "select coalesce(sum(pg_total_relation_size(c.oid)), 0)::int8 from pg_class c \
             join pg_namespace n on n.oid = c.relnamespace where n.nspname = $1",
        )
        .bind(role)
        .fetch_one(&self.pool)
        .await
        .map_err(from_db)?;
        let bytes = bytes.max(0) as u64;
        if let Ok(mut sizes) = self.sizes.lock() {
            sizes.insert(role.to_owned(), (Instant::now(), bytes));
        }
        Ok(bytes)
    }

    /// Runs `statement` as the App, in a transaction of its own: its rows when `want` is
    /// [`Kind::Read`] (a write with `returning` too), else the rows it changed. Every write, read
    /// back or not, is held to the quota.
    pub async fn run(
        &self,
        app: &Placed,
        statement: String,
        params: Vec<Value>,
        want: Kind,
    ) -> Result<(Option<Rows>, u64), SqlError> {
        let kind = check(&statement).map_err(|why| {
            tracing::warn!(app = %app.id, kind = "sql", %why, "refused");
            SqlError::Refused(why)
        })?;
        let role = role_of(&app.id);
        if kind == Kind::Write && self.size(&role).await? >= self.limits.quota_bytes {
            tracing::warn!(app = %app.id, kind = "sql", "refused: quota");
            return Err(SqlError::Quota(format!(
                "the application's tables hold {} bytes or more, its quota",
                self.limits.quota_bytes
            )));
        }
        let semaphore = self
            .statements
            .lock()
            .map_err(|_| SqlError::Unavailable("busy".into()))?
            .entry(app.id.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(self.limits.per_app_statements)))
            .clone();
        let _permit = semaphore.try_acquire_owned().map_err(|_| {
            SqlError::Unavailable("the application has as many statements running as it may".into())
        })?;

        let mut tx = self.pool.begin().await.map_err(from_db)?;
        // Transaction-local only (the `true`): the next App on this connection starts clean.
        sqlx::query(
            "select set_config('role', $1, true), set_config('jc.app_id', $2, true), \
             set_config('search_path', $1, true), set_config('statement_timeout', $3, true)",
        )
        .bind(&role)
        .bind(&app.id)
        .bind(self.limits.statement_timeout.as_millis().to_string())
        .execute(&mut *tx)
        .await
        .map_err(|err| {
            tracing::error!(app = %app.id, %err, "the shard cannot take the application's role");
            SqlError::Refused(
                "the application's database role is not available to this shard".into(),
            )
        })?;

        // The App's own statement is dynamic by nature: what makes it safe is `check` above and the
        // role it runs as, with every value a bound parameter.
        let query = bind(sqlx::query(sqlx::AssertSqlSafe(statement)), params);
        let outcome = if want == Kind::Read {
            let mut stream = query.fetch(&mut *tx);
            let mut columns = Vec::new();
            let mut values = Vec::new();
            let mut size = 0usize;
            while let Some(row) = stream.try_next().await.map_err(from_db)? {
                if values.len() == self.limits.row_cap {
                    return Err(SqlError::Refused(format!(
                        "more than {} rows; narrow the query",
                        self.limits.row_cap
                    )));
                }
                if columns.is_empty() {
                    columns = row.columns().iter().map(|c| c.name().to_owned()).collect();
                }
                let decoded = (0..row.len())
                    .map(|i| value_of(&row, i))
                    .collect::<Result<Vec<_>, _>>()?;
                size += decoded.iter().map(weight).sum::<usize>();
                if size > self.limits.result_bytes {
                    return Err(SqlError::Refused(format!(
                        "the result is larger than {} bytes; narrow the query",
                        self.limits.result_bytes
                    )));
                }
                values.push(decoded);
            }
            drop(stream);
            (Some(Rows { columns, values }), 0)
        } else {
            let done = query.execute(&mut *tx).await.map_err(from_db)?;
            (None, done.rows_affected())
        };
        tx.commit().await.map_err(from_db)?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_an_app_may_send() {
        for (statement, kind) in [
            ("select * from notes where id = $1", Kind::Read),
            ("  SELECT 1;", Kind::Read),
            ("values (1)", Kind::Read),
            ("with x as (select 1) select * from x", Kind::Read),
            (
                "insert into notes (body) values ($1) returning id",
                Kind::Write,
            ),
            ("update notes set body = $1 where id = $2", Kind::Write),
            ("delete from notes where id = $1", Kind::Write),
            (
                "with gone as (delete from notes returning id) select count(*) from gone",
                Kind::Write,
            ),
            ("select 'set role x; drop table y' as text_only", Kind::Read),
            ("select $tag$ set_config $tag$, \"body\" from t", Kind::Read),
            ("select 1 -- set_config\n", Kind::Read),
            ("select /* set_config /* nested */ */ 1", Kind::Read),
            // T-3364: what the parser reads as one allowed statement.
            ("select * from notes for update", Kind::Read),
            ("select count(*), lower(body) from notes group by 2", Kind::Read),
            (
                "insert into notes (body) values ($1) on conflict (id) do update set body = excluded.body",
                Kind::Write,
            ),
        ] {
            assert_eq!(check(statement), Ok(kind), "{statement}");
        }
    }

    #[test]
    fn what_never_reaches_postgres() {
        for statement in [
            "set role app_b",
            "SET search_path = app_b",
            "reset role",
            // An E'' string's escaped quote keeps the call after it outside any string.
            "select E'\\' ', set_config('role', 'app_b', true) --'",
            "select e'\\\\', set_config('role', 'app_b', true)",
            "select set_config('role', 'app_b', true)",
            "select pg_catalog.set_config('search_path', 'app_b', true)",
            // T-3342: a quoted name is the same function, and an escaped one hides it.
            "select \"set_config\"('role', 'app_b', true)",
            "select \"pg_catalog\".\"set_config\"('role', 'app_b', true)",
            "select U&\"\\0073et_config\"('role', 'app_b', true)",
            "select \"query_to_xml\"('select 1', true, true, '')",
            // Text search runs a query given as text too.
            "select ts_stat('select set_config(''role'', ''app_b'', true)::tsvector')",
            "select ts_rewrite('a'::tsquery, 'select ''a''::tsquery, ''b''::tsquery')",
            "create table x (i int)",
            "drop table notes",
            "alter role app_a superuser",
            "grant all on notes to public",
            "listen x",
            "notify x",
            "select pg_notify('x', 'y')",
            "copy notes to program 'id'",
            "select pg_advisory_lock(1)",
            "select pg_try_advisory_xact_lock(1)",
            "select lo_import('/etc/passwd')",
            "select pg_read_file('/etc/passwd')",
            "select query_to_xml('select set_config(''role'',''app_b'',true)', true, true, '')",
            "select * into notes_copy from notes",
            "select 1; select 2",
            "select 1; set role app_b",
            "do $$ begin perform 1; end $$",
            "begin",
            "commit",
            "",
            "   ",
            "select 'unclosed",
            "select /* unclosed",
            "call proc()",
            "prepare p as select 1",
            "explain analyze delete from notes",
            "vacuum notes",
            // T-3364: every form of a role switch, and calls the scanner never sees as words.
            "SET ROLE app_b",
            "set local role app_b",
            "RESET ROLE; SET ROLE app_b",
            "select 1; set role app_b",
            "SET SESSION AUTHORIZATION app_b",
            "reset session authorization",
            "select * from pg_catalog.set_config('role', 'app_b', true)",
            "with x as (select set_config('role', 'app_b', true)) select * from x",
            "select (select set_config('role', 'app_b', true))",
            "select 1 order by set_config('role', 'app_b', true)",
            "select * from notes, lateral (select pg_advisory_lock(1)) l",
            "insert into notes (body) select set_config('role', 'app_b', true)",
            "update notes set body = set_config('role', 'app_b', true)",
            "delete from notes where id = any (select pg_notify('x', 'y')::int)",
            "select case when true then set_config('role', 'app_b', true) end",
            "merge into notes using notes n on false when not matched then do nothing",
            "table notes into x",
            "select from from",
        ] {
            assert!(check(statement).is_err(), "{statement}");
        }
    }

    /// The parser holds on its own (T-3364): what it refuses does not depend on the scanner.
    #[test]
    fn the_parser_alone_refuses_a_role_switch_and_a_refused_call_anywhere() {
        for statement in [
            "SET ROLE app_b",
            "set local role app_b",
            "RESET ROLE; SET ROLE app_b",
            "select 1; set role app_b",
            "SET SESSION AUTHORIZATION app_b",
            "select E'\\' ', set_config('role', 'app_b', true) --'",
            "select \"pg_catalog\".\"set_config\"('role', 'app_b', true)",
            "select U&\"\\0073et_config\"('role', 'app_b', true)",
            "select * from pg_catalog.set_config('role', 'app_b', true)",
            "with x as (select set_config('role', 'app_b', true)) select * from x",
            "select * from notes, lateral (select pg_advisory_lock(1)) l",
            "select * into notes_copy from notes",
            "select 'unclosed",
        ] {
            assert!(parsed(statement).is_err(), "{statement}");
        }
        assert_eq!(
            parsed("with gone as (delete from notes returning id) select count(*) from gone"),
            Ok(Kind::Write)
        );
        assert_eq!(parsed("values (1), (2)"), Ok(Kind::Read));
    }

    #[test]
    fn a_role_is_the_apps_id() {
        assert_eq!(role_of("notes_1"), "app_notes_1");
    }
}
