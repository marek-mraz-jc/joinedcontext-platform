//! The token sidecar against stand-ins for the Kubernetes API and Keycloak (PL-19, T-1508).
//!
//! What it must hold: the token it asks the API for is the named pipeline's own ServiceAccount's,
//! for the realm issuer and for 600 s, asked with the runner's own token; Keycloak gets that token
//! as the assertion and no client id or secret; Keycloak's answer comes back as it is; a client
//! id that is not `{project}/{pipeline}` gets nothing minted.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use base64::Engine;
use serde_json::{json, Value};
use std::io::Write;
use token_sidecar::{
    listen_address, percent_decoded, router, Config, Identity, Sidecar, TOKEN_SECONDS,
};
use tower::ServiceExt;
use wiremock::matchers::{body_json, body_string_contains, header as has_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ISSUER: &str = "https://idm.dev.joinedcontext.com/realms/joinedcontext";

struct World {
    kubernetes: MockServer,
    keycloak: MockServer,
    _own: tempfile_free::Own,
    app: axum::Router,
}

/// A temporary file without a crate for it: the runner's own token.
mod tempfile_free {
    pub struct Own(pub std::path::PathBuf);
    impl Drop for Own {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

async fn world() -> World {
    world_of(Identity::Pipeline, "pipeline-runner").await
}

async fn world_of(identity: Identity, namespace: &str) -> World {
    let kubernetes = MockServer::start().await;
    let keycloak = MockServer::start().await;
    let own = std::env::temp_dir().join(format!(
        "sidecar-own-{}",
        std::process::id() as u128 * 1000 + rand_suffix()
    ));
    std::fs::File::create(&own)
        .and_then(|mut file| file.write_all(b"runner-own-token\n"))
        .expect("the own token");
    let config = Config {
        kubernetes_api: kubernetes.uri(),
        namespace: namespace.to_owned(),
        own_token_file: own.clone(),
        token_url: format!(
            "{}/realms/joinedcontext/protocol/openid-connect/token",
            keycloak.uri()
        ),
        audience: ISSUER.to_owned(),
        identity,
    };
    let app = router(Sidecar::new(config, reqwest::Client::new()));
    World {
        kubernetes,
        keycloak,
        _own: tempfile_free::Own(own),
        app,
    }
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() % 1_000_000)
        .unwrap_or(0)
}

/// What Bento's OAuth 2 client sends: the client id and secret in Basic, form-encoded.
fn bento(client_id: &str) -> Request<Body> {
    let basic = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:unused", client_id.replace('/', "%2F")));
    Request::post("/token")
        .header(header::AUTHORIZATION, format!("Basic {basic}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("grant_type=client_credentials"))
        .expect("a request")
}

async fn read(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// PL-19: the named pipeline's own ServiceAccount is asked for, by the runner, for the realm and
/// 600 s; Keycloak gets that token alone and its answer comes back.
#[tokio::test]
async fn a_stream_gets_the_token_of_its_own_pipelines_service_account() {
    let world = world().await;
    Mock::given(method("POST"))
        .and(path(
            "/api/v1/namespaces/pipeline-runner/serviceaccounts/pl-zilina-drepo/token",
        ))
        .and(has_header("authorization", "Bearer runner-own-token"))
        .and(body_json(json!({
            "apiVersion": "authentication.k8s.io/v1",
            "kind": "TokenRequest",
            "spec": { "audiences": [ISSUER], "expirationSeconds": TOKEN_SECONDS },
        })))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({ "status": { "token": "minted.for.drepo" } })),
        )
        .expect(1)
        .mount(&world.kubernetes)
        .await;
    Mock::given(method("POST"))
        .and(path("/realms/joinedcontext/protocol/openid-connect/token"))
        .and(body_string_contains("grant_type=client_credentials"))
        .and(body_string_contains("client_assertion=minted.for.drepo"))
        .and(body_string_contains("client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "access_token": "at", "expires_in": 300, "token_type": "Bearer" })))
        .expect(1)
        .mount(&world.keycloak)
        .await;

    let (status, body) = read(
        world
            .app
            .clone()
            .oneshot(bento("zilina/drepo"))
            .await
            .expect("an answer"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["access_token"], "at");
    let sent = &world.keycloak.received_requests().await.expect("recorded")[0];
    let form = String::from_utf8_lossy(&sent.body);
    assert!(
        !form.contains("client_id") && !form.contains("client_secret"),
        "no client id or secret: {form}"
    );
    assert!(
        sent.headers.get("authorization").is_none(),
        "no Basic to Keycloak"
    );
}

/// PL-19: Keycloak's refusal reaches the stream as Keycloak said it, so Bento retries and logs it.
#[tokio::test]
async fn keycloaks_refusal_comes_back_as_it_is() {
    let world = world().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({ "status": { "token": "t" } })),
        )
        .mount(&world.kubernetes)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({ "error": "invalid_client" })),
        )
        .mount(&world.keycloak)
        .await;
    let (status, body) = read(
        world
            .app
            .clone()
            .oneshot(bento("zilina/drepo"))
            .await
            .expect("an answer"),
    )
    .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("invalid_client"))
    );
}

