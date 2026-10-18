//! The chat route of `jc-assistant` (API/05, AG-98…AG-108, T-3055):
//! `POST /api/v1/d/{publicId}/chat`, one question per request, answered as Server-Sent Events.
//!
//! This route serves the anonymous channels (`public`, `ckan`, `iframe`); an `internal`
//! deployment answers `404` here and is asked through the Portal on
//! `/internal/v1/projects/{project}/knowledge/deployments/{deployment}/chat` (API/05 §1.7,
//! AG-115).

pub mod admin;
pub mod agent;
pub mod cards;
pub mod clock;
pub mod data;
pub mod documents;
pub mod mcp;
pub mod model;
pub mod routed;
pub mod script;
pub mod widget;

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use jc_core::kinds::assistant::connector_target;
use jc_core::kinds::Routes;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;

use self::agent::{Ask, Connected, Event, Spent, Turn};
use self::mcp::{PersonToken, Server, Surface};
use self::model::Model;
use crate::embed::Embedder;
use crate::worker::{Deployment, Snapshot};

/// The largest request body (AG-98).
pub const MAX_BODY: usize = 64 * 1024;
/// The longest question and the longest turn of `history`, in characters (AG-98).
pub const MAX_TEXT_CHARS: usize = 4_000;
/// The most turns of `history` (AG-98).
pub const MAX_TURNS: usize = 6;
/// How long a conversation may be continued; migration 0006's policy deletes it after as long.
const CONVERSATION_HOURS: i64 = 24;
/// The most questions answered at once, whatever the deployments' rate limits allow (T-3059):
/// each one holds the history, up to 250,000 kept characters of tool results and a model call,
/// so this is what keeps the service under its 1 GB. A question past it is refused, never queued.
// ponytail: one number for the service; per deployment when one crowds out the others.
pub const MAX_QUESTIONS: usize = 32;

/// Everything the route answers with.
pub struct ChatState {
    pub pool: PgPool,
    pub embedder: Embedder,
    pub model: Model,
    /// For the connectors' calls: no redirect of its own.
    pub http: reqwest::Client,
    /// The Context Gateway, scheme and authority.
    pub gateway: String,
    /// `jc-functions`, scheme and authority, for a deployment with `sandbox: true`; `None`
    /// offers no script anywhere.
    pub functions: Option<String>,
    /// The Portal's Keycloak client, the one caller of the administration paths (AG-113).
    pub portal_client: String,
    /// This service's own public origin, `https://assistant.{domain}`: the widget's requests
    /// come from it, framed by an allowed origin (AG-114). `None` admits no widget.
    pub public_origin: Option<String>,
    /// The manifests, replaced by the worker every minute.
    pub snapshot: RwLock<Arc<Snapshot>>,
    pub limits: Mutex<Limits>,
    /// One permit per question being answered, [`MAX_QUESTIONS`] in all.
    pub questions: Arc<tokio::sync::Semaphore>,
    /// Answers to first questions asked before (T-4139).
    pub answers: routed::AnswerCache,
    /// The public origin `/api/endpoint/{slug}` is served under (`JC_ASSISTANT_ENDPOINT_URL`):
    /// a data table's download link; `None` offers none.
    pub endpoint_base: Option<url::Url>,
    /// The organization the service answers for, the `sites` row an upload creates (T-4184).
    pub organization: String,
}

/// The most keys [`Limits`] counts at once (T-3314).
pub const MAX_KEYS: usize = 50_000;
/// How long one answer may take, model calls and searches together (T-4149).
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// The time a data question's answer call has after the loop's last tool may be called.
const DATA_ANSWER: Duration = Duration::from_secs(25);
const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3_600);
const DAY: Duration = Duration::from_secs(86_400);
/// The monthly window's length: its key names the calendar month, so a new month is a new key
/// and the window only has to outlast the longest month (T-4286).
const MONTH: Duration = Duration::from_secs(31 * 86_400);

/// Requests per key in fixed windows (AG-101, T-4149), and the answers each visitor has streaming.
/// A visitor is known by a keyed hash of their address and the UTC day, never the address itself:
/// the key is random per process, so a hash says nothing after a restart or past its day.
// ponytail: in memory, one replica; a second replica doubles every limit. Move to the database
// when the assistant runs on more than one.
#[derive(Default)]
pub struct Limits {
    windows: HashMap<String, (Instant, u32, Duration)>,
    streams: HashMap<String, u32>,
    salt: std::hash::RandomState,
    /// The deployments whose cost cap was reached, and on which day it was first.
    capped: HashMap<String, u64>,
}

impl Limits {
    /// Counts one request under every `(key, limit, window)`, or under none: `Err(seconds)` is the
    /// longest wait among the windows that are full, so a refused request spends no place.
    pub fn admit(&mut self, windows: &[(&str, u32, Duration)], now: Instant) -> Result<(), u64> {
        let new_keys = windows
            .iter()
            .filter(|(key, ..)| !self.windows.contains_key(*key))
            .count();
        if self.windows.len() + new_keys > MAX_KEYS {
            self.windows
                .retain(|_, (started, _, window)| now.duration_since(*started) < *window);
            if self.windows.len() + new_keys > MAX_KEYS {
                // A full table refuses a new key rather than grow or forget a counted one: a
                // reset would hand every key already at its limit a fresh window.
                return Err(60);
            }
        }
        let wait = windows
            .iter()
            .filter_map(|(key, limit, _)| {
                let (started, count, window) = self.windows.get(*key)?;
                let elapsed = now.duration_since(*started);
                (elapsed < *window && count >= limit)
                    .then(|| window.saturating_sub(elapsed).as_secs().max(1))
            })
            .max();
        if let Some(wait) = wait {
            return Err(wait);
        }
        for (key, _, window) in windows {
            let (started, count, kept) = self
                .windows
                .entry((*key).to_owned())
                .or_insert((now, 0, *window));
            if now.duration_since(*started) >= *kept {
                *started = now;
                *count = 0;
            }
            *count += 1;
        }
        Ok(())
    }

    /// The visitor `client` as the limits know them on `day` (days since 1970, UTC).
    pub fn visitor(&self, client: &str, day: u64) -> String {
        use std::hash::BuildHasher;
        format!("{:016x}", self.salt.hash_one((day, client)))
    }

    /// Whether `key`'s window already counts `limit` requests at `now`; counts nothing (T-4286).
    pub fn full(&self, key: &str, limit: u32, now: Instant) -> bool {
        self.windows
            .get(key)
            .is_some_and(|(started, count, window)| {
                now.duration_since(*started) < *window && *count >= limit
            })
    }

    /// Takes one of `visitor`'s `limit` streams; `false` when all are taken.
    pub fn open_stream(&mut self, visitor: &str, limit: u32) -> bool {
        let open = self.streams.entry(visitor.to_owned()).or_insert(0);
        if *open >= limit {
            return false;
        }
        *open += 1;
        true
    }

    /// `true` the first time `deployment` is found at its cost cap on `day`, so it is reported
    /// once a day.
    pub fn first_cap_today(&mut self, deployment: &str, day: u64) -> bool {
        self.capped.insert(deployment.to_owned(), day) != Some(day)
    }

    /// Gives back a stream [`Limits::open_stream`] took.
    pub fn close_stream(&mut self, visitor: &str) {
        if let Some(open) = self.streams.get_mut(visitor) {
            *open = open.saturating_sub(1);
            if *open == 0 {
                self.streams.remove(visitor);
            }
        }
    }
}

/// Today, as days since 1970 in UTC.
/// The UTC calendar month as a number (`year * 12 + month - 1`) and the first day of the next
/// one as the visitor reads it, `1. 11. 2026` (T-4286).
pub fn month_of(now: time::OffsetDateTime) -> (u64, String) {
    let (year, month) = (now.year(), u8::from(now.month()));
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let index = u64::try_from(year).unwrap_or(0) * 12 + u64::from(month) - 1;
    (index, format!("1. {next_month}. {next_year}"))
}

fn today() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / DAY.as_secs())
}

