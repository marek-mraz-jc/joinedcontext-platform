//! The data loop of one question (T-4294, AG-107, API/05 §1.4): the router's reads are its first
//! steps, then the answering model is offered the connectors' read-only tools and calls them as
//! it needs, at most `answer.maxDataSteps` tool calls and `answer.maxDataSeconds` per question
//! ([`Ask::data_steps`], [`Ask::data_time`]). A reply
//! without a tool call is the answer. The bounds, the allowlist and the arguments are checked
//! here, never left to the model.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;

use super::agent::{
    fits, markers, name_sources, quoted, send, Ask, Citation, Connected, Event, Spent,
    MAX_ARGUMENTS, MAX_CALLS_PER_REPLY,
};
use super::mcp::{cut, Tool};
use super::model::{Call, CallError};

/// The most of one tool result the model reads in the loop: ten of them stay within a prompt.
const STEP_RESULT_CHARS: usize = 8_000;

/// What the answering model is told beside the routed answer's rules: when to call a tool, how
/// NGSI-LD is asked, and that every number comes from a result. Fixed, so it is cached.
pub const DATA_RULES: &str = "\n\nYou may read more live data before you answer, with the tools you are given. Call a tool only when the data tables you have do not answer the question; when they do, answer at once without a tool.\n\
- At most 10 tool calls per question in all. Each result comes back as JSON with its number `n`: cite it like a table, [n].\n\
- Every number in your answer comes from a table or a tool result you were given, with its [n]. For \"how many\" ask with `count: true` and give `total`.\n\
- query_entities takes `type` (a list), `q` (conditions joined with `;` for AND and `|` for OR; == != > >= < <= ~= for a regular expression; a member of a structured value as `address[addressLocality]==\"X\"`), `idPattern` (a regular expression over ids), `georel` (`near;maxDistance==1000`) with `geometry` and `coordinates` (longitude first), `limit` and `count`.\n\
- For history use query_temporal: `type`, `timerel` (`after`, `before`, `between`) with `timeAt` (and `endTimeAt`), or `lastN`; `aggrMethods` (`avg`, `min`, `max`, `sum`) with `aggrPeriodDuration` such as P1D. At most 31 days, at most 100 values.\n\
- get_type, list_attributes and describe_schema tell you a type's attributes when you do not know which one holds what you need.\n\
- When a tool refuses, correct the arguments once; when it still refuses, answer from what you have and say what you could not read.";

/// What the last call is told when a bound is reached.
const ANSWER_NOW: &str = "No more tool calls: answer the question now from the tables and tool results above, citing them as [n]. Say plainly what you could not read.";

/// One tool the loop offers: the name the model sees, its connector and the tool.
struct Offered<'a> {
    name: String,
    connector: &'a Connected,
    tool: &'a Tool,
}

/// A connector tool's name as the loop offers it: `{endpoint}__{tool}`, a scoped Endpoint's `/`
/// written `--`, at most 64 characters of letters, digits, `_` and `-`, or not offered.
pub fn tool_name(endpoint: &str, tool: &str) -> Option<String> {
    let name = format!("{}__{tool}", endpoint.replace('/', "--"));
    (name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then_some(name)
}

fn offered(connectors: &[Connected]) -> Vec<Offered<'_>> {
    let mut out: Vec<Offered<'_>> = Vec::new();
    for connector in connectors {
        for tool in &connector.tools {
            match tool_name(&connector.endpoint, &tool.name) {
                Some(name) if !out.iter().any(|o| o.name == name) => out.push(Offered {
                    name,
                    connector,
                    tool,
                }),
                _ => {}
            }
        }
    }
    out
}

