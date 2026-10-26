//! What one turn did, step by step, for its stored transcript (AG-99, API/05 §3.1, T-4670): the
//! route, each search, each tool call with its input and output, and each model call with its
//! model, provider, tokens and price. The answer loop writes here as it goes, so a turn cut by
//! the timeout still has the steps and the prices it reached. Since T-4681 a model call keeps
//! its cached tokens and reasoning text too, each table, chart and map shown is a `visual` step,
//! and [`with_gaps`] names the time no step accounts for.

use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use super::agent::Event;
use super::model::{CallError, Completion};

/// The most bytes of a step's input or output kept; the full size is noted beside it (T-4681).
pub const KEPT_BYTES: usize = 64 * 1024;

/// The rows a longer JSON array output keeps (T-4681).
pub const KEPT_ROWS: usize = 20;

/// One step of a turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// `route`, `search`, `tool`, `model`, `answer`, `visual` or `untracked`.
    pub kind: &'static str,
    pub name: String,
    pub endpoint: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    /// The output's whole size in bytes, when it was cut.
    pub output_bytes: Option<u64>,
    /// `ok` or `failed`.
    pub status: &'static str,
    /// When the step started and how long it took, in milliseconds from the turn's start.
    pub start_ms: u64,
    pub duration_ms: u64,
    pub error: Option<String>,
    /// A model call's stage: `router`, `judge`, `data`, `agent` or `answer`.
    pub stage: Option<&'static str>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    /// The provider's price in millionths of a US dollar; `None` when it sent none (T-4178).
    pub cost_micros: Option<u64>,
    /// A model call's cached prompt tokens, reasoning text, finish reason, effort and prompt
    /// version, each as the provider and the request said (T-4681).
    pub cached_tokens: Option<u64>,
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
    pub effort: Option<String>,
    pub prompt_version: Option<String>,
    /// The rows a `visual` step showed, or a tool's JSON answer held (T-5779).
    pub rows: Option<u64>,
}

/// A model call on its way; see [`Trace::pending`].
#[derive(Debug)]
pub struct Pending<'a> {
    trace: &'a Trace,
    stage: &'static str,
    model: String,
    started: Instant,
    /// The input tokens guessed from the request's size, four bytes a token: for the error
    /// text only, never summed with the tokens a provider reported.
    estimate: u64,
    settled: bool,
}

impl Pending<'_> {
    /// The call's reply or failure, recorded as [`Trace::model`] does.
    pub fn done(mut self, result: Result<&Completion, &CallError>) {
        self.settled = true;
        self.trace
            .model(self.stage, &self.model, self.started, result);
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut step = self.trace.timed("model", self.stage, self.started);
        step.stage = Some(self.stage);
        step.model = Some(std::mem::take(&mut self.model));
        step.status = "failed";
        step.error = Some(format!(
            "aborted before the reply; about {} input tokens, the price unknown",
            self.estimate
        ));
        self.trace.push(step);
    }
}

/// The steps of one turn, written by the answer loop and read once the turn ends.
#[derive(Debug)]
pub struct Trace {
    started: Instant,
    steps: Mutex<Vec<Step>>,
    /// Whether the turn is an anonymous visitor's: only then may its log lines carry what was
    /// asked (tool arguments, a search's words), redacted and cut (T-5779).
    anonymous: bool,
}

impl Default for Trace {
    fn default() -> Self {
        Trace::new(false)
    }
}

/// The most characters of a step's input or error a log line carries (T-5779).
const LOGGED_CHARS: usize = 300;

/// Argument names whose values no log line carries, whoever asked (T-5779).
const HIDDEN_KEYS: [&str; 7] = [
    "authorization",
    "cookie",
    "token",
    "password",
    "secret",
    "apikey",
    "api_key",
];

