//! The scheduled jobs of the placed Apps (AP-154, AP-155, T-3372): each minute the scheduler
//! starts the jobs whose cron names that minute, in UTC. A job runs at most once at a time: a run
//! still going when the next one is due skips that one and records the skip. A minute the host
//! was not running is not caught up. A run stops at its wall time. How a run calls the App and
//! with whose token is the [`Runner`]'s; what happened to each job is kept for the Portal.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::OffsetDateTime;

use crate::placement::{Placed, PlacedJob};

/// At most this long per run (AP-154).
pub const WALL_TIME: Duration = Duration::from_secs(60);

/// After this many failed runs in a row the App's owners hear of it (AP-155).
pub const FAILURES_BEFORE_NOTICE: u32 = 3;

/// One job of one App, as the scheduler keys it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobKey {
    pub app: String,
    pub job: String,
}

/// Runs one job of one App: a fresh instance, the App's own ServiceAccount (AP-154).
pub trait Runner: Send + Sync + 'static {
    fn run<'a>(
        &'a self,
        app: &'a Placed,
        job: &'a PlacedJob,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    /// The sentence a person can act on, never a body or a credential (AP-155).
    Failed(String),
    /// Stopped at its wall time.
    TimedOut,
    /// Not started: the run before it was still going.
    Skipped,
}

/// What the scheduler last saw of one job, for `status.jobs[]` (AP-154).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// When the last run was due, UTC.
    pub at: OffsetDateTime,
    pub outcome: Outcome,
    /// How long it ran; zero for a skip.
    pub duration: Duration,
    /// Runs that failed or timed out in a row; a skip neither adds nor clears.
    pub failures_in_a_row: u32,
}

impl Record {
    /// Whether the App's owners are told (AP-155): three failed runs in a row, said once at the
    /// third and again every third after it, while the schedule keeps running.
    pub fn notice(&self) -> bool {
        self.failures_in_a_row >= FAILURES_BEFORE_NOTICE
            && self
                .failures_in_a_row
                .is_multiple_of(FAILURES_BEFORE_NOTICE)
    }
}

/// The scheduler of one shard.
pub struct Scheduler<R: Runner> {
    runner: Arc<R>,
    wall: Duration,
    running: Arc<Mutex<HashSet<JobKey>>>,
    records: Arc<Mutex<HashMap<JobKey, Record>>>,
}

