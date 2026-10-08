//! Per-App counters without a label per App (ADR-N-044 §2.7, AP-147): the top N Apps by requests
//! are named, every other App is summed under `app="_other"`, so 10 000 Apps stay N + 1 series.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

/// How a request ended, as the counters count it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Answered,
    Busy,
    Timeout,
    Failed,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Counts {
    requests: u64,
    busy: u64,
    timeouts: u64,
    failures: u64,
    seconds: f64,
}

impl Counts {
    fn add(&mut self, other: &Counts) {
        self.requests += other.requests;
        self.busy += other.busy;
        self.timeouts += other.timeouts;
        self.failures += other.failures;
        self.seconds += other.seconds;
    }
}

pub struct Metrics {
    top: usize,
    apps: Mutex<HashMap<String, Counts>>,
}

impl Metrics {
    pub fn new(top: usize) -> Self {
        Self {
            top,
            apps: Mutex::new(HashMap::new()),
        }
    }

    pub fn record(&self, app: &str, outcome: Outcome, took: Duration) {
        let Ok(mut apps) = self.apps.lock() else {
            return;
        };
        let counts = apps.entry(app.to_owned()).or_default();
        counts.requests += 1;
        counts.seconds += took.as_secs_f64();
        match outcome {
            Outcome::Answered => {}
            Outcome::Busy => counts.busy += 1,
            Outcome::Timeout => counts.timeouts += 1,
            Outcome::Failed => counts.failures += 1,
        }
    }

    /// The Prometheus text: the top N Apps by requests, `_other` for the rest, and the shard's
    /// totals; `cached` is how many components the shard keeps compiled.
    pub fn render(&self, shard: &str, cached: usize) -> String {
        let apps = self.apps.lock().map(|a| a.clone()).unwrap_or_default();
        let mut ranked: Vec<(String, Counts)> = apps.into_iter().collect();
        ranked.sort_by(|a, b| b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(&b.0)));
        let mut other = Counts::default();
        for (_, counts) in ranked.iter().skip(self.top) {
            other.add(counts);
        }
        let mut series: Vec<(String, Counts)> = ranked.into_iter().take(self.top).collect();
        if other.requests > 0 {
            series.push(("_other".into(), other));
        }
        let mut out = String::new();
        let _ = writeln!(out, "# TYPE jc_wasm_components_cached gauge\njc_wasm_components_cached{{shard=\"{shard}\"}} {cached}");
        for (name, help, pick) in [
            (
                "jc_wasm_requests_total",
                "counter",
                (|c: &Counts| c.requests as f64) as fn(&Counts) -> f64,
            ),
            ("jc_wasm_busy_total", "counter", |c: &Counts| c.busy as f64),
            ("jc_wasm_timeouts_total", "counter", |c: &Counts| {
                c.timeouts as f64
            }),
            ("jc_wasm_failures_total", "counter", |c: &Counts| {
                c.failures as f64
            }),
            ("jc_wasm_request_seconds_total", "counter", |c: &Counts| {
                c.seconds
            }),
        ] {
            let _ = writeln!(out, "# TYPE {name} {help}");
            for (app, counts) in &series {
                let _ = writeln!(
                    out,
                    "{name}{{shard=\"{shard}\",app=\"{app}\"}} {}",
                    pick(counts)
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_top_apps_are_named_and_the_rest_summed() {
        let metrics = Metrics::new(2);
        for (app, n) in [("a", 5), ("b", 3), ("c", 1), ("d", 1)] {
            for _ in 0..n {
                metrics.record(app, Outcome::Answered, Duration::from_millis(10));
            }
        }
        metrics.record("d", Outcome::Timeout, Duration::from_secs(5));
        let text = metrics.render("s1", 4);
        assert!(
            text.contains("jc_wasm_requests_total{shard=\"s1\",app=\"a\"} 5"),
            "{text}"
        );
        assert!(
            text.contains("jc_wasm_requests_total{shard=\"s1\",app=\"b\"} 3"),
            "{text}"
        );
        assert!(
            text.contains("jc_wasm_requests_total{shard=\"s1\",app=\"_other\"} 3"),
            "{text}"
        );
        assert!(
            text.contains("jc_wasm_timeouts_total{shard=\"s1\",app=\"_other\"} 1"),
            "{text}"
        );
        assert!(
            !text.contains("app=\"c\"") && !text.contains("app=\"d\""),
            "{text}"
        );
        assert!(text.contains("jc_wasm_components_cached{shard=\"s1\"} 4"));
    }
}