/// The tool specs the model is offered, each described with the Endpoint it reads.
fn specs(offered: &[Offered<'_>]) -> Vec<Value> {
    offered
        .iter()
        .map(|o| {
            json!({"type": "function", "function": {
                "name": o.name,
                "description": format!("Endpoint {}: {}", o.connector.endpoint, o.tool.description),
                "parameters": o.tool.input_schema,
            }})
        })
        .collect()
}

/// The first entity type a call names, for its stage line and its table.
pub fn type_of(arguments: &Value) -> Option<String> {
    match arguments.get("type")? {
        Value::String(one) => Some(one.clone()),
        Value::Array(many) => many.first()?.as_str().map(str::to_owned),
        _ => None,
    }
}

/// The stage a tool's step shows, and its topic.
fn stage_of(tool: &str, endpoint: &str, arguments: &Value) -> (&'static str, Option<String>) {
    match tool {
        "query_temporal" | "retrieve_temporal" => ("history", type_of(arguments)),
        "list_types" | "get_type" | "list_attributes" | "describe_schema" | "describe_access" => {
            ("schema", Some(endpoint.to_owned()))
        }
        _ => ("reading", type_of(arguments)),
    }
}

/// Whether the loop may still call a tool: steps left of `most` and time left of `time`.
pub fn may_call(steps: usize, elapsed: Duration, most: usize, time: Duration) -> bool {
    steps < most && elapsed < time
}

/// What one question's loop needs from the routed answer before it.
pub struct Start {
    /// The answer call's messages, passages and router's tables included.
    pub messages: Vec<Value>,
    /// Their citations, numbered from 1.
    pub citations: Vec<Citation>,
    /// The tool calls already made: the router's reads.
    pub steps: usize,
    pub started: Instant,
}

/// Runs the loop to its answer; every outcome reaches `events`, and `spent` counts each call.
/// Returns the answer's text and citations, or `None` when an error was sent instead.
pub async fn answer(
    ask: &Ask<'_>,
    start: Start,
    spent: &mut Spent,
    events: &Sender<Event>,
) -> Option<(String, Vec<Citation>)> {
    let Start {
        mut messages,
        mut citations,
        mut steps,
        started,
    } = start;
    let offered = offered(ask.connectors);
    let tools = specs(&offered);
    let call = Call {
        model: ask.answer_model,
        max_tokens: ask.output_tokens,
        json: false,
        provider: ask.provider,
    };
    // A call repeated with the same arguments is answered from the question's own results.
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut forced = false;
    let called_before = spent.calls;
    loop {
        if events.is_closed() {
            return None;
        }
        let allow = !tools.is_empty()
            && !forced
            && may_call(steps, started.elapsed(), ask.data_steps, ask.data_time);
        if !allow && !tools.is_empty() && !forced {
            // `tool_choice: none` alone is not obeyed (T-3205): the last call is told as well.
            messages.push(json!({"role": "user", "content": ANSWER_NOW}));
            forced = true;
        }
        let estimate = (messages
            .iter()
            .map(|m| m.to_string().chars().count())
            .sum::<usize>()
            / 4) as u64
            + ask.output_tokens;
        if ask.spent_before + spent.total + estimate > ask.tokens_per_conversation {
            send(
                events,
                Event::Error {
                    status: 429,
                    title: "Conversation Budget Spent",
                    detail: super::model::CONVERSATION_SPENT.into(),
                },
            )
            .await;
            return None;
        }
        if spent.calls == called_before {
            send(
                events,
                Event::Stage {
                    stage: "answering",
                    topic: None,
                },
            )
            .await;
        }
        let at = Instant::now();
        let stepped = ask
            .model
            .step(
                ask.deployment,
                ask.tokens_per_day,
                &call,
                &messages,
                &tools,
                allow,
            )
            .await;
        super::agent::Stages::add(&mut spent.stages.answer, at);
        let completion = match stepped {
            Ok(completion) => completion,
            Err(err) => {
                let (status, title) = match err {
                    CallError::DaySpent(_) => (429, "Budget Spent"),
                    CallError::Unavailable(_) => (503, "Model Unavailable"),
                };
                send(
                    events,
                    Event::Error {
                        status,
                        title,
                        detail: err.to_string(),
                    },
                )
                .await;
                return None;
            }
        };
        spent.add(&completion);
        if completion.calls.is_empty() || !allow {
            let text = completion.text.trim().to_owned();
            if text.is_empty() {
                send(events, Event::Error {
                    status: 502,
                    title: "No Answer",
                    detail: "The assistant could not put an answer together. Try asking in other words.".into(),
                })
                .await;
                return None;
            }
            spent.first_token_ms = Some(started.elapsed().as_millis() as u64);
            let used = markers(&text);
            citations.retain(|c| used.contains(&c.n));
            name_sources(ask, &mut citations).await;
            tracing::info!(deployment = %ask.deployment, steps, calls = spent.calls, ms = started.elapsed().as_millis() as u64, cited = citations.len(), "data loop answered");
            return Some((text, citations));
        }
        let calls: Vec<Value> = completion
            .calls
            .iter()
            .map(|(id, name, arguments)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments}}))
            .collect();
        messages
            .push(json!({"role": "assistant", "content": completion.text, "tool_calls": calls}));
        for (at, (id, name, arguments)) in completion.calls.iter().enumerate() {
            // Every call is answered, so the history stays whole; what is refused is never sent.
            let content = if at >= MAX_CALLS_PER_REPLY
                || !may_call(steps, started.elapsed(), ask.data_steps, ask.data_time)
            {
                "Not run: the question's tool calls are used up. Answer from what you have."
                    .to_owned()
            } else if arguments.len() > MAX_ARGUMENTS {
                format!("Not run: the arguments are longer than {MAX_ARGUMENTS} characters; call it with fewer.")
            } else {
                steps += 1;
                let key = format!("{name}\u{0}{arguments}");
                match seen.get(&key) {
                    Some(earlier) => earlier.clone(),
                    None => {
                        let content =
                            step(ask, &offered, name, arguments, &mut citations, events).await;
                        seen.insert(key, content.clone());
                        content
                    }
                }
            };
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
        }
    }
}

