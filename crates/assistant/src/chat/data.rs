//! The data loop of one question (T-4294, AG-107, API/05 §1.4): the router's reads are its first
//! steps, then the answering model is offered one standard set of read-only tools, the Endpoint
//! an argument (T-4748), and calls them as it needs, at most `answer.maxDataSteps` tool calls and `answer.maxDataSeconds` per question
//! ([`Ask::data_steps`], [`Ask::data_time`]). A reply
//! without a tool call is the answer. The bounds, the allowlist and the arguments are checked
//! here, never left to the model.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;

use super::agent::{
    markers, name_sources, send, without_dangling, Ask, Citation, Connected, Event, Spent, Stage,
    Table, MAX_ARGUMENTS, MAX_CALLS_PER_REPLY,
};
use super::mcp::{cut, Tool};
use super::model::Call;
use super::standard::{quoted, resolved, standard};

/// The most of one tool result the model reads in the loop: ten of them stay within a prompt.
const STEP_RESULT_CHARS: usize = 8_000;

/// The tokens one question's loop may spend before its next call must be the answer (T-4651).
/// ponytail: one number for every deployment; an `answer` field when one needs another.
pub const QUESTION_TOKENS: u64 = 60_000;

/// What the answering model is told beside the routed answer's rules: when to call a tool, how
/// NGSI-LD is asked, and that every number comes from a result. `{steps}` is the deployment's
/// `answer.maxDataSteps` (T-4651), so the text is the same for every question of a deployment
/// and stays cached.
const DATA_RULES: &str = "\n\nYou may read more live data before you answer, with the tools you are given. Call a tool only when the data tables you have do not answer the question; when they do, answer at once without a tool.\n\
- At most {steps} tool calls per question in all; before each call you are told how many are left. Each result comes back as JSON with its number `n`: cite it like a table, [n].\n\
- Every number in your answer comes from a table or a tool result you were given, with its [n]. For \"how many\" ask with `count: true` and give `total`.\n\
- query_entities takes `type` (a list), `q` (conditions joined with `;` for AND and `|` for OR; == != > >= < <= ~= for a regular expression; a member of a structured value as `address[addressLocality]==\"X\"`), `idPattern` (a regular expression over ids), `georel` (`near;maxDistance==1000`) with `geometry` and `coordinates` (longitude first), `limit` and `count`.\n\
- For history use query_temporal: `type`, `timerel` (`after`, `before`, `between`) with `timeAt` (and `endTimeAt`), or `lastN`; `aggrMethods` (`avg`, `min`, `max`, `sum`) with `aggrPeriodDuration` such as P1D. At most 31 days, at most 100 values.\n\
- A question about history, the last days or weeks or a trend is answered with the values themselves: call query_temporal for the entities the tables and the earlier turns name (their `idPattern` or a `q` on their names), with `aggrMethods` avg,min,max and `aggrPeriodDuration` P1D, and give the numbers per place and day, briefly. Never tell the person to call an API, an address or a query themselves. When the history cannot be read, say in one sentence that it is not available right now.\n\
- When the place asked for has no figure but the same indicator is there for a wider area that holds it (the whole country, `refArea` SK0), read that one, say in one sentence that the place itself has no figure and give the wider area's value with its [n]; never present it as the place's own.\n\
- get_type, list_attributes and describe_schema tell you a type's attributes when you do not know which one holds what you need.\n\
- Every data tool takes `endpoint`: one Endpoint of the list you are given, by its name. The same tools read every Endpoint.\n\
- A query_entities without `q`, `id`, `idPattern`, `georel` or `scopeQ` answers 5 rows and the total: enough to see what one row holds. For a list, filter with `q`; then `limit` is as you ask.\n\
- When a tool refuses, correct the arguments once; when it still refuses, answer from what you have and say which data you could not find.";

/// The data rules for a deployment whose loop ends after `steps` tool calls.
pub fn data_rules(steps: usize) -> String {
    DATA_RULES.replace("{steps}", &steps.to_string()) + super::query_card::LEGEND
}