/// A client id that names no pipeline mints nothing; a TokenRequest the API refuses is a 502 that
/// names the ServiceAccount and carries no token.
#[tokio::test]
async fn a_bad_client_id_mints_nothing_and_a_refused_token_request_says_whose() {
    let world = world().await;
    Mock::given(method("POST"))
        .and(path(
            "/api/v1/namespaces/pipeline-runner/serviceaccounts/pl-zilina-gone/token",
        ))
        .respond_with(ResponseTemplate::new(404))
        .mount(&world.kubernetes)
        .await;
    for bad in [
        "zilina",
        "zilina/",
        "Zilina/drepo",
        "zilina/drepo/x",
        "../x/y",
    ] {
        let (status, _) = read(
            world
                .app
                .clone()
                .oneshot(bento(bad))
                .await
                .expect("an answer"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let unnamed = Request::post("/token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("grant_type=client_credentials"))
        .expect("a request");
    assert_eq!(
        world
            .app
            .clone()
            .oneshot(unnamed)
            .await
            .expect("an answer")
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(
        world
            .kubernetes
            .received_requests()
            .await
            .expect("recorded")
            .is_empty(),
        "nothing was minted"
    );

    let (status, body) = read(
        world
            .app
            .clone()
            .oneshot(bento("zilina/gone"))
            .await
            .expect("an answer"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let said = body["error_description"].as_str().unwrap_or_default();
    assert!(
        said.contains("pl-zilina-gone") && said.contains("404"),
        "{said}"
    );
    assert!(world
        .keycloak
        .received_requests()
        .await
        .expect("recorded")
        .is_empty());
}

#[test]
fn the_client_id_is_read_as_bento_encodes_it() {
    assert_eq!(percent_decoded("zilina%2Fdrepo"), "zilina/drepo");
    assert_eq!(percent_decoded("a+b%zz%2"), "a b%zz%2");
}

#[tokio::test]
async fn the_probe_answers_without_minting() {
    let world = world().await;
    let probe = Request::get("/healthz")
        .body(Body::empty())
        .expect("a request");
    assert_eq!(
        world
            .app
            .clone()
            .oneshot(probe)
            .await
            .expect("an answer")
            .status(),
        StatusCode::OK
    );
    assert!(world
        .kubernetes
        .received_requests()
        .await
        .expect("recorded")
        .is_empty());
}

/// T-1508: the service runs in a pod of its own, so the address the deployment sets, every
/// interface on 4180, is the default and is taken; nonsense stops it at start-up.
#[test]
fn the_service_listens_where_the_deployment_says_across_the_pod_network() {
    assert_eq!(
        listen_address(None).expect("default").to_string(),
        "0.0.0.0:4180"
    );
    assert_eq!(
        listen_address(Some(" ")).expect("blank").to_string(),
        "0.0.0.0:4180"
    );
    assert_eq!(
        listen_address(Some("0.0.0.0:4180"))
            .expect("the deployment's")
            .port(),
        4180
    );
    assert!(listen_address(Some("everywhere")).is_err());
}

/// AP-159: beside a WASM host the service mints the App's job principal's token, in the Apps'
/// identities namespace, and never a pipeline's.
#[tokio::test]
async fn a_job_run_gets_the_token_of_its_apps_job_principal() {
    let world = world_of(Identity::AppJob, "app-identities").await;
    Mock::given(method("POST"))
        .and(path(
            "/api/v1/namespaces/app-identities/serviceaccounts/appjob-zilina-kpi-forecast/token",
        ))
        .and(has_header("authorization", "Bearer runner-own-token"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({ "status": { "token": "minted.for.kpi" } })),
        )
        .expect(1)
        .mount(&world.kubernetes)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("client_assertion=minted.for.kpi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({ "access_token": "job-at", "expires_in": 300, "token_type": "Bearer" }),
        ))
        .expect(1)
        .mount(&world.keycloak)
        .await;
    let request = Request::post("/token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            "grant_type=client_credentials&client_id=zilina%2Fkpi-forecast",
        ))
        .expect("a request");
    let (status, body) = read(world.app.clone().oneshot(request).await.expect("an answer")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["access_token"], "job-at");
}

#[test]
fn the_identity_is_pipeline_unless_appjob_is_named() {
    assert_eq!(Identity::parse(None), Ok(Identity::Pipeline));
    assert_eq!(Identity::parse(Some(" ")), Ok(Identity::Pipeline));
    assert_eq!(Identity::parse(Some("appjob")), Ok(Identity::AppJob));
    let err = Identity::parse(Some("app")).expect_err("refused");
    assert!(
        err.contains("JC_SIDECAR_IDENTITY") && err.contains("app"),
        "{err}"
    );
}