/// `429` with `Retry-After`, in the words the chat shows (T-4149).
fn too_many(retry: u64, detail: &str) -> Response {
    let mut response = problem(429, detail);
    if let Ok(value) = HeaderValue::from_str(&retry.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// The request body (API/05 §1.1).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    #[serde(default)]
    pub conversation: Option<String>,
    pub message: String,
    #[serde(default)]
    pub history: Vec<Turn>,
    #[serde(default)]
    pub connectors: Option<Vec<String>>,
}

/// The longest note of a thumbs down (API/05 §1.8).
pub const MAX_NOTE_CHARS: usize = 500;

/// A visitor's verdict on one answer (API/05 §1.8, T-4174).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feedback {
    pub conversation: String,
    pub turn: u32,
    pub verdict: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// The checks a verdict passes before it is looked up; the note as it is stored, trimmed, `None`
/// when empty. A note with a secret's shape is refused, never stored.
pub fn check_feedback(feedback: &Feedback) -> Result<Option<String>, String> {
    if feedback.verdict != "up" && feedback.verdict != "down" {
        return Err("verdict is up or down".into());
    }
    if !(1..=200).contains(&feedback.turn) {
        return Err("turn is the answer's place in the conversation, 1 to 200".into());
    }
    let note = feedback
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let Some(note) = note else {
        return Ok(None);
    };
    if feedback.verdict != "down" {
        return Err("only a thumbs down carries a note".into());
    }
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(format!("note is at most {MAX_NOTE_CHARS} characters"));
    }
    if agent_proxy::routes::diagnostics::redact(note) != note {
        return Err(
            "Poznámka vyzerá ako heslo alebo kľúč, preto ju neukladáme. Napíšte ju bez neho."
                .into(),
        );
    }
    Ok(Some(note.to_owned()))
}

/// The chat route with its body limit, the MCP servers' search (documents.rs) and the Portal's
/// administration paths (admin.rs).
pub fn router(state: Arc<ChatState>) -> Router {
    Router::new()
        .route("/api/v1/d/{public_id}/chat", post(chat).options(preflight))
        .route(
            "/api/v1/d/{public_id}/feedback",
            post(feedback).options(preflight),
        )
        .route(
            "/internal/v1/projects/{project}/knowledge/deployments/{deployment}/chat",
            post(chat_in_portal),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(Arc::clone(&state))
        .merge(widget::router(Arc::clone(&state)))
        .merge(documents::router(Arc::clone(&state)))
        .merge(admin::router(state))
}

/// The platform's own problem type for `status` (T-3243), never a slug made of a title.
fn problem(status: u16, detail: impl Into<String>) -> Response {
    jc_core::ProblemDetails::for_status(status)
        .with_detail(detail.into())
        .into_response()
}

/// A body the route could not read, in words (T-3243): which of the four it was and the member at
/// fault, never the deserializer's sentence with its Rust type names.
pub(crate) fn body_refused(rejection: &JsonRejection) -> Response {
    match rejection {
        JsonRejection::MissingJsonContentType(_) => {
            problem(415, "send the body as application/json")
        }
        JsonRejection::JsonSyntaxError(_) => problem(400, "the body is not valid JSON"),
        JsonRejection::JsonDataError(_) => {
            // The member is the caller's own path into what they sent (`history[0].text`).
            let text = rejection.body_text();
            let member = text
                .split_once("target type: ")
                .and_then(|(_, rest)| rest.split_once(": "))
                .map(|(member, _)| member)
                .filter(|member| !member.is_empty() && !member.contains(' '));
            match member {
                Some(member) => jc_core::ProblemDetails::for_status(400)
                    .with_field(member)
                    .with_detail(format!(
                        "`{member}` is missing or not of the documented type; see API/05"
                    ))
                    .into_response(),
                None => problem(
                    400,
                    "the body does not have the documented shape: a member is missing or not of its type; see API/05",
                ),
            }
        }
        JsonRejection::BytesRejection(_) => problem(
            400,
            format!(
                "the body exceeds the length limit of {} KiB this route takes",
                MAX_BODY / 1024
            ),
        ),
        _ => problem(400, "the body could not be read"),
    }
}

/// The anonymous deployment `public_id` names.
fn deployment_of(state: &ChatState, public_id: &str) -> Option<Deployment> {
    let snapshot = state.snapshot.read().ok()?.clone();
    snapshot.public(public_id).cloned()
}

/// Where a request comes from, judged against the deployment's `allowedOrigins` (AG-100).
enum Placed {
    /// No `Origin`: not a browser's request.
    Nowhere,
    Allowed(HeaderValue),
    Refused,
}

fn origin(headers: &HeaderMap, deployment: &Deployment, own: Option<&str>) -> Placed {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Placed::Nowhere;
    };
    if origin
        .to_str()
        .is_ok_and(|o| deployment.spec.allowed_origins.iter().any(|a| a == o) || own == Some(o))
    {
        Placed::Allowed(origin.clone())
    } else {
        Placed::Refused
    }
}

fn not_placed_here() -> Response {
    problem(403, "This assistant is not placed on this site.")
}

fn with_cors(mut response: Response, origin: Option<HeaderValue>) -> Response {
    if let Some(origin) = origin {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
}

async fn preflight(
    State(state): State<Arc<ChatState>>,
    Path(public_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(deployment) = deployment_of(&state, &public_id) else {
        return problem(404, "No assistant is published under this id.");
    };
    match origin(&headers, &deployment, state.public_origin.as_deref()) {
        Placed::Allowed(origin) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            let h = response.headers_mut();
            h.insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("POST"),
            );
            h.insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("content-type"),
            );
            h.insert(
                header::ACCESS_CONTROL_MAX_AGE,
                HeaderValue::from_static("600"),
            );
            with_cors(response, Some(origin))
        }
        Placed::Nowhere => problem(403, "A preflight names its origin."),
        Placed::Refused => not_placed_here(),
    }
}

/// The checks of AG-98 a body passes before anything is spent on it.
pub fn validate(question: &Question, deployment: &Deployment) -> Result<(), String> {
    let message = question.message.trim();
    if message.is_empty() || message.chars().count() > MAX_TEXT_CHARS {
        return Err(format!("message is 1 to {MAX_TEXT_CHARS} characters"));
    }
    if question.history.len() > MAX_TURNS {
        return Err(format!("history holds at most {MAX_TURNS} turns"));
    }
    for turn in &question.history {
        if turn.role != "user" && turn.role != "assistant" {
            return Err("a history turn's role is user or assistant".into());
        }
        if turn.text.chars().count() > MAX_TEXT_CHARS {
            return Err(format!(
                "a history turn is at most {MAX_TEXT_CHARS} characters"
            ));
        }
    }
    if let Some(connectors) = &question.connectors {
        if let Some(unknown) = connectors.iter().find(|c| {
            !deployment.spec.connectors.iter().any(|d| &d.endpoint == *c)
                && deployment.spec.mcp_server.as_ref() != Some(*c)
        }) {
            return Err(format!(
                "connectors: this assistant has no connector `{unknown}`"
            ));
        }
    }
    Ok(())
}

/// A conversation of this deployment younger than a day and started from `origin`, created when
/// the request names none; its id, the tokens it spent and the questions it asked so far. `Ok(None)` when the named one is not
/// this assistant's or was started from another origin (AG-100).
async fn conversation(
    pool: &PgPool,
    deployment: &Deployment,
    named: Option<&str>,
    origin: Option<&str>,
) -> Result<Option<(String, u64, u32)>, crate::Error> {
    let mut tx = crate::project_scope(pool, &deployment.project).await?;
    let found: Option<(String, i64, i32)> = match named {
        Some(id) => {
            if uuid_shaped(id) {
                sqlx::query_as(
                    "SELECT id::text, tokens, questions FROM conversations WHERE id = $1::uuid AND deployment = $2 \
                     AND created_at > now() - make_interval(hours => $3) \
                     AND origin IS NOT DISTINCT FROM $4",
                )
                .bind(id)
                .bind(&deployment.name)
                .bind(CONVERSATION_HOURS as i32)
                .bind(origin)
                .fetch_optional(&mut *tx)
                .await?
            } else {
                None
            }
        }
        None => Some(
            sqlx::query_as("INSERT INTO conversations (project, deployment, origin) VALUES ($1, $2, $3) RETURNING id::text, tokens, questions")
                .bind(&deployment.project)
                .bind(&deployment.name)
                .bind(origin)
                .fetch_one(&mut *tx)
                .await?,
        ),
    };
    tx.commit().await?;
    Ok(found.map(|(id, tokens, questions)| {
        (
            id,
            u64::try_from(tokens).unwrap_or(0),
            u32::try_from(questions).unwrap_or(0),
        )
    }))
}