fn cut_chars(text: &str, most: usize) -> String {
    match text.char_indices().nth(most) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// `value` with every member a [`HIDDEN_KEYS`] name holds (any case, any depth) replaced.
fn hidden(value: &Value) -> Value {
    match value {
        Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(key, member)| {
                    let lower = key.to_ascii_lowercase();
                    if HIDDEN_KEYS.iter().any(|h| lower.contains(h)) {
                        (key.clone(), Value::String("[hidden]".into()))
                    } else {
                        (key.clone(), hidden(member))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(hidden).collect()),
        other => other.clone(),
    }
}

/// What a step's log line says of its input: for an anonymous visitor the input with its
/// secrets, personal data and [`HIDDEN_KEYS`] members masked, cut at [`LOGGED_CHARS`]; for a
/// signed-in person only a tool's argument names, since the values may carry their question.
fn logged_input(step: &Step, anonymous: bool) -> Option<String> {
    let input = step.input.as_deref()?;
    let parsed = serde_json::from_str::<Value>(input).ok();
    if !anonymous {
        if step.kind != "tool" {
            return None;
        }
        let names: Vec<&str> = parsed
            .as_ref()?
            .as_object()?
            .keys()
            .map(String::as_str)
            .collect();
        return Some(format!("[{}]", names.join(",")));
    }
    let text = parsed.map_or_else(|| input.to_owned(), |value| hidden(&value).to_string());
    Some(cut_chars(
        &super::redact::redact(&super::redact::secrets(&text)),
        LOGGED_CHARS,
    ))
}

/// The HTTP status a failed call's error names (`the Endpoint answered 503 …`).
fn http_status(error: &str) -> Option<u16> {
    let after = error.split("answered ").nth(1)?;
    after.get(..3)?.parse().ok()
}

/// The rows a tool's JSON answer holds: its array's items, or its `entities`' or `results'`.
fn rows_of(text: &str) -> Option<u64> {
    let value: Value = serde_json::from_str(text).ok()?;
    let items = value
        .as_array()
        .or_else(|| value.get("entities").and_then(Value::as_array))
        .or_else(|| value.get("results").and_then(Value::as_array))?;
    u64::try_from(items.len()).ok()
}

/// One INFO line for `step`, in the `question` span of its turn (T-5779): what it was, where it
/// went, how it ended and how long it took. A step's output text never goes in; a model call's
/// tool names, a search's timing line and a tool's rows and bytes do.
fn log(step: &Step, anonymous: bool) {
    let input = logged_input(step, anonymous);
    let error = step
        .error
        .as_deref()
        .map(|e| cut_chars(&super::redact::secrets(e), LOGGED_CHARS));
    let http = step.error.as_deref().and_then(http_status);
    let bytes = step.output_bytes.or_else(|| {
        step.output
            .as_ref()
            .and_then(|o| u64::try_from(o.len()).ok())
    });
    // A model call's output is its tool names; a search's last line is where its time went.
    let detail = match step.kind {
        "model" => step.output.clone(),
        "search" => step
            .output
            .as_deref()
            .and_then(|o| o.lines().last())
            .filter(|line| line.starts_with("time:"))
            .map(str::to_owned),
        _ => None,
    };
    tracing::info!(
        kind = step.kind,
        name = %step.name,
        endpoint = step.endpoint.as_deref(),
        status = step.status,
        http,
        start_ms = step.start_ms,
        ms = step.duration_ms,
        stage = step.stage,
        model = step.model.as_deref(),
        tokens_in = step.tokens_in,
        tokens_out = step.tokens_out,
        finish = step.finish_reason.as_deref(),
        rows = step.rows,
        bytes,
        detail = detail.as_deref(),
        input = input.as_deref(),
        error = error.as_deref(),
        "step"
    );
}

fn ms(from: Instant, to: Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_millis()).unwrap_or(u64::MAX)
}

/// `text` whole up to [`KEPT_BYTES`]; a longer JSON array as its first [`KEPT_ROWS`] items when
/// they fit, anything else cut on a character boundary; with its whole size when it was cut.
pub fn kept(text: &str) -> (String, Option<u64>) {
    if text.len() <= KEPT_BYTES {
        return (text.to_owned(), None);
    }
    let whole = u64::try_from(text.len()).ok();
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) {
        let first = Value::Array(items.into_iter().take(KEPT_ROWS).collect()).to_string();
        if first.len() <= KEPT_BYTES {
            return (first, whole);
        }
    }
    let mut end = KEPT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), whole)
}

