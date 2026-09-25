//! Edge cases of `app::schema_artifact` (T-1883, EP-26, MP-02).
//!
//! Contract, in one sentence: `GET /api/endpoint/{slug}/schema/{version}/{artifact}` serves one
//! formalism of one **major** version of the endpoint's model, projected to this caller's
//! grants — the JSON documents from the committed artifacts, and SHACL, OWL, RDF, LinkML and
//! Markdown rendered from the projected model rather than read from a file, so that no
//! formalism can carry a slot the grant forbids (T-0162, T-0284, EP-47, EP-49, DM-22).
//!
//! The inputs are the slug, the version segment, the artifact segment, the `Authorization`
//! header, `Accept` (which chooses the formalism when the artifact is the bare `model`) and
//! `If-None-Match`. The property that carries the whole surface: **every** formalism is
//! narrowed by the same `schema::visible`, so a caller who cannot read a class cannot read it
//! as Turtle either. The one that carries the routing: `{version}` is `v{major}` and nothing
//! else, so no other shape of that segment reaches a model.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

/// A public endpoint: one class granted, one not, and one slot of the granted class not.
const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
/// The same model under a grant that names both classes.
const WIDE: &str = "w8k3zq6nxv2htb5rjs9cyd4gpm";
/// The same, needing a token.
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";
/// A grant that names no `information` at all — the dev seed's own `public-read` — so the
/// caller has no type whitelist, on an endpoint that hides two attributes (T-2341).
const UNLISTED: &str = "b6t4hm9xq3vzk7npwr2sjc5dyg";
/// The same open grant, with a prohibition taking one attribute back instead (GW8).
const PROHIBITED: &str = "r3v8sn5kqz2mbt7hxwc9jd4pgf";
/// The same open grant, hiding only the attribute that lives in the committed `@context` and
/// in no `$defs`: the one case where the context builder is the only thing that redacts.
const CONTEXT_ONLY: &str = "y2c7wk4rq9zmxs6btn3hjd5vpf";

/// Every spelling of every formalism this surface serves.
const ARTIFACTS: &[&str] = &[
    "json-schema",
    "model.schema.json",
    "context.jsonld",
    "context",
    "model.shacl.ttl",
    "shacl",
    "model.owl.ttl",
    "owl",
    "model.rdf.ttl",
    "rdf",
    "model.linkml.yaml",
    "linkml",
    "model.md",
    "docs",
    "model",
];

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn air_quality() -> Model {
    Model {
        name: "bb-air-quality".to_owned(),
        version: "1.4.0".to_owned(),
        major: 1,
        classes: vec![
            "AirQualityObserved".to_owned(),
            "InternalIncident".to_owned(),
        ],
        json_schema: Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$defs": {
                "AirQualityObserved": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "type": { "const": "AirQualityObserved" },
                        "pm10": { "type": "number" },
                        "internalNote": { "type": "string" },
                    },
                },
                "InternalIncident": {
                    "type": "object",
                    "properties": { "severity": { "type": "string" } },
                },
            },
        })),
        context: Some(json!({
            "@context": {
                "AirQualityObserved": "https://bb.example.sk/schema/air-quality/AirQualityObserved",
                "InternalIncident": "https://bb.example.sk/schema/air-quality/InternalIncident",
                "pm10": "https://bb.example.sk/schema/air-quality/pm10",
                "internalNote": "https://bb.example.sk/schema/air-quality/internalNote",
                // In the committed context and in no `$defs`: the JSON Schema builder never
                // sees this term, so only the `@context` can give it away.
                "stationApiKey": "https://bb.example.sk/schema/air-quality/stationApiKey",
            }
        })),
    }
}

fn endpoint(slug: &str, audience: Audience, policies: Vec<PolicySpec>) -> Endpoint {
    endpoint_with(slug, audience, policies, &[])
}

fn endpoint_with(
    slug: &str,
    audience: Audience,
    policies: Vec<PolicySpec>,
    hidden: &[&str],
) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: vec![air_quality()],
        view_mapping: None,
        policies,
    }
}