/// The Endpoints the data tools read, for the rules (T-4748): the router's [`catalogue`] under a
/// heading that says to name one as `endpoint`. Same for every question of a deployment, so it
/// stays in the cached rules.
///
/// [`catalogue`]: super::routed::catalogue
pub fn endpoints(live: &[super::routed::Shown]) -> String {
    format!(
        "\n\nThe Endpoints you may read, one per line (endpoint (what it holds): types; the attributes a q may test). Give its name as `endpoint`:\n{}",
        super::routed::catalogue(live).trim_end()
    )
}

/// What the model reads before each call while it may still call a tool (T-4651): what is left
/// of its steps and seconds, and to prepare the answer when little is. A message of the loop,
/// never an event, so the visitor never sees it.
pub fn budget(left: usize, most: usize, secs: u64) -> String {
    let mut line = format!("Budget: {left} of {most} tool steps and {secs} s left.");
    if left <= 2 || secs < 5 {
        line.push_str(" Prepare the final answer now; call a tool only if the answer is impossible without it.");
    }
    line
}

/// What the last call is told when a bound is reached.
const ANSWER_NOW: &str = "No more tool calls: answer the question now from the tables and tool results above, citing them as [n]. Say plainly which data you could not find.";

/// What the model is offered: the standard data tools, and the chart tool beside them.
fn tool_specs(connectors: &[Connected], viz: bool) -> Vec<Value> {
    let mut tools = standard(connectors);
    // A chart or a map of a table, drawn by code (T-4657): offered beside the connectors' tools.
    if viz && !tools.is_empty() {
        tools.push(super::viz::tool());
    }
    tools
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
    /// The router's tables by their citation number; the loop adds the ones it reads.
    pub tables: Vec<(usize, Table)>,
    /// The tool calls already made: the router's reads.
    pub steps: usize,
    pub started: Instant,
}

/// What the loop answered.
pub struct Answered {
    pub text: String,
    pub citations: Vec<Citation>,
    /// Whether its call ended on its own.
    pub whole: bool,
    /// How many tables with rows it cites; when any, every table read was sent before it returned.
    pub tables: usize,
}