impl<R: Runner> Scheduler<R> {
    pub fn new(runner: R, wall: Duration) -> Self {
        Self {
            runner: Arc::new(runner),
            wall,
            running: Arc::new(Mutex::new(HashSet::new())),
            records: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The jobs due at `minute` (UTC, seconds ignored), started; the handles of those started, so
    /// a caller may wait for them. A due job whose last run is still going is skipped.
    pub fn tick(
        &self,
        apps: &[Placed],
        minute: OffsetDateTime,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let minute = minute.to_offset(time::UtcOffset::UTC);
        let mut started = Vec::new();
        for app in apps {
            for job in &app.jobs {
                let due = jc_core::cron::matches(
                    &job.schedule,
                    minute.minute() as u32,
                    minute.hour() as u32,
                    minute.day() as u32,
                    minute.month() as u32,
                    minute.weekday().number_days_from_sunday() as u32,
                );
                // A schedule the reconciler should have refused runs never, and says so once a run is due.
                let Some(true) = due else {
                    if due.is_none() {
                        tracing::warn!(app = %app.id, job = %job.name, "the job's schedule is not cron; it does not run");
                    }
                    continue;
                };
                let key = JobKey {
                    app: app.id.clone(),
                    job: job.name.clone(),
                };
                let fresh = self
                    .running
                    .lock()
                    .map(|mut running| running.insert(key.clone()))
                    .unwrap_or(false);
                if !fresh {
                    self.record(&key, minute, Outcome::Skipped, Duration::ZERO);
                    tracing::info!(app = %app.id, job = %job.name, "skipped: the run before it is still going");
                    continue;
                }
                let (runner, running, records, wall) = (
                    self.runner.clone(),
                    self.running.clone(),
                    self.records.clone(),
                    self.wall,
                );
                let (app, job) = (app.clone(), job.clone());
                started.push(tokio::spawn(async move {
                    let begun = tokio::time::Instant::now();
                    let outcome = match tokio::time::timeout(wall, runner.run(&app, &job)).await {
                        Ok(Ok(())) => Outcome::Succeeded,
                        Ok(Err(why)) => Outcome::Failed(why),
                        Err(_) => Outcome::TimedOut,
                    };
                    let took = begun.elapsed();
                    match &outcome {
                        Outcome::Succeeded => tracing::info!(app = %app.id, job = %job.name, ms = took.as_millis() as u64, "job ran"),
                        Outcome::Failed(why) => tracing::warn!(app = %app.id, job = %job.name, %why, "job failed"),
                        _ => tracing::warn!(app = %app.id, job = %job.name, "job stopped at its wall time"),
                    }
                    Self::store(&records, &key, minute, outcome, took);
                    if let Ok(mut running) = running.lock() {
                        running.remove(&key);
                    }
                }));
            }
        }
        started
    }

    fn record(&self, key: &JobKey, at: OffsetDateTime, outcome: Outcome, duration: Duration) {
        Self::store(&self.records, key, at, outcome, duration);
    }

    fn store(
        records: &Mutex<HashMap<JobKey, Record>>,
        key: &JobKey,
        at: OffsetDateTime,
        outcome: Outcome,
        duration: Duration,
    ) {
        let Ok(mut records) = records.lock() else {
            return;
        };
        let before = records
            .get(key)
            .map_or(0, |record| record.failures_in_a_row);
        let failures_in_a_row = match outcome {
            Outcome::Succeeded => 0,
            Outcome::Failed(_) | Outcome::TimedOut => before + 1,
            Outcome::Skipped => before,
        };
        let record = Record {
            at,
            outcome,
            duration,
            failures_in_a_row,
        };
        if record.notice() {
            tracing::warn!(app = %key.app, job = %key.job, failures = failures_in_a_row, "notice: the job failed its last runs in a row");
        }
        records.insert(key.clone(), record);
    }

    /// What the scheduler last saw of each job.
    pub fn records(&self) -> HashMap<JobKey, Record> {
        self.records
            .lock()
            .map(|records| records.clone())
            .unwrap_or_default()
    }

    /// Ticks at the start of every minute, for the Apps `apps` names at that minute. A minute
    /// already gone is never ticked: a host that was down misses its runs (AP-154).
    pub async fn every_minute(self, apps: impl Fn() -> Vec<Placed>) {
        loop {
            let now = OffsetDateTime::now_utc();
            let next = (now
                - Duration::from_nanos(now.nanosecond() as u64)
                - Duration::from_secs(now.second() as u64))
                + Duration::from_secs(60);
            let wait = (next - now).unsigned_abs();
            tokio::time::sleep(wait).await;
            drop(self.tick(&apps(), next));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// Runs as long as it was told, then succeeds or fails as told, and counts its runs.
    struct Fake {
        takes: Duration,
        fails: bool,
        runs: Arc<Mutex<u32>>,
    }

    impl Runner for Fake {
        fn run<'a>(
            &'a self,
            _: &'a Placed,
            _: &'a PlacedJob,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async move {
                if let Ok(mut runs) = self.runs.lock() {
                    *runs += 1;
                }
                tokio::time::sleep(self.takes).await;
                if self.fails {
                    Err("the indicator could not be written: 403".into())
                } else {
                    Ok(())
                }
            })
        }
    }

    fn app(schedule: &str) -> Placed {
        Placed {
            name: "kpi-forecast".into(),
            id: "helsinki_kpi_forecast".into(),
            tenant: "helsinki".into(),
            digest: format!("sha256:{}", "a".repeat(64)),
            endpoint: None,
            jobs: vec![PlacedJob {
                name: "hourly".into(),
                schedule: schedule.into(),
                export: "compute-kpi".into(),
            }],
        }
    }

    fn fake(takes: Duration, fails: bool) -> (Scheduler<Fake>, Arc<Mutex<u32>>) {
        let runs = Arc::new(Mutex::new(0));
        (
            Scheduler::new(
                Fake {
                    takes,
                    fails,
                    runs: runs.clone(),
                },
                WALL_TIME,
            ),
            runs,
        )
    }

    fn key() -> JobKey {
        JobKey {
            app: "helsinki_kpi_forecast".into(),
            job: "hourly".into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_job_runs_on_its_minute_and_on_no_other() {
        let (scheduler, runs) = fake(Duration::from_secs(1), false);
        let apps = [app("0 * * * *")];
        assert!(scheduler
            .tick(&apps, datetime!(2026-10-08 14:01 UTC))
            .is_empty());
        for handle in scheduler.tick(&apps, datetime!(2026-10-08 15:00 UTC)) {
            handle.await.expect("run");
        }
        assert_eq!(*runs.lock().expect("runs"), 1);
        let record = &scheduler.records()[&key()];
        assert_eq!(
            (record.at, &record.outcome, record.duration),
            (
                datetime!(2026-10-08 15:00 UTC),
                &Outcome::Succeeded,
                Duration::from_secs(1)
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_run_waits_for_the_first_by_skipping_and_saying_so() {
        let (scheduler, runs) = fake(Duration::from_secs(7 * 60), false);
        let apps = [app("*/5 * * * *")];
        let first = scheduler.tick(&apps, datetime!(2026-10-08 15:00 UTC));
        tokio::time::advance(Duration::from_secs(5 * 60)).await;
        assert!(scheduler
            .tick(&apps, datetime!(2026-10-08 15:05 UTC))
            .is_empty());
        assert_eq!(scheduler.records()[&key()].outcome, Outcome::Skipped);
        for handle in first {
            handle.await.expect("run");
        }
        assert_eq!(*runs.lock().expect("runs"), 1);
        // Free again: the next minute it is due, it runs.
        assert_eq!(
            scheduler.tick(&apps, datetime!(2026-10-08 15:10 UTC)).len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_past_its_wall_time_is_stopped_and_recorded() {
        let (scheduler, _) = fake(Duration::from_secs(120), false);
        for handle in scheduler.tick(&[app("0 * * * *")], datetime!(2026-10-08 15:00 UTC)) {
            handle.await.expect("run");
        }
        let record = &scheduler.records()[&key()];
        assert_eq!(record.outcome, Outcome::TimedOut);
        assert_eq!(record.duration, WALL_TIME);
        assert_eq!(record.failures_in_a_row, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_missed_minute_is_not_caught_up() {
        // Down from 15:02 to 15:13: the scheduler only ever ticks the minute it is in.
        let (scheduler, runs) = fake(Duration::from_secs(1), false);
        let apps = [app("*/5 * * * *")];
        for handle in scheduler.tick(&apps, datetime!(2026-10-08 15:13 UTC)) {
            handle.await.expect("run");
        }
        for handle in scheduler.tick(&apps, datetime!(2026-10-08 15:15 UTC)) {
            handle.await.expect("run");
        }
        assert_eq!(
            *runs.lock().expect("runs"),
            1,
            "15:05 and 15:10 are not run late"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn three_failed_runs_in_a_row_raise_a_notice_and_a_success_clears_them() {
        let (scheduler, _) = fake(Duration::from_secs(1), true);
        let apps = [app("0 * * * *")];
        for hour in [15, 16, 17] {
            let at = datetime!(2026-10-08 00:00 UTC)
                .replace_hour(hour)
                .expect("hour");
            for handle in scheduler.tick(&apps, at) {
                handle.await.expect("run");
            }
        }
        let record = &scheduler.records()[&key()];
        assert_eq!(
            record.outcome,
            Outcome::Failed("the indicator could not be written: 403".into())
        );
        assert!(record.notice());
        let (ok, _) = fake(Duration::from_secs(1), false);
        Scheduler::<Fake>::store(
            &ok.records,
            &key(),
            datetime!(2026-10-08 18:00 UTC),
            Outcome::Failed("x".into()),
            Duration::ZERO,
        );
        Scheduler::<Fake>::store(
            &ok.records,
            &key(),
            datetime!(2026-10-08 19:00 UTC),
            Outcome::Succeeded,
            Duration::ZERO,
        );
        assert_eq!(ok.records()[&key()].failures_in_a_row, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_schedule_that_is_not_cron_never_runs() {
        let (scheduler, runs) = fake(Duration::from_secs(1), false);
        assert!(scheduler
            .tick(&[app("every hour")], datetime!(2026-10-08 15:00 UTC))
            .is_empty());
        assert_eq!(*runs.lock().expect("runs"), 0);
        assert!(scheduler.records().is_empty());
    }
}