fn narrow_grant() -> Vec<PolicySpec> {
    vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
"#,
    )]
}

fn wide_grant() -> Vec<PolicySpec> {
    vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
      - type: InternalIncident
"#,
    )]
}

/// Every operation, no `information`: the shape the dev seed's `public-read` policy has, and
/// the one that leaves `Visible::types` empty (T-2341).
fn unlisted_grant() -> Vec<PolicySpec> {
    vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#,
    )]
}

fn prohibited_grant() -> Vec<PolicySpec> {
    let mut policies = unlisted_grant();
    policies.push(policy(
        r#"contextSpaceRef: ovzdusie
effect: prohibition
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [internalNote]
"#,
    ));
    policies
}

fn app_of(realm: &common::Realm) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(OPEN, Audience::Public, narrow_grant()),
            endpoint(WIDE, Audience::Public, wide_grant()),
            endpoint(CLOSED, Audience::Organization, narrow_grant()),
            endpoint_with(
                UNLISTED,
                Audience::Public,
                unlisted_grant(),
                &["internalNote", "stationApiKey"],
            ),
            endpoint(PROHIBITED, Audience::Public, prohibited_grant()),
            endpoint_with(
                CONTEXT_ONLY,
                Audience::Public,
                unlisted_grant(),
                &["stationApiKey"],
            ),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://bb.example.sk".to_owned()),
        ),
    ))
}

fn app() -> axum::Router {
    app_of(&common::Realm::new())
}

/// The status, the media type, the `ETag` and the body as text.
async fn call(request: Request<Body>) -> (StatusCode, String, String, String) {
    let response = app().oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let header = |name: axum::http::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let media = header(axum::http::header::CONTENT_TYPE);
    let etag = header(axum::http::header::ETAG);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        media,
        etag,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

fn artifact(slug: &str, version: &str, artifact: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/endpoint/{slug}/schema/{version}/{artifact}"))
        .body(Body::empty())
        .expect("a request")
}

/// T-0284, EP-47: the first case, and the whole reason the non-JSON formalisms are rendered
/// rather than served from a file. Every spelling of every artifact is fetched, and none of
/// them carries the class the grant does not name or the slot it leaves out.
#[tokio::test]
async fn no_formalism_carries_a_class_or_a_slot_the_grant_leaves_out() {
    for name in ARTIFACTS {
        let (status, media, _, body) = call(artifact(OPEN, "v1", name)).await;
        assert_eq!(status, StatusCode::OK, "{name} answered {body:.200}");
        assert!(!media.is_empty(), "{name} named no media type");

        for secret in ["InternalIncident", "internalNote", "severity"] {
            assert!(!body.contains(secret), "{name} leaked {secret:?}: {body}");
        }
        assert!(
            body.contains("AirQualityObserved"),
            "{name} rendered nothing at all: {body:.200}",
        );
    }
}

/// EP-47: the same artifacts under a grant that names both classes do carry the second one,
/// so the case above is the narrowing and not an artifact that is empty for everybody.
#[tokio::test]
async fn a_wider_grant_does_see_the_second_class() {
    for name in ARTIFACTS {
        let (status, _, _, body) = call(artifact(WIDE, "v1", name)).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert!(
            body.contains("InternalIncident"),
            "{name} narrowed a grant that names it: {body:.300}",
        );
    }
}

/// DM-22: the version segment is `v{major}` and nothing else. Every other shape of it is 404,
/// including the ones that look like a version — a full semantic version, a zero-padded
/// major, a negative one, and one that overflows the integer it is parsed into.
#[tokio::test]
async fn nothing_but_v_major_names_a_version() {
    for version in [
        "1",
        "v",
        "V1",
        "v-1",
        "v1.4",
        "v1.4.0",
        "1.4.0",
        "v1x",
        "v%201",
        "v4294967296",
        "v99999999999999999999",
        "v0x1",
        "latest",
        "..",
        "%2e%2e",
        "v1%00",
    ] {
        let (status, _, _, body) = call(artifact(OPEN, version, "json-schema")).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{version:?} answered {body:.200}",
        );
    }

    // And the one that does: `v0` is a legal major this endpoint has no model for, which is
    // the same 404 — a major nobody publishes is not a different answer from a bad segment.
    let (absent_major, _, _, _) = call(artifact(OPEN, "v0", "json-schema")).await;
    assert_eq!(absent_major, StatusCode::NOT_FOUND);

    // `u32::from_str` accepts a leading `+`, so `v+1` is a second URL for the v1 document
    // and carries the same ETag. Nothing of the projection changes, so it is a second
    // spelling rather than a way past anything; recorded as a defect (chyby.md) because a
    // document with two URLs is a document a cache holds twice.
    let canonical = call(artifact(OPEN, "v1", "json-schema")).await;
    let plus = call(artifact(OPEN, "v+1", "json-schema")).await;
    assert_eq!(plus, canonical, "v+1 is the same document as v1");
}