/// Runs the loop to its answer; every outcome reaches `events`, and `spent` counts each call.
/// Returns what it answered, or `None` when an error was sent instead.
pub async fn answer(
    ask: &Ask<'_>,
    start: Start,
    spent: &mut Spent,
    events: &Sender<Event>,
) -> Option<Answered> {
    let Start {
        mut messages,
        mut citations,
        mut tables,
        mut steps,
        started,
    } = start;
    let viz = ask.viz.and_then(super::viz::domain);
    let tools = tool_specs(ask.connectors, viz.is_some());
    let mut shown = false;
    let call = Call {
        model: ask.answer_model,
        max_tokens: ask.output_tokens,
        json: false,
        provider: ask.provider,
        reasoning: ask.reasoning.answer.unwrap_or_default(),
    };
    // A call repeated with the same arguments is answered from the question's own results.
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut forced = false;
    // The Endpoints and types whose query card a result of this question carried (T-4759).
    let mut given = super::query_card::Given::default();
    let called_before = spent.calls;
    loop {
        if events.is_closed() {
            return None;
        }
        let estimate = (messages
            .iter()
            .map(|m| m.to_string().chars().count())
            .sum::<usize>()
            / 4) as u64
            + ask.output_tokens;
        // At the conversation's budget the loop answers from what it read, with one last call
        // that offers no tool; the next turn is the one refused (T-4729, od-32 on dev).
        let over = ask.spent_before + spent.total + estimate > ask.tokens_per_conversation;
        let allow = !tools.is_empty()
            && !forced
            && !over
            && may_call(steps, started.elapsed(), ask.data_steps, ask.data_time)
            && spent.total + estimate <= QUESTION_TOKENS;
        if !allow && !tools.is_empty() && !forced {
            // `tool_choice: none` alone is not obeyed (T-3205): the last call is told as well.
            messages.push(json!({"role": "user", "content": ANSWER_NOW}));
            forced = true;
        } else if allow {
            let secs = ask.data_time.saturating_sub(started.elapsed()).as_secs();
            let left = ask.data_steps.saturating_sub(steps);
            messages.push(json!({"role": "user", "content": budget(left, ask.data_steps, secs)}));
        }
        if over && spent.calls == called_before {
            tables_read(&tables, events).await;
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
        let pending = ask.trace.pending(
            "data",
            call.model.unwrap_or(ask.model.name()),
            at,
            &messages,
        );
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
        pending.done(stepped.as_ref());
        super::agent::Stages::add(&mut spent.stages.answer, at);
        let completion = match stepped {
            Ok(completion) => completion,
            Err(err) => {
                tables_read(&tables, events).await;
                send(events, super::routed::refusal(ask, &err)).await;
                return None;
            }
        };
        spent.add(&completion);
        spent.served(Stage::Answer, &completion);
        if completion.calls.is_empty() || !allow {
            if completion.declined() {
                tables_read(&tables, events).await;
                send(events, super::agent::declined()).await;
                return None;
            }
            let text = completion.text.trim().to_owned();
            if text.is_empty() {
                tables_read(&tables, events).await;
                send(events, super::agent::no_answer(spent)).await;
                return None;
            }
            spent.first_token_ms = Some(started.elapsed().as_millis() as u64);
            let (text, dangling) = without_dangling(&text, &citations);
            spent.dangling += dangling;
            let used = markers(&text);
            citations.retain(|c| used.contains(&c.n));
            name_sources(ask, &mut citations).await;
            // The tables are shown once the answer is written, and only when it cites one of them
            // (T-4741): an answer that found nothing came beside two tables of other indicators
            // on prod. An answer that cites one keeps every table read, as before.
            let cited = tables.iter().filter(|(n, _)| used.contains(n)).count();
            if cited > 0 {
                tables_read(&tables, events).await;
            }
            // Whether the model stopped early or ran into the cap (T-4651).
            let steps_left_at_answer = ask.data_steps.saturating_sub(steps);
            tracing::info!(deployment = %ask.deployment, steps, steps_left_at_answer, calls = spent.calls, tokens = spent.total, reasoning = spent.reasoning.answer.unwrap_or_default(), ms = started.elapsed().as_millis() as u64, cited = citations.len(), "data loop answered");
            return Some(Answered {
                text,
                citations,
                whole: completion.whole(),
                tables: cited,
            });
        }
        let calls: Vec<Value> = completion
            .calls
            .iter()
            .map(|(id, name, arguments)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments}}))
            .collect();
        messages
            .push(json!({"role": "assistant", "content": completion.text, "tool_calls": calls}));
        // Every call is answered, so the history stays whole; what is refused is never sent. The
        // calls to send go out at once and are numbered in the reply's order (T-4731): one after
        // another, a reply of two reads lost about 0.9 s on dev.
        let mut replies: Vec<Reply<'_>> = Vec::new();
        for (at, (_, name, arguments)) in completion.calls.iter().enumerate() {
            let reply = if at >= MAX_CALLS_PER_REPLY
                || !may_call(steps, started.elapsed(), ask.data_steps, ask.data_time)
            {
                Reply::Said(
                    "Not run: the question's tool calls are used up. Answer from what you have."
                        .to_owned(),
                )
            } else if arguments.len() > MAX_ARGUMENTS {
                Reply::Said(format!("Not run: the arguments are longer than {MAX_ARGUMENTS} characters; call it with fewer."))
            } else {
                steps += 1;
                let key = format!("{name}\u{0}{arguments}");
                match (seen.get(&key), viz) {
                    (Some(earlier), _) => Reply::Said(earlier.clone()),
                    (None, Some(domain)) if name == super::viz::TOOL => {
                        Reply::Chart(arguments, domain)
                    }
                    (None, _) => match replies.iter().position(|r| r.runs(&key)) {
                        Some(earlier) => Reply::Same(earlier),
                        None => match checked(ask, (name, arguments)) {
                            Ok(c) => Reply::Run(c, key),
                            Err(refused) => {
                                seen.insert(key, refused.clone());
                                Reply::Said(refused)
                            }
                        },
                    },
                }
            };
            replies.push(reply);
        }
        let left = ask.data_time.saturating_sub(started.elapsed());
        for reply in &replies {
            if let Reply::Run(c, _) = reply {
                self::started(c, events).await;
            }
        }
        let outs = futures_util::future::join_all(replies.iter().map(|reply| async move {
            match reply {
                Reply::Run(c, _) => Some(sent(ask, c, left).await),
                _ => None,
            }
        }))
        .await;
        let mut contents: Vec<String> = Vec::new();
        let mut cards: Vec<String> = Vec::new();
        for (reply, out) in replies.iter().zip(outs) {
            let content = match (reply, out) {
                (Reply::Run(c, key), Some((out, model))) => {
                    let content = recorded(ask, c, out, &mut citations, &mut tables, events).await;
                    seen.insert(key.clone(), content.clone());
                    cards.extend(given.card(
                        c.connector,
                        &c.tool.name,
                        &c.arguments,
                        model.as_deref(),
                    ));
                    content
                }
                (Reply::Same(earlier), _) => contents.get(*earlier).cloned().unwrap_or_default(),
                (Reply::Chart(arguments, domain), _) => {
                    show(&tables, arguments, domain, &mut shown, events).await
                }
                (Reply::Said(text), _) => text.clone(),
                // every call to send has its answer; this is the pairing's other half
                (Reply::Run(..), None) => {
                    "The call was not made. Answer from what you have.".into()
                }
            };
            contents.push(content);
        }
        for ((id, _, _), content) in completion.calls.iter().zip(contents) {
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
        }
        // The query cards of the Endpoints first read here, as text after the results they go
        // with, so each result stays the JSON the rules describe (T-4759).
        if !cards.is_empty() {
            messages.push(json!({"role": "user", "content": cards.join("\n\n")}));
        }
    }
}

