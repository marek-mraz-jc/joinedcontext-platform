//! Edge cases of `app::space_record`, `GET /cs/{space}` (T-1886, EP-26, MP-02).
//!
//! Contract, in one sentence: it answers the DCAT-AP record of a space this caller may discover, in
//! one of the three representations, and for every other name — one that does not exist, one whose
//! grants do not reach the caller, an endpoint slug, another spelling — it answers the one `404`
//! that tells them apart from nothing (SP-06, SP-10, SP-11, R20). Nothing a manifest wrote can
//! close the page or the triple it is rendered into.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const OPEN: &str = "ovzdusie";
const CLOSED: &str = "uctovnictvo";
const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
const HOST: &str = "https://bb.example.sk";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn public_read(space: &str) -> PolicySpec {
    policy(&format!(
        r#"contextSpaceRef: {space}
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity, retrieveEntity]
"#
    ))
}

fn accountants_only(space: &str) -> PolicySpec {
    policy(&format!(
        r#"contextSpaceRef: {space}
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: accountant }}
operations: [queryEntity]
"#
    ))
}

fn endpoint_of(space: &str, policies: Vec<PolicySpec>) -> Endpoint {
    Endpoint {
        slug: space.to_owned(),
        title: BTreeMap::new(),
        description: BTreeMap::new(),
        space: space.to_owned(),
        project: space.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/cs/{space}"),
        models: Vec::new(),
        view_mapping: None,
        policies,
    }
}

fn space_named(space: &str, title: &str, policies: Vec<PolicySpec>) -> Space {
    Space {
        endpoint: Arc::new(endpoint_of(space, policies)),
        title: BTreeMap::from([("en".to_owned(), title.to_owned())]),
        description: BTreeMap::new(),
        is_sandbox: false,
        default_locale: Some("sk".to_owned()),
    }
}

/// The deployment these cases run against: one space anonymous callers reach, one they do not, and
/// one endpoint whose slug is a name of its own (EP-03).
fn gateway(spaces: Vec<Space>) -> axum::Router {
    let mut endpoint = endpoint_of(OPEN, vec![public_read(OPEN)]);
    endpoint.slug = SLUG.to_owned();
    endpoint.base_path = format!("/api/endpoint/{SLUG}");
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint])
        .serve_spaces(spaces),
    ))
}

fn two_spaces() -> Vec<Space> {
    vec![
        space_named(OPEN, "The air quality space", vec![public_read(OPEN)]),
        space_named(CLOSED, "Účtovníctvo", vec![accountants_only(CLOSED)]),
    ]
}

async fn call(app: axum::Router, request: Request<Body>) -> (StatusCode, String, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let media = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a request")
}

fn get_accepting(path: &str, accept: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header(axum::http::header::ACCEPT, accept)
        .body(Body::empty())
        .expect("a request")
}

/// The first case: no other spelling of a space name is that name. The names are guessable words,
/// so every near miss has to answer what a name nobody created answers (SP-06, R20).
#[tokio::test]
async fn no_other_spelling_of_a_space_name_is_that_space() {
    let (expected_status, expected_media, expected_body) =
        call(gateway(two_spaces()), get("/cs/nothing-here")).await;
    assert_eq!(expected_status, StatusCode::NOT_FOUND);

    for path in [
        "/cs/OVZDUSIE",
        "/cs/Ovzdusie",
        "/cs/ovzdusie.",
        "/cs/ovzdusie%20",
        "/cs/ovzdusie%2520",
        "/cs/ovzdusi",
        "/cs/ovzdusiee",
        "/cs/ovzdusie/",
    ] {
        let (status, media, body) = call(gateway(two_spaces()), get(path)).await;
        assert_eq!(status, expected_status, "{path}: {body}");
        assert_eq!(media, expected_media, "{path}");
        assert_eq!(body, expected_body, "{path}: one answer for all of them");
    }

    // A percent-encoded unreserved character is the same character (RFC 3986 2.3), so `%6F` is an
    // `o` and names the same space. That is equivalence, not a second name.
    let (status, _, _) = call(gateway(two_spaces()), get("/cs/%6Fvzdusie")).await;
    assert_eq!(status, StatusCode::OK, "%6F is an `o`");
}