/// EP-49: an artifact name this surface does not serve is 404, and nothing in the segment
/// reaches a file path — a traversal, an absolute path and an encoded separator are all just
/// names nothing answers to.
#[tokio::test]
async fn nothing_but_a_known_artifact_name_is_served() {
    for name in [
        "",
        "%20",
        "MODEL",
        "model.schema.JSON",
        "model.json",
        "schema.json",
        "model.ttl",
        "..%2f..%2f..%2fetc%2fpasswd",
        "%2e%2e%2f%2e%2e%2fCargo.toml",
        "model.schema.json%00.txt",
        "model%20",
        "model%0d%0aX-Injected:%201",
        &"a".repeat(4096),
    ] {
        let (status, _, _, body) = call(artifact(OPEN, "v1", name)).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{name:?} answered {body:.200}"
        );
    }
}

/// EP-49: the bare `model` is negotiated from `Accept`, and each formalism it can resolve to
/// is labelled as itself. A client that asks for Turtle and is handed JSON would parse
/// nothing.
#[tokio::test]
async fn the_bare_model_is_negotiated_and_labelled() {
    for (accept, expected) in [
        ("", "application/schema+json"),
        ("*/*", "application/schema+json"),
        ("application/json", "application/schema+json"),
        ("application/ld+json", "application/ld+json"),
        ("text/turtle", "text/turtle"),
        ("text/turtle;profile=owl", "text/turtle; profile=\"owl\""),
        ("application/yaml", "text/yaml"),
        ("text/markdown", "text/markdown"),
    ] {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/endpoint/{OPEN}/schema/v1/model"))
                    .header(axum::http::header::ACCEPT, accept)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::OK, "{accept:?}");
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(expected),
            "{accept:?}",
        );
    }
}

/// EP-51: every artifact carries the digest of the bytes that were sent, and the tag the
/// caller was given brings back a 304 with nothing in it.
#[tokio::test]
async fn every_artifact_revalidates_against_its_own_bytes() {
    for name in ARTIFACTS {
        let (_, _, etag, body) = call(artifact(OPEN, "v1", name)).await;
        assert!(!etag.is_empty(), "{name} carries no ETag");
        assert!(!body.is_empty(), "{name} is empty");

        let response = app()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/endpoint/{OPEN}/schema/v1/{name}"))
                    .header(axum::http::header::IF_NONE_MATCH, &etag)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{name}");
        let returned = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("a readable body");
        assert!(returned.is_empty(), "{name} sent a body with its 304");
    }
}

/// EP-51: the tag belongs to the projection, so one grant's tag never revalidates another
/// grant's artifact. This is the case that would otherwise hand a narrow caller a 304 for a
/// document they have never been allowed to see.
#[tokio::test]
async fn one_grants_tag_never_revalidates_another_grants_artifact() {
    for name in ARTIFACTS {
        let (_, _, narrow, _) = call(artifact(OPEN, "v1", name)).await;
        let (_, _, wide, _) = call(artifact(WIDE, "v1", name)).await;
        assert_ne!(narrow, wide, "{name}: two projections, one tag");

        let response = app()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/endpoint/{WIDE}/schema/v1/{name}"))
                    .header(axum::http::header::IF_NONE_MATCH, &narrow)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::OK, "{name}");
    }
}

