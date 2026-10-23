//! Who called which MCP tool, on the project's Activity (T-5322, Deployment/05 section 5).
//!
//! Every `tools/call` an Endpoint answers, through its own URL, the hub or a named server, is
//! one `mcp.tool` event of the Endpoint's project and space: "user:ana called query_entities on
//! air-quality". The gateway posts the events as OTLP/HTTP JSON log records to the collector
//! (`JC_GATEWAY_ACTIVITY_URL`), which keeps the allow-listed attributes and hands them to the
//! Portal's `POST /api/v1/activity`.
//!
//! What is recorded is the caller as the audit line names it, the tool, the Endpoint's slug and
//! the outcome. A tool's arguments and answer never reach this module, so no query text, entity
//! id, value or token can leave through it. Calls are added up per caller, tool, Endpoint and
//! outcome: a signed-in caller's every [`INTERVAL`], an anonymous caller's once a minute, so a
//! public server answering thousands of calls writes a handful of rows. A post the collector
//! does not take is dropped: the Activity is a projection, the log lines keep the calls.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::auth::subject::principal_of;
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;

/// How often the signed-in callers' calls are posted.
pub const INTERVAL: Duration = Duration::from_secs(15);
/// Anonymous calls are posted every this many [`INTERVAL`]s: once a minute.
pub const ANONYMOUS_EVERY: u32 = 4;
/// Distinct rows one replica holds between two posts; a call past it is not recorded, which
/// bounds memory whatever the callers send.
const MAX_KEYS: usize = 10_000;
/// The caller of a call without a token, as [`principal_of`] names it.
const ANONYMOUS: &str = "anonymous";

/// How a call ended, as the Activity's `details.outcome` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Outcome {
    /// The tool answered.
    Answered,
    /// The tool ran and answered `isError` (a refused filter, a write the model refused).
    ToolError,
    /// The call was refused before the tool ran: an ungranted tool, an anonymous write.
    Refused,
    /// The broker did not answer.
    Failed,
}

impl Outcome {
    /// The name the Activity shows.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::ToolError => "tool-error",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }

    /// The outcome a JSON-RPC answer to `tools/call` reports.
    pub fn of(answer: &Value) -> Self {
        if answer.get("error").is_some() {
            Self::Refused
        } else if answer["result"]["isError"] == Value::Bool(true) {
            Self::ToolError
        } else {
            Self::Answered
        }
    }
}

/// One row of the Activity: the calls one caller made of one tool on one Endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Call {
    /// The Endpoint's project, whose Activity shows the row.
    pub project: String,
    /// The space's own name, as the Portal's bindings name it.
    pub space: String,
    /// The Endpoint's slug.
    pub endpoint: String,
    /// The caller as the audit line names it: `user:ana`, `serviceAccount:x`, `anonymous`.
    pub caller: String,
    /// The tool's name, or `unknown`.
    pub tool: String,
    /// How the calls ended.
    pub outcome: Outcome,
}

/// The calls of one [`Call`] since the last post, and when the first of them was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    /// The calls since the last post.
    pub count: u64,
    /// When the first of them was made: the event's time.
    pub first: SystemTime,
}

type Tally = HashMap<Call, Held>;

fn tally() -> &'static Mutex<Tally> {
    static TALLY: OnceLock<Mutex<Tally>> = OnceLock::new();
    TALLY.get_or_init(Mutex::default)
}

fn spaces() -> &'static RwLock<BTreeMap<String, String>> {
    static SPACES: OnceLock<RwLock<BTreeMap<String, String>>> = OnceLock::new();
    SPACES.get_or_init(RwLock::default)
}

/// The spaces' names by tenant segment, replaced on every reload of the repository.
pub fn name_spaces(names: &BTreeMap<String, String>) {
    *spaces().write().unwrap_or_else(|e| e.into_inner()) = names.clone();
}

/// A tool name as the caller sent it, when it is one a tool could have; anything else (a
/// sentence, a thousand characters) is `unknown`, so a caller cannot write text into the
/// Activity or a row per invented name.
fn tool_name(tool: &str) -> &str {
    let named = (1..=64).contains(&tool.len())
        && tool
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if named {
        tool
    } else {
        "unknown"
    }
}

/// Records one `tools/call` on `endpoint`. A space the last reload did not name is not
/// recorded: an event without a space reaches every reader of the project, also one whose
/// bindings stop at another space.
pub fn record(endpoint: &Endpoint, subject: &Subject, tool: &str, outcome: Outcome) {
    let caller = principal_of(subject);
    note(
        &endpoint.project,
        &endpoint.space,
        &endpoint.slug,
        caller,
        tool,
        outcome,
    );
}