/// One tool call of the model: refused unless it names an offered tool with arguments its schema
/// admits, else sent, shown as a stage and a `tool` event, and numbered for citing.
async fn step(
    ask: &Ask<'_>,
    offered: &[Offered<'_>],
    name: &str,
    arguments: &str,
    citations: &mut Vec<Citation>,
    events: &Sender<Event>,
) -> String {
    let Some(o) = offered.iter().find(|o| o.name == name) else {
        tracing::warn!(deployment = %ask.deployment, tool = %name, "the model asked for a tool it was not offered");
        return "There is no tool of that name. Use only the tools you were given.".into();
    };
    let arguments: Value = serde_json::from_str(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if let Err(why) = fits(&o.tool.input_schema, &arguments) {
        return format!(
            "The call was not made: {why}. Correct the arguments, or answer without this tool."
        );
    }
    let endpoint = o.connector.endpoint.clone();
    let (stage, topic) = stage_of(&o.tool.name, &endpoint, &arguments);
    send(events, Event::Stage { stage, topic }).await;
    let tool = |status| Event::Tool {
        name: o.tool.name.clone(),
        endpoint: Some(endpoint.clone()),
        status,
    };
    send(events, tool("started")).await;
    let began = Instant::now();
    let answered = o
        .connector
        .surface
        .call(ask.http, &o.tool.name, arguments.clone())
        .await;
    let ms = began.elapsed().as_millis() as u64;
    match answered {
        Ok(text) => {
            send(events, tool("done")).await;
            let mut rows = None;
            if o.tool.name == "query_entities" {
                if let Some(kind) = type_of(&arguments) {
                    if let Some(table) = super::routed::table(ask, o.connector, &kind, &text).await
                    {
                        rows = Some(table.rows.len());
                        if !table.rows.is_empty() {
                            send(events, Event::Data(table)).await;
                        }
                    }
                }
            }
            // What a step did, never what it read (T-4294's per-step record).
            tracing::info!(deployment = %ask.deployment, tool = %o.tool.name, endpoint = %endpoint, status = "done", rows, ms, "data step");
            let n = citations.len() + 1;
            citations.push(Citation {
                n,
                url: None,
                tool: Some(o.tool.name.clone()),
                endpoint: Some(endpoint),
                title: None,
                cards: Vec::new(),
            });
            json!({"n": n, "tool": o.tool.name, "endpoint": o.connector.endpoint, "result": cut(&text, STEP_RESULT_CHARS)}).to_string()
        }
        Err(why) => {
            tracing::info!(deployment = %ask.deployment, tool = %o.tool.name, endpoint = %endpoint, status = "failed", ms, "data step");
            send(events, tool("failed")).await;
            format!("The tool refused or failed ({}). Correct the arguments once, or answer without it and say so.", quoted(&why))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scoped_endpoint_is_named_with_dashes_and_long_names_are_not_offered() {
        assert_eq!(
            tool_name("bbsk/kraj", "query_entities").as_deref(),
            Some("bbsk--kraj__query_entities")
        );
        assert_eq!(
            tool_name("ovzdusie", "get_type").as_deref(),
            Some("ovzdusie__get_type")
        );
        assert_eq!(tool_name("a", &"t".repeat(70)), None);
        assert_eq!(tool_name("a b", "q"), None);
    }

    #[test]
    fn the_loop_stops_at_its_steps_or_its_seconds() {
        let twenty = Duration::from_secs(20);
        assert!(may_call(0, Duration::ZERO, 10, twenty));
        assert!(may_call(9, Duration::from_secs(19), 10, twenty));
        assert!(
            !may_call(10, Duration::ZERO, 10, twenty),
            "the tenth call was the last"
        );
        assert!(!may_call(3, twenty, 10, twenty), "the time budget is spent");
        assert!(
            may_call(24, Duration::from_secs(59), 25, Duration::from_secs(60)),
            "raised"
        );
        assert!(!may_call(25, Duration::ZERO, 25, Duration::from_secs(60)));
    }

    #[test]
    fn each_tool_shows_its_stage_with_the_type_or_the_endpoint() {
        let typed = json!({"type": ["AirQualityObserved"]});
        assert_eq!(
            stage_of("query_entities", "ovzdusie", &typed),
            ("reading", Some("AirQualityObserved".into()))
        );
        assert_eq!(
            stage_of(
                "query_temporal",
                "ovzdusie",
                &json!({"type": "AirQualityObserved"})
            ),
            ("history", Some("AirQualityObserved".into()))
        );
        assert_eq!(
            stage_of("describe_schema", "ovzdusie", &json!({})),
            ("schema", Some("ovzdusie".into()))
        );
        assert_eq!(
            stage_of("get_entity", "e", &json!({"id": "urn:x"})),
            ("reading", None)
        );
        assert_eq!(type_of(&json!({"type": []})), None);
    }
}
