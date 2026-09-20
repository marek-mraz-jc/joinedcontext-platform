//! Edge cases of the four deciding functions of `pdp::conditional` (T-1908, T-1909, T-1910,
//! T-1911; EP-26, MP-02, R45, GW16, GW18).
//!
//! Contracts, one sentence each:
//!
//! - `required`: a write is read first exactly when it addresses one stored entity and something
//!   about that stored entity decides it — the caller's `If-Match`, or a grant that narrows by
//!   filter or by area.
//! - `batch_refusal`: a batch that touches stored entities under such a grant is refused, because
//!   its entities are named in the payload and a payload can sit inside the grant while the
//!   entities it names are stored outside it.
//! - `precondition_failed`: the 412 keeps the platform's own problem type and carries only the
//!   detail it was given.
//! - `matches`: only a strong comparison satisfies an `If-Match`, and a precondition the platform
//!   cannot evaluate is never treated as met.
//!
//! `conditional_write_tests.rs` walks these through the router on the happy paths and on the
//! strong comparison. These are the inputs around them, where a wrong answer is a write that
//! should have been read first and was not.

use axum::http::header::IF_MATCH;
use axum::http::{HeaderMap, HeaderValue};
use context_gateway::pdp::conditional::{
    batch_refusal, matches, precondition_failed, required, state_dependent,
};
use context_gateway::pdp::evaluator::Constraints;
use jc_core::kinds::Operation;

// The path as `serve_ngsi_ld` hands it over: relative to `{base_path}/ngsi-ld/v1`, undecoded.
const ENTITY: &str = "/entities/urn:ngsi-ld:AirQualityObserved:bbsk.sk:kraj:s-1";
const ATTRS: &str = "/entities/urn:ngsi-ld:AirQualityObserved:bbsk.sk:kraj:s-1/attrs";
const COLLECTION: &str = "/entities";
const BATCH: &str = "/entityOperations/upsert";
const AREA: &str = "georel=within;geometry=Polygon;coordinates=[[[21.0,48.0],[21.5,48.0],[21.5,48.5],[21.0,48.5],[21.0,48.0]]]";

fn plain() -> Constraints {
    Constraints::default()
}

fn filtered() -> Constraints {
    Constraints {
        q: Some("temperature>10".to_owned()),
        ..Constraints::default()
    }
}

fn in_an_area() -> Constraints {
    Constraints {
        geo_areas: vec![AREA.to_owned()],
        ..Constraints::default()
    }
}

fn headers(if_match: Option<&str>) -> HeaderMap {
    let mut map = HeaderMap::new();
    if let Some(tag) = if_match {
        map.insert(
            IF_MATCH,
            HeaderValue::from_str(tag).expect("a header value"),
        );
    }
    map
}

// --- state_dependent, which the other three lean on --------------------------------------

#[test]
fn a_grant_is_state_dependent_when_it_narrows_by_filter_or_by_area_and_not_otherwise() {
    assert!(
        !state_dependent(&plain()),
        "a grant on types alone is decided from the payload"
    );
    assert!(state_dependent(&filtered()));
    assert!(state_dependent(&in_an_area()));

    // An empty filter string is still a filter the broker would apply, so it still decides from
    // the stored entity; treating it as absent would skip the read.
    let empty_filter = Constraints {
        q: Some(String::new()),
        ..Constraints::default()
    };
    assert!(state_dependent(&empty_filter));

    // An empty list of areas narrows nothing, which is the documented meaning of the field.
    let no_areas = Constraints {
        geo_areas: Vec::new(),
        ..Constraints::default()
    };
    assert!(!state_dependent(&no_areas));
}

// --- required (T-1908) --------------------------------------------------------------------

