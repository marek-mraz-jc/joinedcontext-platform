//! The stored transcripts of a deployment's conversations (T-4670, AG-99, API/05 §3.1): a turn
//! and its steps read back as written, every text redacted, a timed-out turn kept with its
//! steps, the cost of a conversation the sum of its steps, the retention sweep and the erasure.

#[path = "common/db.rs"]
mod db;

use assistant::chat::agent::{Event, Turn};
use assistant::chat::forget_expired;
use assistant::chat::trace::Step;
use assistant::chat::transcript::{self, Filter, Record, Seen};
use serde_json::{json, Value};

const PROJECT: &str = "hronov";
const RAG: &str = "11111111-1111-4111-8111-111111111111";
const DATA: &str = "22222222-2222-4222-8222-222222222222";
const LATE: &str = "33333333-3333-4333-8333-333333333333";

fn model(stage: &'static str, cost: Option<u64>) -> Step {
    Step {
        kind: "model",
        name: stage.into(),
        status: "ok",
        stage: Some(stage),
        model: Some("google/gemini-3.8-flash".into()),
        tokens_in: Some(1000),
        tokens_out: Some(100),
        cost_micros: cost,
        duration_ms: 900,
        ..Step::default()
    }
}

fn tool(name: &str, output: &str) -> Step {
    Step {
        kind: "tool",
        name: name.into(),
        endpoint: Some("hronov-air".into()),
        input: Some(r#"{"type":"AirQualityObserved"}"#.into()),
        output: Some(output.into()),
        status: "ok",
        duration_ms: 120,
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

/// Turn 1 of `conversation`, routed `rag`, with no history; a test overrides the rest.
fn record<'a>(
    conversation: &'a str,
    question: &'a str,
    seen: &'a Seen,
    steps: &'a [Step],
) -> Record<'a> {
    Record {
        project: PROJECT,
        deployment: "obcan",
        conversation,
        turn: 1,
        origin: Some("https://www.hronov.example"),
        client: "c0ffee",
        question,
        history: &[],
        seen,
        steps,
        route: "rag",
        first_token_ms: Some(800),
        total_ms: 2400,
        timed_out: false,
        days: 30,
    }
}

async fn stored(pool: &sqlx::PgPool, conversation: &str) -> Value {
    transcript::detail(pool, PROJECT, conversation)
        .await
        .expect("the detail reads")
        .expect("the conversation is stored")
}

#[tokio::test]
async fn a_turn_reads_back_redacted_with_its_steps_and_the_cost_is_their_sum() {
    let (admin, pool, name) = db::database("transcript_round_trip").await;

    let rag = seen(&[Event::Answer(
        "Úrad má otvorené do 15:00, píšte na podatelna@hronov.example.".into(),
    )]);
    let rag_steps = [model("router", Some(1200)), model("answer", Some(3400))];
    transcript::store(
        &pool,
        &record(
            RAG,
            "Kedy má úrad otvorené? Volám z +421 905 123 456.",
            &rag,
            &rag_steps,
        ),
    )
    .await
    .expect("the rag turn is stored");

    let history = [Turn {
        role: "user".into(),
        text: "moje číslo je 850315/1250".into(),
    }];
    let data = seen(&[
        Event::Delta("PM10 je ".into()),
        Event::Delta("dnes 31 µg/m³.".into()),
    ]);
    let data_steps = [
        model("data", Some(2000)),
        tool("query_entities", "[{\"pm10\":31}]"),
        tool(
            "get_entity",
            "{\"id\":\"urn:ngsi-ld:AirQualityObserved:1\"}",
        ),
        tool("query_temporal", "IBAN SK3112000000198742637541"),
        model("data", None),
    ];
    transcript::store(
        &pool,
        &Record {
            turn: 2,
            history: &history,
            route: "data",
            ..record(DATA, "Aké je PM10?", &data, &data_steps)
        },
    )
    .await
    .expect("the data turn is stored");
    // A retried question with the same conversation and turn keeps the first.
    transcript::store(
        &pool,
        &Record {
            turn: 2,
            ..record(DATA, "iné", &data, &[])
        },
    )
    .await
    .expect("a repeat is no error");

    let rag_back = stored(&pool, RAG).await;
    let turn = &rag_back["turns"][0];
    assert_eq!(rag_back["deployment"], "obcan");
    assert_eq!(turn["route"], "rag");
    assert!(!turn["question"].as_str().unwrap_or("").contains("905"));
    assert!(!turn["answer"].as_str().unwrap_or("").contains("podatelna@"));
    assert_eq!(turn["costUsd"], json!(0.0046));
    assert_eq!(turn["uncosted"], 0);
    assert_eq!(turn["steps"].as_array().map(Vec::len), Some(2));

    let data_back = stored(&pool, DATA).await;
    let turns = data_back["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 1, "the repeat added no turn");
    let turn = &turns[0];
    assert_eq!(turn["question"], "Aké je PM10?");
    assert_eq!(turn["answer"], "PM10 je dnes 31 µg/m³.");
    assert!(!turn["history"].to_string().contains("850315"));
    let steps = turn["steps"].as_array().expect("steps");
    assert_eq!(
        steps.iter().map(|s| s["kind"].clone()).collect::<Vec<_>>(),
        ["model", "tool", "tool", "tool", "model"]
    );
    assert_eq!(steps[1]["name"], "query_entities");
    assert_eq!(steps[1]["endpoint"], "hronov-air");
    assert!(!steps[3]["output"].to_string().contains("SK3112"));
    let step_sum: f64 = steps.iter().filter_map(|s| s["costUsd"].as_f64()).sum();
    assert_eq!(turn["costUsd"].as_f64(), Some(step_sum));
    assert_eq!(
        turn["uncosted"], 1,
        "the unpriced call is counted, not zero"
    );

    let (items, next) = transcript::list(&pool, PROJECT, &Filter::default())
        .await
        .expect("the list reads");
    assert_eq!(items.len(), 2);
    assert_eq!(next, None);
    let listed = items
        .iter()
        .find(|i| i["id"] == DATA)
        .expect("the data conversation is listed");
    assert_eq!(listed["costUsd"].as_f64(), Some(step_sum));
    assert_eq!(listed["uncosted"], 1);
    let (other, _) = transcript::list(&pool, "elsewhere", &Filter::default())
        .await
        .expect("another project's list reads");
    assert!(other.is_empty(), "row-level security keeps projects apart");
    assert_eq!(
        transcript::detail(&pool, "elsewhere", DATA)
            .await
            .expect("another project's read answers"),
        None,
        "a conversation of {PROJECT} is no conversation in another project"
    );

    db::drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn a_timed_out_turn_keeps_its_steps_and_the_error() {
    let (admin, pool, name) = db::database("transcript_timeout").await;
    let late = seen(&[
        Event::Delta("Hľadám".into()),
        Event::Error {
            status: 504,
            title: "Answer Too Slow",
            detail: "The assistant took too long.".into(),
        },
    ]);
    let steps = [model("data", Some(500)), tool("query_entities", "[]")];
    transcript::store(
        &pool,
        &Record {
            route: "data",
            timed_out: true,
            ..record(LATE, "Všetko o vzduchu", &late, &steps)
        },
    )
    .await
    .expect("the late turn is stored");

    let back = stored(&pool, LATE).await;
    let turn = &back["turns"][0];
    assert_eq!(turn["timedOut"], true);
    assert_eq!(turn["error"]["status"], 504);
    assert_eq!(turn["steps"].as_array().map(Vec::len), Some(2));
    let (errors, _) = transcript::list(
        &pool,
        PROJECT,
        &Filter {
            status: Some("timeout".into()),
            ..Filter::default()
        },
    )
    .await
    .expect("the filtered list reads");
    assert_eq!(errors.len(), 1);

    db::drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn the_sweep_drops_expired_turns_and_erase_drops_one_conversation() {
    let (admin, pool, name) = db::database("transcript_sweep").await;
    let shown = seen(&[Event::Answer("Áno.".into())]);
    let steps = [model("answer", Some(100))];
    for conversation in [RAG, DATA, LATE] {
        transcript::store(&pool, &record(conversation, "Otázka", &shown, &steps))
            .await
            .expect("stored");
    }
    // Retention 0 stores nothing.
    let none = Record {
        days: 0,
        ..record(
            "44444444-4444-4444-8444-444444444444",
            "Otázka",
            &shown,
            &steps,
        )
    };
    transcript::store(&pool, &none).await.expect("no error");
    let (items, _) = transcript::list(&pool, PROJECT, &Filter::default())
        .await
        .expect("lists");
    assert_eq!(items.len(), 3, "retention 0 stored nothing");

    let owner = sqlx::PgPool::connect_with(
        std::env::var("JC_ASSISTANT_TEST_DATABASE_URL")
            .expect("the test database")
            .parse::<sqlx::postgres::PgConnectOptions>()
            .expect("parses")
            .database(&name),
    )
    .await
    .expect("connect as the owner");
    sqlx::query(
        "UPDATE turns SET expires_at = now() - interval '1 minute' WHERE conversation = $1::uuid",
    )
    .bind(RAG)
    .execute(&owner)
    .await
    .expect("expire one");
    owner.close().await;

    forget_expired(&pool).await.expect("the sweep runs");
    assert!(transcript::detail(&pool, PROJECT, RAG)
        .await
        .expect("reads")
        .is_none());
    assert!(transcript::detail(&pool, PROJECT, DATA)
        .await
        .expect("reads")
        .is_some());

    assert!(transcript::erase(&pool, PROJECT, DATA)
        .await
        .expect("erases"));
    assert!(!transcript::erase(&pool, PROJECT, DATA)
        .await
        .expect("a second erase is no error"));
    assert!(!transcript::erase(&pool, "elsewhere", LATE)
        .await
        .expect("another project erases nothing"));
    let (items, _) = transcript::list(&pool, PROJECT, &Filter::default())
        .await
        .expect("lists");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], LATE);

    db::drop_database(admin, pool, &name).await;
}
