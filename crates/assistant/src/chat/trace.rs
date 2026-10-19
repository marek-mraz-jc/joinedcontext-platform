//! What one turn did, step by step, for its stored transcript (AG-99, API/05 §3.1, T-4670): the
//! route, each search, each tool call with its input and output, and each model call with its
//! model, provider, tokens and price. The answer loop writes here as it goes, so a turn cut by
//! the timeout still has the steps and the prices it reached.

use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use super::model::{CallError, Completion};

/// The most bytes of a step's input or output kept; the full size is noted beside it.
pub const KEPT_BYTES: usize = 8 * 1024;

/// One step of a turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// `route`, `search`, `tool`, `model` or `answer`.
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

/// `text` cut at [`KEPT_BYTES`] on a character boundary, with its whole size when it was cut.
pub fn kept(text: &str) -> (String, Option<u64>) {
    if text.len() <= KEPT_BYTES {
        return (text.to_owned(), None);
    }
    let mut end = KEPT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), u64::try_from(text.len()).ok())
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
    fn an_output_past_8_kb_is_cut_on_a_character_and_its_size_kept() {
        let long = "ž".repeat(5_000); // 10,000 bytes
        let (cut, whole) = kept(&long);
        assert!(cut.len() <= KEPT_BYTES && cut.len() > KEPT_BYTES - 2);
        assert!(cut.chars().all(|c| c == 'ž'));
        assert_eq!(whole, Some(10_000));
        assert_eq!(kept("krátke"), ("krátke".to_owned(), None));
    }
}
