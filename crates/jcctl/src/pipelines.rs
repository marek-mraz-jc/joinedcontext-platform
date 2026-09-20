//! Rendering a `Pipeline` into the runtime that fits its cadence (T-0139, PL-07, PL-10,
//! PL-26, PL-27, PL-28, Architecture/08 section 1).
//!
//! The thirty-second rule: a pipeline that polls more often than every thirty seconds, or
//! is driven by its source rather than a clock, stays resident in its project's Bento
//! streams runner; anything slower becomes a Kubernetes CronJob that scales to zero
//! between runs. Cron has one-minute granularity, so a period under a minute runs every
//! minute and the generated Bento input fetches several times per run.

use jc_core::kinds::{PipelineClass, PipelineSpec};

/// Where a pipeline runs (PL-04, PL-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runtime {
    /// A stream in the project's pipeline runner, one Bento process per project (PL-07).
    Resident,
    /// An ephemeral CronJob pod, non-root and scoped to the owning project (PL-10).
    Scheduled(CronJob),
}

/// How the CronJob and the Bento input it runs are parameterized (PL-27).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronJob {
    /// The cron expression of the Kubernetes CronJob.
    pub schedule: String,
    /// `input.generate.interval` of the generated `bento.yaml`, when the job fetches more
    /// than once per run.
    pub interval: Option<String>,
    /// `input.generate.count`: how many fetches one run performs before it exits.
    pub count: u32,
    /// A run must never overlap its predecessor.
    pub concurrency_policy: &'static str,
    /// A run that could not start within this window is skipped rather than piled up.
    pub starting_deadline_seconds: u32,
    /// `spec.suspend` of the CronJob: a paused pipeline keeps its schedule and runs nothing (PL-40).
    pub suspend: bool,
}

/// The cadence at which a scheduled run stops being worth a pod start (PL-26).
const RESIDENT_BELOW_SECONDS: u64 = 30;

/// Decides where one pipeline runs (PL-26, PL-28).
///
/// An explicit `class` always wins; `auto` reads `spec.period`, and a pipeline with no
/// period is push-based and therefore resident. A paused pipeline (`spec.enabled: false`,
/// PL-40) keeps its runtime: the streams renderer leaves it out of the runner and the
/// CronJob carries `suspend`, so a Resume changes nothing but the flag.
pub fn runtime_of(spec: &PipelineSpec) -> Runtime {
    match spec.class {
        PipelineClass::Resident => Runtime::Resident,
        PipelineClass::Scheduled => Runtime::Scheduled(cron_job(spec)),
        PipelineClass::Auto => match spec.period_seconds() {
            Some(seconds) if seconds >= RESIDENT_BELOW_SECONDS => {
                Runtime::Scheduled(cron_job(spec))
            }
            _ => Runtime::Resident,
        },
    }
}

/// Parameterizes the CronJob for a scheduled pipeline (PL-27).
///
/// Under a minute the job runs every minute and fetches `floor(60 / period)` times, so a
/// 45-second poll still happens on time without a pod start per fetch. At a minute or
/// more the declared cron expression runs the job once; a period that cron cannot express
/// falls back to every minute with the same interval and count.
fn cron_job(spec: &PipelineSpec) -> CronJob {
    let every_minute = |period: &str, count: u32| CronJob {
        schedule: "* * * * *".to_owned(),
        interval: Some(period.to_owned()),
        count,
        concurrency_policy: "Forbid",
        starting_deadline_seconds: 30,
        suspend: !spec.enabled,
    };

    match (
        spec.period_seconds(),
        spec.period.as_deref(),
        spec.schedule.as_deref(),
    ) {
        (Some(seconds), Some(period), _) if seconds > 0 && seconds < 60 => {
            every_minute(period, (60 / seconds) as u32)
        }
        (Some(_), Some(period), None) => every_minute(period, 1),
        (_, _, Some(schedule)) => CronJob {
            schedule: schedule.to_owned(),
            interval: None,
            count: 1,
            concurrency_policy: "Forbid",
            starting_deadline_seconds: 30,
            suspend: !spec.enabled,
        },
        // `class: scheduled` without either is refused by `PipelineSpec::validate`; a
        // manifest that reached here anyway runs once a minute rather than not at all.
        (_, _, None) => CronJob {
            schedule: "* * * * *".to_owned(),
            interval: None,
            count: 1,
            concurrency_policy: "Forbid",
            starting_deadline_seconds: 30,
            suspend: !spec.enabled,
        },
    }
}