/// [`record`] of a call on the space of tenant `segment`.
fn note(
    project: &str,
    segment: &str,
    endpoint: &str,
    caller: String,
    tool: &str,
    outcome: Outcome,
) {
    let names = spaces().read().unwrap_or_else(|e| e.into_inner());
    let Some(space) = names.get(segment).cloned() else {
        return;
    };
    drop(names);
    let call = Call {
        project: project.to_owned(),
        space,
        endpoint: endpoint.to_owned(),
        caller,
        tool: tool_name(tool).to_owned(),
        outcome,
    };
    let mut tally = tally().lock().unwrap_or_else(|e| e.into_inner());
    add(&mut tally, call, SystemTime::now());
}

fn add(tally: &mut Tally, call: Call, now: SystemTime) {
    if let Some(held) = tally.get_mut(&call) {
        held.count = held.count.saturating_add(1);
    } else if tally.len() < MAX_KEYS {
        tally.insert(
            call,
            Held {
                count: 1,
                first: now,
            },
        );
    }
}

/// The rows to post now, sorted, taken out of the tally: the signed-in callers' always, the
/// anonymous ones only when `anonymous_too`.
pub fn take(anonymous_too: bool) -> Vec<(Call, Held)> {
    let mut tally = tally().lock().unwrap_or_else(|e| e.into_inner());
    let mut taken: Vec<(Call, Held)> = Vec::new();
    tally.retain(|call, held| {
        let due = anonymous_too || call.caller != ANONYMOUS;
        if due {
            taken.push((call.clone(), *held));
        }
        !due
    });
    taken.sort_by(|a, b| a.0.cmp(&b.0));
    taken
}

fn attribute(key: &str, value: Value) -> Value {
    json!({ "key": key, "value": value })
}

fn text(key: &str, value: &str) -> Value {
    attribute(key, json!({ "stringValue": value }))
}

/// One OTLP log record per row, in the vocabulary the Portal's ingest reads.
pub fn log_record(call: &Call, held: &Held) -> Value {
    let summary = match held.count {
        1 => format!("{} called {} on {}", call.caller, call.tool, call.endpoint),
        n => format!(
            "{} called {} on {} {n} times",
            call.caller, call.tool, call.endpoint
        ),
    };
    let nanos = held
        .first
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    let (severity, number) = match call.outcome {
        Outcome::Failed => ("warning", 13),
        _ => ("info", 9),
    };
    json!({
        "timeUnixNano": nanos.to_string(),
        "severityNumber": number,
        "severityText": severity.to_uppercase(),
        "body": { "stringValue": summary },
        "attributes": [
            text("project", &call.project),
            text("space", &call.space),
            text("kind", "mcp.tool"),
            text("source", "gateway"),
            text("severity", severity),
            text("summary", &summary),
            text("endpoint", &call.endpoint),
            text("tool", &call.tool),
            text("outcome", call.outcome.as_str()),
            attribute("count", json!({ "intValue": held.count.to_string() })),
        ],
    })
}

/// The OTLP/HTTP JSON body of one post.
pub fn export(rows: &[(Call, Held)]) -> Value {
    let records: Vec<Value> = rows
        .iter()
        .map(|(call, held)| log_record(call, held))
        .collect();
    json!({ "resourceLogs": [{
        "resource": { "attributes": [text("service.name", "context-gateway")] },
        "scopeLogs": [{ "scope": { "name": "context-gateway" }, "logRecords": records }],
    }] })
}

/// Posts the due rows to `url`; what the collector does not take is dropped.
pub async fn flush(client: &reqwest::Client, url: &str, anonymous_too: bool) {
    let rows = take(anonymous_too);
    if rows.is_empty() {
        return;
    }
    match client.post(url).json(&export(&rows)).send().await {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            tracing::warn!(status = %response.status(), rows = rows.len(), "the collector did not take the MCP activity; dropped");
        }
        Err(error) => {
            tracing::warn!(%error, rows = rows.len(), "the collector did not take the MCP activity; dropped");
        }
    }
}