/// Sends every table the loop read, in their order: beside an answer that cites one, or beside
/// the error when no answer came, as they were shown before an answer existed (T-4741).
async fn tables_read(tables: &[(usize, Table)], events: &Sender<Event>) {
    for (_, table) in tables {
        send(events, Event::Data(table.clone())).await;
    }
}

/// What one tool call of a reply is answered with.
enum Reply<'a> {
    /// A text that needs no call: a refusal or a repeat of an earlier step.
    Said(String),
    /// A checked call to send, with its key in the question's `seen`.
    Run(Checked<'a>, String),
    /// The same call as an earlier one of this reply, by its place.
    Same(usize),
    /// A chart of the tables read so far, this reply's earlier reads included.
    Chart(&'a str, &'a str),
}

impl Reply<'_> {
    fn runs(&self, key: &str) -> bool {
        matches!(self, Reply::Run(_, k) if k == key)
    }
}

/// One `show_visualization` call (T-4657): built from a table of this answer and sent as a
/// `visualization` event, at most once; what is wrong goes back to the model.
async fn show(
    tables: &[(usize, Table)],
    arguments: &str,
    domain: &str,
    shown: &mut bool,
    events: &Sender<Event>,
) -> String {
    if *shown {
        return "Not shown: one chart per answer, and this answer has one. Answer in text.".into();
    }
    match super::viz::build(arguments, tables, domain) {
        Ok(visualization) => {
            *shown = true;
            send(events, Event::Visualization(visualization)).await;
            "Shown to the person. Do not describe the picture; answer in text, naming the few values that matter.".into()
        }
        Err(why) => why,
    }
}

/// One tool call of the model, checked: a tool its Endpoint offers, with arguments that
/// Endpoint's schema admits.
struct Checked<'a> {
    connector: &'a Connected,
    tool: &'a Tool,
    arguments: Value,
}

/// What a sent call came back with: the answer or why it failed, the table a read makes, and how
/// long the call took.
type Sent = (Result<String, String>, Option<Table>, u64);

/// A tool call checked before it is sent (T-4748): refused unless it names a standard tool and,
/// as [`ENDPOINT`], a connector that offers it, with arguments the connector's own schema admits.
fn checked<'a>(ask: &Ask<'a>, (name, arguments): (&str, &str)) -> Result<Checked<'a>, String> {
    let arguments: Value = serde_json::from_str(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let (connector, tool, arguments) = resolved(ask.connectors, name, arguments).inspect_err(|_| {
        tracing::warn!(deployment = %ask.deployment, tool = %name, "a data call was refused before it was sent");
    })?;
    Ok(Checked {
        connector,
        tool,
        arguments,
    })
}

/// A checked call's `tool` event with `status`.
fn tool_event(c: &Checked<'_>, status: &'static str) -> Event {
    Event::Tool {
        name: c.tool.name.clone(),
        endpoint: Some(c.connector.endpoint.clone()),
        status,
    }
}