#[test]
fn the_path_is_the_one_relative_to_the_endpoint_and_a_caller_that_forgets_that_reads_nothing() {
    // `serve_ngsi_ld` strips `{base_path}/ngsi-ld/v1` before handing the path over. A caller
    // that passed the whole path would address no entity here, so the write would skip its read
    // instead of reading something wrong. Recorded because the failure would be silent.
    let whole = "/api/endpoint/abc/ngsi-ld/v1/entities/urn:ngsi-ld:X:y:z:1/attrs";
    assert!(!required(
        Operation::UpdateAttrs,
        whole,
        &headers(Some("*")),
        &filtered()
    ));
    assert!(required(
        Operation::UpdateAttrs,
        ATTRS,
        &headers(Some("*")),
        &filtered()
    ));
}

#[test]
fn a_read_is_never_preceded_by_a_read() {
    for operation in [
        Operation::RetrieveEntity,
        Operation::QueryEntity,
        Operation::RetrieveTemporal,
    ] {
        assert!(!required(
            operation,
            ENTITY,
            &headers(Some("\"7f3a\"")),
            &filtered()
        ));
    }
}

#[test]
fn a_write_that_addresses_no_one_entity_has_no_stored_state_to_read() {
    // A create posts to the collection and a batch names its entities in the payload; there is
    // nothing at either path to read, which is exactly why `batch_refusal` exists beside this.
    for path in [
        COLLECTION,
        BATCH,
        "",
        "/",
        "/entities",
        "/entityOperations/delete",
    ] {
        assert!(
            !required(Operation::CreateEntity, path, &headers(None), &filtered()),
            "{path} was read as addressing an entity"
        );
        assert!(
            !required(
                Operation::UpsertBatch,
                path,
                &headers(Some("*")),
                &filtered()
            ),
            "{path} was read as addressing an entity"
        );
    }
}

#[test]
fn an_if_match_makes_a_write_conditional_even_under_a_grant_that_needs_no_read() {
    // The caller's own precondition is checked by the same read, so it has to force one.
    assert!(required(
        Operation::UpdateAttrs,
        ATTRS,
        &headers(Some("\"7f3a\"")),
        &plain()
    ));
    assert!(!required(
        Operation::UpdateAttrs,
        ATTRS,
        &headers(None),
        &plain()
    ));
}

#[test]
fn a_state_dependent_grant_makes_a_write_conditional_even_without_an_if_match() {
    assert!(required(
        Operation::UpdateAttrs,
        ATTRS,
        &headers(None),
        &filtered()
    ));
    assert!(required(
        Operation::DeleteEntity,
        ENTITY,
        &headers(None),
        &in_an_area()
    ));
}

#[test]
fn an_empty_if_match_still_counts_as_the_caller_sending_one() {
    // The value is not parsed here: the header being present is what makes the write
    // conditional, and whether it is satisfiable is `matches`'s answer, which an empty value
    // fails. Reading "present but empty" as "absent" would skip the read and let the write
    // through unchecked.
    assert!(required(
        Operation::UpdateAttrs,
        ATTRS,
        &headers(Some("")),
        &plain()
    ));
}

#[test]
fn a_write_under_the_temporal_path_addresses_an_entity_too() {
    let temporal = "/temporal/entities/urn:ngsi-ld:AirQualityObserved:bbsk.sk:kraj:s-1/attrs";
    assert!(required(
        Operation::UpdateAttrs,
        temporal,
        &headers(None),
        &filtered()
    ));
}

// --- batch_refusal (T-1909) ---------------------------------------------------------------

#[test]
fn every_batch_that_touches_a_stored_entity_is_refused_under_a_state_dependent_grant() {
    for operation in [
        Operation::UpsertBatch,
        Operation::UpdateBatch,
        Operation::MergeBatch,
        Operation::DeleteBatch,
    ] {
        let refusal = batch_refusal(operation, &filtered())
            .unwrap_or_else(|| panic!("{operation:?} was allowed under a filtered grant"));
        assert_eq!(refusal.status, 403);
        assert!(
            batch_refusal(operation, &in_an_area()).is_some(),
            "{operation:?} under an area"
        );
    }
}

