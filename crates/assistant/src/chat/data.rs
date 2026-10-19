//! The data loop of one question (T-4294, AG-107, API/05 §1.4): the router's reads are its first
//! steps, then the answering model is offered the connectors' read-only tools and calls them as
//! it needs, at most `answer.maxDataSteps` tool calls and `answer.maxDataSeconds` per question
//! ([`Ask::data_steps`], [`Ask::data_time`]). A reply
//! without a tool call is the answer. The bounds, the allowlist and the arguments are checked
//! here, never left to the model.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;

use super::agent::{
    fits, markers, name_sources, quoted, send, without_dangling, Ask, Citation, Connected, Event,
    Spent, Stage, Table, MAX_ARGUMENTS, MAX_CALLS_PER_REPLY,
};
use super::mcp::{cut, Tool};
use super::model::Call;

/// The most of one tool result the model reads in the loop: ten of them stay within a prompt.
const STEP_RESULT_CHARS: usize = 8_000;

/// The tokens one question's loop may spend before its next call must be the answer (T-4651).
/// ponytail: one number for every deployment; an `answer` field when one needs another.
pub const QUESTION_TOKENS: u64 = 60_000;

/// The most input tokens one call of the loop carries, its tool specs counted (T-4731, owner's
/// order): calls of 125k-140k tokens were most of a data answer's time and cost on dev.
const CALL_TOKENS: u64 = 100_000;

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
- get_type, list_attributes and describe_schema tell you a type's attributes when you do not know which one holds what you need.\n\
- When a more_tools tool is offered and the tools you have cannot answer, because the data is on another Endpoint or needs another tool (list_types, list_attributes, describe_schema, get_entity, retrieve_temporal), call more_tools first: it offers every tool of every Endpoint.\n\
- When a tool refuses, correct the arguments once; when it still refuses, answer from what you have and say which data you could not find.";

/// The data rules for a deployment whose loop ends after `steps` tool calls.
pub fn data_rules(steps: usize) -> String {
    DATA_RULES.replace("{steps}", &steps.to_string())
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

/// The tools a question's loop is offered first, on the Endpoints the router named (T-4731): a
/// read, a history, a summary and a type's attributes. Every connector's whole toolset went on
/// every call before, about 125k tokens of a 4-step loop; the rest is one [`MORE_TOOLS`] away.
const FIRST_TOOLS: &[&str] = &["query_entities", "query_temporal", "summarize", "get_type"];

/// The loop's escape hatch (T-4731): a question the router scoped too narrowly gets every tool
/// of every Endpoint one call later, and that call costs no step of the question.
pub const MORE_TOOLS: &str = "more_tools";

fn more_tools() -> Value {
    json!({"type": "function", "function": {
        "name": MORE_TOOLS,
        "description": "Offers every read-only tool of every Endpoint of this assistant, when the tools you have cannot answer: the data is on another Endpoint, or it needs list_types, list_attributes, describe_schema, get_entity or retrieve_temporal. Costs no tool step.",
        "parameters": {"type": "object", "additionalProperties": false, "properties": {}},
    }})
}

/// The tools offered first: [`FIRST_TOOLS`] of the Endpoints `named`, and whether that is fewer
/// than all. No Endpoint named, or none of them a connector's, offers all, as before T-4731.
fn first<'a>(all: &[Offered<'a>], named: &[String]) -> (Vec<Offered<'a>>, bool) {
    let few: Vec<Offered<'a>> = all
        .iter()
        .filter(|o| {
            named.contains(&o.connector.endpoint) && FIRST_TOOLS.contains(&o.tool.name.as_str())
        })
        .cloned()
        .collect();
    if few.is_empty() || few.len() == all.len() {
        (all.to_vec(), false)
    } else {
        (few, true)
    }
}

