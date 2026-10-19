//! The stored transcripts of a deployment's conversations (AG-99, API/05 §3.1, T-4670): what a
//! turn showed the visitor, read off the events as they pass, and the steps of its `Trace`,
//! redacted and written once the turn ends; and the reads and the erasure the Portal's
//! administration routes answer.

use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use super::agent::{Event, Turn};
use super::redact::masked;
use super::trace::Step;

static RFC3339: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,9})?(Z|[+-]\d{2}:\d{2})$")
        .expect("valid regex")
});
/// The most conversations one list answers.
pub const MAX_LIST: u32 = 100;

/// What a turn showed the visitor, read off its events on their way to the stream.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Seen {
    answer: Option<String>,
    streamed: String,
    /// The tables, scripts and citations the turn showed, as their events carried them.
    pub blocks: Vec<Value>,
    /// The `error` event the turn ended with, if any.
    pub error: Option<Value>,
}

impl Seen {
    /// Takes one event of the turn.
    pub fn see(&mut self, event: &Event) {
        match event {
            Event::Delta(piece) => self.streamed.push_str(piece),
            Event::Answer(text) => self.answer = Some(text.clone()),
            Event::Data(table) => {
                let mut block = json!(table);
                block["kind"] = json!("table");
                self.blocks.push(block);
            }
            Event::Visualization(shown) => {
                let map = shown.pointer("/spec/$schema").and_then(Value::as_str)
                    == Some(super::viz::MAP_SCHEMA);
                let mut block = shown.clone();
                block["kind"] = json!(if map { "map" } else { "chart" });
                self.blocks.push(block);
            }
            Event::Citations(citations) => self
                .blocks
                .push(json!({"kind": "citations", "items": citations})),
            Event::Script { code, output } => self.blocks.push(match output {
                Ok(output) => json!({"kind": "script", "code": code, "output": output}),
                Err(error) => json!({"kind": "script", "code": code, "error": error}),
            }),
            Event::Error {
                status,
                title,
                detail,
            } => self.error = Some(json!({"status": status, "title": title, "detail": detail})),
            // Progress, follow-ups and the totals are not what the answer showed; an event added
            // later (T-4657's visualization) is stored once it is named here, and never breaks
            // the build of a branch that lands before it.
            _ => {}
        }
    }

    /// The answer as the visitor read it last: the `answer` event's text, else what streamed.
    pub fn answer(&self) -> &str {
        self.answer.as_deref().unwrap_or(&self.streamed)
    }
}

/// One turn to store.
#[derive(Debug)]
pub struct Record<'a> {
    pub project: &'a str,
    pub deployment: &'a str,
    pub conversation: &'a str,
    pub turn: u32,
    pub origin: Option<&'a str>,
    /// The salted daily hash of the client, never its address.
    pub client: &'a str,
    pub question: &'a str,
    pub history: &'a [Turn],
    pub seen: &'a Seen,
    pub steps: &'a [Step],
    pub route: &'a str,
    pub first_token_ms: Option<u64>,
    pub total_ms: u64,
    pub timed_out: bool,
    /// The deployment's `transcriptRetention` in days; 0 stores nothing.
    pub days: u32,
    /// Whether the deployment's `transcriptRedaction` is `personal`; else only secrets are
    /// masked (T-4681).
    pub personal: bool,
}

/// The sum of the priced model calls of `steps` and how many calls had no price (T-4178).
pub fn priced(steps: &[Step]) -> (u64, u32) {
    steps
        .iter()
        .filter(|s| s.kind == "model")
        .fold((0, 0), |(sum, uncosted), s| match s.cost_micros {
            Some(cost) => (sum.saturating_add(cost), uncosted),
            None => (sum, uncosted + 1),
        })
}