/// EP-03, PF-46: an unknown slug and an endpoint that needs a token publish no artifact, and
/// their refusals carry no part of one.
#[tokio::test]
async fn an_endpoint_the_caller_may_not_have_publishes_no_artifact() {
    for name in ARTIFACTS {
        let (absent, _, _, absent_body) =
            call(artifact("zzzzzzzzzzzzzzzzzzzzzzzzzz", "v1", name)).await;
        assert_eq!(absent, StatusCode::NOT_FOUND, "{name}");

        let (closed, _, _, closed_body) = call(artifact(CLOSED, "v1", name)).await;
        assert_eq!(closed, StatusCode::UNAUTHORIZED, "{name}");

        for body in [&absent_body, &closed_body] {
            for secret in ["AirQualityObserved", "pm10", "bb-air-quality"] {
                assert!(!body.contains(secret), "{name} leaked {secret:?}: {body}");
            }
        }
    }
}

/// A refusal is never a 304: presenting a tag against a slug the caller may not have does not
/// confirm that their cached copy is current, which would confirm the endpoint is there.
#[tokio::test]
async fn a_refusal_is_never_answered_as_not_modified() {
    let (_, _, etag, _) = call(artifact(OPEN, "v1", "json-schema")).await;

    for slug in ["zzzzzzzzzzzzzzzzzzzzzzzzzz", CLOSED] {
        for presented in [etag.as_str(), "*"] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/endpoint/{slug}/schema/v1/json-schema"))
                        .header(axum::http::header::IF_NONE_MATCH, presented)
                        .body(Body::empty())
                        .expect("a request"),
                )
                .await
                .expect("the gateway answers");
            assert_ne!(response.status(), StatusCode::NOT_MODIFIED, "{slug}");
        }
    }
}

/// R20: an artifact describes the model and nothing of the deployment that serves it.
#[tokio::test]
async fn no_artifact_names_anything_of_the_deployment() {
    for name in ARTIFACTS {
        let (_, _, _, body) = call(artifact(OPEN, "v1", name)).await;
        for secret in ["127.0.0.1", "realms/joinedcontext", "crates/", "/src/"] {
            assert!(!body.contains(secret), "{name} leaked {secret:?}: {body}");
        }
    }
}

/// GW20: the tenancy headers are stripped before anything is resolved, so a forged one cannot
/// make this surface render another space's model.
#[tokio::test]
async fn a_forged_tenant_header_does_not_change_an_artifact() {
    let honest = call(artifact(OPEN, "v1", "model.shacl.ttl")).await;
    let forged = call(
        Request::builder()
            .uri(format!("/api/endpoint/{OPEN}/schema/v1/model.shacl.ttl"))
            .header("NGSILD-Tenant", "somebody-elses-space")
            .header("X-Forwarded-Groups", "data-steward")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(honest.0, StatusCode::OK);
    assert_eq!(forged, honest);
}

/// The artifacts are read-only, and the route takes exactly two segments after `schema`:
/// nothing but `GET` reaches them, and no deeper path does either.
#[tokio::test]
async fn nothing_but_a_get_on_two_segments_reaches_an_artifact() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/api/endpoint/{OPEN}/schema/v1/json-schema"))
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} reached an artifact",
        );
    }

    for path in [
        format!("/api/endpoint/{OPEN}/schema/v1/json-schema/"),
        format!("/api/endpoint/{OPEN}/schema/v1/json-schema/extra"),
        format!("/api/endpoint/{OPEN}/schema/v1"),
        format!("/api/endpoint/{OPEN}/schema/v1/"),
    ] {
        let (status, _, _, _) = call(
            Request::builder()
                .uri(&path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} answered");
    }
}