/// `steps` in the order they started, with an `untracked` step for each stretch of the turn's
/// `total_ms` that no step covers, so the turn's time is accounted for whole (T-4681, T-4680).
/// Steps that overlap (tools called side by side) cover their union.
pub fn with_gaps(mut steps: Vec<Step>, total_ms: u64) -> Vec<Step> {
    steps.sort_by_key(|s| s.start_ms);
    let mut out = Vec::with_capacity(steps.len() * 2 + 1);
    let mut covered = 0u64;
    let gap = |from: u64, to: u64| Step {
        kind: "untracked",
        name: "untracked".to_owned(),
        status: "ok",
        start_ms: from,
        duration_ms: to - from,
        ..Step::default()
    };
    for step in steps {
        if step.start_ms > covered {
            out.push(gap(covered, step.start_ms));
        }
        covered = covered.max(step.start_ms.saturating_add(step.duration_ms));
        out.push(step);
    }
    if total_ms > covered {
        out.push(gap(covered, total_ms));
    }
    out
}

impl Trace {
    /// A turn's trace; `anonymous` when an anonymous visitor asked, whose log lines may then carry
    /// the tool arguments and search words, redacted and cut.
    pub fn new(anonymous: bool) -> Self {
        Trace {
            started: Instant::now(),
            steps: Mutex::new(Vec::new()),
            anonymous,
        }
    }

    fn push(&self, step: Step) {
        log(&step, self.anonymous);
        if let Ok(mut steps) = self.steps.lock() {
            steps.push(step);
        }
    }

    fn timed(&self, kind: &'static str, name: &str, started: Instant) -> Step {
        let now = Instant::now();
        Step {
            kind,
            name: name.to_owned(),
            status: "ok",
            start_ms: ms(self.started, started),
            duration_ms: ms(started, now),
            ..Step::default()
        }
    }

    /// A model call of `stage` on `model` that started at `started`.
    pub fn model(
        &self,
        stage: &'static str,
        model: &str,
        started: Instant,
        result: Result<&Completion, &CallError>,
    ) {
        let mut step = self.timed("model", stage, started);
        step.stage = Some(stage);
        step.model = Some(model.to_owned());
        match result {
            Ok(done) => {
                step.provider = done.provider.clone();
                step.tokens_in = Some(done.input);
                step.tokens_out = Some(done.output);
                step.reasoning_tokens = done.reasoning;
                step.cost_micros = done.cost_micros;
                step.cached_tokens = done.cached;
                step.reasoning.clone_from(&done.thought);
                step.finish_reason.clone_from(&done.finish);
                step.effort.clone_from(&done.effort);
                step.prompt_version.clone_from(&done.prompt);
                if !done.calls.is_empty() {
                    let names: Vec<&str> = done.calls.iter().map(|c| c.1.as_str()).collect();
                    step.output = Some(format!("tools: {}", names.join(", ")));
                }
            }
            Err(err) => {
                step.status = "failed";
                step.error = Some(err.to_string());
            }
        }
        self.push(step);
    }

