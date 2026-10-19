//! What a project's stored turns add up to (API/05 §3.1, T-4728): one aggregate over the turns of
//! a filter, the cost only over priced turns (an unpriced one is counted apart, never as $0), the
//! speed per phase from the steps, the newest days; and the same turns as CSV, for the Portal's
//! administrators.

use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use super::transcript::{usd, RFC3339};

/// The most turns one export answers; more is a refusal that asks for a narrower range.
pub const MAX_EXPORT: i64 = 20_000;

/// The days the summary lists, newest first.
const DAYS: i64 = 14;

/// The turns a summary or an export reads.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Range {
    pub deployment: Option<String>,
    /// RFC 3339 times; `from` inclusive, `to` exclusive.
    pub from: Option<String>,
    pub to: Option<String>,
    /// The route the turns took (`rag`, `data`, `direct`, …).
    pub route: Option<String>,
}

impl Range {
    /// Why the range cannot be read, naming the parameter; `None` when it can.
    pub fn refusal(&self) -> Option<&'static str> {
        let time_ok = |t: &Option<String>| t.as_deref().is_none_or(|t| RFC3339.is_match(t));
        if !time_ok(&self.from) {
            return Some("from is an RFC 3339 time");
        }
        if !time_ok(&self.to) {
            return Some("to is an RFC 3339 time");
        }
        if self
            .route
            .as_deref()
            .is_some_and(|r| r.is_empty() || r.len() > 16)
        {
            return Some("route is 1 to 16 characters");
        }
        None
    }
}

/// The turns of a range: `$1` deployment, `$2` from, `$3` to, `$4` route; row security keeps them
/// to the project in scope.
const TURNS: &str = "SELECT * FROM turns WHERE ($1::text IS NULL OR deployment = $1) \
     AND ($2::timestamptz IS NULL OR at >= $2::timestamptz) \
     AND ($3::timestamptz IS NULL OR at < $3::timestamptz) \
     AND ($4::text IS NULL OR route = $4)";

/// Each turn of the range with its cached tokens and the time its steps spent per phase: the
/// router call, the work (search, tools, the data, judge and agent calls) and the answer call.
fn phased() -> String {
    format!(
        "WITH t AS ({TURNS}), s AS (SELECT turn_id, sum(cached_tokens)::bigint AS cached, \
         sum(duration_ms) FILTER (WHERE kind = 'route' OR stage = 'router') AS route_ms, \
         sum(duration_ms) FILTER (WHERE kind IN ('search', 'tool') \
           OR stage IN ('data', 'judge', 'agent')) AS work_ms, \
         sum(duration_ms) FILTER (WHERE kind = 'answer' OR stage = 'answer') AS answer_ms, \
         array_agg(DISTINCT model) FILTER (WHERE model IS NOT NULL) AS models \
         FROM turn_steps WHERE turn_id IN (SELECT id FROM t) GROUP BY turn_id) \
         SELECT t.*, s.cached, s.route_ms, s.work_ms, s.answer_ms, s.models \
         FROM t LEFT JOIN s ON s.turn_id = t.id"
    )
}

/// `p50`, `p90` and `max` of `column` as SQL select items.
fn spread(column: &str) -> String {
    format!(
        "percentile_cont(0.5) WITHIN GROUP (ORDER BY {column}), \
         percentile_cont(0.9) WITHIN GROUP (ORDER BY {column}), max({column})::float8"
    )
}

fn summary_sql() -> String {
    format!(
        "WITH j AS ({}) SELECT count(*), count(*) FILTER (WHERE timed_out), \
         count(*) FILTER (WHERE error IS NOT NULL AND NOT timed_out), \
         count(*) FILTER (WHERE uncosted > 0), \
         sum(cost_micros) FILTER (WHERE uncosted = 0)::bigint, \
         avg(cost_micros) FILTER (WHERE uncosted = 0)::float8, \
         percentile_cont(0.5) WITHIN GROUP (ORDER BY cost_micros) FILTER (WHERE uncosted = 0), \
         percentile_cont(0.9) WITHIN GROUP (ORDER BY cost_micros) FILTER (WHERE uncosted = 0), \
         avg(tokens_in)::float8, avg(tokens_out)::float8, avg(cached)::float8, \
         (sum(cached) / nullif(sum(tokens_in) FILTER (WHERE cached IS NOT NULL), 0))::float8, \
         {}, {}, {}, {}, {} FROM j",
        phased(),
        spread("first_token_ms"),
        spread("total_ms"),
        spread("route_ms"),
        spread("work_ms"),
        spread("answer_ms"),
    )
}

