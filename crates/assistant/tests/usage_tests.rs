//! What a project's stored turns add up to, and their CSV (T-4728, API/05 §3.1): the cost only
//! over priced turns and its percentiles on a fixed fixture, an unpriced turn counted apart, a
//! timeout counted and not an error, the phases from the steps, an empty range, another project's
//! turns never counted; the export's header, its quoting and a masked secret staying masked.

#[path = "common/db.rs"]
mod db;

use assistant::chat::agent::Event;
use assistant::chat::trace::Step;
use assistant::chat::transcript::{self, Record, Seen};
use assistant::chat::usage::{self, Range, COLUMNS};
use serde_json::{json, Value};

const PROJECT: &str = "hronov";

fn model(stage: &'static str, cost: Option<u64>, cached: Option<u64>) -> Step {
    Step {
        kind: "model",
        name: stage.into(),
        status: "ok",
        stage: Some(stage),
        model: Some("google/gemini-3.8-flash".into()),
        tokens_in: Some(1000),
        tokens_out: Some(100),
        cost_micros: cost,
        cached_tokens: cached,
        duration_ms: 900,
        ..Step::default()
    }
}

fn seen(events: &[Event]) -> Seen {
    let mut seen = Seen::default();
    for event in events {
        seen.see(event);
    }
    seen
}

/// Stores one turn as conversation `n` of `project`, routed `route`, taking `total_ms`, cut by
/// the timeout when `timed_out`.
async fn turn(
    pool: &sqlx::PgPool,
    (project, n): (&str, u32),
    question: &str,
    steps: &[Step],
    (route, total_ms, timed_out): (&str, u64, bool),
) {
    let conversation = format!("00000000-0000-4000-8000-{n:012}");
    let seen = if timed_out {
        seen(&[Event::Error {
            status: 504,
            title: "Answer Too Slow",
            detail: "The assistant took too long.".into(),
        }])
    } else {
        seen(&[Event::Answer("Odpoveď.".into())])
    };
    transcript::store(
        pool,
        &Record {
            project,
            deployment: "obcan",
            conversation: &conversation,
            turn: 1,
            origin: None,
            client: "c0ffee",
            question,
            history: &[],
            seen: &seen,
            steps,
            route,
            first_token_ms: (!timed_out).then_some(800),
            total_ms,
            timed_out,
            days: 30,
            personal: true,
        },
    )
    .await
    .expect("the turn is stored");
}

/// Five turns of `hronov` and one of another project:
/// priced 4000, 8000, 12000 and 500 micro-dollars, one unpriced, the last one timed out.
async fn fixture(pool: &sqlx::PgPool) {
    let routed = [
        model("router", Some(1000), Some(400)),
        model("answer", Some(3000), None),
    ];
    turn(
        pool,
        (PROJECT, 1),
        "Kedy má úrad otvorené?",
        &routed,
        ("rag", 2000, false),
    )
    .await;
    let answer = |cost| [model("answer", cost, None)];
    turn(
        pool,
        (PROJECT, 2),
        "Kde je podateľňa?",
        &answer(Some(8000)),
        ("rag", 4000, false),
    )
    .await;
    turn(
        pool,
        (PROJECT, 3),
        "Aké sú poplatky?",
        &answer(Some(12_000)),
        ("rag", 6000, false),
    )
    .await;
    turn(
        pool,
        (PROJECT, 4),
        "Kto je starosta?",
        &answer(None),
        ("rag", 3000, false),
    )
    .await;
    let data = [model("data", Some(500), None)];
    turn(
        pool,
        (PROJECT, 5),
        "Všetko o vzduchu",
        &data,
        ("data", 30_000, true),
    )
    .await;
    let elsewhere = [model("answer", Some(999_999), None)];
    turn(
        pool,
        ("lucenec", 6),
        "Cudzí projekt",
        &elsewhere,
        ("rag", 1000, false),
    )
    .await;
}

fn close(value: &Value, expected: f64) -> bool {
    value.as_f64().is_some_and(|v| (v - expected).abs() < 1e-9)
}