/// EP-61, EP-26, T-2341: an endpoint's `hiddenAttributes` are what it does not serve, so the
/// `@context` may not name them either — the schema and the data cannot disagree about which
/// attributes exist. The caller here has no type whitelist, which is what used to make the
/// class test in the term condition answer true for every name and swallow the attribute test
/// in front of it.
#[tokio::test]
async fn a_context_under_a_grant_with_no_type_whitelist_still_hides_the_endpoints_attributes() {
    let (status, media, _, body) = call(artifact(UNLISTED, "v1", "context.jsonld")).await;
    assert_eq!(status, StatusCode::OK, "{body:.200}");
    assert_eq!(media, "application/ld+json");

    for hidden in ["internalNote", "stationApiKey"] {
        assert!(
            !body.contains(hidden),
            "the @context names {hidden}, which this endpoint hides: {body:.400}"
        );
    }
    assert!(
        body.contains("pm10"),
        "the granted attribute was narrowed away with the hidden ones: {body:.400}"
    );
}

/// GW8, EP-61, T-2341: a prohibition takes an attribute back from a grant that would otherwise
/// carry it, and `denied_attrs` is the same set the endpoint's `hiddenAttributes` land in — so
/// the term goes the same way out of the `@context`.
#[tokio::test]
async fn a_prohibition_takes_a_term_out_of_the_context_as_well() {
    let (status, _, _, body) = call(artifact(PROHIBITED, "v1", "context.jsonld")).await;
    assert_eq!(status, StatusCode::OK, "{body:.200}");
    assert!(
        !body.contains("internalNote"),
        "the @context names an attribute a prohibition took back: {body:.400}"
    );
    assert!(
        body.contains("pm10"),
        "the attribute the prohibition left alone is gone too: {body:.400}"
    );
}

/// MP-02, T-2341: the class terms are the reason the type test is in that condition at all, and
/// this is the path that already worked — a caller **with** a type whitelist. The class the
/// grant names stays in the `@context`, the class it does not is still redacted, and neither
/// answer changes because the attribute terms are now decided separately.
#[tokio::test]
async fn the_class_terms_of_a_projected_model_survive_the_attribute_narrowing() {
    let (status, _, _, narrow) = call(artifact(OPEN, "v1", "context.jsonld")).await;
    assert_eq!(status, StatusCode::OK, "{narrow:.200}");
    assert!(
        narrow.contains("AirQualityObserved"),
        "the granted class left the @context: {narrow:.400}"
    );
    assert!(
        !narrow.contains("InternalIncident"),
        "a class the grant does not name is in the @context: {narrow:.400}"
    );

    let (status, _, _, wide) = call(artifact(WIDE, "v1", "context.jsonld")).await;
    assert_eq!(status, StatusCode::OK, "{wide:.200}");
    for class in ["AirQualityObserved", "InternalIncident"] {
        assert!(
            wide.contains(class),
            "the wider grant lost {class} from the @context: {wide:.400}"
        );
    }
}

/// EP-47, T-2341: the index says *that* something was left out and never what. A term the
/// committed `@context` carries and no `$defs` mentions is only ever redacted by the context
/// builder, so the flag has to be raised from what that builder kept back too — otherwise an
/// endpoint that hides exactly such an attribute reports a complete model.
#[tokio::test]
async fn the_index_reports_a_term_only_the_context_left_out() {
    let request = Request::builder()
        .uri(format!("/api/endpoint/{CONTEXT_ONLY}/schema/index.json"))
        .body(Body::empty())
        .expect("a request");
    let (status, _, _, body) = call(request).await;
    assert_eq!(status, StatusCode::OK, "{body:.200}");

    let index: serde_json::Value = serde_json::from_str(&body).expect("the index is JSON");
    let model = index["models"]
        .as_array()
        .and_then(|models| models.first())
        .expect("the index describes the model");
    assert_eq!(
        model["redacted"],
        json!(true),
        "the index reports a complete model although the @context hides two terms: {body:.400}"
    );
    assert!(
        !body.contains("stationApiKey"),
        "the index names what was left out: {body:.400}"
    );
}