#[test]
fn a_batch_that_only_creates_is_not_refused_because_a_create_has_no_stored_state() {
    assert!(batch_refusal(Operation::CreateBatch, &filtered()).is_none());
    assert!(batch_refusal(Operation::CreateBatch, &in_an_area()).is_none());
}

#[test]
fn a_batch_under_a_grant_that_needs_no_stored_state_is_not_refused() {
    // R45 answers this case by partitioning write authority by URN prefix, which is decided from
    // the payload alone; refusing it would take away a working way of writing in bulk.
    for operation in [
        Operation::UpsertBatch,
        Operation::UpdateBatch,
        Operation::MergeBatch,
        Operation::DeleteBatch,
    ] {
        assert!(
            batch_refusal(operation, &plain()).is_none(),
            "{operation:?}"
        );
    }
}

#[test]
fn a_single_entity_write_is_never_answered_by_the_batch_refusal() {
    // It is read first instead; a refusal here would make a legitimate conditional write
    // impossible under exactly the grants that need one.
    for operation in [
        Operation::UpdateAttrs,
        Operation::MergeEntity,
        Operation::DeleteEntity,
        Operation::AppendAttrs,
        Operation::ReplaceEntity,
    ] {
        assert!(
            batch_refusal(operation, &filtered()).is_none(),
            "{operation:?}"
        );
    }
}

#[test]
fn the_batch_refusal_names_no_entity_and_no_filter() {
    // The refusal is read by a caller who may not see what the grant narrows by; naming the
    // filter would hand them the condition they are being kept outside of.
    let refusal = batch_refusal(Operation::UpsertBatch, &filtered()).expect("a refusal");
    let detail = refusal.detail.clone().unwrap_or_default();
    assert!(!detail.contains("temperature"), "{detail}");
    assert!(
        detail.contains("one entity per request"),
        "it says what to do instead: {detail}"
    );
}

// --- precondition_failed (T-1910) ---------------------------------------------------------

#[test]
fn the_precondition_failure_is_a_412_of_the_platforms_own_type() {
    // CIM 009 clause 5.5.3 has no error type for this, so borrowing an ETSI term would tell a
    // client keying on `type` something that is not true.
    let problem = precondition_failed("the entity changed");
    assert_eq!(problem.status, 412);
    assert_eq!(problem.title, "Precondition Failed");
    assert!(
        !problem.type_uri.contains("uri.etsi.org"),
        "an ETSI type here would mean something else: {}",
        problem.type_uri
    );
    assert!(
        problem.type_uri.contains("precondition-failed"),
        "{}",
        problem.type_uri
    );
    assert_eq!(problem.detail.as_deref(), Some("the entity changed"));
}

#[test]
fn the_detail_is_carried_as_given_and_nothing_is_added_to_it() {
    for detail in [
        "",
        "   ",
        "a detail with \"quotes\" and a urn:ngsi-ld:X:y:z:1",
    ] {
        let problem = precondition_failed(detail);
        assert_eq!(problem.detail.as_deref(), Some(detail));
        assert_eq!(problem.status, 412);
    }
}

// --- matches (T-1911) ---------------------------------------------------------------------

#[test]
fn a_precondition_the_platform_cannot_evaluate_is_never_met() {
    // A broker that publishes no ETag makes every explicit `If-Match` fail. Treating "unknown"
    // as "satisfied" would turn a lost race into a silent overwrite.
    for sent in ["\"7f3a\"", "\"a\", \"b\"", "", "   ", ",", "\"\""] {
        assert!(
            !matches(sent, None),
            "{sent:?} was satisfied by no entity tag at all"
        );
    }
}

#[test]
fn an_entity_tag_the_broker_did_not_quote_satisfies_nothing() {
    // An unquoted value is not an entity tag, so a broker sending one leaves every precondition
    // unevaluated rather than accidentally matching a caller who sent the same characters.
    for etag in ["7f3a", "W/\"7f3a\"", "", " ", "'7f3a'"] {
        assert!(!matches("7f3a", Some(etag)), "{etag:?}");
        assert!(!matches("\"7f3a\"", Some(etag)), "{etag:?}");
    }
}