/// What the model is offered: the connectors' tools, the chart tool beside them and, while the
/// tools are the first few, [`MORE_TOOLS`].
fn tool_specs(offered: &[Offered<'_>], viz: bool, narrowed: bool) -> Vec<Value> {
    let mut tools = specs(offered);
    // A chart or a map of a table, drawn by code (T-4657): offered beside the connectors' tools.
    if viz && !tools.is_empty() {
        tools.push(super::viz::tool());
    }
    if narrowed {
        tools.push(more_tools());
    }
    tools
}

/// One tool the loop offers: the name the model sees, its connector and the tool.
#[derive(Clone)]
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
    /// The router's tables by their citation number; the loop adds the ones it reads.
    pub tables: Vec<(usize, Table)>,
    /// The tool calls already made: the router's reads.
    pub steps: usize,
    /// The Endpoints the router read, whose first tools the loop is offered (T-4731).
    pub named: Vec<String>,
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
        named,
    } = start;
    let all = offered(ask.connectors);
    let (mut offered, mut narrowed) = first(&all, &named);
    let viz = ask.viz.and_then(super::viz::domain);
    let mut tools = tool_specs(&offered, viz.is_some(), narrowed);
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
        let sent = capped(&messages, &tools, &tables);
        let at = Instant::now();
        let pending = ask
            .trace
            .pending("data", call.model.unwrap_or(ask.model.name()), at, &sent);
        let stepped = ask
            .model
            .step(
                ask.deployment,
                ask.tokens_per_day,
                &call,
                &sent,
                &tools,
                allow,
            )
            .await;
        drop(sent);
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
        for (at, (id, name, arguments)) in completion.calls.iter().enumerate() {
            // Every call is answered, so the history stays whole; what is refused is never sent.
            let content = if at >= MAX_CALLS_PER_REPLY
                || !may_call(steps, started.elapsed(), ask.data_steps, ask.data_time)
            {
                "Not run: the question's tool calls are used up. Answer from what you have."
                    .to_owned()
            } else if narrowed && name == MORE_TOOLS {
                // The escape hatch costs no step: the question's reads are still its own.
                narrowed = false;
                offered = all.clone();
                tools = tool_specs(&offered, viz.is_some(), narrowed);
                tracing::info!(deployment = %ask.deployment, tools = offered.len(), "the data loop asked for every tool");
                "Every tool of every Endpoint is offered now. Call the one the question needs."
                    .to_owned()
            } else if arguments.len() > MAX_ARGUMENTS {
                format!("Not run: the arguments are longer than {MAX_ARGUMENTS} characters; call it with fewer.")
            } else {
                steps += 1;
                let key = format!("{name}\u{0}{arguments}");
                match (seen.get(&key), viz) {
                    (Some(earlier), _) => earlier.clone(),
                    (None, Some(domain)) if name == super::viz::TOOL => {
                        show(&tables, arguments, domain, &mut shown, events).await
                    }
                    (None, _) => {
                        let left = ask.data_time.saturating_sub(started.elapsed());
                        let content = step(
                            ask,
                            &offered,
                            (name, arguments),
                            left,
                            &mut citations,
                            &mut tables,
                            events,
                        )
                        .await;
                        seen.insert(key, content.clone());
                        content
                    }
                }
            };
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
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

/// A rough token count of `values`, four characters a token, as the loop's own estimate.
fn tokens_in(values: &[Value]) -> u64 {
    (values
        .iter()
        .map(|v| v.to_string().chars().count())
        .sum::<usize>()
        / 4) as u64
}

/// The messages one call sends: within [`CALL_TOKENS`] with its `tools`, the oldest step results
/// shortened first to a line naming their tool, Endpoint and rows. The newest two step results,
/// the rules, the question and the router's tables and passages stay whole, and a call that is
/// still over goes as it is: size never refuses a question (T-4731).
fn capped<'m>(
    messages: &'m [Value],
    tools: &[Value],
    tables: &[(usize, Table)],
) -> Cow<'m, [Value]> {
    let mut size = tokens_in(messages) + tokens_in(tools);
    if size <= CALL_TOKENS {
        return Cow::Borrowed(messages);
    }
    let results: Vec<usize> = (0..messages.len())
        .filter(|&i| messages[i]["role"] == "tool")
        .collect();
    let mut out = messages.to_vec();
    for &i in &results[..results.len().saturating_sub(2)] {
        if size <= CALL_TOKENS {
            break;
        }
        let Some(short) = shortened(&out[i]["content"], tables) else {
            continue;
        };
        let before = tokens_in(&out[i..=i]);
        out[i]["content"] = Value::String(short);
        size = size - before + tokens_in(&out[i..=i]);
    }
    Cow::Owned(out)
}