/// A call about to be sent, shown as a stage and a started `tool` event.
async fn started(c: &Checked<'_>, events: &Sender<Event>) {
    let (stage, topic) = stage_of(&c.tool.name, &c.connector.endpoint, &c.arguments);
    send(events, Event::Stage { stage, topic }).await;
    send(events, tool_event(c, "started")).await;
}

/// Sends a checked call and makes a read's table, waiting at most `left` of the question's data
/// seconds: each waiting out its connector's own timeout took a question to the 45 s cut
/// (T-4663). Sends no event, so the calls of one reply run at once (T-4731). Beside it, the
/// Endpoint's LinkML for its query card (T-4759), read once per deployment and kept.
async fn sent(
    ask: &Ask<'_>,
    c: &Checked<'_>,
    left: Duration,
) -> (Sent, Option<std::sync::Arc<str>>) {
    let (sent, model) = tokio::join!(
        call_of(ask, c, left),
        super::routed::model_of(ask, c.connector)
    );
    (sent, model)
}

/// The call [`sent`] makes and the table of a read.
async fn call_of(ask: &Ask<'_>, c: &Checked<'_>, left: Duration) -> Sent {
    let Checked {
        connector,
        tool,
        arguments,
    } = c;
    let began = Instant::now();
    let answered = tokio::time::timeout(
        left,
        connector
            .surface
            .call(ask.http, &tool.name, arguments.clone()),
    )
    .await
    .unwrap_or_else(|_| Err("the source did not answer within the question's time".into()));
    ask.trace.tool(
        &tool.name,
        Some(&connector.endpoint),
        arguments,
        began,
        &answered,
    );
    let mut table = None;
    if let (Ok(text), "query_entities", Some(kind)) =
        (&answered, tool.name.as_str(), type_of(arguments))
    {
        table = super::routed::table(
            ask,
            connector,
            &kind,
            text,
            &super::routed::asked_of(arguments),
        )
        .await;
    }
    (answered, table, began.elapsed().as_millis() as u64)
}