fn days_sql() -> String {
    format!(
        "SELECT to_char(date_trunc('day', at AT TIME ZONE 'UTC'), 'YYYY-MM-DD') AS day, count(*), \
         sum(cost_micros) FILTER (WHERE uncosted = 0)::bigint, \
         count(*) FILTER (WHERE uncosted > 0), \
         percentile_cont(0.5) WITHIN GROUP (ORDER BY total_ms), \
         count(*) FILTER (WHERE timed_out), \
         count(*) FILTER (WHERE error IS NOT NULL AND NOT timed_out) \
         FROM ({TURNS}) t GROUP BY 1 ORDER BY 1 DESC LIMIT {DAYS}"
    )
}

/// A float column as a JSON number, `null` when SQL had none (no turn, or no priced turn).
fn number(row: &PgRow, i: usize) -> Result<Value, sqlx::Error> {
    Ok(row
        .try_get::<Option<f64>, _>(i)?
        .map_or(Value::Null, |v| json!(v)))
}

/// A dollar column in millionths as a JSON number of dollars, `null` when SQL had none.
fn dollars(row: &PgRow, i: usize) -> Result<Value, sqlx::Error> {
    Ok(row
        .try_get::<Option<f64>, _>(i)?
        .map_or(Value::Null, |v| json!(v / 1_000_000.0)))
}

fn spread_json(row: &PgRow, at: usize) -> Result<Value, sqlx::Error> {
    Ok(json!({"p50": number(row, at)?, "p90": number(row, at + 1)?, "max": number(row, at + 2)?}))
}

/// What `project`'s turns in `range` add up to, and the newest days of them.
pub async fn summary(pool: &PgPool, project: &str, range: &Range) -> Result<Value, crate::Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let bind = |sql: &str| {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .bind(range.deployment.clone())
            .bind(range.from.clone())
            .bind(range.to.clone())
            .bind(range.route.clone())
    };
    let row = bind(&summary_sql()).fetch_one(&mut *tx).await?;
    let days = bind(&days_sql()).fetch_all(&mut *tx).await?;
    let _ = tx.rollback().await;
    let total: Option<i64> = row.try_get(4)?;
    let days = days
        .iter()
        .map(|d| {
            let cost: Option<i64> = d.try_get(2)?;
            Ok(json!({
                "day": d.try_get::<String, _>(0)?, "questions": d.try_get::<i64, _>(1)?,
                "costUsd": cost.map(usd), "uncostedQuestions": d.try_get::<i64, _>(3)?,
                "p50TotalMs": number(d, 4)?, "timeouts": d.try_get::<i64, _>(5)?,
                "errors": d.try_get::<i64, _>(6)?,
            }))
        })
        .collect::<Result<Vec<Value>, sqlx::Error>>()?;
    Ok(json!({
        "questions": row.try_get::<i64, _>(0)?,
        "timeouts": row.try_get::<i64, _>(1)?,
        "errors": row.try_get::<i64, _>(2)?,
        "uncostedQuestions": row.try_get::<i64, _>(3)?,
        "costUsd": {
            "total": total.map(usd), "mean": dollars(&row, 5)?,
            "p50": dollars(&row, 6)?, "p90": dollars(&row, 7)?,
        },
        "tokens": {
            "in": number(&row, 8)?, "out": number(&row, 9)?, "cached": number(&row, 10)?,
            "cachedShare": number(&row, 11)?,
        },
        "firstTokenMs": spread_json(&row, 12)?,
        "totalMs": spread_json(&row, 15)?,
        "phases": {
            "route": spread_json(&row, 18)?, "work": spread_json(&row, 21)?,
            "answer": spread_json(&row, 24)?,
        },
        "days": days,
    }))
}

/// The columns of an export, in order.
pub const COLUMNS: [&str; 16] = [
    "at",
    "deployment",
    "conversation",
    "turn",
    "route",
    "question",
    "firstTokenMs",
    "totalMs",
    "tokensIn",
    "tokensOut",
    "cachedTokens",
    "models",
    "costUsd",
    "uncosted",
    "timedOut",
    "error",
];