#[test]
fn the_star_is_satisfied_by_a_representation_existing_and_by_nothing_less() {
    // RFC 9110 section 13.1.1: `*` asks only that a current representation exist, which the
    // caller of this function has just read.
    assert!(matches("*", Some("\"7f3a\"")));
    assert!(matches("*", None));
    assert!(
        matches("  *  ", Some("\"7f3a\"")),
        "surrounding space is not part of the value"
    );

    // A star inside a list is not the star: the list is compared member by member, and `*` is
    // not an entity tag, so it matches nothing.
    assert!(!matches("*, \"7f3a\"", Some("\"other\"")));
    assert!(!matches("\"*\"", Some("\"7f3a\"")));
}

#[test]
fn a_list_matches_by_member_and_an_empty_member_matches_nothing() {
    assert!(matches("\"a\", \"7f3a\", \"b\"", Some("\"7f3a\"")));
    assert!(
        matches("\"7f3a\",\"b\"", Some("\"7f3a\"")),
        "no space after the comma"
    );
    assert!(!matches("\"a\", , \"b\"", Some("\"7f3a\"")));
    assert!(
        !matches(", ,", Some("\"7f3a\"")),
        "a list of nothing matches nothing, and must not match by an empty candidate"
    );
}

#[test]
fn an_entity_tag_is_compared_byte_for_byte() {
    // Strong comparison: not case-insensitive, not whitespace-insensitive inside the quotes,
    // and not a prefix match.
    assert!(!matches("\"7F3A\"", Some("\"7f3a\"")));
    assert!(!matches("\"7f3\"", Some("\"7f3a\"")));
    assert!(!matches("\"7f3a \"", Some("\"7f3a\"")));
    assert!(
        matches("  \"7f3a\"  ", Some("  \"7f3a\"  ")),
        "only the ends are trimmed"
    );
}

#[test]
fn a_tag_that_is_not_plain_text_is_still_only_equal_to_itself() {
    let unicode = "\"ž-7f3a-\u{1f600}\"";
    assert!(matches(unicode, Some(unicode)));
    assert!(!matches(unicode, Some("\"z-7f3a-\"")));
    // Percent-encoding is not decoded here: an entity tag is an opaque string.
    assert!(!matches("\"%7A\"", Some("\"z\"")));
}

#[test]
fn a_very_long_list_is_answered_rather_than_walked_for_ever() {
    // A caller can send a large `If-Match`; the answer has to be the right one and arrive.
    let mut sent: Vec<String> = (0..5_000).map(|n| format!("\"tag-{n}\"")).collect();
    assert!(!matches(&sent.join(", "), Some("\"7f3a\"")));
    sent.push("\"7f3a\"".to_owned());
    assert!(matches(&sent.join(", "), Some("\"7f3a\"")));
}

// --- evaluate: what the read before a write does with a broker that misbehaves (T-1912) ----

mod reading_first {
    use super::*;
    use axum::body::Body;
    use axum::extract::State;
    use axum::routing::any;
    use axum::Router;
    use context_gateway::pdp::conditional::{evaluate, Precondition};
    use context_gateway::proxy::Broker;

    /// What the stub broker answers every read with.
    #[derive(Clone, Copy)]
    struct Answer {
        status: u16,
        /// The body, exactly as bytes: a test needs to send something that is not JSON.
        body: &'static str,
        etag: Option<&'static str>,
    }

    async fn broker(answer: Answer) -> Broker {
        let app = Router::new()
            .fallback(any(|State(answer): State<Answer>| async move {
                let mut response = axum::http::Response::builder()
                    .status(answer.status)
                    .header("content-type", "application/json");
                if let Some(etag) = answer.etag {
                    response = response.header("etag", etag);
                }
                response.body(Body::from(answer.body)).expect("a response")
            }))
            .with_state(answer);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free port");
        let address = listener.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Broker::new(format!("http://{address}"))
    }

