use jc_core::kinds::{Pipeline, PipelineClass};
use jcctl::pipelines::{runtime_of, Runtime};

fn pipeline(class: &str, extra: &str) -> jc_core::kinds::PipelineSpec {
    let yaml = format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: aq-mqtt-ingest
  namespace: ovzdusie
spec:
  class: {class}
  targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:public-air
{extra}"#
    );
    let manifest = Pipeline::from_yaml(&yaml).expect("the pipeline manifest parses");
    manifest
        .validate()
        .expect("the pipeline manifest validates");
    manifest.spec
}

/// PL-40: a paused scheduled pipeline keeps its schedule and carries `suspend`, so a Resume
/// is the flag alone; a running one renders `suspend: false`.
#[test]
fn a_paused_pipeline_renders_a_suspended_cron_job() {
    let Runtime::Scheduled(paused) =
        runtime_of(&pipeline("auto", "  period: 5m\n  enabled: false\n"))
    else {
        panic!("five minutes is scheduled")
    };
    assert!(paused.suspend);
    assert_eq!(paused.schedule, "* * * * *");

    let Runtime::Scheduled(running) = runtime_of(&pipeline("auto", "  period: 5m\n")) else {
        panic!("five minutes is scheduled")
    };
    assert!(!running.suspend);
}

/// PL-26: at fifteen seconds a pod start would cost more than the period itself, so the
/// pipeline stays in the project's resident runner.
#[test]
fn a_fifteen_second_period_stays_resident() {
    assert_eq!(
        runtime_of(&pipeline("auto", "  period: 15s\n")),
        Runtime::Resident
    );
}

/// PL-27: forty-five seconds runs every minute and fetches once per period.
#[test]
fn a_forty_five_second_period_becomes_a_cronjob_that_fetches_once_a_period() {
    let Runtime::Scheduled(job) = runtime_of(&pipeline("auto", "  period: 45s\n")) else {
        panic!("45s is at or above the thirty-second rule");
    };

    assert_eq!(job.schedule, "* * * * *");
    assert_eq!(job.interval.as_deref(), Some("45s"));
    assert_eq!(job.count, 1, "floor(60 / 45)");
    assert_eq!(job.concurrency_policy, "Forbid");
    assert_eq!(job.starting_deadline_seconds, 30);
}

/// Exactly on the boundary: thirty seconds is scheduled, and one run fetches twice.
#[test]
fn thirty_seconds_is_the_boundary_and_fetches_twice_per_run() {
    let Runtime::Scheduled(job) = runtime_of(&pipeline("auto", "  period: 30s\n")) else {
        panic!("30 s is scheduled, not resident");
    };

    assert_eq!(job.schedule, "* * * * *");
    assert_eq!(job.interval.as_deref(), Some("30s"));
    assert_eq!(job.count, 2);
}

/// Five minutes is expressible as cron, so the job runs once per trigger.
#[test]
fn a_five_minute_period_uses_a_cron_expression_and_fetches_once() {
    let spec = pipeline("scheduled", "  period: 5m\n  schedule: \"*/5 * * * *\"\n");
    let Runtime::Scheduled(job) = runtime_of(&spec) else {
        panic!("a scheduled pipeline is a CronJob");
    };

    assert_eq!(job.schedule, "*/5 * * * *");
    assert_eq!(job.interval, None);
    assert_eq!(job.count, 1);
}

/// A period cron cannot express (ninety seconds) falls back to every minute with the
/// interval and count carrying the cadence.
#[test]
fn a_ninety_second_period_falls_back_to_every_minute() {
    let Runtime::Scheduled(job) = runtime_of(&pipeline("auto", "  period: 90s\n")) else {
        panic!("90 s is scheduled");
    };

    assert_eq!(job.schedule, "* * * * *");
    assert_eq!(job.interval.as_deref(), Some("90s"));
    assert_eq!(job.count, 1);
}

/// PL-26: no period at all means the source drives the pipeline, so it is resident.
#[test]
fn a_push_based_pipeline_is_resident() {
    let spec = pipeline("auto", "");
    assert_eq!(spec.class, PipelineClass::Auto);
    assert_eq!(spec.period_seconds(), None);
    assert_eq!(runtime_of(&spec), Runtime::Resident);
}

/// PL-28: an explicit class overrides the cadence in both directions.
#[test]
fn an_explicit_class_overrides_the_thirty_second_rule() {
    // A slow poll forced to stay in memory because latency matters more than memory.
    assert_eq!(
        runtime_of(&pipeline("resident", "  period: 5m\n")),
        Runtime::Resident
    );

    // A fast poll forced into a CronJob to release memory between runs.
    let spec = pipeline("scheduled", "  period: 15s\n  schedule: \"* * * * *\"\n");
    let Runtime::Scheduled(job) = runtime_of(&spec) else {
        panic!("an explicit scheduled class is a CronJob");
    };
    assert_eq!(job.interval.as_deref(), Some("15s"));
    assert_eq!(job.count, 4, "floor(60 / 15)");
}
