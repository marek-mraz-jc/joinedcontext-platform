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
    /// The rows a `visual` step showed.
    pub rows: Option<u64>,
}

/// The steps of one turn, written by the answer loop and read once the turn ends.
#[derive(Debug)]
pub struct Trace {
    started: Instant,
    steps: Mutex<Vec<Step>>,
}

impl Default for Trace {
    fn default() -> Self {
        Trace {
            started: Instant::now(),
            steps: Mutex::new(Vec::new()),
        }
    }
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
    fn push(&self, step: Step) {
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
            total: Some(12),
            dataset: None,
            download: None,
            map: None,
            geometries: vec![],
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
}