    fn refusal(precondition: Precondition) -> jc_core::ProblemDetails {
        match precondition {
            Precondition::Refuse(problem) => problem,
            Precondition::Forward(tag) => {
                panic!("the write was forwarded carrying {tag:?} instead of being refused")
            }
        }
    }

    #[tokio::test]
    async fn a_write_that_addresses_no_entity_is_forwarded_without_a_read() {
        // A create has no stored state, so there is nothing to read and nothing to refuse. The
        // broker here would answer 500 to anything, which is how the test knows it was not asked.
        let broker = broker(Answer {
            status: 500,
            body: "{}",
            etag: None,
        })
        .await;
        let decision = evaluate(&broker, COLLECTION, &headers(None), &filtered()).await;
        assert!(matches!(decision, Precondition::Forward(None)));
    }

    #[tokio::test]
    async fn a_broker_that_fails_the_read_refuses_the_write_rather_than_letting_it_through() {
        // The gateway cannot decide, so it does not: an upstream that is down must never become
        // an upstream that is unguarded.
        for status in [500, 502, 503, 400, 401, 403, 409, 429] {
            let broker = broker(Answer {
                status,
                body: "{}",
                etag: None,
            })
            .await;
            let problem = refusal(evaluate(&broker, ENTITY, &headers(None), &filtered()).await);
            assert_eq!(
                problem.status, 502,
                "a broker answering {status} was not reported as an unavailable upstream"
            );
            assert_eq!(problem.title, "Broker Unavailable");
        }
    }

    #[tokio::test]
    async fn a_broker_that_does_not_answer_at_all_refuses_the_write() {
        // Nothing is listening on this port, so the read fails before a status exists.
        let broker = Broker::new("http://127.0.0.1:1".to_owned());
        let problem = refusal(evaluate(&broker, ENTITY, &headers(None), &filtered()).await);
        assert!(
            problem.status >= 500,
            "a write went ahead on an unreachable broker: {problem:?}"
        );
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_read_as_no_entity_and_the_write_is_refused() {
        // `serde_json::from_slice(..).unwrap_or(Value::Null)`: the read "succeeded" with a body
        // nothing can be decided from, and a null entity satisfies no projection, so the answer
        // is the miss a caller who may not see the entity would get anyway (R20).
        let broker = broker(Answer {
            status: 200,
            body: "<html>not the broker you were looking for</html>",
            etag: Some("\"7f3a\""),
        })
        .await;
        let constraints = Constraints {
            types: ["AirQualityObserved".to_owned()].into_iter().collect(),
            ..filtered()
        };
        let problem = refusal(evaluate(&broker, ENTITY, &headers(None), &constraints).await);
        assert_eq!(
            problem.status, 404,
            "a body nobody can read is not a permit"
        );
    }

    #[tokio::test]
    async fn an_entity_the_broker_does_not_hold_is_a_miss_and_not_a_refusal() {
        // R20: a write to an entity the caller cannot see, and one to an entity that is not
        // there, are the same answer, so a probe cannot tell them apart.
        let broker = broker(Answer {
            status: 404,
            body: r#"{"type":"https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound","status":404}"#,
            etag: None,
        })
        .await;
        let problem = refusal(evaluate(&broker, ENTITY, &headers(None), &plain()).await);
        assert_eq!(problem.status, 404);
    }

    #[tokio::test]
    async fn an_if_match_against_an_entity_that_is_not_there_is_412_and_not_404() {
        // RFC 9110 section 13.1.1: a precondition on a target with no current representation
        // fails, whatever the tag says. The caller asked a question about a version, and the
        // honest answer is that it does not match, not that the resource is missing.
        let broker = broker(Answer {
            status: 404,
            body: "{}",
            etag: None,
        })
        .await;
        for sent in ["*", "\"7f3a\""] {
            let problem = refusal(evaluate(&broker, ENTITY, &headers(Some(sent)), &plain()).await);
            assert_eq!(problem.status, 412, "If-Match: {sent}");
        }
    }
}
