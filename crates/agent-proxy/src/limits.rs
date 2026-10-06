//! Per-run rate-limiting, concurrency, and token budget enforcement (AG-41).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// A run's usage. Everything lives under one lock: a request is counted in the same critical
/// section that checks it, so two requests arriving together cannot both pass as the last one.
#[derive(Default)]
struct RunUsage {
    rpm_window: Option<Instant>,
    rpm_count: u32,
    tokens_consumed: u64,
    /// Model calls the run has made: one call is one step (AG-25, AG-51).
    steps: u32,
    /// Bytes the run has read from the internet through the fetch route (AG-65).
    egress_bytes: u64,
}

/// One minute, the window `requests_per_minute` is counted over.
const WINDOW: Duration = Duration::from_secs(60);

/// The tokens spent on one UTC day, per consumer and per person (AG-97).
#[derive(Default)]
struct DailySpend {
    day: u64,
    consumers: HashMap<&'static str, u64>,
    people: HashMap<String, u64>,
}

/// Whose daily budget a run's calls count against (AG-97).
pub fn consumer_of(kind: &str) -> &'static str {
    match kind {
        "conversation" => "assistant",
        "application" => "app-builder",
        _ => "other",
    }
}

/// Days since the epoch, UTC: the day a daily budget belongs to.
fn utc_day() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 86_400)
}

#[derive(Clone, Default)]
pub struct LimitManager {
    runs: Arc<Mutex<HashMap<String, RunUsage>>>,
    // ponytail: in memory, per replica; a restart or a second replica starts the day again from
    // zero. Persist in the Portal when one replica's day is not the installation's.
    daily: Arc<Mutex<DailySpend>>,
}