/// A slug is not a space name and a space name is not a slug: the two namespaces do not meet, so a
/// slug that happened to spell a space cannot reach it and a space name cannot be guessed as a slug
/// (EP-03).
#[tokio::test]
async fn a_slug_is_not_a_space_name_and_a_space_name_is_not_a_slug() {
    for path in [
        format!("/cs/{SLUG}"),
        format!("/api/endpoint/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"),
        format!("/api/endpoint/{OPEN}"),
    ] {
        let (status, _, body) = call(gateway(two_spaces()), get(&path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
}

/// A space whose grants do not reach this caller is the same `404`, and a token that does not
/// verify against it says no more: neither is a way to learn the name is real (SP-11, T-0814).
#[tokio::test]
async fn an_ungranted_space_says_no_more_than_an_unknown_one() {
    let (unknown_status, unknown_media, unknown_body) =
        call(gateway(two_spaces()), get("/cs/nothing-here")).await;

    for request in [
        get(&format!("/cs/{CLOSED}")),
        get_accepting(&format!("/cs/{CLOSED}"), "text/turtle"),
        get_accepting(&format!("/cs/{CLOSED}"), "text/html"),
        Request::builder()
            .uri(format!("/cs/{CLOSED}"))
            .header(axum::http::header::AUTHORIZATION, "Bearer not-a-token")
            .body(Body::empty())
            .expect("a request"),
    ] {
        let (status, media, body) = call(gateway(two_spaces()), request).await;
        assert_eq!(status, unknown_status);
        assert_eq!(media, unknown_media);
        assert_eq!(body, unknown_body, "and the body is the same bytes");
    }
}

/// The record of a space the caller may not discover leaks nothing about it: not its title, not its
/// locale, not the role its grant names.
#[tokio::test]
async fn the_refusal_carries_nothing_of_the_space_it_refuses() {
    let (_, _, body) = call(gateway(two_spaces()), get(&format!("/cs/{CLOSED}"))).await;

    for secret in [
        CLOSED,
        "Účtovníctvo",
        "accountant",
        "queryEntity",
        "did:web",
    ] {
        assert!(!body.contains(secret), "{secret} came out in {body}");
    }
}

/// `Accept` picks one of the three representations and never a fourth: an unknown type, a
/// q-value, an empty header and a list all end in a representation the caller can read.
#[tokio::test]
async fn accept_decides_between_exactly_three_representations() {
    for (accept, media) in [
        ("application/ld+json", "application/ld+json"),
        ("application/json", "application/ld+json"),
        ("text/turtle", "text/turtle"),
        ("text/html", "text/html; charset=utf-8"),
        ("application/xhtml+xml", "text/html; charset=utf-8"),
        ("*/*", "application/ld+json"),
        ("", "application/ld+json"),
        ("application/pdf", "application/ld+json"),
        (
            "application/ld+json;profile=\"urn:x\"",
            "application/ld+json",
        ),
        (
            "text/html,application/xhtml+xml,*/*;q=0.8",
            "text/html; charset=utf-8",
        ),
        ("application/pdf,text/turtle", "text/turtle"),
        ("text/turtle;q=0.1,text/html;q=0.9", "text/turtle"),
    ] {
        let (status, answered, body) = call(
            gateway(two_spaces()),
            get_accepting(&format!("/cs/{OPEN}"), accept),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{accept:?}: {body}");
        assert_eq!(answered, media, "{accept:?}");
    }
}

/// An `Accept` that is not text at all does not reach the header at all: a byte string no header
/// may carry is refused by the surface, not answered with a guess.
#[tokio::test]
async fn an_accept_header_of_raw_bytes_is_refused_by_the_surface() {
    let request = Request::builder()
        .uri(format!("/cs/{OPEN}"))
        .header(
            axum::http::header::ACCEPT,
            axum::http::HeaderValue::from_bytes(b"text/\xffhtml").expect("a header value"),
        )
        .body(Body::empty())
        .expect("a request");

    let (status, _, body) = call(gateway(two_spaces()), request).await;

    // The header does not parse as text, so nothing negotiates on it: the JSON-LD record is the
    // answer, and no byte of that header is echoed anywhere in it.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains('\u{fffd}'), "{body}");
}

/// A title a manifest wrote cannot close the page it is rendered into: the HTML representation
/// escapes it, so a script in a title stays text (SP-10).
#[tokio::test]
async fn a_title_cannot_close_the_page_it_is_rendered_into() {
    let hostile = "</title><script>alert(1)</script><p title=\"";
    let spaces = vec![space_named(OPEN, hostile, vec![public_read(OPEN)])];

    let (status, media, body) = call(
        gateway(spaces),
        get_accepting(&format!("/cs/{OPEN}"), "text/html"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/html; charset=utf-8");
    assert!(
        !body.contains("<script"),
        "no tag of the title survives as a tag: {body}"
    );
    assert!(
        !body.contains("title=\""),
        "and no attribute either: {body}"
    );
    assert!(
        body.contains("&lt;script&gt;"),
        "it is text, and it is still there: {body}"
    );
    assert!(
        body.contains("&quot;"),
        "the quote is a quote entity: {body}"
    );
}

/// The same title cannot close a Turtle literal either: a quote, a backslash and a newline are
/// escaped, so the triple a partner's connector parses is still one triple.
#[tokio::test]
async fn a_title_cannot_close_the_triple_it_is_rendered_into() {
    let hostile = "quote \" backslash \\ newline \n <urn:x> a dcat:Dataset .";
    let spaces = vec![space_named(OPEN, hostile, vec![public_read(OPEN)])];

    let (status, media, body) = call(
        gateway(spaces),
        get_accepting(&format!("/cs/{OPEN}"), "text/turtle"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/turtle");
    assert!(
        body.contains("quote \\\" backslash"),
        "the quote is escaped: {body}"
    );
    assert!(
        body.contains("backslash \\\\ newline"),
        "the backslash is escaped: {body}"
    );
    assert!(
        body.contains("newline \\n <urn:x>"),
        "the newline is escaped: {body}"
    );
    // Everything the title said stayed inside one `dct:title` literal, so none of it became a
    // triple of its own.
    let carrying: Vec<&str> = body.lines().filter(|line| line.contains("urn:x")).collect();
    assert_eq!(carrying.len(), 1, "{carrying:?}");
    assert!(
        carrying[0].trim_start().starts_with("dct:title \""),
        "the title is a literal and nothing else: {:?}",
        carrying[0]
    );
}

/// A space that declares no locale says nothing about one, and an empty description is absent
/// rather than an empty string: a catalogue reads absence, not `null`.
#[tokio::test]
async fn what_a_space_does_not_declare_is_absent_from_its_record() {
    let mut space = space_named(OPEN, "The air quality space", vec![public_read(OPEN)]);
    space.default_locale = None;
    space.description = BTreeMap::new();

    let (status, _, body) = call(gateway(vec![space]), get(&format!("/cs/{OPEN}"))).await;

    assert_eq!(status, StatusCode::OK);
    let record: Value = serde_json::from_str(&body).expect("json-ld");
    assert!(record.get("dct:language").is_none(), "{body}");
    assert!(record.get("dct:description").is_none(), "{body}");
    assert!(!body.contains("null"), "{body}");
}

/// A sandbox says it is one, so a catalogue that copies the record does not treat a space that is
/// thrown away on its TTL as a lasting dataset (PF-19).
#[tokio::test]
async fn a_sandbox_space_says_it_is_temporary() {
    let mut space = space_named(OPEN, "A sandbox", vec![public_read(OPEN)]);
    space.is_sandbox = true;

    let (status, _, body) = call(gateway(vec![space]), get(&format!("/cs/{OPEN}"))).await;

    assert_eq!(status, StatusCode::OK);
    let record: Value = serde_json::from_str(&body).expect("json-ld");
    assert_eq!(
        record["adms:status"],
        json!("http://purl.org/adms/status/UnderDevelopment"),
        "{body}"
    );
}

/// The services of a record are exactly the children the space serves: an MCP service only when the
/// endpoint serves MCP, and no service for a representation it does not (SP-04).
#[tokio::test]
async fn the_record_offers_only_the_children_the_space_serves() {
    let mut space = space_named(OPEN, "The air quality space", vec![public_read(OPEN)]);
    let mut endpoint = endpoint_of(OPEN, vec![public_read(OPEN)]);
    endpoint.representations = vec![Representation::NgsiLd];
    space.endpoint = Arc::new(endpoint);

    let (status, _, body) = call(gateway(vec![space]), get(&format!("/cs/{OPEN}"))).await;

    assert_eq!(status, StatusCode::OK);
    let record: Value = serde_json::from_str(&body).expect("json-ld");
    let urls: Vec<&str> = record["dcat:service"]
        .as_array()
        .expect("services")
        .iter()
        .filter_map(|service| service["dcat:endpointURL"].as_str())
        .collect();
    assert!(
        urls.contains(&format!("{HOST}/cs/{OPEN}/ngsi-ld/v1/").as_str())
            || urls
                .iter()
                .any(|url| url.ends_with(&format!("/cs/{OPEN}/ngsi-ld/v1/"))),
        "{urls:?}"
    );
    assert!(
        urls.iter().all(|url| !url.ends_with("/mcp")),
        "no MCP service for an endpoint that does not serve MCP: {urls:?}"
    );
}

/// Only a read reaches this record: a write method is not an answer and not a 500.
#[tokio::test]
async fn a_write_method_on_the_record_is_refused() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let request = Request::builder()
            .method(method)
            .uri(format!("/cs/{OPEN}"))
            .body(Body::empty())
            .expect("a request");
        let (status, _, body) = call(gateway(two_spaces()), request).await;
        assert!(
            status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND,
            "{method}: {status} {body}"
        );
    }
}

/// Two callers share this URL and are answered differently, so no shared cache may replay one
/// answer to the other (R9, T-2261).
#[tokio::test]
async fn the_record_is_never_stored_in_a_shared_cache() {
    let response = gateway(two_spaces())
        .oneshot(get(&format!("/cs/{OPEN}")))
        .await
        .expect("the gateway answers");

    let headers = response.headers();
    let cache = headers
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(cache.contains("private"), "{cache}");
    assert!(
        cache.contains("no-store") || cache.contains("no-cache"),
        "{cache}"
    );
    assert!(
        headers
            .get(axum::http::header::VARY)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|vary| vary.contains("Authorization")),
        "the answer depends on the token: {headers:?}"
    );
    assert!(
        headers.get("ngsild-tenant").is_none(),
        "no internal tenant comes out: {headers:?}"
    );
}

/// The same request twice is the same document, byte for byte: nothing in the record depends on the
/// order of a map or on when it was built.
#[tokio::test]
async fn the_record_is_reproducible() {
    let (first_status, first_media, first_body) =
        call(gateway(two_spaces()), get(&format!("/cs/{OPEN}"))).await;
    let (again_status, again_media, again_body) =
        call(gateway(two_spaces()), get(&format!("/cs/{OPEN}"))).await;

    assert_eq!(first_status, again_status);
    assert_eq!(first_media, again_media);
    assert_eq!(first_body, again_body);
}