    /// A model call of `stage` on `model` about to be sent with `messages` (T-4728): settle it
    /// with [`Pending::done`]. A call dropped before its reply (a judge cut off by a direct route,
    /// the turn's timeout) is recorded when the guard drops, as a failed, unpriced call without
    /// tokens (the estimate only in its error), so the turn reads `uncosted`, never a cost or a
    /// token count it does not know.
    pub fn pending<'a>(
        &'a self,
        stage: &'static str,
        model: &str,
        started: Instant,
        messages: &[Value],
    ) -> Pending<'a> {
        let bytes = serde_json::to_string(messages).map_or(0, |text| text.len());
        Pending {
            trace: self,
            stage,
            model: model.to_owned(),
            started,
            estimate: u64::try_from(bytes / 4).unwrap_or(u64::MAX),
            settled: false,
        }
    }

    /// A tool call that started at `started`.
    pub fn tool(
        &self,
        name: &str,
        endpoint: Option<&str>,
        input: &Value,
        started: Instant,
        result: &Result<String, String>,
    ) {
        let mut step = self.timed("tool", name, started);
        step.endpoint = endpoint.map(str::to_owned);
        step.input = Some(kept(&input.to_string()).0);
        match result {
            Ok(text) => {
                let (output, whole) = kept(text);
                step.output = Some(output);
                step.output_bytes = whole;
                step.rows = rows_of(text);
            }
            Err(err) => {
                step.status = "failed";
                step.error = Some(err.clone());
            }
        }
        self.push(step);
    }

    /// A step that is neither a model nor a tool call: the route taken, a search and its hits.
    pub fn note(
        &self,
        kind: &'static str,
        name: &str,
        started: Instant,
        input: Option<&str>,
        output: Option<&str>,
    ) {
        let mut step = self.timed(kind, name, started);
        step.input = input.map(|text| kept(text).0);
        if let Some(text) = output {
            let (output, whole) = kept(text);
            step.output = Some(output);
            step.output_bytes = whole;
        }
        self.push(step);
    }

    /// A table, chart or map the visitor was shown, as a `visual` step: its spec without the
    /// data, and its rows. It is built in line with the answer, so its time is the moment it went
    /// out; any other event is no step.
    pub fn visual(&self, event: &Event) {
        let (name, spec, rows) = match event {
            Event::Data(table) => (
                "table",
                serde_json::json!({"type": table.entity_type, "columns": table.columns,
                    "units": table.units, "total": table.total}),
                table.rows.len(),
            ),
            Event::Visualization(shown) => {
                let mut spec = shown.get("spec").cloned().unwrap_or(Value::Null);
                let map =
                    spec.get("$schema").and_then(Value::as_str) == Some(super::viz::MAP_SCHEMA);
                let rows = if map {
                    spec.pointer("/data/features")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                } else {
                    shown
                        .get("rows")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                };
                if let Some(spec) = spec.as_object_mut() {
                    spec.remove("data");
                }
                (if map { "map" } else { "chart" }, spec, rows)
            }
            _ => return,
        };
        let mut step = self.timed("visual", name, Instant::now());
        step.input = Some(kept(&spec.to_string()).0);
        step.rows = u64::try_from(rows).ok();
        self.push(step);
    }

    /// The milliseconds since the turn's trace began, the connectors' setup included.
    pub fn elapsed_ms(&self) -> u64 {
        ms(self.started, Instant::now())
    }

    /// The steps so far, in the order they ended.
    pub fn steps(&self) -> Vec<Step> {
        self.steps.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// The route the turn took: the name of its first `route` step, `direct` without one.
    pub fn route(&self) -> String {
        self.steps()
            .iter()
            .find(|s| s.kind == "route")
            .map_or_else(|| "direct".to_owned(), |s| s.name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn completion(cost: Option<u64>) -> Completion {
        Completion {
            text: "ok".into(),
            calls: vec![("1".into(), "query_entities".into(), "{}".into())],
            tokens: 30,
            input: 20,
            output: 10,
            cost_micros: cost,
            provider: Some("Google".into()),
            reasoning: Some(4),
            finish: Some("stop".into()),
            ..Default::default()
        }
    }

    #[test]
    fn model_tool_and_note_steps_are_kept_in_order_with_their_prices() {
        let trace = Trace::default();
        trace.note(
            "route",
            "data",
            Instant::now(),
            Some("Aké je ovzdušie?"),
            None,
        );
        let (priced, unpriced) = (completion(Some(1_200)), completion(None));
        trace.model("router", "google/x", Instant::now(), Ok(&priced));
        trace.model("answer", "google/x", Instant::now(), Ok(&unpriced));
        let failed = CallError::Unavailable("down".into());
        trace.model("judge", "google/x", Instant::now(), Err(&failed));
        trace.tool(
            "query_entities",
            Some("ovzdusie"),
            &json!({"type": "AirQualityObserved"}),
            Instant::now(),
            &Err("the tool failed: 500".into()),
        );
        let steps = trace.steps();
        assert_eq!(
            steps.iter().map(|s| (s.kind, s.status)).collect::<Vec<_>>(),
            [
                ("route", "ok"),
                ("model", "ok"),
                ("model", "ok"),
                ("model", "failed"),
                ("tool", "failed")
            ]
        );
        assert_eq!(steps[1].cost_micros, Some(1_200));
        assert_eq!(steps[2].cost_micros, None, "no price is never a zero price");
        assert_eq!(steps[1].output.as_deref(), Some("tools: query_entities"));
        assert_eq!(steps[3].error.as_deref(), Some("down"));
        assert_eq!(steps[4].endpoint.as_deref(), Some("ovzdusie"));
        assert_eq!(trace.route(), "data");
        assert_eq!(Trace::default().route(), "direct");
    }

    #[test]
    fn an_output_past_64_kb_is_cut_on_a_character_and_its_size_kept() {
        let long = "ž".repeat(40_000); // 80,000 bytes
        let (cut, whole) = kept(&long);
        assert!(cut.len() <= KEPT_BYTES && cut.len() > KEPT_BYTES - 2);
        assert!(cut.chars().all(|c| c == 'ž'));
        assert_eq!(whole, Some(80_000));
        assert_eq!(kept("krátke"), ("krátke".to_owned(), None));
        let rows = Value::Array(
            (0..5_000)
                .map(|i| json!({"id": i, "name": "Stanica"}))
                .collect(),
        )
        .to_string();
        let (first, size) = kept(&rows);
        let first: Vec<Value> = serde_json::from_str(&first).expect("still JSON");
        assert_eq!(
            (first.len(), first[19]["id"].as_u64()),
            (KEPT_ROWS, Some(19))
        );
        assert_eq!(size, u64::try_from(rows.len()).ok());
    }

    #[test]
    fn a_call_dropped_before_its_reply_is_an_unpriced_failed_step_and_a_settled_one_is_not() {
        let trace = Trace::default();
        let messages = [json!({"role": "user", "content": "x".repeat(400)})];
        drop(trace.pending("judge", "google/x", Instant::now(), &messages));
        let done = completion(Some(30));
        trace
            .pending("router", "google/x", Instant::now(), &messages)
            .done(Ok(&done));
        let steps = trace.steps();
        assert_eq!(steps.len(), 2);
        let aborted = &steps[0];
        assert_eq!(
            (aborted.kind, aborted.stage, aborted.status),
            ("model", Some("judge"), "failed")
        );
        assert_eq!(aborted.cost_micros, None);
        assert_eq!(
            aborted.tokens_in, None,
            "an estimate is never a measured token: {aborted:?}"
        );
        assert!(
            aborted
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("aborted") && e.contains("input tokens")),
            "{aborted:?}"
        );
        assert_eq!(steps[1].cost_micros, Some(30));
        assert_eq!(super::super::transcript::priced(&steps), (30, 1));
    }

    fn at(kind: &'static str, start_ms: u64, duration_ms: u64) -> Step {
        Step {
            kind,
            start_ms,
            duration_ms,
            ..Step::default()
        }
    }

    /// T-4681: the steps and the untracked stretches between them add up to the turn's time.
    #[test]
    fn the_time_no_step_covers_is_an_untracked_step() {
        let steps = with_gaps(
            vec![
                at("model", 100, 400),
                at("tool", 600, 300),
                at("tool", 650, 100),
                at("answer", 900, 50),
            ],
            1_200,
        );
        assert_eq!(
            steps
                .iter()
                .map(|s| (s.kind, s.start_ms, s.duration_ms))
                .collect::<Vec<_>>(),
            [
                ("untracked", 0, 100),
                ("model", 100, 400),
                ("untracked", 500, 100),
                ("tool", 600, 300),
                ("tool", 650, 100),
                ("answer", 900, 50),
                ("untracked", 950, 250)
            ]
        );
        let untracked: u64 = steps
            .iter()
            .filter(|s| s.kind == "untracked")
            .map(|s| s.duration_ms)
            .sum();
        assert_eq!(
            untracked + 400 + 300 + 50,
            1_200,
            "overlapping tools count once"
        );
        assert_eq!(
            with_gaps(vec![at("model", 0, 10)], 10).len(),
            1,
            "no gap, no step"
        );
        assert_eq!(with_gaps(Vec::new(), 0).len(), 0);
    }

    /// T-4681: a model step keeps the cached tokens, the reasoning text and what was asked.
    #[test]
    fn a_model_step_keeps_its_cache_its_reasoning_and_its_prompt_version() {
        let trace = Trace::default();
        let done = Completion {
            cached: Some(640),
            thought: Some("Hľadám PM10.".into()),
            effort: Some("low".into()),
            prompt: Some("0a1b2c3d4e5f".into()),
            ..completion(Some(10))
        };
        trace.model("answer", "google/x", Instant::now(), Ok(&done));
        let step = &trace.steps()[0];
        assert_eq!(step.cached_tokens, Some(640));
        assert_eq!(step.reasoning.as_deref(), Some("Hľadám PM10."));
        assert_eq!(
            (step.finish_reason.as_deref(), step.effort.as_deref()),
            (Some("stop"), Some("low"))
        );
        assert_eq!(step.prompt_version.as_deref(), Some("0a1b2c3d4e5f"));
    }

    /// T-4681: a table, a chart and a map are visual steps with their spec and rows, the data
    /// left out; any other event is no step.
    #[test]
    fn what_the_visitor_was_shown_is_a_visual_step() {
        let trace = Trace::default();
        let table = super::super::agent::Table {
            endpoint: "ovzdusie".into(),
            entity_type: "AirQualityObserved".into(),
            columns: vec!["name".into(), "pm10".into()],
            rows: vec![
                vec![json!("SNP"), json!(21)],
                vec![json!("Hronka"), json!(9)],
            ],
            units: vec![None, Some("µg/m³".into())],
            labels: vec![None, None],
            summary: vec![false, false],
            enums: vec![None, None],
            total: Some(12),
            dataset: None,
            download: None,
            geojson: None,
            map: None,
            geometries: vec![],
            profile: None,
        };
        trace.visual(&Event::Data(table));
        trace.visual(&Event::Visualization(
            json!({"spec": {"mark": "bar", "data": {"name": "source"}},
            "rows": [{"a": 1}, {"a": 2}, {"a": 3}]}),
        ));
        trace.visual(&Event::Visualization(
            json!({"spec": {"$schema": super::super::viz::MAP_SCHEMA,
            "data": {"type": "FeatureCollection", "features": [{}, {}]}}}),
        ));
        trace.visual(&Event::Delta("text".into()));
        let steps = trace.steps();
        assert_eq!(
            steps
                .iter()
                .map(|s| (s.kind, s.name.as_str(), s.rows))
                .collect::<Vec<_>>(),
            [
                ("visual", "table", Some(2)),
                ("visual", "chart", Some(3)),
                ("visual", "map", Some(2))
            ]
        );
        assert!(steps[0]
            .input
            .as_deref()
            .is_some_and(|i| i.contains("µg/m³") && !i.contains("Hronka")));
        assert!(steps
            .iter()
            .skip(1)
            .all(|s| !s.input.as_deref().unwrap_or("").contains("data")));
    }

    /// The log lines `run` writes, as the worker's subscriber formats them (T-5779).
    fn logged(run: impl FnOnce()) -> String {
        use std::sync::Arc;
        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if let Ok(mut all) = self.0.lock() {
                    all.extend_from_slice(bytes);
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = lines.0.lock().map(|b| b.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn a_tool_call_logs_its_endpoint_status_http_rows_and_ms() {
        let out = logged(|| {
            let trace = Trace::new(true);
            trace.tool(
                "query_entities",
                Some("ovzdusie-sk"),
                &json!({"type": "AirQualityObserved", "limit": 5}),
                Instant::now(),
                &Ok(r#"[{"id":"a"},{"id":"b"}]"#.into()),
            );
            trace.tool(
                "query_entities",
                Some("podujatia"),
                &json!({"type": "Event"}),
                Instant::now(),
                &Err("the Endpoint answered 503 Service Unavailable".into()),
            );
        });
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        for (line, endpoint) in lines.iter().zip(["ovzdusie-sk", "podujatia"]) {
            assert!(line.contains(&format!("endpoint=\"{endpoint}\"")), "{line}");
            assert!(line.contains(" ms="), "{line}");
            assert!(line.contains("kind=\"tool\""), "{line}");
            assert!(line.contains("name=query_entities"), "{line}");
        }
        assert!(lines[0].contains("status=\"ok\""), "{}", lines[0]);
        assert!(lines[0].contains("rows=2"), "{}", lines[0]);
        assert!(lines[0].contains("AirQualityObserved"), "{}", lines[0]);
        assert!(lines[1].contains("status=\"failed\""), "{}", lines[1]);
        assert!(lines[1].contains("http=503"), "{}", lines[1]);
        assert!(lines[1].contains("answered 503"), "{}", lines[1]);
    }

    #[test]
    fn a_model_call_logs_its_model_tokens_finish_and_tool_names() {
        let out = logged(|| {
            Trace::new(false).model(
                "answer",
                "google/x",
                Instant::now(),
                Ok(&completion(Some(9))),
            );
        });
        for field in [
            "kind=\"model\"",
            "stage=\"answer\"",
            "model=\"google/x\"",
            "tokens_in=20",
            "tokens_out=10",
            "finish=\"stop\"",
            "detail=\"tools: query_entities\"",
            " ms=",
        ] {
            assert!(out.contains(field), "{field} missing: {out}");
        }
    }

    #[test]
    fn a_search_logs_where_its_time_went_and_never_a_signed_in_persons_words() {
        let out = logged(|| {
            Trace::new(false).note(
                "search",
                "search",
                Instant::now(),
                Some("moja diagnóza a adresa"),
                Some("0.031 https://x.sk/a\ntime: embedding waited 2 ms, ran 40 ms; index 900 ms"),
            );
        });
        assert!(
            out.contains("detail=\"time: embedding waited 2 ms, ran 40 ms; index 900 ms\""),
            "{out}"
        );
        assert!(!out.contains("diagnóza"), "{out}");
        assert!(!out.contains("x.sk"), "a search's hits stay out: {out}");
    }

    #[test]
    fn no_log_line_carries_a_token_an_authorization_header_or_personal_data() {
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJqYW4ifQ.c2lnbmF0dXJlLXNpZ25hdHVyZQ";
        let arguments = json!({
            "q": format!("name==\"jan.novak@mesto.sk\";note==\"Bearer {jwt}\""),
            "headers": {"Authorization": format!("Bearer {jwt}"), "Cookie": "session=abc123"},
            "token": "plain-secret-value",
        });
        let out = logged(|| {
            for anonymous in [true, false] {
                let trace = Trace::new(anonymous);
                trace.tool(
                    "query_entities",
                    Some("ep"),
                    &arguments,
                    Instant::now(),
                    &Err(format!("the Endpoint refused: Bearer {jwt}")),
                );
            }
        });
        assert_eq!(out.lines().count(), 2, "{out}");
        for secret in [jwt, "abc123", "plain-secret-value", "jan.novak@mesto.sk"] {
            assert!(!out.contains(secret), "{secret} logged: {out}");
        }
        let signed_in = out.lines().nth(1).unwrap_or_default();
        assert!(
            signed_in.contains("input=\"[q,headers,token]\""),
            "{signed_in}"
        );
        assert!(signed_in.contains("endpoint=\"ep\""), "{signed_in}");
    }
}
