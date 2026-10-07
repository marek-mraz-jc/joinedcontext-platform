//! The chat route of `jc-assistant` (API/05, AG-98…AG-108, T-3055):
//! `POST /api/v1/d/{publicId}/chat`, one question per request, answered as Server-Sent Events.
//!
//! This route serves the anonymous channels (`public`, `ckan`, `iframe`); an `internal`
//! deployment answers `404` here and is asked through the Portal on
//! `/internal/v1/projects/{project}/knowledge/deployments/{deployment}/chat` (API/05 §1.7,
//! AG-115).

pub mod admin;
pub mod agent;
pub mod mcp;
pub mod model;
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
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;

use self::agent::{Ask, Connected, Event, Spent, Turn};
use self::mcp::{PersonToken, Surface};
use self::model::Model;
use crate::embed::Embedder;
use crate::worker::{Deployment, Snapshot};

/// The largest request body (AG-98).
pub const MAX_BODY: usize = 64 * 1024;
/// The longest question and the longest turn of `history`, in characters (AG-98).
pub const MAX_TEXT_CHARS: usize = 4_000;
/// The most turns of `history` (AG-98).
pub const MAX_TURNS: usize = 6;
/// How long a conversation may be continued.
const CONVERSATION_HOURS: i64 = 24;

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
}

/// Requests per minute, per key, in fixed one-minute windows (AG-101).
// ponytail: in memory, one replica; a second replica doubles every limit. Move to the database
// when the assistant runs on more than one.
#[derive(Default)]
pub struct Limits {
    windows: HashMap<String, (Instant, u32)>,
}