/// A sent call's result, in the order of the reply's calls: its `tool` event, its table, and the
/// number it is cited by.
async fn recorded(
    ask: &Ask<'_>,
    c: &Checked<'_>,
    (answered, table, ms): Sent,
    citations: &mut Vec<Citation>,
    tables: &mut Vec<(usize, Table)>,
    events: &Sender<Event>,
) -> String {
    let (tool, endpoint) = (&c.tool.name, c.connector.endpoint.clone());
    match answered {
        Ok(text) => {
            send(events, tool_event(c, "done")).await;
            let (mut rows, mut dataset) = (None, None);
            if let Some(table) = table {
                rows = Some(table.rows.len());
                dataset = table.dataset.clone();
                if !table.rows.is_empty() {
                    tables.push((citations.len() + 1, table));
                }
            }
            // What a step did, never what it read (T-4294's per-step record).
            tracing::info!(deployment = %ask.deployment, tool = %tool, endpoint = %endpoint, status = "done", rows, ms, "data step");
            let n = citations.len() + 1;
            citations.push(Citation {
                n,
                url: dataset,
                tool: Some(tool.clone()),
                endpoint: Some(endpoint.clone()),
                title: None,
                cards: Vec::new(),
                dates: Default::default(),
            });
            json!({"n": n, "tool": tool, "endpoint": endpoint, "result": cut(&text, STEP_RESULT_CHARS)}).to_string()
        }
        Err(why) => {
            tracing::info!(deployment = %ask.deployment, tool = %tool, endpoint = %endpoint, status = "failed", ms, "data step");
            send(events, tool_event(c, "failed")).await;
            format!("The tool refused or failed ({}). Correct the arguments once, or answer without it and say so.", quoted(&why))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::mcp::READ_ONLY_TOOLS;
    use super::super::standard::{endpoint_of, ENDPOINT, SAMPLE_ROWS};
    use super::*;

    fn connector(endpoint: &str, tools: &[&str]) -> Connected {
        Connected {
            endpoint: endpoint.into(),
            title: None,
            surface: super::super::mcp::Surface {
                gateway: "http://gateway".into(),
                slug: endpoint.into(),
                timeout: Duration::from_secs(1),
                token: None,
            },
            tools: tools
                .iter()
                .map(|name| Tool {
                    name: (*name).into(),
                    description: String::new(),
                    input_schema: json!({"type": "object"}),
                })
                .collect(),
            label: None,
            siblings: Vec::new(),
        }
    }

    /// The recorded `tools/list` of bbsk-otvorene-data on dev (2026-10-19), as a connector
    /// named `endpoint` lists it, its types replaced by `types`.
    fn recorded(endpoint: &str, types: &[&str]) -> Connected {
        let listed: Vec<Value> = serde_json::from_str(include_str!(
            "../../tests/fixtures/mcp/tools-list-bbsk-otvorene-data.json"
        ))
        .unwrap();
        let allowed: Vec<String> = READ_ONLY_TOOLS.iter().map(|t| (*t).to_owned()).collect();
        let mut c = connector(endpoint, &[]);
        c.tools = super::super::mcp::picked(&listed, &allowed);
        for tool in &mut c.tools {
            for at in ["/properties/type/enum", "/properties/type/items/enum"] {
                if let Some(e) = tool.input_schema.pointer_mut(at) {
                    *e = json!(types);
                }
            }
            tool.description.push_str(&format!(
                "\n\nQuery guide, per type:\n{endpoint}: no time axis."
            ));
        }
        c
    }

    /// T-4748: the same standard tools for one connector and for fifty, under 20 KB, each with
    /// `endpoint` required and no Endpoint's own names; prod offered ~550 copies, ~145k tokens.
    #[test]
    fn the_tools_are_the_same_for_one_endpoint_and_for_fifty() {
        let one = [recorded("bbsk/registre", &["School"])];
        let fifty: Vec<Connected> = (0..50)
            .map(|i| recorded(&format!("p{i}/data-{i}"), &["School", "Grant"][..1 + i % 2]))
            .collect();
        let tools = tool_specs(&fifty, false);
        assert_eq!(
            serde_json::to_string(&tool_specs(&one, false)).unwrap(),
            serde_json::to_string(&tools).unwrap()
        );
        let bytes = serde_json::to_string(&tools).unwrap().len();
        assert!(bytes < 20_000, "{bytes} bytes");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert_eq!(names, READ_ONLY_TOOLS);
        for tool in &tools {
            let f = &tool["function"];
            assert_eq!(f["parameters"]["required"][0], ENDPOINT, "{}", f["name"]);
            assert!(
                !f["description"].as_str().unwrap().contains("Query guide"),
                "{}",
                f["name"]
            );
            let text = f["parameters"].to_string();
            assert!(!text.contains("\"School\""), "{}: {text}", f["name"]);
        }
        // The chart tool rides beside them; no connector, no data tool and no chart.
        assert_eq!(tool_specs(&one, true).len(), tools.len() + 1);
        assert!(tool_specs(&[], true).is_empty());
        // A connector whose listing failed is not connected at all; one listing nothing adds nothing.
        let mut with_empty = fifty.clone();
        with_empty.insert(0, connector("empty", &[]));
        assert_eq!(tool_specs(&with_empty, false), tools);
    }

    /// T-4748: `endpoint` resolves among the deployment's connectors only: by name, without its
    /// `{project}/`, by slug or by title; a miss or a shared name is refused with the names.
    #[test]
    fn a_call_names_its_endpoint_and_only_a_connectors() {
        let mut bbsk = recorded("bbsk/bbsk-otvorene-data", &["PublicOrganization"]);
        bbsk.surface.slug = "6b4yqhzq".into();
        bbsk.title = Some("Otvorené dáta BBSK".into());
        let connectors = [
            bbsk,
            recorded("slovensko/susr-om-pohyb", &["StatisticalObservation"]),
            recorded("zilina/registre", &["School"]),
            recorded("bbsk/registre", &["School"]),
        ];
        for (named, endpoint) in [
            ("bbsk/bbsk-otvorene-data", "bbsk/bbsk-otvorene-data"),
            ("bbsk-otvorene-data", "bbsk/bbsk-otvorene-data"),
            ("6b4yqhzq", "bbsk/bbsk-otvorene-data"),
            (" Otvorené dáta BBSK", "bbsk/bbsk-otvorene-data"),
            // The scoped name the router wrote in turn 11 on prod, and the bare one.
            ("slovensko/susr-om-pohyb", "slovensko/susr-om-pohyb"),
            ("susr-om-pohyb", "slovensko/susr-om-pohyb"),
            ("x/susr-om-pohyb", "slovensko/susr-om-pohyb"),
        ] {
            let c = endpoint_of(&connectors, named).unwrap_or_else(|e| panic!("{named}: {e}"));
            assert_eq!(c.endpoint, endpoint, "{named}");
        }
        let shared = endpoint_of(&connectors, "registre").unwrap_err();
        assert!(
            shared.contains("several")
                && shared.contains("zilina/registre")
                && shared.contains("bbsk/registre"),
            "{shared}"
        );
        let missing = endpoint_of(&connectors, "praha/praha-mesto").unwrap_err();
        assert!(
            missing.contains("no Endpoint of this assistant")
                && missing.contains("bbsk/bbsk-otvorene-data"),
            "{missing}"
        );

        // A call without `endpoint` is refused with the names; with one it goes to that connector,
        // `endpoint` taken off, under the connector's own schema (its type enum).
        let read = |args: Value| resolved(&connectors, "query_entities", args);
        let no_endpoint = read(json!({"type": ["School"]})).unwrap_err();
        assert!(
            no_endpoint.contains("`endpoint` is required")
                && no_endpoint.contains("zilina/registre"),
            "{no_endpoint}"
        );
        let (c, tool, sent) = read(json!({"endpoint": "zilina/registre", "type": ["School"], "q": "name==\"X\"", "limit": 50})).unwrap();
        assert_eq!(
            (c.endpoint.as_str(), tool.name.as_str()),
            ("zilina/registre", "query_entities")
        );
        assert_eq!(
            sent,
            json!({"type": ["School"], "q": "name==\"X\"", "limit": 50})
        );
        let wrong_type =
            read(json!({"endpoint": "zilina/registre", "type": ["Grant"], "q": "a==1"}))
                .unwrap_err();
        assert!(wrong_type.contains("not made"), "{wrong_type}");
        assert!(resolved(
            &connectors,
            "delete_entity",
            json!({"endpoint": "zilina/registre"})
        )
        .unwrap_err()
        .contains("no tool of that name"));
        let mut narrow = connector("ovzdusie", &["query_entities"]);
        narrow.tools[0].input_schema =
            json!({"type": "object", "properties": {"type": {"type": "array"}}});
        let few = [narrow];
        let offers = resolved(&few, "get_type", json!({"endpoint": "ovzdusie"})).unwrap_err();
        assert!(offers.contains("no tool of that name"), "{offers}");
        let both = [few[0].clone(), connector("air", &["get_type"])];
        let offers = resolved(&both, "get_type", json!({"endpoint": "ovzdusie"})).unwrap_err();
        assert!(
            offers.contains("offers no get_type; it offers query_entities"),
            "{offers}"
        );
    }

    /// T-4748: an unfiltered read is a sample of 5 rows with the total; a filtered one, the list
    /// question (all six secondary schools of Zvolen), keeps its limit.
    #[test]
    fn an_unfiltered_read_is_a_sample_and_a_list_keeps_its_limit() {
        let connectors = [recorded("bbsk/bbsk-otvorene-data", &["PublicOrganization"])];
        let read = |args: Value| resolved(&connectors, "query_entities", args).map(|(_, _, a)| a);
        let sample = read(
            json!({"endpoint": "bbsk-otvorene-data", "type": ["PublicOrganization"], "limit": 50}),
        )
        .unwrap();
        assert_eq!(
            sample,
            json!({"type": ["PublicOrganization"], "limit": SAMPLE_ROWS, "count": true})
        );
        let bare = read(json!({"endpoint": "bbsk-otvorene-data", "type": ["PublicOrganization"]}))
            .unwrap();
        assert_eq!(bare["limit"], SAMPLE_ROWS);
        let fewer = read(
            json!({"endpoint": "bbsk-otvorene-data", "type": ["PublicOrganization"], "limit": 2}),
        )
        .unwrap();
        assert_eq!(fewer["limit"], 2);
        let list = read(json!({"endpoint": "bbsk-otvorene-data", "type": ["PublicOrganization"], "q": "address[addressLocality]==\"Zvolen\"", "limit": 50})).unwrap();
        assert_eq!(list["limit"], 50);
        assert!(list.get("count").is_none());
        // Other tools are sent as asked.
        let (_, _, summary) = resolved(
            &connectors,
            "summarize",
            json!({"endpoint": "bbsk-otvorene-data", "type": ["PublicOrganization"]}),
        )
        .unwrap();
        assert_eq!(summary, json!({"type": ["PublicOrganization"]}));
        assert!(DATA_RULES.contains("answers 5 rows and the total"));
        assert!(DATA_RULES.contains("Every data tool takes `endpoint`"));
        assert!(!DATA_RULES.contains("more_tools"));
    }

    /// T-4741: the owner asked for BBSK's births; SUSR holds them for Slovakia only (SK0), and the
    /// answer said nothing was published instead of giving the national figure as such.
    #[test]
    fn a_figure_absent_for_the_place_is_given_for_the_wider_area_as_such() {
        assert!(DATA_RULES.contains("the same indicator is there for a wider area"));
        assert!(DATA_RULES.contains("`refArea` SK0"));
        assert!(DATA_RULES.contains("never present it as the place's own"));
    }

    #[test]
    fn a_history_question_is_answered_with_values_and_never_points_to_the_api() {
        // A history question gets the values, or one sentence when they cannot be read.
        assert!(DATA_RULES.contains(
            "call query_temporal for the entities the tables and the earlier turns name"
        ));
        assert!(DATA_RULES
            .contains("Never tell the person to call an API, an address or a query themselves."));
        assert!(DATA_RULES.contains("say in one sentence that it is not available right now"));
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
    fn the_rules_name_the_deployments_steps_and_the_budget_counts_down() {
        let rules = data_rules(4);
        assert!(rules.contains("At most 4 tool calls"), "{rules}");
        assert!(!rules.contains("{steps}") && !rules.contains("10 tool calls"));
        let most = 6;
        for k in 0..most {
            let line = budget(most - k, most, 18);
            assert!(line.starts_with(&format!(
                "Budget: {} of 6 tool steps and 18 s left.",
                most - k
            )));
            assert_eq!(
                line.contains("Prepare the final answer now"),
                most - k <= 2,
                "step {k}: {line}"
            );
        }
        assert!(
            budget(5, 6, 4).contains("Prepare the final answer now"),
            "under 5 s left"
        );
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

    /// T-4657: an accepted call sends one `visualization` event; a second call in the answer and
    /// a call naming a table the answer did not read are told to the model and send nothing.
    #[tokio::test]
    async fn one_chart_per_answer_and_only_from_its_own_tables() {
        let table = Table {
            endpoint: "ovzdusie".into(),
            entity_type: "AirQualityObserved".into(),
            columns: vec!["name".into(), "pm10".into()],
            // Three values: a chart of fewer is not drawn (T-4682).
            rows: vec![
                vec![json!("Námestie SNP"), json!(21)],
                vec![json!("Zvolen"), json!(9)],
                vec![json!("Žilina"), json!(14)],
            ],
            units: vec![None, Some("µg/m³".into())],
            labels: Vec::new(),
            enums: Vec::new(),
            summary: Vec::new(),
            total: None,
            dataset: None,
            download: None,
            geojson: None,
            map: None,
            geometries: vec![None, None, None],
            profile: None,
        };
        let tables = [(2, table)];
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut shown = false;
        let bar = r#"{"kind":"bar","data":2,"label":"name","value":"pm10","title":"PM10"}"#;
        let missing = r#"{"kind":"bar","data":7,"label":"name","value":"pm10","title":"PM10"}"#;
        assert!(show(&tables, missing, "dev.example.org", &mut shown, &tx)
            .await
            .contains("no table 7"));
        assert!(!shown);
        assert!(show(&tables, bar, "dev.example.org", &mut shown, &tx)
            .await
            .starts_with("Shown"));
        assert!(show(&tables, bar, "dev.example.org", &mut shown, &tx)
            .await
            .contains("one chart per answer"));
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert!(
            matches!(events.as_slice(), [Event::Visualization(v)] if v["rows"][0]["value"] == 21)
        );
    }
}