impl LimitManager {
    /// Counts one request of the run against its per-minute limit; refuses the one past it.
    pub async fn check_rpm(&self, run_id: &str, limit: u32) -> Result<(), &'static str> {
        self.check_rpm_at(run_id, limit, Instant::now()).await
    }

    async fn check_rpm_at(
        &self,
        run_id: &str,
        limit: u32,
        now: Instant,
    ) -> Result<(), &'static str> {
        let mut map = self.runs.lock().await;
        let entry = map.entry(run_id.to_string()).or_default();
        // ponytail: a fixed window that restarts a minute after its first request; a sliding
        // window is the upgrade if bursts at the boundary ever matter.
        match entry.rpm_window {
            Some(started) if now.duration_since(started) < WINDOW => {}
            _ => {
                entry.rpm_window = Some(now);
                entry.rpm_count = 0;
            }
        }
        if entry.rpm_count >= limit {
            return Err("rate limit exceeded (RPM limit)");
        }
        entry.rpm_count += 1;
        Ok(())
    }

    /// Counts one model call of the run against the profile's `stepsPerRun`; refuses the one past
    /// it, so a run that loops stops at the ceiling its profile declares (AG-25, AG-51). A limit of
    /// `0` counts nothing: a Portal that does not send the field yet bounds the run by tokens alone.
    pub async fn check_steps(&self, run_id: &str, limit: u32) -> Result<(), &'static str> {
        if limit == 0 {
            return Ok(());
        }
        // Counted in the same critical section that checks it, as the per-minute window is.
        let mut map = self.runs.lock().await;
        let entry = map.entry(run_id.to_string()).or_default();
        if entry.steps >= limit {
            return Err("step limit exceeded (stepsPerRun)");
        }
        entry.steps += 1;
        Ok(())
    }

    pub async fn check_tokens(&self, run_id: &str, limit: u64) -> Result<(), &'static str> {
        let map = self.runs.lock().await;
        let consumed = map.get(run_id).map_or(0, |entry| entry.tokens_consumed);
        if consumed >= limit {
            return Err("token budget exhausted");
        }
        Ok(())
    }

    /// What is left of the run's egress budget. `0` refuses the next fetch, and it is also what
    /// a run with no budget at all has, so a profile that names no host reaches nothing without
    /// a second rule saying so (AG-50, AG-65).
    pub async fn egress_remaining(&self, run_id: &str, budget: u64) -> u64 {
        let map = self.runs.lock().await;
        let spent = map.get(run_id).map_or(0, |entry| entry.egress_bytes);
        budget.saturating_sub(spent)
    }

    /// Counts one answer against the budget and says what is left.
    ///
    /// Counted after the answer arrives rather than reserved before it: the size is not known
    /// until then, and one answer is already bounded by the run's `maxResponseBytes`. So a run
    /// may cross its budget by at most one answer, and the fetch after it is refused.
    pub async fn record_egress(&self, run_id: &str, bytes: u64, budget: u64) -> u64 {
        let mut map = self.runs.lock().await;
        let entry = map.entry(run_id.to_string()).or_default();
        entry.egress_bytes = entry.egress_bytes.saturating_add(bytes);
        budget.saturating_sub(entry.egress_bytes)
    }

    /// Refuses a call of a `kind` run started by `person` once today's spend of its consumer or
    /// of the person has reached its cap; the sentence says whose budget it is (AG-97).
    pub async fn check_daily(
        &self,
        caps: &crate::config::DailyCaps,
        kind: &str,
        person: &str,
    ) -> Result<(), String> {
        self.check_daily_on(caps, kind, person, utc_day()).await
    }

    async fn check_daily_on(
        &self,
        caps: &crate::config::DailyCaps,
        kind: &str,
        person: &str,
        day: u64,
    ) -> Result<(), String> {
        let mut daily = self.daily.lock().await;
        if daily.day != day {
            *daily = DailySpend {
                day,
                ..DailySpend::default()
            };
        }
        let consumer = consumer_of(kind);
        let (cap, whose) = match consumer {
            "assistant" => (caps.assistant, "the assistant"),
            "app-builder" => (caps.app_builder, "the app builder"),
            _ => (caps.other, "agent runs"),
        };
        let again = "It starts again at 00:00 UTC; an administrator can raise it.";
        if cap > 0 && daily.consumers.get(consumer).copied().unwrap_or(0) >= cap {
            return Err(format!(
                "Today's model budget for {whose} is spent. {again}"
            ));
        }
        if caps.per_person > 0 && daily.people.get(person).copied().unwrap_or(0) >= caps.per_person
        {
            return Err(format!("Your model budget for today is spent. {again}"));
        }
        Ok(())
    }

    /// Counts one call's tokens against today's budgets of its consumer and its person.
    pub async fn record_daily(&self, kind: &str, person: &str, tokens: u64) {
        self.record_daily_on(kind, person, tokens, utc_day()).await;
    }

    async fn record_daily_on(&self, kind: &str, person: &str, tokens: u64, day: u64) {
        let mut daily = self.daily.lock().await;
        if daily.day != day {
            *daily = DailySpend {
                day,
                ..DailySpend::default()
            };
        }
        *daily.consumers.entry(consumer_of(kind)).or_default() += tokens;
        *daily.people.entry(person.to_owned()).or_default() += tokens;
    }

    pub async fn record_tokens(&self, run_id: &str, count: u64) {
        let mut map = self.runs.lock().await;
        map.entry(run_id.to_string()).or_default().tokens_consumed += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_model_call_past_the_step_limit_is_refused_and_the_run_next_to_it_is_not() {
        let limits = LimitManager::default();
        for _ in 0..2 {
            assert_eq!(limits.check_steps("run-1", 2).await, Ok(()));
        }
        assert_eq!(
            limits.check_steps("run-1", 2).await,
            Err("step limit exceeded (stepsPerRun)")
        );
        assert_eq!(limits.check_steps("run-2", 2).await, Ok(()));
    }

    #[tokio::test]
    async fn a_profile_without_a_step_limit_counts_nothing() {
        let limits = LimitManager::default();
        for _ in 0..50 {
            assert_eq!(limits.check_steps("run-1", 0).await, Ok(()));
        }
        // The steps of the unbounded calls are not held against a limit that arrives later.
        assert_eq!(limits.check_steps("run-1", 1).await, Ok(()));
    }

    #[tokio::test]
    async fn the_request_past_the_limit_is_refused_and_the_window_restarts_after_a_minute() {
        let limits = LimitManager::default();
        let start = Instant::now();
        for _ in 0..3 {
            assert_eq!(limits.check_rpm_at("run-1", 3, start).await, Ok(()));
        }
        assert!(
            limits.check_rpm_at("run-1", 3, start).await.is_err(),
            "the fourth in the minute"
        );
        assert_eq!(
            limits.check_rpm_at("run-2", 3, start).await,
            Ok(()),
            "another run has its own count"
        );
        let later = start + WINDOW;
        assert_eq!(
            limits.check_rpm_at("run-1", 3, later).await,
            Ok(()),
            "a new minute, a new count"
        );
    }

    const CAPS: crate::config::DailyCaps = crate::config::DailyCaps {
        assistant: 100,
        app_builder: 0,
        other: 50,
        per_person: 150,
    };

    #[tokio::test]
    async fn a_consumer_past_its_daily_cap_is_refused_with_whose_budget_and_the_next_day_is_new() {
        let limits = LimitManager::default();
        assert_eq!(
            limits.check_daily_on(&CAPS, "conversation", "ana", 7).await,
            Ok(())
        );
        limits.record_daily_on("conversation", "ana", 100, 7).await;
        let refused = limits.check_daily_on(&CAPS, "conversation", "bo", 7).await;
        assert_eq!(
            refused,
            Err("Today's model budget for the assistant is spent. It starts again at 00:00 UTC; an administrator can raise it.".to_owned())
        );
        // Another consumer has its own budget, and an uncapped one (0) none at all.
        assert_eq!(
            limits.check_daily_on(&CAPS, "application", "bo", 7).await,
            Ok(())
        );
        assert_eq!(
            limits.check_daily_on(&CAPS, "dashboard", "bo", 7).await,
            Ok(())
        );
        // The next UTC day starts from nothing.
        assert_eq!(
            limits.check_daily_on(&CAPS, "conversation", "bo", 8).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_person_past_the_per_person_cap_is_refused_in_every_consumer() {
        let limits = LimitManager::default();
        limits.record_daily_on("application", "ana", 150, 7).await;
        assert_eq!(
            limits.check_daily_on(&CAPS, "application", "ana", 7).await,
            Err("Your model budget for today is spent. It starts again at 00:00 UTC; an administrator can raise it.".to_owned())
        );
        assert!(limits
            .check_daily_on(&CAPS, "conversation", "ana", 7)
            .await
            .is_err());
        assert_eq!(
            limits.check_daily_on(&CAPS, "application", "bo", 7).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn no_caps_refuse_nothing() {
        let limits = LimitManager::default();
        limits
            .record_daily_on("conversation", "ana", u64::MAX / 2, 7)
            .await;
        let none = crate::config::DailyCaps::default();
        assert_eq!(
            limits.check_daily_on(&none, "conversation", "ana", 7).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn tokens_are_summed_and_the_budget_refuses_once_reached() {
        let limits = LimitManager::default();
        assert_eq!(limits.check_tokens("run-1", 100).await, Ok(()));
        limits.record_tokens("run-1", 60).await;
        limits.record_tokens("run-1", 40).await;
        assert!(limits.check_tokens("run-1", 100).await.is_err());
        assert_eq!(limits.check_tokens("run-2", 100).await, Ok(()));
    }
}