#[tokio::test]
async fn the_summary_adds_up_the_priced_turns_and_counts_the_rest_apart() {
    let (admin, pool, name) = db::database("usage_summary").await;
    fixture(&pool).await;

    let s = usage::summary(&pool, PROJECT, &Range::default())
        .await
        .expect("the summary reads");
    assert_eq!(s["questions"], 5, "{s}");
    assert_eq!(s["timeouts"], 1, "{s}");
    assert_eq!(s["errors"], 0, "a timeout is not an error: {s}");
    assert_eq!(s["uncostedQuestions"], 1, "{s}");
    // priced: 500, 4000, 8000, 12000 micro-dollars; another project's 999,999 never counts
    let cost = &s["costUsd"];
    assert!(close(&cost["total"], 0.0245), "{cost}");
    assert!(close(&cost["mean"], 0.006125), "{cost}");
    assert!(close(&cost["p50"], 0.006), "{cost}");
    assert!(close(&cost["p90"], 0.0108), "{cost}");
    // tokens in: 2000 + 4 × 1000 over 5 turns; 400 cached of turn 1's 2000
    assert!(close(&s["tokens"]["in"], 1200.0), "{s}");
    assert!(close(&s["tokens"]["out"], 120.0), "{s}");
    assert!(close(&s["tokens"]["cached"], 400.0), "{s}");
    assert!(close(&s["tokens"]["cachedShare"], 0.2), "{s}");
    assert!(close(&s["totalMs"]["p50"], 4000.0), "{s}");
    assert!(close(&s["totalMs"]["max"], 30_000.0), "{s}");
    assert!(close(&s["firstTokenMs"]["p50"], 800.0), "{s}");
    assert!(close(&s["phases"]["route"]["p50"], 900.0), "{s}");
    assert!(close(&s["phases"]["work"]["max"], 900.0), "{s}");
    assert!(close(&s["phases"]["answer"]["p90"], 900.0), "{s}");
    // every model call by stage, the timed-out turn's 500 included, the unpriced call named
    assert_eq!(
        s["costByStage"],
        json!([
            {"stage": "answer", "calls": 4, "costUsd": 0.023, "uncostedCalls": 1},
            {"stage": "router", "calls": 1, "costUsd": 0.001, "uncostedCalls": 0},
            {"stage": "data", "calls": 1, "costUsd": 0.0005, "uncostedCalls": 0},
        ]),
        "{s}"
    );
    let days = s["days"].as_array().cloned().unwrap_or_default();
    assert_eq!(days.len(), 1, "{s}");
    assert_eq!(days[0]["questions"], 5);
    assert_eq!(days[0]["uncostedQuestions"], 1);
    assert_eq!(days[0]["timeouts"], 1);

    let data = usage::summary(
        &pool,
        PROJECT,
        &Range {
            route: Some("data".into()),
            ..Range::default()
        },
    )
    .await
    .expect("the data summary reads");
    assert_eq!(data["questions"], 1, "{data}");
    assert!(close(&data["costUsd"]["total"], 0.0005), "{data}");
    assert_eq!(data["phases"]["route"]["p50"], Value::Null, "{data}");

    let other = usage::summary(&pool, "lucenec", &Range::default())
        .await
        .expect("another project's summary reads");
    assert_eq!(other["questions"], 1, "{other}");
    assert!(close(&other["costUsd"]["total"], 0.999_999), "{other}");

    db::drop_database(admin, pool, &name).await;
}