/// A step result as one line: its number, tool, Endpoint and rows, without what it read. `None`
/// for a message that is no step result (a refusal, the escape hatch's reply), which is short.
fn shortened(content: &Value, tables: &[(usize, Table)]) -> Option<String> {
    let result: Value = serde_json::from_str(content.as_str()?).ok()?;
    let n = result.get("n")?.as_u64()?;
    let rows = tables
        .iter()
        .find(|(at, _)| *at as u64 == n)
        .map(|(_, t)| t.rows.len());
    Some(
        json!({
            "n": n,
            "tool": result["tool"],
            "endpoint": result["endpoint"],
            "rows": rows,
            "result": "shortened to keep the call small: call the tool again for its values",
        })
        .to_string(),
    )
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

/// One tool call of the model: refused unless it names an offered tool with arguments its schema
/// admits, else sent, shown as a stage and a `tool` event, and numbered for citing. It waits at
/// most `left` of the question's data seconds: up to four calls of one reply run one after the
/// other, and each waiting out its connector's own timeout took a question to the 45 s cut
/// (T-4663).
async fn step(
    ask: &Ask<'_>,
    offered: &[Offered<'_>],
    (name, arguments): (&str, &str),
    left: Duration,
    citations: &mut Vec<Citation>,
    tables: &mut Vec<(usize, Table)>,
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
    let answered = tokio::time::timeout(
        left,
        o.connector
            .surface
            .call(ask.http, &o.tool.name, arguments.clone()),
    )
    .await
    .unwrap_or_else(|_| Err("the source did not answer within the question's time".into()));
    ask.trace
        .tool(&o.tool.name, Some(&endpoint), &arguments, began, &answered);
    let ms = began.elapsed().as_millis() as u64;
    match answered {
        Ok(text) => {
            send(events, tool("done")).await;
            let (mut rows, mut dataset) = (None, None);
            if o.tool.name == "query_entities" {
                if let Some(kind) = type_of(&arguments) {
                    if let Some(table) = super::routed::table(
                        ask,
                        o.connector,
                        &kind,
                        &text,
                        &super::routed::asked_of(&arguments),
                    )
                    .await
                    {
                        rows = Some(table.rows.len());
                        dataset = table.dataset.clone();
                        if !table.rows.is_empty() {
                            tables.push((citations.len() + 1, table));
                        }
                    }
                }
            }
            // What a step did, never what it read (T-4294's per-step record).
            tracing::info!(deployment = %ask.deployment, tool = %o.tool.name, endpoint = %endpoint, status = "done", rows, ms, "data step");
            let n = citations.len() + 1;
            citations.push(Citation {
                n,
                url: dataset,
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

    /// A step result of `chars` characters, as [`step`] writes it.
    fn result(n: usize, chars: usize) -> Value {
        let content = json!({"n": n, "tool": "query_entities", "endpoint": "ovzdusie", "result": "x".repeat(chars)});
        json!({"role": "tool", "tool_call_id": format!("c{n}"), "content": content.to_string()})
    }

    /// The rules, a question of `question` characters and `results` step results of 40 000
    /// characters each (10 000 tokens), numbered from 2.
    fn conversation(question: usize, results: usize) -> Vec<Value> {
        let mut messages = vec![
            json!({"role": "system", "content": "rules"}),
            json!({"role": "user", "content": "q".repeat(question)}),
        ];
        for n in 2..2 + results {
            messages.push(json!({"role": "assistant", "content": "", "tool_calls": []}));
            messages.push(result(n, 40_000));
        }
        messages
    }

    fn rows(n: usize, rows: usize) -> (usize, Table) {
        let table = Table {
            rows: vec![vec![json!(1)]; rows],
            ..Table::default()
        };
        (n, table)
    }

    #[test]
    fn a_call_under_the_cap_or_empty_is_sent_as_it_is() {
        let messages = conversation(4_000, 3);
        assert!(matches!(capped(&messages, &[], &[]), Cow::Borrowed(_)));
        assert!(matches!(capped(&[], &[], &[]), Cow::Borrowed(_)));
    }

    /// 75 000 tokens of question and tables and four results of 10 000: the two oldest are
    /// shortened, the newest two and the question stay whole, and the call fits (T-4731).
    #[test]
    fn a_call_over_the_cap_shortens_the_oldest_results_first() {
        let messages = conversation(300_000, 4);
        let tables = [rows(2, 7), rows(3, 2)];
        let sent = capped(&messages, &[], &tables);
        assert!(tokens_in(&sent) <= CALL_TOKENS, "{}", tokens_in(&sent));
        assert_eq!(sent[1], messages[1], "the question is whole");
        let short: Value = serde_json::from_str(sent[3]["content"].as_str().unwrap_or_default())
            .unwrap_or_default();
        assert_eq!(
            (&short["n"], &short["endpoint"], &short["rows"]),
            (&json!(2), &json!("ovzdusie"), &json!(7))
        );
        assert_ne!(sent[5], messages[5], "the second oldest is shortened too");
        assert_eq!(sent[7], messages[7]);
        assert_eq!(sent[9], messages[9]);
    }

    /// The tool specs count, and a call still over once only the newest two are whole goes as it
    /// is: the question is never cut and never refused for its size.
    #[test]
    fn a_call_that_stays_over_keeps_the_newest_two_and_the_question() {
        let messages = conversation(380_000, 3);
        let tools = vec![json!({"description": "t".repeat(40_000)})];
        let sent = capped(&messages, &tools, &[]);
        assert!(tokens_in(&sent) + tokens_in(&tools) > CALL_TOKENS);
        assert_eq!(sent.len(), messages.len());
        assert_eq!(sent[1], messages[1]);
        assert_ne!(sent[3], messages[3]);
        assert_eq!((&sent[5], &sent[7]), (&messages[5], &messages[7]));
        // a reply that is no step result is never rewritten
        let refused = vec![
            json!({"role": "user", "content": "q".repeat(420_000)}),
            json!({"role": "tool", "tool_call_id": "c", "content": "The tool refused or failed."}),
            result(2, 10),
            result(3, 10),
        ];
        assert_eq!(capped(&refused, &[], &[]).as_ref(), &refused[..]);
    }

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

    fn names(offered: &[Offered<'_>]) -> Vec<String> {
        offered.iter().map(|o| o.name.clone()).collect()
    }

    /// T-4731: the loop starts on the four first tools of the Endpoints the router read, with the
    /// escape hatch beside them; no Endpoint named, an unknown one, or nothing to cut offers every
    /// tool as before, without the hatch.
    #[test]
    fn the_loop_offers_the_routers_endpoints_first_and_everything_one_call_away() {
        let all_tools = [
            "list_types",
            "get_type",
            "list_attributes",
            "query_entities",
            "summarize",
            "get_entity",
            "query_temporal",
            "describe_schema",
        ];
        let connectors = [
            connector("bbsk/registre", &all_tools),
            connector("ovzdusie", &all_tools),
        ];
        let all = offered(&connectors);
        let (few, narrowed) = first(&all, &["ovzdusie".to_owned()]);
        assert!(narrowed);
        assert_eq!(
            names(&few),
            [
                "ovzdusie__get_type",
                "ovzdusie__query_entities",
                "ovzdusie__summarize",
                "ovzdusie__query_temporal"
            ]
        );
        let tools = tool_specs(&few, true, narrowed);
        let offered_names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert_eq!(offered_names.len(), 6, "{offered_names:?}");
        assert_eq!(offered_names[4], super::super::viz::TOOL);
        assert_eq!(offered_names[5], MORE_TOOLS);

        for named in [Vec::new(), vec!["praha".to_owned()]] {
            let (every, narrowed) = first(&all, &named);
            assert!(!narrowed, "{named:?}");
            assert_eq!(names(&every), names(&all));
            let tools = tool_specs(&every, false, narrowed);
            assert!(tools.iter().all(|t| t["function"]["name"] != MORE_TOOLS));
        }
        // A connector that offers no more than the first tools has nothing to cut.
        let small = [connector("ovzdusie", &["query_entities", "get_type"])];
        let all = offered(&small);
        assert!(!first(&all, &["ovzdusie".to_owned()]).1);
        // The rules say when to reach for the hatch, the same text for every question.
        assert!(DATA_RULES.contains("When a more_tools tool is offered"));
        assert_eq!(
            more_tools()["function"]["parameters"]["properties"],
            json!({})
        );
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