fn big(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A JSON value with every string in it masked.
fn redact_value(value: &Value, personal: bool) -> Value {
    match value {
        Value::String(text) => Value::String(masked(text, personal)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| redact_value(v, personal)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_value(v, personal)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Writes one turn and its steps, every text masked first; nothing when `days` is 0.
pub async fn store(pool: &PgPool, record: &Record<'_>) -> Result<(), crate::Error> {
    if record.days == 0 {
        return Ok(());
    }
    let personal = record.personal;
    let redact = |text: &str| masked(text, personal);
    let redacted = |text: Option<&str>| text.map(redact);
    let (cost, uncosted) = priced(record.steps);
    let models = record.steps.iter().filter(|s| s.kind == "model");
    let tokens_in: u64 = models.clone().filter_map(|s| s.tokens_in).sum();
    let tokens_out: u64 = models.filter_map(|s| s.tokens_out).sum();
    let history: Vec<Value> = record
        .history
        .iter()
        .map(|t| json!({"role": t.role, "text": redact(&t.text)}))
        .collect();
    let mut tx = crate::project_scope(pool, record.project).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO turns (project, deployment, conversation, turn, expires_at, origin, client, \
         question, answer, blocks, history, route, first_token_ms, total_ms, error, timed_out, \
         tokens_in, tokens_out, cost_micros, uncosted, question_hash) \
         VALUES ($1, $2, $3::uuid, $4, now() + make_interval(days => $5), $6, $7, $8, $9, $10, \
         $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, \
         md5(btrim(regexp_replace(lower(unaccent($8)), '[^[:alnum:]]+', ' ', 'g')))) \
         ON CONFLICT (conversation, turn) DO NOTHING RETURNING id",
    )
    .bind(record.project)
    .bind(record.deployment)
    .bind(record.conversation)
    .bind(i32::try_from(record.turn).unwrap_or(i32::MAX))
    .bind(i32::try_from(record.days).unwrap_or(i32::MAX))
    .bind(record.origin)
    .bind(record.client)
    .bind(redact(record.question))
    .bind(redact(record.seen.answer()))
    .bind(redact_value(
        &Value::Array(record.seen.blocks.clone()),
        personal,
    ))
    .bind(Value::Array(history))
    .bind(record.route)
    .bind(record.first_token_ms.map(big))
    .bind(big(record.total_ms))
    .bind(
        record
            .seen
            .error
            .as_ref()
            .map(|e| redact_value(e, personal)),
    )
    .bind(record.timed_out)
    .bind(big(tokens_in))
    .bind(big(tokens_out))
    .bind(big(cost))
    .bind(i32::try_from(uncosted).unwrap_or(i32::MAX))
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or_default();
    if id == 0 {
        // The same turn twice (a retried question) keeps the first.
        return Ok(());
    }
    for (ordinal, step) in record.steps.iter().enumerate() {
        sqlx::query(
            "INSERT INTO turn_steps (turn_id, project, ordinal, kind, name, endpoint, input, \
             output, output_bytes, status, start_ms, duration_ms, error, stage, model, provider, \
             tokens_in, tokens_out, reasoning_tokens, cost_micros, cached_tokens, reasoning, \
             finish_reason, effort, prompt_version, rows) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
             $18, $19, $20, $21, $22, $23, $24, $25, $26)",
        )
        .bind(id)
        .bind(record.project)
        .bind(i32::try_from(ordinal).unwrap_or(i32::MAX))
        .bind(step.kind)
        .bind(cut(&step.name, 200))
        .bind(step.endpoint.as_deref().map(|e| cut(e, 200)))
        .bind(redacted(step.input.as_deref()).map(|t| kept_redacted(&t)))
        .bind(redacted(step.output.as_deref()).map(|t| kept_redacted(&t)))
        .bind(step.output_bytes.map(big))
        .bind(step.status)
        .bind(big(step.start_ms))
        .bind(big(step.duration_ms))
        .bind(redacted(step.error.as_deref()).map(|e| cut(&e, 2000)))
        .bind(step.stage)
        .bind(step.model.as_deref().map(|m| cut(m, 100)))
        .bind(step.provider.as_deref().map(|p| cut(p, 64)))
        .bind(step.tokens_in.map(big))
        .bind(step.tokens_out.map(big))
        .bind(step.reasoning_tokens.map(big))
        .bind(step.cost_micros.map(big))
        .bind(step.cached_tokens.map(big))
        .bind(redacted(step.reasoning.as_deref()).map(|t| kept_redacted(&t)))
        .bind(step.finish_reason.as_deref().map(|f| cut(f, 32)))
        .bind(step.effort.as_deref().map(|e| cut(e, 16)))
        .bind(step.prompt_version.as_deref().map(|p| cut(p, 64)))
        .bind(step.rows.map(big))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A masked text may be longer than the 64 kB it was cut to (`[rodné číslo]` for ten digits).
fn kept_redacted(text: &str) -> String {
    super::trace::kept(text).0
}

fn cut(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// The filter of a list (API/05 §3.1).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Filter {
    pub deployment: Option<String>,
    /// RFC 3339 times; `from` inclusive, `to` exclusive.
    pub from: Option<String>,
    pub to: Option<String>,
    /// `error`, `timeout` or `ok`.
    pub status: Option<String>,
    /// `up`, `down` or `none`.
    pub feedback: Option<String>,
    pub slow: Option<bool>,
    pub min_cost_usd: Option<f64>,
    pub q: Option<String>,
    /// `lastAt` (default) or `costUsd`.
    pub sort: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

impl Filter {
    /// Why the filter cannot be run, naming the parameter; `None` when it can.
    pub fn refusal(&self) -> Option<&'static str> {
        let time_ok = |t: &Option<String>| t.as_deref().is_none_or(|t| RFC3339.is_match(t));
        if !time_ok(&self.from) {
            return Some("from is an RFC 3339 time");
        }
        if !time_ok(&self.to) {
            return Some("to is an RFC 3339 time");
        }
        if !matches!(
            self.status.as_deref(),
            None | Some("error" | "timeout" | "ok")
        ) {
            return Some("status is error, timeout or ok");
        }
        if !matches!(
            self.feedback.as_deref(),
            None | Some("up" | "down" | "none")
        ) {
            return Some("feedback is up, down or none");
        }
        if !matches!(self.sort.as_deref(), None | Some("lastAt" | "costUsd")) {
            return Some("sort is lastAt or costUsd");
        }
        if self
            .min_cost_usd
            .is_some_and(|c| !c.is_finite() || !(0.0..=1000.0).contains(&c))
        {
            return Some("minCostUsd is 0 to 1000");
        }
        if self
            .q
            .as_deref()
            .is_some_and(|q| !(2..=100).contains(&q.trim().chars().count()))
        {
            return Some("q is 2 to 100 characters");
        }
        if self.limit.is_some_and(|l| !(1..=MAX_LIST).contains(&l)) {
            return Some("limit is 1 to 100");
        }
        if self
            .cursor
            .as_deref()
            .is_some_and(|c| c.parse::<u32>().is_err())
        {
            return Some("cursor is a nextCursor this route answered");
        }
        None
    }
}

/// `text` as a LIKE pattern that matches it anywhere, its wildcards escaped.
fn anywhere(text: &str) -> String {
    let escaped = text
        .trim()
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

fn usd(micros: i64) -> f64 {
    micros as f64 / 1_000_000.0
}

/// The list's query, ordered by `$order`: one row per conversation with what its turns add up to.
macro_rules! list_sql {
    ($order:literal) => {
        concat!(
            "SELECT t.conversation::text, t.deployment, min(t.origin), \
             to_char(min(t.at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
             to_char(max(t.at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
             (array_agg(t.question ORDER BY t.turn))[1], count(*), \
             array_agg(DISTINCT t.route), max(t.total_ms), \
             count(*) FILTER (WHERE t.error IS NOT NULL AND NOT t.timed_out), \
             count(*) FILTER (WHERE t.timed_out), sum(t.cost_micros)::bigint, \
             sum(t.uncosted)::bigint, coalesce(max(f.up), 0), coalesce(max(f.down), 0) \
             FROM turns t LEFT JOIN LATERAL (SELECT count(*) FILTER (WHERE verdict = 'up') AS up, \
             count(*) FILTER (WHERE verdict = 'down') AS down FROM feedback \
             WHERE feedback.project = t.project AND feedback.conversation = t.conversation) f ON true \
             WHERE ($1::text IS NULL OR t.deployment = $1) \
             AND ($2::timestamptz IS NULL OR t.at >= $2::timestamptz) \
             AND ($3::timestamptz IS NULL OR t.at < $3::timestamptz) \
             GROUP BY t.conversation, t.deployment \
             HAVING ($4::text IS NULL \
               OR ($4 = 'error' AND bool_or(t.error IS NOT NULL AND NOT t.timed_out)) \
               OR ($4 = 'timeout' AND bool_or(t.timed_out)) \
               OR ($4 = 'ok' AND NOT bool_or(t.error IS NOT NULL OR t.timed_out))) \
             AND ($5::text IS NULL OR ($5 = 'up' AND max(f.up) > 0) \
               OR ($5 = 'down' AND max(f.down) > 0) \
               OR ($5 = 'none' AND coalesce(max(f.up), 0) + coalesce(max(f.down), 0) = 0)) \
             AND (NOT $6 OR max(t.total_ms) > 15000) /* slow: 15 s */ \
             AND ($7::bigint IS NULL OR sum(t.cost_micros) >= $7) \
             AND ($8::text IS NULL OR bool_or(lower(unaccent(t.question || ' ' || t.answer)) \
               LIKE lower(unaccent($8)))) \
             ORDER BY ",
            $order,
            " LIMIT $9 OFFSET $10"
        )
    };
}

/// One page of `project`'s conversations, newest first or the most expensive first, and the
/// cursor of the next page.
pub async fn list(
    pool: &PgPool,
    project: &str,
    filter: &Filter,
) -> Result<(Vec<Value>, Option<String>), crate::Error> {
    let limit = filter.limit.unwrap_or(50).min(MAX_LIST);
    let offset: u32 = filter
        .cursor
        .as_deref()
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    type Row = (
        String,
        String,
        Option<String>,
        String,
        String,
        String,
        i64,
        Vec<String>,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    );
    let sql = if filter.sort.as_deref() == Some("costUsd") {
        list_sql!("sum(t.cost_micros) DESC, max(t.at) DESC")
    } else {
        list_sql!("max(t.at) DESC")
    };
    let mut tx = crate::project_scope(pool, project).await?;
    let rows: Vec<Row> = sqlx::query_as(sql)
        .bind(filter.deployment.as_deref())
        .bind(filter.from.as_deref())
        .bind(filter.to.as_deref())
        .bind(filter.status.as_deref())
        .bind(filter.feedback.as_deref())
        .bind(filter.slow.unwrap_or(false))
        .bind(
            filter
                .min_cost_usd
                .map(|c| (c * 1_000_000.0).round() as i64),
        )
        .bind(filter.q.as_deref().map(anywhere))
        .bind(i64::from(limit) + 1)
        .bind(i64::from(offset))
        .fetch_all(&mut *tx)
        .await?;
    let _ = tx.rollback().await;
    let more = rows.len() > limit as usize;
    let items = rows
        .into_iter()
        .take(limit as usize)
        .map(|r| {
            json!({
                "id": r.0, "deployment": r.1, "origin": r.2, "startedAt": r.3, "lastAt": r.4,
                "firstQuestion": r.5, "turns": r.6, "routes": r.7, "totalMs": r.8,
                "errors": r.9, "timeouts": r.10, "costUsd": usd(r.11), "uncosted": r.12,
                "thumbsUp": r.13, "thumbsDown": r.14,
            })
        })
        .collect();
    Ok((items, more.then(|| (offset + limit).to_string())))
}

/// One conversation of `project` with every turn and its steps; `None` when it holds no turn.
pub async fn detail(
    pool: &PgPool,
    project: &str,
    conversation: &str,
) -> Result<Option<Value>, crate::Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let turns: Vec<PgRow> = sqlx::query(
        "SELECT t.id, t.deployment, t.origin, t.client, t.turn, \
         to_char(t.at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS at, t.question, \
         t.answer, t.blocks, t.history, t.route, t.first_token_ms, t.total_ms, t.error, \
         t.timed_out, t.cost_micros, t.uncosted, f.verdict \
         FROM turns t LEFT JOIN feedback f ON f.project = t.project \
         AND f.conversation = t.conversation AND f.turn = t.turn \
         WHERE t.conversation = $1::uuid ORDER BY t.turn",
    )
    .bind(conversation)
    .fetch_all(&mut *tx)
    .await?;
    let Some(first) = turns.first() else {
        let _ = tx.rollback().await;
        return Ok(None);
    };
    let mut out = json!({
        "id": conversation,
        "deployment": first.try_get::<String, _>("deployment")?,
        "origin": first.try_get::<Option<String>, _>("origin")?,
        "client": first.try_get::<Option<String>, _>("client")?,
    });
    let ids = turns
        .iter()
        .map(|t| t.try_get::<i64, _>("id"))
        .collect::<Result<Vec<_>, _>>()?;
    let steps: Vec<PgRow> = sqlx::query(
        "SELECT turn_id, kind, name, endpoint, input, output, output_bytes, status, start_ms, \
         duration_ms, error, stage, model, provider, tokens_in, tokens_out, reasoning_tokens, \
         cost_micros, cached_tokens, reasoning, finish_reason, effort, prompt_version, rows \
         FROM turn_steps WHERE turn_id = ANY($1) ORDER BY turn_id, ordinal",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    let _ = tx.rollback().await;
    let mut shown = Vec::with_capacity(turns.len());
    for (t, id) in turns.iter().zip(&ids) {
        let mut own = Vec::new();
        for s in steps
            .iter()
            .filter(|s| s.try_get::<i64, _>("turn_id").ok() == Some(*id))
        {
            own.push(step_json(s)?);
        }
        shown.push(json!({
            "turn": t.try_get::<i32, _>("turn")?,
            "at": t.try_get::<String, _>("at")?,
            "question": t.try_get::<String, _>("question")?,
            "answer": t.try_get::<String, _>("answer")?,
            "blocks": t.try_get::<Value, _>("blocks")?,
            "history": t.try_get::<Value, _>("history")?,
            "route": t.try_get::<String, _>("route")?,
            "firstTokenMs": t.try_get::<Option<i64>, _>("first_token_ms")?,
            "totalMs": t.try_get::<i64, _>("total_ms")?,
            "error": t.try_get::<Option<Value>, _>("error")?,
            "timedOut": t.try_get::<bool, _>("timed_out")?,
            "costUsd": usd(t.try_get::<i64, _>("cost_micros")?),
            "uncosted": t.try_get::<i32, _>("uncosted")?,
            "verdict": t.try_get::<Option<String>, _>("verdict")?,
            "steps": own,
        }));
    }
    out["turns"] = Value::Array(shown);
    Ok(Some(out))
}

fn step_json(s: &PgRow) -> Result<Value, sqlx::Error> {
    let text = |column: &str| s.try_get::<Option<String>, _>(column);
    let number = |column: &str| s.try_get::<Option<i64>, _>(column);
    Ok(json!({
        "kind": s.try_get::<String, _>("kind")?,
        "name": s.try_get::<String, _>("name")?,
        "endpoint": text("endpoint")?,
        "input": text("input")?,
        "output": text("output")?,
        "outputBytes": number("output_bytes")?,
        "status": s.try_get::<String, _>("status")?,
        "startMs": s.try_get::<i64, _>("start_ms")?,
        "durationMs": s.try_get::<i64, _>("duration_ms")?,
        "error": text("error")?,
        "stage": text("stage")?,
        "model": text("model")?,
        "provider": text("provider")?,
        "tokensIn": number("tokens_in")?,
        "tokensOut": number("tokens_out")?,
        "reasoningTokens": number("reasoning_tokens")?,
        "costUsd": number("cost_micros")?.map(usd),
        "cachedTokens": number("cached_tokens")?,
        "reasoning": text("reasoning")?,
        "finishReason": text("finish_reason")?,
        "effort": text("effort")?,
        "promptVersion": text("prompt_version")?,
        "rows": number("rows")?,
    }))
}

/// The most turns [`same`] lists.
pub const MAX_SAME: i64 = 50;

/// The stored turns of `project` whose question reads as turn `turn` of `conversation`'s,
/// newest first, at most [`MAX_SAME`]; `None` when that turn is not stored (API/05 §3.1, T-4681).
pub async fn same(
    pool: &PgPool,
    project: &str,
    conversation: &str,
    turn: u32,
) -> Result<Option<Vec<Value>>, crate::Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let hash: Option<String> = sqlx::query_scalar(
        "SELECT question_hash FROM turns WHERE conversation = $1::uuid AND turn = $2",
    )
    .bind(conversation)
    .bind(i32::try_from(turn).unwrap_or(i32::MAX))
    .fetch_optional(&mut *tx)
    .await?;
    let Some(hash) = hash else {
        let _ = tx.rollback().await;
        return Ok(None);
    };
    type Row = (String, i32, String, String, i64, i64, i64);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT t.conversation::text, t.turn, \
         to_char(t.at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), t.deployment, \
         t.total_ms, t.cost_micros, (SELECT count(*) FROM turn_steps s WHERE s.turn_id = t.id) \
         FROM turns t WHERE t.question_hash = $1 ORDER BY t.at DESC LIMIT $2",
    )
    .bind(&hash)
    .bind(MAX_SAME)
    .fetch_all(&mut *tx)
    .await?;
    let _ = tx.rollback().await;
    Ok(Some(
        rows.into_iter()
            .map(|r| {
                json!({"conversation": r.0, "turn": r.1, "at": r.2, "deployment": r.3,
                    "totalMs": r.4, "costUsd": usd(r.5), "steps": r.6})
            })
            .collect(),
    ))
}

/// Erases one conversation's turns and steps (T-4668); whether it held any.
pub async fn erase(pool: &PgPool, project: &str, conversation: &str) -> Result<bool, crate::Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let gone = sqlx::query("DELETE FROM turns WHERE conversation = $1::uuid")
        .bind(conversation)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(gone > 0)
}

/// The most terms one erasure searches for, and the length bounds of one.
pub const MAX_TERMS: usize = 1001;
const TERM_CHARS: std::ops::RangeInclusive<usize> = 5..=1024;

/// Why `terms` cannot be searched for, if they cannot: 1 to [`MAX_TERMS`] of 5 to 1024
/// characters, none a control character.
pub fn terms_refusal(terms: &[String]) -> Option<String> {
    if terms.is_empty() || terms.len() > MAX_TERMS {
        return Some(format!("terms names 1 to {MAX_TERMS} texts"));
    }
    terms
        .iter()
        .any(|t| !TERM_CHARS.contains(&t.trim().chars().count()) || t.chars().any(char::is_control))
        .then(|| "each term is 5 to 1024 characters of plain text".to_owned())
}

/// Erases every stored conversation of `project` one of whose turns mentions one of `terms`
/// (case-insensitively in the question, the answer, what it showed, its history or a step's
/// input or output), with its feedback: the data subject's erasure (DM-126, API/01 §46,
/// T-4668). The terms are bound, never spliced. Returns the conversations erased.
pub async fn erase_mentioning(
    pool: &PgPool,
    project: &str,
    terms: &[String],
) -> Result<u64, crate::Error> {
    let terms: Vec<String> = terms.iter().map(|t| t.trim().to_lowercase()).collect();
    let mut tx = crate::project_scope(pool, project).await?;
    let gone: i64 = sqlx::query_scalar(
        "WITH hit AS ( \
           SELECT DISTINCT t.conversation FROM turns t, unnest($1::text[]) AS term(v) \
           WHERE strpos(lower(t.question), term.v) > 0 OR strpos(lower(t.answer), term.v) > 0 \
              OR strpos(lower(t.blocks::text), term.v) > 0 \
              OR strpos(lower(t.history::text), term.v) > 0 \
              OR EXISTS (SELECT 1 FROM turn_steps s WHERE s.turn_id = t.id \
                         AND (strpos(lower(coalesce(s.input, '')), term.v) > 0 \
                              OR strpos(lower(coalesce(s.output, '')), term.v) > 0))), \
         said AS (DELETE FROM feedback WHERE conversation IN (SELECT conversation FROM hit)), \
         gone AS (DELETE FROM turns WHERE conversation IN (SELECT conversation FROM hit) \
                  RETURNING conversation) \
         SELECT count(DISTINCT conversation) FROM gone",
    )
    .bind(&terms)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(u64::try_from(gone).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::agent::{Citation, Table};

    #[test]
    fn seen_keeps_the_answer_the_visitor_read_and_what_it_showed() {
        let mut seen = Seen::default();
        for event in [
            Event::Stage {
                stage: "answering",
                topic: None,
            },
            Event::Delta("Dnes ".into()),
            Event::Delta("prší.".into()),
            Event::Data(Table {
                endpoint: "pocasie".into(),
                entity_type: "WeatherObserved".into(),
                columns: vec!["name".into()],
                rows: vec![vec![json!("Donovaly")]],
                units: vec![None],
                labels: vec![None],
                summary: vec![false],
                enums: Vec::new(),
                total: None,
                dataset: None,
                download: None,
                map: None,
                geometries: vec![],
            }),
            Event::Citations(vec![Citation {
                n: 1,
                url: Some("https://x.sk".into()),
                tool: None,
                endpoint: None,
                title: None,
                cards: Default::default(),
            }]),
        ] {
            seen.see(&event);
        }
        assert_eq!(seen.answer(), "Dnes prší.");
        assert_eq!(seen.blocks[0]["kind"], "table");
        assert_eq!(seen.blocks[1]["kind"], "citations");
        seen.see(&Event::Answer("Dnes prší [1].".into()));
        assert_eq!(
            seen.answer(),
            "Dnes prší [1].",
            "the answer event wins over the stream"
        );
        seen.see(&Event::Error {
            status: 504,
            title: "Answer Took Too Long",
            detail: "…".into(),
        });
        assert_eq!(
            seen.error.as_ref().map(|e| e["status"].clone()),
            Some(json!(504))
        );
    }

    #[test]
    fn a_price_missing_from_one_call_is_counted_apart_never_as_zero() {
        let model = |cost| Step {
            kind: "model",
            cost_micros: cost,
            ..Step::default()
        };
        let tool = Step {
            kind: "tool",
            ..Step::default()
        };
        assert_eq!(
            priced(&[model(Some(1_000)), model(Some(250)), tool.clone()]),
            (1_250, 0)
        );
        assert_eq!(priced(&[model(Some(1_000)), model(None), tool]), (1_000, 1));
        assert_eq!(priced(&[]), (0, 0));
    }

    #[test]
    fn a_filter_names_the_parameter_it_cannot_run() {
        let ok = Filter {
            from: Some("2026-10-18T00:00:00Z".into()),
            status: Some("timeout".into()),
            q: Some("odvoz".into()),
            limit: Some(100),
            ..Filter::default()
        };
        assert_eq!(ok.refusal(), None);
        let bad = |f: Filter| f.refusal().unwrap_or_default();
        assert!(bad(Filter {
            from: Some("včera".into()),
            ..Filter::default()
        })
        .starts_with("from"));
        assert!(bad(Filter {
            status: Some("slow".into()),
            ..Filter::default()
        })
        .starts_with("status"));
        assert!(bad(Filter {
            q: Some("x".into()),
            ..Filter::default()
        })
        .starts_with("q"));
        assert!(bad(Filter {
            limit: Some(101),
            ..Filter::default()
        })
        .starts_with("limit"));
        assert!(bad(Filter {
            min_cost_usd: Some(f64::NAN),
            ..Filter::default()
        })
        .starts_with("minCostUsd"));
        assert!(bad(Filter {
            cursor: Some("abc".into()),
            ..Filter::default()
        })
        .starts_with("cursor"));
        assert_eq!(anywhere(" 50%_off\\ "), "%50\\%\\_off\\\\%");
    }

    #[test]
    fn every_string_of_a_block_is_masked_as_the_deployment_says() {
        let block = json!([{"kind": "table", "rows": [["jan@x.sk", 3]],
            "note": "tel. 0905 123 456, heslo: tajne1"}]);
        assert_eq!(
            redact_value(&block, true),
            json!([{"kind": "table", "rows": [["[e-mail]", 3]], "note": "tel. [telefón], heslo: [heslo]"}])
        );
        assert_eq!(
            redact_value(&block, false),
            json!([{"kind": "table", "rows": [["jan@x.sk", 3]], "note": "tel. 0905 123 456, heslo: [heslo]"}])
        );
    }

    /// T-4681: a chart and a map the visitor saw are blocks with their kind, as a table is.
    #[test]
    fn a_chart_and_a_map_shown_are_kept_with_their_kind() {
        let mut seen = Seen::default();
        seen.see(&Event::Visualization(
            json!({"spec": {"mark": "bar"}, "rows": [{"a": 1}]}),
        ));
        seen.see(&Event::Visualization(
            json!({"spec": {"$schema": super::super::viz::MAP_SCHEMA}}),
        ));
        assert_eq!(
            seen.blocks
                .iter()
                .map(|b| b["kind"].as_str())
                .collect::<Vec<_>>(),
            [Some("chart"), Some("map")]
        );
        assert_eq!(seen.blocks[0]["rows"][0]["a"], 1);
    }
}