/// Deletes every conversation past `CONVERSATION_HOURS`, of every project (T-3314, AG-99);
/// returns how many. Run by the minute loop.
pub async fn forget_expired(pool: &PgPool) -> Result<u64, crate::Error> {
    // No WHERE: the `conversations_expired` policy (migration 0006) is the whole selection, and a
    // column named here would need a SELECT policy across projects.
    // Verdicts are kept for 90 days, by their own policy (migration 0008).
    sqlx::query("DELETE FROM feedback").execute(pool).await?;
    // Timings are kept for 30 days, by their own policy (migration 0010).
    sqlx::query("DELETE FROM answer_timings")
        .execute(pool)
        .await?;
    let gone = sqlx::query("DELETE FROM conversations")
        .execute(pool)
        .await?;
    Ok(gone.rows_affected())
}

fn uuid_shaped(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// Counts what a question spent against its conversation and its deployment's day, and keeps
/// how long it took (T-4293).
async fn record(
    pool: &PgPool,
    deployment: &Deployment,
    conversation: &str,
    spent: &Spent,
    total_ms: u64,
) -> Result<(), crate::Error> {
    let mut tx = crate::project_scope(pool, &deployment.project).await?;
    sqlx::query(
        "UPDATE conversations SET tokens = tokens + $2, questions = questions + 1, last_at = now() \
         WHERE id = $1::uuid",
    )
    .bind(conversation)
    .bind(i64::try_from(spent.total).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO usage (project, deployment, day, requests, tokens_in, tokens_out, cost_micros) \
         VALUES ($1, $2, current_date, 1, $3, $4, $5) \
         ON CONFLICT (project, deployment, day) DO UPDATE SET requests = usage.requests + 1, \
         tokens_in = usage.tokens_in + EXCLUDED.tokens_in, tokens_out = usage.tokens_out + EXCLUDED.tokens_out, \
         cost_micros = usage.cost_micros + EXCLUDED.cost_micros",
    )
    .bind(&deployment.project)
    .bind(&deployment.name)
    .bind(i64::try_from(spent.input).unwrap_or(i64::MAX))
    .bind(i64::try_from(spent.output).unwrap_or(i64::MAX))
    .bind(i64::try_from(spent.cost_micros).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    let ms = |ms: Option<u64>| ms.map(|ms| i64::try_from(ms).unwrap_or(i64::MAX));
    let tokens = |tokens: Option<u64>| tokens.map(|t| i32::try_from(t).unwrap_or(i32::MAX));
    sqlx::query(
        "INSERT INTO answer_timings (project, deployment, cached, model_calls, first_token_ms, \
         total_ms, route_ms, search_ms, judge_ms, answer_ms, judge_skipped, dangling_markers, \
         router_provider, judge_provider, answer_provider, route_reasoning_tokens, \
         judge_reasoning_tokens, answer_reasoning_tokens, no_answer) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)",
    )
    .bind(&deployment.project)
    .bind(&deployment.name)
    .bind(spent.cached)
    .bind(i32::try_from(spent.calls).unwrap_or(i32::MAX))
    .bind(ms(spent.first_token_ms))
    .bind(ms(Some(total_ms)))
    .bind(ms(spent.stages.route))
    .bind(ms(spent.stages.search))
    .bind(ms(spent.stages.judge))
    .bind(ms(spent.stages.answer))
    .bind(spent.stages.judge_skipped)
    .bind(i32::try_from(spent.dangling).unwrap_or(i32::MAX))
    .bind(&spent.providers.route)
    .bind(&spent.providers.judge)
    .bind(&spent.providers.answer)
    .bind(tokens(spent.reasoning.route))
    .bind(tokens(spent.reasoning.judge))
    .bind(tokens(spent.reasoning.answer))
    .bind(spent.no_answer)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// What the deployment's model calls cost today (UTC), in millionths of a US dollar (T-4149).
async fn spent_today(pool: &PgPool, deployment: &Deployment) -> Result<u64, crate::Error> {
    let mut tx = crate::project_scope(pool, &deployment.project).await?;
    let cost: Option<i64> = sqlx::query_scalar(
        "SELECT cost_micros FROM usage WHERE project = $1 AND deployment = $2 AND day = current_date",
    )
    .bind(&deployment.project)
    .bind(&deployment.name)
    .fetch_optional(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(cost.map_or(0, |c| u64::try_from(c).unwrap_or(0)))
}

/// Whether today's spend has reached the deployment's `budget.costPerDayUsd`.
pub fn over_cost_cap(spent_micros: u64, cap_usd: f64) -> bool {
    spent_micros as f64 >= cap_usd * 1_000_000.0
}

/// Whether a [`warm_types`] runs, so a slow minute never stacks a second one on it.
static WARMING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Warms routing's type lists for every deployment's connectors that anyone may read, ahead of
/// the first question (T-4611). Called after each read of the manifests; a run still going when
/// the next one is due is left to finish.
pub async fn warm_types(state: Arc<ChatState>) {
    use std::sync::atomic::Ordering;
    if WARMING.swap(true, Ordering::AcqRel) {
        return;
    }
    let snapshot = state
        .snapshot
        .read()
        .map(|snapshot| snapshot.clone())
        .unwrap_or_default();
    let mut connectors = Vec::new();
    for deployment in &snapshot.deployments {
        // No person: only what a visitor may read is listed, with no token.
        let mut failed = Vec::new();
        for connector in connected(&state, deployment, None, None, &mut failed).await {
            if !connectors
                .iter()
                .any(|c: &Connected| c.surface.slug == connector.surface.slug)
            {
                connectors.push(connector);
            }
        }
    }
    routed::warm(&state.answers, &state.http, &connectors).await;
    WARMING.store(false, Ordering::Release);
}

/// The connectors switched on for this question whose Endpoint the asker may reach, with their
/// tools: a public Endpoint without a token; on an `internal` deployment, any other Endpoint
/// with the person's token, when there is one (AG-106, AG-115). A connector whose tools cannot be
/// listed is reported and left out (AG-104).
async fn connected(
    state: &ChatState,
    deployment: &Deployment,
    chosen: Option<&[String]>,
    person: Option<&PersonToken>,
    failed: &mut Vec<Event>,
) -> Vec<Connected> {
    let Ok(snapshot) = state.snapshot.read().map(|s| s.clone()) else {
        return Vec::new();
    };
    let mut connected = Vec::new();
    for connector in &deployment.spec.connectors {
        if chosen.is_some_and(|chosen| !chosen.contains(&connector.endpoint)) {
            continue;
        }
        // `{project}/{endpoint}` reaches a project of `scope.projects` (T-4177); the manifest was
        // validated, and the scope is checked again so no Endpoint outside it is ever reached.
        let (project, name) = match connector_target(&connector.endpoint) {
            (Some(project), name)
                if deployment
                    .spec
                    .scope_projects()
                    .iter()
                    .any(|p| p == project) =>
            {
                (project, name)
            }
            (Some(_), _) => {
                tracing::warn!(deployment = %deployment.name, endpoint = %connector.endpoint, "a connector names a project outside the deployment's scope; not offered");
                continue;
            }
            (None, name) => (deployment.project.as_str(), name),
        };
        let Some(endpoint) = snapshot
            .endpoints
            .get(&(project.to_owned(), name.to_owned()))
        else {
            tracing::warn!(deployment = %deployment.name, endpoint = %connector.endpoint, "a connector names no Endpoint; not offered");
            continue;
        };
        let foreign = project != deployment.project;
        let token = if endpoint.audience == "public" {
            None
        } else if foreign {
            // Another project's Endpoint is read only when it is public, on every channel, and
            // the person's token never goes to another project.
            tracing::warn!(deployment = %deployment.name, endpoint = %connector.endpoint, "another project's Endpoint is not public; not offered");
            continue;
        } else if deployment.spec.channel.is_anonymous() {
            tracing::warn!(deployment = %deployment.name, endpoint = %connector.endpoint, "a public deployment's connector names a non-public Endpoint; not offered");
            continue;
        } else if let Some(person) = person {
            Some(person.clone())
        } else {
            tracing::info!(deployment = %deployment.name, endpoint = %connector.endpoint, "no person's token for a non-public Endpoint; not offered");
            continue;
        };
        let surface = Surface {
            gateway: state.gateway.clone(),
            slug: endpoint.slug.clone(),
            timeout: Duration::from_secs(u64::from(connector.timeout_seconds)),
            token,
        };
        match surface.tools(&state.http, &connector.tools).await {
            Ok(tools) if !tools.is_empty() => connected.push(Connected {
                endpoint: connector.endpoint.clone(),
                surface,
                tools,
            }),
            Ok(_) => {
                tracing::warn!(endpoint = %connector.endpoint, "the Endpoint offers none of the connector's tools")
            }
            // A visitor can do nothing about it, so a public deployment answers without it and
            // says nothing; it is logged once an hour, not on every turn (AG-104, T-4370).
            Err(why) if deployment.spec.channel.is_anonymous() => {
                let key = format!(
                    "{}/{}/{}",
                    deployment.project, deployment.name, connector.endpoint
                );
                if first_this_hour(&FAILED_LISTINGS, &key, std::time::Instant::now()) {
                    tracing::warn!(deployment = %deployment.name, endpoint = %connector.endpoint, %why, "a connector's tools could not be listed; left out, logged once an hour");
                }
            }
            Err(why) => {
                tracing::warn!(endpoint = %connector.endpoint, %why, "a connector's tools could not be listed");
                failed.push(Event::Tool {
                    name: "tools/list".into(),
                    endpoint: Some(connector.endpoint.clone()),
                    status: "failed",
                });
            }
        }
    }
    if let Some(server) = deployment
        .spec
        .mcp_server
        .as_deref()
        .filter(|server| chosen.is_none_or(|chosen| chosen.iter().any(|c| c == server)))
    {
        lent(state, deployment, server, person, &mut connected, failed).await;
    }
    connected
}

/// The Endpoints the deployment's named server lends this asker, each a connector of its own
/// Endpoint with every read tool (MF-56, ADR-N-043 §7.6). The server lists them with the same
/// token rule as a connector: anyone on an anonymous channel, the person on `internal`, whose
/// token then reaches only the non-public Endpoints the server named for them. A server of
/// another project than the deployment's own or the organization's is not offered.
async fn lent(
    state: &ChatState,
    deployment: &Deployment,
    server: &str,
    person: Option<&PersonToken>,
    connected: &mut Vec<Connected>,
    failed: &mut Vec<Event>,
) {
    let own = matches!(
        connector_target(server),
        (Some(project), _) if project == deployment.project || project == jc_core::envelope::ORG_NAMESPACE
    );
    if !own {
        tracing::warn!(deployment = %deployment.name, %server, "the mcpServer is another project's; not offered");
        return;
    }
    let anonymous = deployment.spec.channel.is_anonymous();
    let token = if anonymous { None } else { person.cloned() };
    let listing = Server {
        gateway: state.gateway.clone(),
        server: server.to_owned(),
        timeout: Duration::from_secs(SERVER_SECONDS),
        token: token.clone(),
    };
    let endpoints = match listing.endpoints(&state.http).await {
        Ok(endpoints) => endpoints,
        Err(why) => {
            if !anonymous {
                failed.push(Event::Tool {
                    name: "tools/list".into(),
                    endpoint: Some(server.to_owned()),
                    status: "failed",
                });
            }
            let key = format!("{}/{}/{server}", deployment.project, deployment.name);
            if !anonymous || first_this_hour(&FAILED_LISTINGS, &key, std::time::Instant::now()) {
                tracing::warn!(deployment = %deployment.name, %server, %why, "the mcpServer's Endpoints could not be listed");
            }
            return;
        }
    };
    let tools: Vec<String> = mcp::READ_ONLY_TOOLS
        .iter()
        .map(|t| (*t).to_owned())
        .collect();
    for endpoint in endpoints {
        if connected.iter().any(|c| c.surface.slug == endpoint.slug) {
            continue;
        }
        let token = match (&token, endpoint.public) {
            (_, true) => None,
            (Some(token), false) => Some(token.clone()),
            // No person behind the question: the server listed it for nobody it can read as.
            (None, false) => continue,
        };
        let surface = Surface {
            gateway: state.gateway.clone(),
            slug: endpoint.slug.clone(),
            timeout: Duration::from_secs(SERVER_SECONDS),
            token,
        };
        match surface.tools(&state.http, &tools).await {
            Ok(offered) if !offered.is_empty() => connected.push(Connected {
                endpoint: endpoint.slug,
                surface,
                tools: offered,
            }),
            Ok(_) => {}
            Err(why) => {
                tracing::warn!(%server, endpoint = %endpoint.slug, %why, "a lent Endpoint's tools could not be listed")
            }
        }
    }
}

/// How long a named server's listing and each lent Endpoint's call may take: a connector's
/// default `timeoutSeconds` (MF-52).
const SERVER_SECONDS: u64 = 20;

/// When each public deployment's failing connector was last logged (T-4370); one key per
/// configured connector, so it stays as small as the deployments.
static FAILED_LISTINGS: std::sync::Mutex<Vec<(String, std::time::Instant)>> =
    std::sync::Mutex::new(Vec::new());

/// Whether `key` was not seen in the hour before `now`; records `now` when so.
fn first_this_hour(
    seen: &std::sync::Mutex<Vec<(String, std::time::Instant)>>,
    key: &str,
    now: std::time::Instant,
) -> bool {
    let Ok(mut seen) = seen.lock() else {
        return true;
    };
    let hour = Duration::from_secs(3600);
    match seen.iter_mut().find(|(k, _)| k == key) {
        Some((_, at)) if now.saturating_duration_since(*at) < hour => false,
        Some((_, at)) => {
            *at = now;
            true
        }
        None => {
            seen.push((key.to_owned(), now));
            true
        }
    }
}

/// `at` as RFC 3339 UTC to the second, like `2026-10-16T12:17:05Z`.
fn utc_second(at: time::OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        at.date(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// One event as the stream sends it. An anonymous channel's visitor gets no internals (T-4654):
/// no Endpoint slug on a tool or a table, and a `done` without tokens, calls, providers or cost;
/// those stay in the log, the usage and the timings (`record`).
fn sse(event: &Event, anonymous: bool) -> SseEvent {
    let (name, data) = match event {
        Event::Tool {
            name,
            endpoint,
            status,
        } => {
            let mut data = json!({"name": name, "status": status});
            if let Some(endpoint) = endpoint.as_ref().filter(|_| !anonymous) {
                data["endpoint"] = json!(endpoint);
            }
            ("tool", data)
        }
        Event::Stage { stage, topic } => {
            let mut data = json!({"stage": stage});
            if let Some(topic) = topic {
                data["topic"] = json!(topic);
            }
            ("stage", data)
        }
        Event::Delta(text) => ("delta", json!({"text": text})),
        Event::Data(table) => {
            let mut data = json!(table);
            if anonymous {
                if let Some(table) = data.as_object_mut() {
                    table.remove("endpoint");
                }
            }
            ("data", data)
        }
        Event::Answer(text) => ("answer", json!({"text": text})),
        Event::Citations(citations) => ("citations", json!(citations)),
        Event::Followups(questions) => ("followups", json!({"questions": questions})),
        Event::Error {
            status,
            title,
            detail,
        } => (
            "error",
            json!({"status": status, "title": title, "detail": detail}),
        ),
        Event::Script { code, output } => (
            "script",
            match output {
                Ok(output) => json!({"code": code, "output": output}),
                Err(error) => json!({"code": code, "error": error}),
            },
        ),
        Event::Done(spent, total_ms) => ("done", done(spent, *total_ms, anonymous)),
    };
    SseEvent::default().event(name).data(data.to_string())
}

/// The `done` event: the timings for everyone, and what the answer spent only off an anonymous
/// channel (T-4654).
fn done(spent: &Spent, total_ms: u64, anonymous: bool) -> serde_json::Value {
    if anonymous {
        return json!({
            "firstTokenMs": spent.first_token_ms,
            "totalMs": total_ms,
            "cached": spent.cached,
            "stagesMs": spent.stages.json(),
        });
    }
    let mut done = json!({
        "tokens": spent.total,
        "tokensIn": spent.input,
        "tokensOut": spent.output,
        "modelCalls": spent.calls,
        "firstTokenMs": spent.first_token_ms,
        "totalMs": total_ms,
        "cached": spent.cached,
        "stagesMs": spent.stages.json(),
        "judgeSkipped": spent.stages.judge_skipped,
        "providers": spent.providers.json(),
        "reasoningTokens": spent.reasoning.json(),
    });
    // Present only when every call reported its cost (T-4178): a missing one would make the sum
    // look cheaper than the answer was.
    if let Some(cost) = spent.cost_usd() {
        done["costUsd"] = json!(cost);
    }
    done
}

async fn chat(
    State(state): State<Arc<ChatState>>,
    Path(public_id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<Question>, JsonRejection>,
) -> Response {
    let Some(deployment) = deployment_of(&state, &public_id) else {
        return problem(404, "No assistant is published under this id.");
    };
    let origin = match origin(&headers, &deployment, state.public_origin.as_deref()) {
        Placed::Nowhere => None,
        Placed::Allowed(origin) => Some(origin),
        Placed::Refused => return not_placed_here(),
    };
    let client = client_of(&headers);
    let started_at = origin
        .as_ref()
        .and_then(|o| o.to_str().ok())
        .map(str::to_owned);
    let response = ask(
        &state,
        deployment,
        body,
        &client,
        None,
        started_at.as_deref(),
    )
    .await;
    with_cors(response, origin)
}

async fn feedback(
    State(state): State<Arc<ChatState>>,
    Path(public_id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<Feedback>, JsonRejection>,
) -> Response {
    let Some(deployment) = deployment_of(&state, &public_id) else {
        return problem(404, "No assistant is published under this id.");
    };
    let origin = match origin(&headers, &deployment, state.public_origin.as_deref()) {
        Placed::Nowhere => None,
        Placed::Allowed(origin) => Some(origin),
        Placed::Refused => return not_placed_here(),
    };
    // A deployment with `feedback: false` takes no verdict (MF-52, T-4281).
    let response = if deployment.spec.feedback {
        verdict(&state, &deployment, &headers, origin.as_ref(), body).await
    } else {
        problem(404, "This assistant takes no feedback.")
    };
    with_cors(response, origin)
}

async fn verdict(
    state: &ChatState,
    deployment: &Deployment,
    headers: &HeaderMap,
    origin: Option<&HeaderValue>,
    body: Result<Json<Feedback>, JsonRejection>,
) -> Response {
    let feedback = match body {
        Ok(Json(feedback)) => feedback,
        Err(rejection) => return body_refused(&rejection),
    };
    let note = match check_feedback(&feedback) {
        Ok(note) => note,
        Err(why) => return problem(400, why),
    };
    if !uuid_shaped(&feedback.conversation) {
        return problem(
            400,
            "conversation: not a current conversation of this assistant",
        );
    }
    if let Some(rate) = &deployment.spec.rate_limit {
        let mut limits = state.limits.lock().await;
        let key = format!(
            "{}/{}\u{0}{}\u{0}f",
            deployment.project,
            deployment.name,
            limits.visitor(&client_of(headers), today())
        );
        if let Err(retry) = limits.admit(
            &[(&key, rate.per_client_per_minute, MINUTE)],
            Instant::now(),
        ) {
            return too_many(
                retry,
                "Poslali ste veľa hodnotení za krátky čas. Počkajte chvíľu a skúste to znova.",
            );
        }
    }
    let started_at = origin.and_then(|o| o.to_str().ok());
    match conversation(
        &state.pool,
        deployment,
        Some(&feedback.conversation),
        started_at,
    )
    .await
    {
        Ok(Some((_, _, asked))) if feedback.turn <= asked => {}
        Ok(_) => return problem(
            400,
            "conversation: not a current conversation of this assistant, or no such answer in it",
        ),
        Err(err) => {
            tracing::error!(%err, "the conversation of a verdict could not be read");
            return problem(
                503,
                "The verdict could not be stored. Try again in a minute.",
            );
        }
    }
    let stored = async {
        let mut tx = crate::project_scope(&state.pool, &deployment.project).await?;
        sqlx::query(
            "INSERT INTO feedback (project, deployment, conversation, turn, verdict, note) \
             VALUES ($1, $2, $3::uuid, $4, $5, $6) \
             ON CONFLICT (conversation, turn) DO UPDATE SET verdict = EXCLUDED.verdict, \
             note = EXCLUDED.note, created_at = now()",
        )
        .bind(&deployment.project)
        .bind(&deployment.name)
        .bind(&feedback.conversation)
        .bind(i32::try_from(feedback.turn).unwrap_or(i32::MAX))
        .bind(&feedback.verdict)
        .bind(&note)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, crate::Error>(())
    }
    .await;
    match stored {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            tracing::error!(%err, "a verdict was not stored");
            problem(
                503,
                "The verdict could not be stored. Try again in a minute.",
            )
        }
    }
}

/// The Portal asking for a signed-in person (API/05 §1.7, AG-115): any channel by its name, the
/// person's username for the per-client window and their token for an internal deployment's
/// non-public connectors.
async fn chat_in_portal(
    State(state): State<Arc<ChatState>>,
    Path((project, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<Question>, JsonRejection>,
) -> Response {
    if let Err(refusal) = admin::the_portal(&state, &headers).await {
        return *refusal;
    }
    let Some(person) = headers
        .get("x-jc-person")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.len() <= 256)
        .map(str::to_owned)
    else {
        return problem(400, "X-JC-Person names the person who asks");
    };
    let token = headers
        .get("x-jc-person-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| PersonToken(v.to_owned()));
    let deployment = state.snapshot.read().ok().and_then(|snapshot| {
        snapshot
            .deployments
            .iter()
            .find(|d| d.project == project && d.name == name)
            .cloned()
    });
    let Some(deployment) = deployment else {
        return problem(404, "The project declares no such assistant.");
    };
    let token = token.filter(|_| !deployment.spec.channel.is_anonymous());
    ask(
        &state,
        deployment,
        body,
        &format!("person:{person}"),
        token,
        None,
    )
    .await
}

/// One question to `deployment` from `client`, answered as Server-Sent Events.
/// The visitor as the edge saw them (T-3059): `X-Forwarded-For` read from the right, past the
/// cluster's own proxies (private, loopback and link-local hops), to the first address the edge
/// appended. The leftmost entry is whatever the caller wrote, so a per-client limit keyed on it
/// would be a limit the caller chooses; behind no public hop at all, the first entry is used.
fn client_of(headers: &HeaderMap) -> String {
    let Some(chain) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) else {
        return "unknown".to_owned();
    };
    let hops: Vec<&str> = chain
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect();
    hops.iter()
        .rev()
        .find(|hop| {
            hop.parse::<std::net::IpAddr>()
                .map_or(true, |ip| !internal(ip))
        })
        .or(hops.first())
        .map_or_else(|| "unknown".to_owned(), |hop| (*hop).to_owned())
}

/// An address of the cluster's own network: a proxy hop, never a visitor.
fn internal(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [first, second, ..] = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || (first == 100 && second & 0xc0 == 64)
        }
        std::net::IpAddr::V6(v6) => {
            let head = v6.segments()[0];
            v6.is_loopback() || head & 0xfe00 == 0xfc00 || head & 0xffc0 == 0xfe80
        }
    }
}

async fn ask(
    state: &Arc<ChatState>,
    deployment: Deployment,
    body: Result<Json<Question>, JsonRejection>,
    client: &str,
    person: Option<PersonToken>,
    origin: Option<&str>,
) -> Response {
    let question = match body {
        Ok(Json(question)) => question,
        Err(rejection) => return body_refused(&rejection),
    };
    if !deployment.spec.enabled {
        return problem(
            503,
            "Asistent je teraz vypnutý. Skúste to neskôr alebo navštívte webovú stránku mesta.",
        );
    }
    if let Err(why) = validate(&question, &deployment) {
        return problem(400, why);
    }
    let Some(budget) = deployment.spec.budget.clone() else {
        if deployment.spec.channel.is_anonymous() {
            // jc-core refuses such a manifest; one that slipped through is not served.
            return problem(404, "No assistant is published under this id.");
        }
        return problem(
            409,
            "This assistant has no budget yet: an administrator sets budget.tokensPerDay on it before anyone can ask.",
        );
    };
    let chars = question.message.trim().chars().count();
    if chars > budget.question_chars as usize {
        return problem(
            400,
            format!("message is 1 to {} characters", budget.question_chars),
        );
    }
    let rate = deployment.spec.rate_limit.clone();
    if rate.is_none() && deployment.spec.channel.is_anonymous() {
        return problem(404, "No assistant is published under this id.");
    }
    let key = format!("{}/{}", deployment.project, deployment.name);
    let visitor = format!(
        "{key}\u{0}{}",
        state.limits.lock().await.visitor(client, today())
    );
    let (month, resets) = month_of(time::OffsetDateTime::now_utc());
    // A visitor past their month is answered as search, with links (T-4286): the other windows
    // still count, so the search itself stays paced.
    let mut month_spent = false;
    if let Some(rate) = &rate {
        let (minute, hour, day) = (
            format!("{visitor}\u{0}m"),
            format!("{visitor}\u{0}h"),
            format!("{visitor}\u{0}d"),
        );
        let mut limits = state.limits.lock().await;
        // Keyed by the month, not the day, so the count survives the visitor's day changing.
        let monthly = format!("{key}\u{0}{}\u{0}mo", limits.visitor(client, month));
        let now = Instant::now();
        month_spent = limits.full(&monthly, rate.per_client_per_month, now);
        let mut windows = vec![
            (key.as_str(), rate.requests_per_minute, MINUTE),
            (minute.as_str(), rate.per_client_per_minute, MINUTE),
            (hour.as_str(), rate.per_client_per_hour, HOUR),
            (day.as_str(), rate.per_client_per_day, DAY),
        ];
        if !month_spent {
            windows.push((monthly.as_str(), rate.per_client_per_month, MONTH));
        }
        let admitted = limits.admit(&windows, now);
        drop(limits);
        if let Err(retry) = admitted {
            return too_many(
                retry,
                "Položili ste veľa otázok za krátky čas. Počkajte chvíľu a skúste to znova.",
            );
        }
    }
    let Ok(permit) = Arc::clone(&state.questions).try_acquire_owned() else {
        let mut response = problem(
            503,
            "The assistant is answering as many questions as it can at once. Ask again in a moment.",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        return response;
    };
    let (conversation, spent_before, asked_before) = match conversation(
        &state.pool,
        &deployment,
        question.conversation.as_deref(),
        origin,
    )
    .await
    {
        Ok(Some(found)) => found,
        Ok(None) => return problem(
            400,
            "conversation: not a current conversation of this assistant; send none to start one",
        ),
        Err(err) => {
            tracing::error!(%err, "the conversation could not be read");
            return problem(
                503,
                "The assistant cannot answer right now. Try again in a minute.",
            );
        }
    };

    if asked_before >= budget.messages_per_conversation {
        return too_many(
            1,
            "Tento rozhovor dosiahol najväčší počet otázok. Začnite nový rozhovor.",
        );
    }
    let streams = rate.as_ref().map(|rate| rate.streams_per_client);
    if let Some(limit) = streams {
        if !state.limits.lock().await.open_stream(&visitor, limit) {
            return too_many(
                5,
                "Ešte odpovedám na vaše predchádzajúce otázky. Počkajte, kým dopíšem, a skúste to znova.",
            );
        }
    }
    let day_spent = match spent_today(&state.pool, &deployment).await {
        Ok(spent) => over_cost_cap(spent, budget.cost_per_day_usd),
        Err(err) => {
            // Unknown spend is spend: the chat goes on as search rather than past the cap.
            tracing::error!(%err, "today's cost could not be read; answering as search");
            true
        }
    };
    let search_only = if month_spent {
        Some(routed::Cap::Month { resets })
    } else {
        day_spent.then_some(routed::Cap::Day)
    };
    if day_spent && state.limits.lock().await.first_cap_today(&key, today()) {
        tracing::warn!(
            deployment = %key,
            cap_usd = budget.cost_per_day_usd,
            "@user the assistant reached its daily cost cap; it answers as search until midnight UTC"
        );
    }
    let (tokens_per_day, tokens_per_conversation) =
        (budget.tokens_per_day, budget.tokens_per_conversation);
    let anonymous = deployment.spec.channel.is_anonymous();
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(32);
    // One moment for the question: every model call is told it and the chat shows it (T-4339).
    let asked_at = time::OffsetDateTime::now_utc();
    let task_state = Arc::clone(state);
    let conversation_id = conversation.clone();
    tokio::spawn(async move {
        // Held until the answer is done, whether or not anyone still reads it.
        let _permit = permit;
        let state = task_state;
        let mut failed = Vec::new();
        // `answer.routes` narrows what the answer may use (T-4281): `rag` connects nothing,
        // `data` searches nothing.
        let routes = deployment.spec.routes();
        let connectors = if routes == Routes::Rag {
            Vec::new()
        } else {
            connected(
                &state,
                &deployment,
                question.connectors.as_deref(),
                person.as_ref(),
                &mut failed,
            )
            .await
        };
        let sources = if routes == Routes::Data {
            Vec::new()
        } else {
            crate::sources_by_project(&deployment.project, &deployment.spec.sources)
        };
        // The token reached the surfaces that need it; nothing else holds it past this point.
        drop(person);
        for event in failed {
            let _ = tx.send(event).await;
        }
        let name = format!("{}/{}", deployment.project, deployment.name);
        let ask = Ask {
            pool: &state.pool,
            embedder: &state.embedder,
            model: &state.model,
            http: &state.http,
            project: &deployment.project,
            deployment: &name,
            system_prompt: deployment.spec.system_prompt.as_deref(),
            sources: &sources,
            public_only: deployment.spec.channel.is_anonymous(),
            connectors: &connectors,
            tokens_per_day,
            tokens_per_conversation,
            spent_before,
            functions: state
                .functions
                .as_deref()
                .filter(|_| deployment.spec.sandbox),
            router_model: deployment
                .spec
                .model
                .as_ref()
                .and_then(|m| m.router.as_deref()),
            answer_model: deployment
                .spec
                .model
                .as_ref()
                .and_then(|m| m.answer.as_deref()),
            provider: deployment
                .spec
                .model
                .as_ref()
                .and_then(|m| m.provider.as_ref()),
            reasoning: deployment
                .spec
                .model
                .as_ref()
                .and_then(|m| m.reasoning)
                .unwrap_or_default(),
            output_tokens: u64::from(budget.output_tokens_per_answer),
            cache: &state.answers,
            endpoint_base: state.endpoint_base.as_ref(),
            search_only,
            data_steps: deployment.spec.data_steps() as usize,
            data_time: Duration::from_secs(u64::from(deployment.spec.data_seconds())),
            now: asked_at,
            zone: clock::Zone::of(&deployment.spec.languages),
        };
        let started = Instant::now();
        // A raised data loop keeps its answer call's time after its last tool (T-4294).
        let spent = match tokio::time::timeout(
            ANSWER_TIMEOUT.max(ask.data_time + DATA_ANSWER),
            agent::answer(&ask, &question.history, &question.message, &tx),
        )
        .await
        {
            Ok(spent) => spent,
            Err(_) => {
                let _ = tx
                    .send(Event::Error {
                        status: 504,
                        title: "Answer Took Too Long",
                        detail:
                            "Odpoveď trvala príliš dlho. Skúste otázku položiť kratšie alebo inak."
                                .into(),
                    })
                    .await;
                // ponytail: the calls of a cut answer count no tokens; a usage counter shared
                // with the loop when the cap must hold to the last cent.
                Spent::default()
            }
        };
        let total_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            deployment = %name,
            tokens_in = spent.input,
            tokens_out = spent.output,
            model_calls = spent.calls,
            first_token_ms = spent.first_token_ms,
            total_ms,
            cached = spent.cached,
            "question answered"
        );
        if let Err(err) = record(&state.pool, &deployment, &conversation_id, &spent, total_ms).await
        {
            tracing::error!(%err, "a question's tokens were not recorded");
        }
        if streams.is_some() {
            state.limits.lock().await.close_stream(&visitor);
        }
        let _ = tx.send(Event::Done(spent, total_ms)).await;
    });

    let first = futures_util::stream::once(async move {
        Ok::<_, Infallible>(
            SseEvent::default()
                .event("conversation")
                .data(json!({"id": conversation, "at": utc_second(asked_at)}).to_string()),
        )
    });
    let rest = futures_util::stream::unfold(rx, move |mut rx| async move {
        let event = rx.recv().await?;
        Some((Ok::<_, Infallible>(sse(&event, anonymous)), rx))
    });
    let stream = futures_util::StreamExt::chain(first, rest);
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    // The edge is nginx: without this it holds the events back until the answer is complete.
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_visitor_s_stream_carries_no_slug_and_no_spending_but_a_signed_in_one_does() {
        let spent = Spent {
            total: 50,
            input: 40,
            output: 10,
            calls: 1,
            cost_micros: 12_000,
            ..Spent::default()
        };
        let visitor = done(&spent, 900, true);
        assert_eq!(
            visitor.as_object().map(|o| {
                let mut keys: Vec<String> = o.keys().cloned().collect();
                keys.sort();
                keys
            }),
            Some(
                ["cached", "firstTokenMs", "stagesMs", "totalMs"]
                    .map(String::from)
                    .to_vec()
            ),
            "{visitor}"
        );
        let person = done(&spent, 900, false);
        assert_eq!(
            (
                &person["tokensIn"],
                &person["modelCalls"],
                &person["costUsd"]
            ),
            (&json!(40), &json!(1), &json!(0.012))
        );
        assert!(person["providers"].is_object() && person["reasoningTokens"].is_object());
        // T-4178: a call whose provider reported no cost leaves the sum out, never a floor.
        let uncosted = Spent {
            uncosted: 1,
            ..spent
        };
        assert!(done(&uncosted, 900, false).get("costUsd").is_none());

        let tool = Event::Tool {
            name: "query_entities".into(),
            endpoint: Some("ovzdusie-sk".into()),
            status: "started",
        };
        let table = Event::Data(agent::Table {
            endpoint: "ovzdusie-sk".into(),
            entity_type: "AirQualityObserved".into(),
            columns: vec!["name".into()],
            rows: vec![vec![json!("Žilina")]],
            total: None,
            dataset: Some("https://data.dev.example/dataset/ovzdusie".into()),
            download: None,
        });
        for event in [&tool, &table] {
            let shown = |anonymous| format!("{:?}", sse(event, anonymous));
            assert!(!shown(true).contains("ovzdusie-sk"), "{}", shown(true));
            assert!(shown(false).contains("ovzdusie-sk"), "{}", shown(false));
        }
        assert!(
            format!("{:?}", sse(&table, true)).contains("ovzdusie"),
            "the dataset link stays"
        );
    }

    /// T-3059: the per-client key is the address the edge appended, past the cluster's own
    /// proxies, never one the caller wrote in front of it.
    /// T-4370: a failing connector of a public deployment is logged once an hour per key.
    #[test]
    fn a_failing_listing_is_logged_once_an_hour() {
        let seen = std::sync::Mutex::new(Vec::new());
        let at = std::time::Instant::now();
        assert!(first_this_hour(&seen, "bbsk/data/od", at));
        assert!(!first_this_hour(
            &seen,
            "bbsk/data/od",
            at + Duration::from_secs(3599)
        ));
        assert!(
            first_this_hour(&seen, "bbsk/zilina/od", at),
            "another key is its own"
        );
        assert!(first_this_hour(
            &seen,
            "bbsk/data/od",
            at + Duration::from_secs(3600)
        ));
        assert!(!first_this_hour(
            &seen,
            "bbsk/data/od",
            at + Duration::from_secs(3700)
        ));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_client_is_the_address_the_edge_appended() {
        let key = |chain: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", HeaderValue::from_static(chain));
            client_of(&headers)
        };
        assert_eq!(client_of(&HeaderMap::new()), "unknown");
        assert_eq!(key("198.51.100.9"), "198.51.100.9");
        // The edge appended the visitor, the ingress appended itself.
        assert_eq!(key("198.51.100.9, 10.0.0.1"), "198.51.100.9");
        // A caller who writes an address in front changes nothing.
        assert_eq!(
            key("6.6.6.6, 198.51.100.9, 10.42.0.7, 127.0.0.1"),
            "198.51.100.9"
        );
        assert_eq!(key("2001:db8::1, fd00::1, fe80::1"), "2001:db8::1");
        // Behind no public hop at all, the first address there is.
        assert_eq!(key("10.0.0.5"), "10.0.0.5");
        assert_eq!(key(" , "), "unknown");
    }

    /// T-3243: a body the chat cannot read is refused in words, naming the member at fault and
    /// never a Rust type or the deserializer's sentence.
    #[tokio::test]
    async fn a_body_that_cannot_be_read_is_refused_in_words() {
        use tower::ServiceExt;
        let app = Router::new().route(
            "/",
            post(|body: Result<Json<Question>, JsonRejection>| async move {
                match body {
                    Ok(_) => StatusCode::OK.into_response(),
                    Err(rejection) => body_refused(&rejection),
                }
            }),
        );
        for (content_type, body, status, said, field) in [
            (
                "text/plain",
                r#"{"message":"a"}"#,
                415,
                "application/json",
                None,
            ),
            ("application/json", "{not json", 400, "not valid JSON", None),
            (
                "application/json",
                r#"{"message":7}"#,
                400,
                "`message`",
                Some("message"),
            ),
            (
                "application/json",
                r#"{"message":"a","history":[{"role":"user","text":1}]}"#,
                400,
                "`history[0].text`",
                Some("history[0].text"),
            ),
            ("application/json", "{}", 400, "documented", None),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::post("/")
                        .header("content-type", content_type)
                        .body(axum::body::Body::from(body))
                        .expect("a request"),
                )
                .await
                .expect("an answer");
            assert_eq!(response.status().as_u16(), status, "{body}");
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("a body");
            let problem: serde_json::Value = serde_json::from_slice(&bytes).expect("a problem");
            let detail = problem["detail"].as_str().unwrap_or_default();
            assert!(detail.contains(said), "{body}: {problem}");
            for raw in [
                "invalid type",
                "expected",
                "Question",
                "Turn",
                "Failed to deserialize",
            ] {
                assert!(!detail.contains(raw), "{body}: {problem}");
            }
            assert_eq!(problem["field"].as_str(), field, "{body}: {problem}");
        }
    }

    fn one(key: &str, limit: u32) -> [(&str, u32, Duration); 1] {
        [(key, limit, MINUTE)]
    }

    #[test]
    fn a_window_admits_its_limit_then_says_when_to_come_back() {
        let mut limits = Limits::default();
        let start = Instant::now();
        assert!(limits.admit(&one("a", 2), start).is_ok());
        assert!(limits.admit(&one("a", 2), start).is_ok());
        let retry = limits
            .admit(&one("a", 2), start + Duration::from_secs(20))
            .expect_err("full");
        assert_eq!(retry, 40);
        assert!(
            limits.admit(&one("b", 2), start).is_ok(),
            "another key has its own window"
        );
        // The window's last second still refuses; its end admits again.
        assert_eq!(
            limits.admit(&one("a", 2), start + Duration::from_millis(59_999)),
            Err(1)
        );
        assert!(limits
            .admit(&one("a", 2), start + Duration::from_secs(60))
            .is_ok());
    }

    /// T-4286: 300 questions a month per visitor. The 300th is counted, the 301st finds the window
    /// full for that visitor alone, and the next month is a new key with a fresh count.
    #[test]
    fn a_visitor_s_month_holds_300_questions_and_a_new_month_starts_over() {
        let mut limits = Limits::default();
        let start = Instant::now();
        let october = format!("d\u{0}{}\u{0}mo", limits.visitor("10.0.0.1", 24_321));
        for asked in 0..300 {
            assert!(!limits.full(&october, 300, start), "question {}", asked + 1);
            assert!(limits.admit(&[(&october, 300, MONTH)], start).is_ok());
        }
        // Weeks later, still October: the 301st finds the month full.
        let later = start + Duration::from_secs(20 * 86_400);
        assert!(limits.full(&october, 300, later));
        // Another visitor of the same deployment is served.
        let other = format!("d\u{0}{}\u{0}mo", limits.visitor("10.0.0.2", 24_321));
        assert!(!limits.full(&other, 300, later));
        // November is another key: the visitor starts over.
        let november = format!("d\u{0}{}\u{0}mo", limits.visitor("10.0.0.1", 24_322));
        assert_ne!(november, october);
        assert!(!limits.full(&november, 300, later));
        // `full` counts nothing.
        assert!(!limits.full("never-seen", 1, later));
        assert!(!limits.full("never-seen", 1, later));
    }

    /// T-4286: no cap refuses a visitor, so the sentence they read at 08:25Z is nowhere left.
    #[test]
    fn no_sentence_says_the_day_s_answers_are_used_up() {
        for (file, text) in [
            ("model.rs", include_str!("model.rs")),
            ("routed.rs", include_str!("routed.rs")),
            ("agent.rs", include_str!("agent.rs")),
            ("widget.js", include_str!("../../widget/widget.js")),
            ("render.js", include_str!("../../widget/render.js")),
        ] {
            for refusal in [
                "used up today's answers",
                "odpovede tohto asistenta sú vyčerpané",
                "odpovědi tohoto asistenta jsou vyčerpané",
            ] {
                assert!(!text.contains(refusal), "{file} says {refusal:?}");
            }
        }
    }

    #[test]
    fn the_month_resets_on_the_first_of_the_next_one() {
        use time::macros::datetime;
        let (october, resets) = month_of(datetime!(2026-10-16 08:25 UTC));
        assert_eq!(resets, "1. 11. 2026");
        let (december, resets) = month_of(datetime!(2026-12-31 23:59 UTC));
        assert_eq!(resets, "1. 1. 2027");
        let (january, _) = month_of(datetime!(2027-01-01 00:00 UTC));
        assert_eq!((december - october, january - december), (2, 1));
    }

    /// T-4149: the owner's defaults, 10 a minute, 60 an hour, 200 a day: the 11th question of a
    /// minute waits for the minute, the 61st of an hour for the hour, the 201st of a day for the day.
    #[test]
    fn a_visitor_is_held_to_the_minute_the_hour_and_the_day() {
        let mut limits = Limits::default();
        let start = Instant::now();
        let windows = [
            ("v\u{0}m", 10, MINUTE),
            ("v\u{0}h", 60, HOUR),
            ("v\u{0}d", 200, DAY),
        ];
        for _ in 0..10 {
            assert!(limits.admit(&windows, start).is_ok());
        }
        assert_eq!(limits.admit(&windows, start), Err(60));
        // Six minutes of ten fill the hour.
        let mut at = start;
        for _ in 0..5 {
            at += MINUTE;
            for _ in 0..10 {
                assert!(limits.admit(&windows, at).is_ok());
            }
        }
        at += MINUTE;
        let wait = limits.admit(&windows, at).expect_err("the hour is full");
        assert_eq!(wait, HOUR.as_secs() - 6 * 60);
        // The next hours fill the day: 200 in all.
        let mut admitted = 60;
        while admitted < 200 {
            at += HOUR;
            for _ in 0..10 {
                if admitted < 200 {
                    assert!(limits.admit(&windows, at).is_ok(), "{admitted}");
                    admitted += 1;
                }
            }
        }
        at += HOUR;
        let wait = limits.admit(&windows, at).expect_err("the day is full");
        assert!(wait > HOUR.as_secs(), "waits for the day, {wait}");
        assert!(limits.admit(&windows, start + DAY).is_ok());
    }

    /// T-4149: a request one window refuses counts in none, so a visitor at the deployment-wide
    /// limit does not spend their own minute.
    #[test]
    fn a_refused_request_spends_no_place_in_any_window() {
        let mut limits = Limits::default();
        let start = Instant::now();
        assert!(limits.admit(&[("all", 1, MINUTE)], start).is_ok());
        for _ in 0..3 {
            assert!(limits
                .admit(&[("all", 1, MINUTE), ("me", 1, MINUTE)], start)
                .is_err());
        }
        assert!(
            limits.admit(&[("me", 1, MINUTE)], start).is_ok(),
            "my own minute is untouched"
        );
    }

    /// T-3314: within one minute, a flood of new keys stops at [`MAX_KEYS`]; the keys already
    /// counted keep their windows, and an expired window makes room again.
    #[test]
    fn the_table_of_keys_stays_bounded() {
        let mut limits = Limits::default();
        let start = Instant::now();
        for n in 0..MAX_KEYS {
            assert!(limits.admit(&one(&n.to_string(), 2), start).is_ok());
        }
        assert_eq!(limits.admit(&one("one more", 2), start), Err(60));
        assert_eq!(limits.windows.len(), MAX_KEYS);
        assert!(
            limits.admit(&one("7", 2), start).is_ok(),
            "a counted key still counts"
        );
        assert!(
            limits.admit(&one("7", 2), start).is_err(),
            "and keeps its limit"
        );
        assert!(limits
            .admit(&one("one more", 2), start + Duration::from_secs(60))
            .is_ok());
        assert_eq!(limits.windows.len(), 1);
    }

    /// T-4149: at most two answers stream to one visitor at once; one that ends frees its place,
    /// and an empty count leaves the table.
    #[test]
    fn a_visitor_streams_at_most_their_limit_at_once() {
        let mut limits = Limits::default();
        assert!(limits.open_stream("v", 2));
        assert!(limits.open_stream("v", 2));
        assert!(!limits.open_stream("v", 2), "a third waits");
        assert!(limits.open_stream("w", 2), "another visitor has their own");
        limits.close_stream("v");
        assert!(limits.open_stream("v", 2));
        for _ in 0..3 {
            limits.close_stream("v");
        }
        limits.close_stream("w");
        assert!(limits.streams.is_empty());
    }

    /// T-4149: the limits know a visitor by a hash that changes with the day and holds no part of
    /// the address; a caller's own `X-Forwarded-For` entry changes neither (T-3059).
    #[test]
    fn a_visitor_is_a_daily_hash_of_the_edge_s_address() {
        let limits = Limits::default();
        let key = |chain: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", HeaderValue::from_static(chain));
            limits.visitor(&client_of(&headers), 20_000)
        };
        let visitor = key("198.51.100.9, 10.0.0.1");
        assert_eq!(visitor, key("6.6.6.6, 198.51.100.9, 10.0.0.1"));
        assert_ne!(visitor, key("198.51.100.10, 10.0.0.1"));
        assert!(
            !visitor.contains("198") && !visitor.contains('.'),
            "{visitor}"
        );
        assert_ne!(
            visitor,
            limits.visitor("198.51.100.9", 20_001),
            "rotated daily"
        );
        assert_ne!(
            visitor,
            Limits::default().visitor("198.51.100.9", 20_000),
            "salted per process"
        );
    }

    /// T-4174: a verdict is up or down on a turn 1 to 200; only a thumbs down carries a note, of
    /// at most 500 characters and never one with a secret's shape.
    #[test]
    fn a_verdict_and_its_note_are_checked_before_anything_is_looked_up() {
        let verdict = |verdict: &str, turn: u32, note: Option<&str>| {
            check_feedback(&Feedback {
                conversation: "6f1c0e9e-3b1a-4d7e-9b51-2c4f8f2a7c11".into(),
                turn,
                verdict: verdict.into(),
                note: note.map(str::to_owned),
            })
        };
        assert_eq!(verdict("up", 1, None), Ok(None));
        assert_eq!(verdict("up", 1, Some("  ")), Ok(None));
        assert_eq!(
            verdict("down", 200, Some("  Chýbali hodiny. ")),
            Ok(Some("Chýbali hodiny.".into()))
        );
        assert_eq!(
            verdict("down", 1, Some(&"č".repeat(MAX_NOTE_CHARS))),
            Ok(Some("č".repeat(MAX_NOTE_CHARS)))
        );
        for (v, turn, note) in [
            ("meh", 1, None),
            ("UP", 1, None),
            ("up", 0, None),
            ("down", 201, None),
            ("up", 1, Some("pekné")),
        ] {
            assert!(verdict(v, turn, note).is_err(), "{v} {turn} {note:?}");
        }
        assert!(verdict("down", 1, Some(&"č".repeat(MAX_NOTE_CHARS + 1))).is_err());
        for secret in [
            format!("môj token ghp_{}", "abc123def456ghi789jkl012"),
            format!("kľúč AKIA{}", "IOSFODNN7EXAMPLE"),
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl".to_owned(),
        ] {
            let refused = verdict("down", 1, Some(&secret)).expect_err("a secret");
            assert!(refused.contains("heslo"), "{refused}");
        }
    }

    /// T-4149: the cap holds from the cent it is reached, and is reported once a day.
    #[test]
    fn the_cost_cap_holds_from_the_cent_it_is_reached_and_is_reported_once_a_day() {
        assert!(!over_cost_cap(0, 3.0));
        assert!(!over_cost_cap(2_999_999, 3.0));
        assert!(over_cost_cap(3_000_000, 3.0));
        assert!(over_cost_cap(9_000_000, 3.0));
        assert!(over_cost_cap(10_000, 0.01));
        let mut limits = Limits::default();
        assert!(limits.first_cap_today("bb/verejny", 20_000));
        assert!(!limits.first_cap_today("bb/verejny", 20_000));
        assert!(
            limits.first_cap_today("bbsk/verejny", 20_000),
            "per deployment"
        );
        assert!(
            limits.first_cap_today("bb/verejny", 20_001),
            "again the next day"
        );
    }

    #[test]
    fn a_conversation_id_is_uuid_shaped_or_not_looked_up() {
        assert!(uuid_shaped("6f1c0e9e-3b1a-4d7e-9b51-2c4f8f2a7c11"));
        for bad in [
            "",
            "6f1c0e9e3b1a4d7e9b512c4f8f2a7c11",
            "6f1c0e9e-3b1a-4d7e-9b51-2c4f8f2a7c1z",
            "' OR 1=1 --",
        ] {
            assert!(!uuid_shaped(bad), "{bad}");
        }
    }
}
