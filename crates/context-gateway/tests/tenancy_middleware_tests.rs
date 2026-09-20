use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::Router;
use context_gateway::middleware::tenancy::{pin_tenant, strip_client_headers, FORGEABLE, TENANT};
use tower::ServiceExt;

/// The broker, as far as this test is concerned: it reports back exactly the headers it
/// was handed, which is the only thing the tenancy rules are about.
async fn broker(headers: HeaderMap) -> String {
    let tenant = headers
        .get(TENANT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<none>");
    let forged: Vec<&str> = FORGEABLE
        .iter()
        .copied()
        .filter(|name| *name != TENANT.as_str() && headers.contains_key(*name))
        .collect();
    format!("tenant={tenant} forged={}", forged.join(","))
}

/// The PEP as the gateway runs it: strip whatever the client claimed, then pin the tenant
/// from the endpoint that was resolved (EP-21, EP-22, GW20).
fn gateway(space: &'static str) -> Router {
    Router::new()
        .route("/ngsi-ld/v1/entities", get(broker))
        .layer(axum::middleware::from_fn(
            move |mut request: Request, next: axum::middleware::Next| async move {
                strip_client_headers(&mut request);
                pin_tenant(&mut request, space).expect("a space name is a legal header value");
                next.run(request).await
            },
        ))
}

async fn call(request: Request) -> (StatusCode, String) {
    let response = gateway("ovzdusie")
        .oneshot(request)
        .await
        .expect("the stack answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("a small body");
    (status, String::from_utf8(body.to_vec()).expect("utf-8"))
}

/// GW20: whatever the client sends as its tenant, the broker sees the endpoint's space.
#[tokio::test]
async fn a_forged_tenant_header_never_reaches_the_broker() {
    let (status, body) = call(
        Request::builder()
            .uri("/ngsi-ld/v1/entities")
            .header("NGSILD-Tenant", "doprava")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "tenant=ovzdusie forged=");
}

/// A header sent twice must not leave a copy behind for whoever reads the last value.
#[tokio::test]
async fn a_repeated_tenant_header_is_removed_in_full() {
    let (_, body) = call(
        Request::builder()
            .uri("/ngsi-ld/v1/entities")
            .header("NGSILD-Tenant", "doprava")
            .header("NGSILD-Tenant", "urbanizmus")
            .header("ngsild-tenant", "ovzdusie-shadow")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(body, "tenant=ovzdusie forged=");
}

/// EP-21, GW25: identity and scope headers are gateway conclusions too. A client that
/// sends them is a client trying to be someone else.
#[tokio::test]
async fn every_forgeable_identity_header_is_stripped() {
    let mut builder = Request::builder().uri("/ngsi-ld/v1/entities");
    for name in FORGEABLE {
        builder = builder.header(*name, "forged-by-the-client");
    }
    let (_, body) = call(builder.body(Body::empty()).expect("a request")).await;

    assert_eq!(body, "tenant=ovzdusie forged=");
}

/// Stripping must not touch the headers the request actually needs.
#[tokio::test]
async fn an_ordinary_header_survives_the_stripping() {
    let mut request = Request::builder()
        .uri("/ngsi-ld/v1/entities")
        .header("accept", "application/ld+json")
        .header("link", "<https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld>; rel=\"http://www.w3.org/ns/json-ld#context\"")
        .header("x-userinfo", "forged")
        .body(Body::empty())
        .expect("a request");

    strip_client_headers(&mut request);

    assert_eq!(
        request.headers().get("accept").expect("kept"),
        "application/ld+json"
    );
    assert!(request.headers().contains_key("link"));
    assert!(!request.headers().contains_key("x-userinfo"));
}

/// The tenant is set from the endpoint, so a space name that is not a legal header value
/// is a reconciler bug: it pins nothing rather than pinning something wrong.
#[tokio::test]
async fn an_illegal_space_name_pins_nothing() {
    let mut request = Request::builder()
        .uri("/ngsi-ld/v1/entities")
        .body(Body::empty())
        .expect("a request");

    assert!(pin_tenant(&mut request, "ovzdusie\r\nx-userinfo: root").is_err());
    assert!(!request.headers().contains_key(TENANT));

    pin_tenant(&mut request, "ovzdusie").expect("a DNS-1123 label is always legal");
    assert_eq!(request.headers().get(TENANT).expect("pinned"), "ovzdusie");
}