/// Posts every [`INTERVAL`], the anonymous rows every [`ANONYMOUS_EVERY`]th time. A post
/// never holds a call: [`record`] only adds to the tally.
pub async fn follow(url: String) {
    let built = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build();
    let client = match built {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(%error, "no HTTP client for the MCP activity; none is posted");
            return;
        }
    };
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + INTERVAL, INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tick: u32 = 0;
    loop {
        ticker.tick().await;
        tick = (tick + 1) % ANONYMOUS_EVERY;
        flush(&client, &url, tick == 0).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(caller: &str, tool: &str) -> Call {
        Call {
            project: "helsinki".to_owned(),
            space: "otvorene-data".to_owned(),
            endpoint: "public-air".to_owned(),
            caller: caller.to_owned(),
            tool: tool.to_owned(),
            outcome: Outcome::Answered,
        }
    }

    fn attributes(record: &Value) -> BTreeMap<String, Value> {
        let mut named = BTreeMap::new();
        for attribute in record["attributes"].as_array().into_iter().flatten() {
            let key = attribute["key"].as_str().unwrap_or_default().to_owned();
            named.insert(key, attribute["value"].clone());
        }
        named
    }

    #[test]
    fn a_record_names_who_called_what_where_and_nothing_else() {
        let held = Held {
            count: 1,
            first: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        };
        let record = log_record(&call("user:ana", "query_entities"), &held);
        let named = attributes(&record);
        let keys: Vec<&str> = named.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "count", "endpoint", "kind", "outcome", "project", "severity", "source", "space",
                "summary", "tool"
            ]
        );
        assert_eq!(named["kind"], json!({ "stringValue": "mcp.tool" }));
        assert_eq!(named["source"], json!({ "stringValue": "gateway" }));
        assert_eq!(named["space"], json!({ "stringValue": "otvorene-data" }));
        assert_eq!(named["count"], json!({ "intValue": "1" }));
        assert_eq!(
            named["summary"],
            json!({ "stringValue": "user:ana called query_entities on public-air" })
        );
        assert_eq!(record["timeUnixNano"], json!("1700000000000000000"));
    }

    #[test]
    fn many_calls_are_one_row_that_counts_them() {
        let mut tally = Tally::new();
        let now = SystemTime::now();
        for _ in 0..3 {
            add(&mut tally, call(ANONYMOUS, "query_entities"), now);
        }
        add(&mut tally, call("user:ana", "query_entities"), now);
        assert_eq!(tally.len(), 2);
        let held = tally[&call(ANONYMOUS, "query_entities")];
        assert_eq!(held.count, 3);
        let record = log_record(&call(ANONYMOUS, "query_entities"), &held);
        assert_eq!(
            record["body"]["stringValue"],
            json!("anonymous called query_entities on public-air 3 times")
        );
    }

    #[test]
    fn the_tally_stops_at_its_cap() {
        let mut tally = Tally::new();
        let now = SystemTime::now();
        for n in 0..MAX_KEYS + 5 {
            add(&mut tally, call(&format!("user:u{n}"), "get_entity"), now);
        }
        assert_eq!(tally.len(), MAX_KEYS);
    }

    #[test]
    fn a_call_on_a_named_space_is_recorded_and_one_on_an_unnamed_space_is_not() {
        name_spaces(&BTreeMap::from([(
            "helsinki-air".to_owned(),
            "air".to_owned(),
        )]));
        let ana = || "user:ana".to_owned();
        note(
            "helsinki",
            "helsinki-air",
            "public-air",
            ana(),
            "query_entities",
            Outcome::ToolError,
        );
        note(
            "helsinki",
            "helsinki-air",
            "public-air",
            ANONYMOUS.to_owned(),
            "query_entities",
            Outcome::Answered,
        );
        note(
            "helsinki",
            "nowhere",
            "public-air",
            ana(),
            "query_entities",
            Outcome::Answered,
        );
        let due = take(false);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0.caller, "user:ana");
        assert_eq!(due[0].0.space, "air");
        assert_eq!(due[0].0.outcome, Outcome::ToolError);
        let anonymous = take(true);
        assert_eq!(anonymous.len(), 1);
        assert_eq!(anonymous[0].0.caller, ANONYMOUS);
        assert!(take(true).is_empty());
    }

    #[test]
    fn a_tool_name_is_a_name_or_unknown() {
        assert_eq!(tool_name("query_entities"), "query_entities");
        assert_eq!(tool_name("describe-access"), "describe-access");
        assert_eq!(tool_name(""), "unknown");
        assert_eq!(tool_name("DROP TABLE x"), "unknown");
        assert_eq!(tool_name(&"a".repeat(65)), "unknown");
    }

    #[test]
    fn an_answer_names_its_outcome() {
        assert_eq!(Outcome::of(&json!({ "result": {} })), Outcome::Answered);
        assert_eq!(
            Outcome::of(&json!({ "result": { "isError": true } })),
            Outcome::ToolError
        );
        assert_eq!(
            Outcome::of(&json!({ "error": { "code": -32602 } })),
            Outcome::Refused
        );
        assert_eq!(Outcome::Failed.as_str(), "failed");
    }
}