impl Limits {
    /// Counts one request under `key`; `Err(seconds)` until the window frees a place.
    pub fn admit(&mut self, key: &str, limit: u32, now: Instant) -> Result<(), u64> {
        if self.windows.len() > 50_000 {
            self.windows
                .retain(|_, (started, _)| now.duration_since(*started) < Duration::from_secs(60));
        }
        let (started, count) = self.windows.entry(key.to_owned()).or_insert((now, 0));
        if now.duration_since(*started) >= Duration::from_secs(60) {
            *started = now;
            *count = 0;
        }
        if *count >= limit {
            let left = 60u64
                .saturating_sub(now.duration_since(*started).as_secs())
                .max(1);
            return Err(left);
        }
        *count += 1;
        Ok(())
    }
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

/// The chat route with its body limit, and the Portal's administration paths (admin.rs).
pub fn router(state: Arc<ChatState>) -> Router {
    Router::new()
        .route("/api/v1/d/{public_id}/chat", post(chat).options(preflight))
        .route(
            "/internal/v1/projects/{project}/knowledge/deployments/{deployment}/chat",
            post(chat_in_portal),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(Arc::clone(&state))
        .merge(widget::router(Arc::clone(&state)))
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
        if let Some(unknown) = connectors
            .iter()
            .find(|c| !deployment.spec.connectors.iter().any(|d| &d.endpoint == *c))
        {
            return Err(format!(
                "connectors: this assistant has no connector `{unknown}`"
            ));
        }
    }
    Ok(())
}

/// A conversation of this deployment younger than a day, created when the request names none;
/// its id and what it spent so far. `Ok(None)` when the named one is not this assistant's.
async fn conversation(
    pool: &PgPool,
    deployment: &Deployment,
    named: Option<&str>,
) -> Result<Option<(String, u64)>, crate::Error> {
    let mut tx = crate::project_scope(pool, &deployment.project).await?;
    let found: Option<(String, i64)> = match named {
        Some(id) => {
            if uuid_shaped(id) {
                sqlx::query_as(
                    "SELECT id::text, tokens FROM conversations WHERE id = $1::uuid AND deployment = $2 \
                     AND created_at > now() - make_interval(hours => $3)",
                )
                .bind(id)
                .bind(&deployment.name)
                .bind(CONVERSATION_HOURS as i32)
                .fetch_optional(&mut *tx)
                .await?
            } else {
                None
            }
        }
        None => Some(
            sqlx::query_as("INSERT INTO conversations (project, deployment) VALUES ($1, $2) RETURNING id::text, tokens")
                .bind(&deployment.project)
                .bind(&deployment.name)
                .fetch_one(&mut *tx)
                .await?,
        ),
    };
    tx.commit().await?;
    Ok(found.map(|(id, tokens)| (id, u64::try_from(tokens).unwrap_or(0))))
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

/// Counts what a question spent against its conversation and its deployment's day.
async fn record(
    pool: &PgPool,
    deployment: &Deployment,
    conversation: &str,
    spent: Spent,
) -> Result<(), crate::Error> {
    let mut tx = crate::project_scope(pool, &deployment.project).await?;
    sqlx::query(
        "UPDATE conversations SET tokens = tokens + $2, last_at = now() WHERE id = $1::uuid",
    )
    .bind(conversation)
    .bind(i64::try_from(spent.total).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO usage (project, deployment, day, requests, tokens_in, tokens_out) \
         VALUES ($1, $2, current_date, 1, $3, $4) \
         ON CONFLICT (project, deployment, day) DO UPDATE SET requests = usage.requests + 1, \
         tokens_in = usage.tokens_in + EXCLUDED.tokens_in, tokens_out = usage.tokens_out + EXCLUDED.tokens_out",
    )
    .bind(&deployment.project)
    .bind(&deployment.name)
    .bind(i64::try_from(spent.input).unwrap_or(i64::MAX))
    .bind(i64::try_from(spent.output).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
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
        let Some(endpoint) = snapshot
            .endpoints
            .get(&(deployment.project.clone(), connector.endpoint.clone()))
        else {
            tracing::warn!(deployment = %deployment.name, endpoint = %connector.endpoint, "a connector names no Endpoint of the project; not offered");
            continue;
        };
        let token = if endpoint.audience == "public" {
            None
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
    connected
}

fn sse(event: &Event) -> SseEvent {
    let (name, data) = match event {
        Event::Tool {
            name,
            endpoint,
            status,
        } => {
            let mut data = json!({"name": name, "status": status});
            if let Some(endpoint) = endpoint {
                data["endpoint"] = json!(endpoint);
            }
            ("tool", data)
        }
        Event::Answer(text) => ("answer", json!({"text": text})),
        Event::Citations(citations) => ("citations", json!(citations)),
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
        Event::Done(tokens) => ("done", json!({"tokens": tokens})),
    };
    SseEvent::default().event(name).data(data.to_string())
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
    let client = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("unknown")
        .to_owned();
    let response = ask(&state, deployment, body, &client, None).await;
    with_cors(response, origin)
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
    ask(&state, deployment, body, &format!("person:{person}"), token).await
}

/// One question to `deployment` from `client`, answered as Server-Sent Events.
async fn ask(
    state: &Arc<ChatState>,
    deployment: Deployment,
    body: Result<Json<Question>, JsonRejection>,
    client: &str,
    person: Option<PersonToken>,
) -> Response {
    let question = match body {
        Ok(Json(question)) => question,
        Err(rejection) => return body_refused(&rejection),
    };
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
    let rate = deployment.spec.rate_limit.clone();
    if rate.is_none() && deployment.spec.channel.is_anonymous() {
        return problem(404, "No assistant is published under this id.");
    }
    if let Some(rate) = rate {
        let now = Instant::now();
        let mut limits = state.limits.lock().await;
        let key = format!("{}/{}", deployment.project, deployment.name);
        let admitted = limits
            .admit(&key, rate.requests_per_minute, now)
            .and_then(|()| {
                limits.admit(
                    &format!("{key}\u{0}{client}"),
                    rate.per_client_per_minute,
                    now,
                )
            });
        if let Err(retry) = admitted {
            let mut response = problem(
                429,
                "The assistant is answering many questions. Ask again in a moment.",
            );
            if let Ok(value) = HeaderValue::from_str(&retry.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            return response;
        }
    }
    let (conversation, spent_before) = match conversation(
        &state.pool,
        &deployment,
        question.conversation.as_deref(),
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

    let (tokens_per_day, tokens_per_conversation) =
        (budget.tokens_per_day, budget.tokens_per_conversation);
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(32);
    let task_state = Arc::clone(state);
    let conversation_id = conversation.clone();
    tokio::spawn(async move {
        let state = task_state;
        let mut failed = Vec::new();
        let connectors = connected(
            &state,
            &deployment,
            question.connectors.as_deref(),
            person.as_ref(),
            &mut failed,
        )
        .await;
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
            sources: &deployment.spec.sources,
            public_only: deployment.spec.channel.is_anonymous(),
            connectors: &connectors,
            tokens_per_day,
            tokens_per_conversation,
            spent_before,
            functions: state
                .functions
                .as_deref()
                .filter(|_| deployment.spec.sandbox),
        };
        let spent = agent::answer(&ask, &question.history, &question.message, &tx).await;
        if let Err(err) = record(&state.pool, &deployment, &conversation_id, spent).await {
            tracing::error!(%err, "a question's tokens were not recorded");
        }
        let _ = tx.send(Event::Done(spent.total)).await;
    });

    let first = futures_util::stream::once(async move {
        Ok::<_, Infallible>(
            SseEvent::default()
                .event("conversation")
                .data(json!({"id": conversation}).to_string()),
        )
    });
    let rest = futures_util::stream::unfold(rx, |mut rx| async move {
        let event = rx.recv().await?;
        Some((Ok::<_, Infallible>(sse(&event)), rx))
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

    #[test]
    fn a_window_admits_its_limit_then_says_when_to_come_back() {
        let mut limits = Limits::default();
        let start = Instant::now();
        assert!(limits.admit("a", 2, start).is_ok());
        assert!(limits.admit("a", 2, start).is_ok());
        let retry = limits
            .admit("a", 2, start + Duration::from_secs(20))
            .expect_err("full");
        assert_eq!(retry, 40);
        assert!(
            limits.admit("b", 2, start).is_ok(),
            "another key has its own window"
        );
        assert!(limits
            .admit("a", 2, start + Duration::from_secs(61))
            .is_ok());
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