/// T-4731: a turn answered from the kept answers counts as a hit, and the time and cost it saved
/// are estimated against the other turns.
#[tokio::test]
async fn the_summary_counts_the_answer_cache_s_hits_and_what_they_saved() {
    let (admin, pool, name) = db::database("usage_cache").await;
    fixture(&pool).await;
    turn(
        &pool,
        (PROJECT, 7),
        "Kde je podateľňa?",
        &[],
        ("cached", 200, false),
    )
    .await;
    let s = usage::summary(&pool, PROJECT, &Range::default())
        .await
        .expect("the summary reads");
    let cache = &s["answerCache"];
    assert_eq!(cache["hits"], 1, "{s}");
    assert!(close(&cache["hitShare"], 1.0 / 6.0), "{cache}");
    // the other five turns take 9000 ms on average, the hit 200
    assert!(close(&cache["savedMs"], 8800.0), "{cache}");
    // the other priced turns: 500, 4000, 8000 and 12000 micro-dollars
    assert!(close(&cache["savedUsd"], 0.006125), "{cache}");

    let none = usage::summary(
        &pool,
        PROJECT,
        &Range {
            route: Some("rag".into()),
            ..Range::default()
        },
    )
    .await
    .expect("the rag summary reads");
    assert_eq!(
        none["answerCache"],
        json!({"hits": 0, "hitShare": 0.0, "savedMs": null, "savedUsd": null}),
        "{none}"
    );
    db::drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn an_empty_range_answers_zeros_and_nulls_never_a_made_up_number() {
    let (admin, pool, name) = db::database("usage_empty").await;
    fixture(&pool).await;
    let s = usage::summary(
        &pool,
        PROJECT,
        &Range {
            from: Some("2099-01-01T00:00:00Z".into()),
            ..Range::default()
        },
    )
    .await
    .expect("the empty summary reads");
    assert_eq!(s["questions"], 0, "{s}");
    assert_eq!(s["uncostedQuestions"], 0, "{s}");
    assert_eq!(
        s["costUsd"],
        json!({"total": null, "mean": null, "p50": null, "p90": null})
    );
    assert_eq!(s["tokens"]["cachedShare"], Value::Null);
    assert_eq!(
        s["answerCache"],
        json!({"hits": 0, "hitShare": null, "savedMs": null, "savedUsd": null})
    );
    assert_eq!(s["totalMs"], json!({"p50": null, "p90": null, "max": null}));
    assert_eq!(s["days"], json!([]));
    assert_eq!(s["costByStage"], json!([]));
    db::drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn the_export_is_one_quoted_row_per_turn_and_keeps_secrets_masked() {
    let (admin, pool, name) = db::database("usage_export").await;
    fixture(&pool).await;
    let tricky = "Napíšte mi na jan@hronov.example, \"prosím\"\na =1+1";
    turn(
        &pool,
        (PROJECT, 7),
        tricky,
        &[model("answer", Some(100), None)],
        ("rag", 900, false),
    )
    .await;
    turn(
        &pool,
        (PROJECT, 8),
        "=HYPERLINK(\"x\")",
        &[],
        ("direct", 10, false),
    )
    .await;

    let csv = usage::export(&pool, PROJECT, &Range::default())
        .await
        .expect("the export reads")
        .expect("under the export's ceiling");
    let header = csv.split("\r\n").next().unwrap_or_default();
    assert_eq!(header, COLUMNS.join(","));
    assert!(
        !csv.contains("jan@hronov.example"),
        "a masked address: {csv}"
    );
    assert!(
        csv.contains("\"Napíšte mi na [e-mail], \"\"prosím\"\"\na =1+1\""),
        "{csv}"
    );
    assert!(csv.contains(",\"'=HYPERLINK(\"\"x\"\")\","), "{csv}");
    assert!(
        !csv.contains("Cudzí projekt"),
        "another project's turn: {csv}"
    );
    // the unpriced turn has an empty cost and its count; the timed-out one says so
    let rows: Vec<&str> = csv.split("\r\n").filter(|r| r.contains("obcan")).collect();
    assert_eq!(rows.len(), 7, "{csv}");
    let unpriced = rows
        .iter()
        .find(|r| r.contains("Kto je starosta?"))
        .copied()
        .unwrap_or_default();
    assert!(unpriced.ends_with(",,1,false,"), "{unpriced}");
    let late = rows
        .iter()
        .find(|r| r.contains("Všetko o vzduchu"))
        .copied()
        .unwrap_or_default();
    assert!(late.ends_with(",0.000500,0,true,Answer Too Slow"), "{late}");

    let none = usage::export(
        &pool,
        PROJECT,
        &Range {
            deployment: Some("ine".into()),
            ..Range::default()
        },
    )
    .await
    .expect("an empty export reads")
    .expect("under the ceiling");
    assert_eq!(none, COLUMNS.join(",") + "\r\n");
    db::drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn a_list_row_carries_tokens_times_and_models_and_sorts_by_them() {
    let (admin, pool, name) = db::database("usage_list").await;
    fixture(&pool).await;
    let list = |sort: &str| {
        let filter = transcript::Filter {
            sort: Some(sort.into()),
            ..transcript::Filter::default()
        };
        let pool = pool.clone();
        async move {
            transcript::list(&pool, PROJECT, &filter)
                .await
                .expect("the list reads")
                .0
        }
    };
    let by_tokens = list("tokens").await;
    let first = &by_tokens[0];
    assert_eq!(first["firstQuestion"], "Kedy má úrad otvorené?", "{first}");
    assert_eq!(first["tokensIn"], 2000);
    assert_eq!(first["tokensOut"], 200);
    assert_eq!(first["cachedTokens"], 400);
    assert_eq!(first["firstTokenMs"], 800);
    assert_eq!(first["models"], json!(["google/gemini-3.8-flash"]));
    let by_time = list("totalMs").await;
    assert_eq!(by_time[0]["firstQuestion"], "Všetko o vzduchu");
    assert_eq!(by_time[0]["firstTokenMs"], Value::Null);
    assert_eq!(by_time[0]["cachedTokens"], Value::Null);
    let refused = transcript::Filter {
        sort: Some("question".into()),
        ..transcript::Filter::default()
    };
    assert_eq!(
        refused.refusal(),
        Some("sort is lastAt, costUsd, tokens or totalMs")
    );
    db::drop_database(admin, pool, &name).await;
}