/// One CSV cell (RFC 4180): quoted when it holds a quote, a comma or a line break, and a text a
/// spreadsheet would run as a formula (`=`, `+`, `-`, `@` first) gets a leading `'`.
pub fn cell(text: &str) -> String {
    let text = if text.starts_with(['=', '+', '-', '@']) {
        format!("'{text}")
    } else {
        text.to_owned()
    };
    if text.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

fn export_row(r: &PgRow) -> Result<String, sqlx::Error> {
    let num = |v: Option<i64>| v.map(|n| n.to_string()).unwrap_or_default();
    let uncosted: i32 = r.try_get("uncosted")?;
    let cost: i64 = r.try_get("cost_micros")?;
    let error: Option<Value> = r.try_get("error")?;
    let error = error
        .map(|e| {
            e.get("title")
                .and_then(Value::as_str)
                .map_or_else(|| e.to_string(), str::to_owned)
        })
        .unwrap_or_default();
    let models: Option<Vec<String>> = r.try_get("models")?;
    let cells = [
        r.try_get::<String, _>("at_text")?,
        r.try_get("deployment")?,
        r.try_get("conversation_text")?,
        r.try_get::<i32, _>("turn")?.to_string(),
        r.try_get("route")?,
        r.try_get("question")?,
        num(r.try_get("first_token_ms")?),
        r.try_get::<i64, _>("total_ms")?.to_string(),
        r.try_get::<i64, _>("tokens_in")?.to_string(),
        r.try_get::<i64, _>("tokens_out")?.to_string(),
        num(r.try_get("cached")?),
        models.unwrap_or_default().join(" "),
        if uncosted > 0 {
            String::new()
        } else {
            format!("{:.6}", usd(cost))
        },
        uncosted.to_string(),
        r.try_get::<bool, _>("timed_out")?.to_string(),
        error,
    ];
    Ok(cells.iter().map(|c| cell(c)).collect::<Vec<_>>().join(",") + "\r\n")
}

/// `project`'s turns in `range` as CSV, newest first; `None` when they are more than
/// [`MAX_EXPORT`].
pub async fn export(
    pool: &PgPool,
    project: &str,
    range: &Range,
) -> Result<Option<String>, crate::Error> {
    let sql = format!(
        "WITH j AS ({}) SELECT j.*, \
         to_char(at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS at_text, \
         conversation::text AS conversation_text FROM j ORDER BY at DESC, turn DESC LIMIT {}",
        phased(),
        MAX_EXPORT + 1
    );
    let mut tx = crate::project_scope(pool, project).await?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(range.deployment.as_deref())
        .bind(range.from.as_deref())
        .bind(range.to.as_deref())
        .bind(range.route.as_deref())
        .fetch_all(&mut *tx)
        .await?;
    let _ = tx.rollback().await;
    if rows.len() as i64 > MAX_EXPORT {
        return Ok(None);
    }
    let mut csv = COLUMNS.join(",") + "\r\n";
    for row in &rows {
        csv.push_str(&export_row(row)?);
    }
    Ok(Some(csv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_is_quoted_only_when_it_must_be_and_never_runs_as_a_formula() {
        assert_eq!(cell("plain"), "plain");
        assert_eq!(cell("a, b"), "\"a, b\"");
        assert_eq!(cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(cell("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(cell("=SUM(A1)"), "'=SUM(A1)");
        assert_eq!(cell("-1,5"), "\"'-1,5\"");
        assert_eq!(cell(""), "");
    }

    #[test]
    fn a_range_names_what_it_refuses() {
        assert_eq!(Range::default().refusal(), None);
        let bad_time = Range {
            from: Some("yesterday".into()),
            ..Range::default()
        };
        assert_eq!(bad_time.refusal(), Some("from is an RFC 3339 time"));
        let bad_to = Range {
            to: Some("2026-10-19".into()),
            ..Range::default()
        };
        assert_eq!(bad_to.refusal(), Some("to is an RFC 3339 time"));
        let long = Range {
            route: Some("r".repeat(17)),
            ..Range::default()
        };
        assert_eq!(long.refusal(), Some("route is 1 to 16 characters"));
    }
}
