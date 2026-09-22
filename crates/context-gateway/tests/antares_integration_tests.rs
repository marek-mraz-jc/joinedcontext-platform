//! The gateway in front of a real broker (T-0005).
//!
//! Everything else about the gateway is tested against its own types. This one starts
//! Antares in Docker and drives the whole path — resolve, decide, guard, forward, project
//! — because the interesting failures are the ones at the seam: a header the broker does
//! not get, a query parameter it does not understand, a status the gateway swallows.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::process::Command;
use tower::ServiceExt;

const IMAGE: &str = "ghcr.io/marek-mraz/antares-broker:dev";
const SPACE: &str = "ovzdusie";
const ORG: &str = "banskabystrica.sk";
const PUBLIC_SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const MEMBERS_SLUG: &str = "t9x2wqvn7mzc4hd6bkp3rjs5ga";
const WRITER_SLUG: &str = "p3mq8vzt5xkc2nhw7brj4gd6sy";
/// The dev seed's shape: a public grant that narrows nothing (T-0379).
const OPEN_SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
const STATION: &str =
    "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-integration-01";

/// A broker container that stops when the test ends, however the test ends.
struct Antares {
    id: String,
    port: u16,
}

impl Antares {
    /// Starts the broker on a free port and waits until it answers its own health probe.
    ///
    /// `None` means Docker is not available here, which is a skipped test rather than a
    /// failed one: the assertion is about the gateway, not about the sandbox.
    fn start() -> Option<Self> {
        if !Command::new("docker")
            .arg("version")
            .output()
            .is_ok_and(|out| out.status.success())
        {
            eprintln!("skipped: no Docker in this environment");
            return None;
        }

        let run = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "-p",
                "127.0.0.1:0:9090",
                "-e",
                "ANTARES_STORE=memory",
                IMAGE,
            ])
            .output()
            .expect("docker run is callable");
        assert!(
            run.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let id = String::from_utf8_lossy(&run.stdout).trim().to_owned();

        let published = Command::new("docker")
            .args(["port", &id, "9090/tcp"])
            .output()
            .expect("docker port is callable");
        let port = String::from_utf8_lossy(&published.stdout)
            .lines()
            .next()
            .and_then(|line| line.rsplit(':').next().map(str::to_owned))
            .and_then(|port| port.trim().parse().ok())
            .unwrap_or_else(|| {
                panic!(
                    "no published port: {}",
                    String::from_utf8_lossy(&published.stdout)
                )
            });

        let broker = Self { id, port };
        broker.await_health();
        Some(broker)
    }

    fn await_health(&self) {
        let url = format!("http://127.0.0.1:{}/q/health", self.port);
        for _ in 0..120 {
            let probe = Command::new("docker")
                .args(["exec", &self.id, "/antares", "--health"])
                .output();
            if probe.is_ok_and(|out| out.status.success()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        panic!("{url} never became healthy");
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Antares {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.id]).output();
    }
}

/// The grant DEMO step 4 describes: an anonymous caller may read and write three
/// attributes of one type in this space, and nothing else.
fn public_grant() -> PolicySpec {
    serde_norway::from_str(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity, createEntity, deleteEntity]
information:
  - entities:
      - type: AirQualityObserved
        idPattern: "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:.*$"
    propertyNames: [pm10, pm25, location]
"#,
    )
    .expect("the grant parses")
}

fn endpoint(slug: &str, audience: Audience) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: slug.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        base_path: format!("/api/endpoint/{slug}"),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::GeoJson],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: Vec::new(),
        view_mapping: None,
        policies: vec![public_grant()],
    }
}

/// The grant the dev cluster actually seeds: anonymous read, and nothing narrowed. It is the
/// case `public_grant` never covered, because that one names a type and so always selected.
fn unrestricted_grant() -> PolicySpec {
    serde_norway::from_str(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#,
    )
    .expect("the grant parses")
}

fn gateway(broker_url: String) -> axum::Router {
    let mut open = endpoint(OPEN_SLUG, Audience::Public);
    open.policies = vec![unrestricted_grant()];
    let gateway = Gateway::new(Broker::new(broker_url), Box::new(PolicyPdp), ORG).serve([
        endpoint(PUBLIC_SLUG, Audience::Public),
        endpoint(MEMBERS_SLUG, Audience::Organization),
        open,
    ]);
    router(std::sync::Arc::new(gateway))
}

fn station(extra: Option<(&str, Value)>) -> Value {
    let mut entity = json!({
        "id": STATION,
        "type": "AirQualityObserved",
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld",
        "pm10": { "type": "Property", "value": 34.2 },
        "pm25": { "type": "Property", "value": 12.0 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.15, 48.73] }
        }
    });
    if let Some((name, value)) = extra {
        entity[name] = value;
    }
    entity
}

async fn call(app: &axum::Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, body.to_vec())
}

fn post(slug: &str, path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{slug}/ngsi-ld/v1{path}"))
        .header("content-type", "application/ld+json")
        .body(Body::from(body.to_string()))
        .expect("a request")
}

fn get(slug: &str, path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/api/endpoint/{slug}/ngsi-ld/v1{path}"))
        .header("accept", "application/ld+json")
        .body(Body::empty())
        .expect("a request")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_whole_path_through_the_gateway_to_a_real_broker() {
    let Some(antares) = Antares::start() else {
        return;
    };
    let app = gateway(antares.url());

    // Create, through the endpoint and its grant.
    let (status, body) = call(&app, post(PUBLIC_SLUG, "/entities", &station(None))).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create: {}",
        String::from_utf8_lossy(&body)
    );

    // Read it back. The broker holds it under the tenant the gateway pinned, which the
    // client never named.
    let (status, body) = call(&app, get(PUBLIC_SLUG, &format!("/entities/{STATION}"))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "read: {}",
        String::from_utf8_lossy(&body)
    );
    let entity: Value = serde_json::from_slice(&body).expect("an entity");
    assert_eq!(entity["id"], json!(STATION));
    assert_eq!(entity["pm10"]["value"], json!(34.2));

    // GW33, CIM 009 5.7.2.4: a query names a selector, and the grants do not supply one the
    // caller left out. An `id` list or an `idPattern` alone is the case the specification
    // calls `BadRequestData`, however narrow the grant behind it is (T-0780).
    for raw in [
        "/entities",
        "/entities?limit=20",
        &format!("/entities?id={STATION}"),
        "/entities?idPattern=.%2A&limit=1",
    ] {
        let (status, body) = call(&app, get(PUBLIC_SLUG, raw)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{raw}: {}",
            String::from_utf8_lossy(&body)
        );
        let problem: Value = serde_json::from_slice(&body).expect("problem details");
        assert!(
            problem["detail"]
                .as_str()
                .is_some_and(|d| d.contains("selector")),
            "{raw}: the refusal says what a query needs: {problem}"
        );
    }

    // The same entity by id is a retrieve, not a query, and keeps answering.
    let (status, _) = call(&app, get(PUBLIC_SLUG, &format!("/entities/{STATION}"))).await;
    assert_eq!(status, StatusCode::OK, "a retrieve names no selector");

    // Query it back the way DEMO step 4 does.
    let (status, body) = call(&app, get(PUBLIC_SLUG, "/entities?type=AirQualityObserved")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "query: {}",
        String::from_utf8_lossy(&body)
    );
    let entities: Value = serde_json::from_slice(&body).expect("a list");
    assert_eq!(entities.as_array().map(Vec::len), Some(1));

    // An attribute outside the grant never reaches the wire, whichever way it is asked
    // for (R9): an addressed read that names only ungranted attributes is a miss, never
    // the whole entity (T-0805, GW1, R20).
    let (status, body) = call(
        &app,
        get(
            PUBLIC_SLUG,
            &format!("/entities/{STATION}?attrs=operatorPhone"),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "ungranted attrs: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(!String::from_utf8_lossy(&body).contains("pm10"));

    // DEMO step 4: the same data, several ways. The map reads the FeatureCollection and
    // sees exactly the attributes the grant allows (EP-09, EP-07).
    let (status, body) = call(
        &app,
        Request::builder()
            .uri(format!("/api/endpoint/{PUBLIC_SLUG}/file.geojson"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "geojson: {}",
        String::from_utf8_lossy(&body)
    );
    let collection: Value = serde_json::from_slice(&body).expect("a FeatureCollection");
    assert_eq!(collection["type"], json!("FeatureCollection"));
    let features = collection["features"].as_array().expect("features");
    assert_eq!(features.len(), 1);
    assert_eq!(features[0]["id"], json!(STATION));
    assert_eq!(features[0]["geometry"]["type"], json!("Point"));
    assert_eq!(features[0]["properties"]["pm10"], json!(34.2));
    assert!(features[0]["properties"].get("operatorPhone").is_none());

    // T-0379: the same bare download on the endpoint whose grant narrows nothing, which is
    // the shape the dev cluster seeds. Against a real broker this was a 400 under CIM 009
    // 5.7.2, because the gateway forwarded a query that selected nothing at all.
    let (status, body) = call(
        &app,
        Request::builder()
            .uri(format!("/api/endpoint/{OPEN_SLUG}/file.geojson"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bare geojson on an unrestricted grant: {}",
        String::from_utf8_lossy(&body)
    );
    let collection: Value = serde_json::from_slice(&body).expect("a FeatureCollection");
    assert_eq!(
        collection["features"].as_array().map(Vec::len),
        Some(1),
        "the dataset is the selection: {collection}"
    );

    // The NGSI-LD surface of the very same endpoint keeps the broker's refusal, so the
    // download's convenience is not a hole in the API's conformance.
    let (status, _) = call(&app, get(OPEN_SLUG, "/entities")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // DEMO step 4: what the anonymous caller may do, from the same PDP (EP-55).
    let (status, body) = call(
        &app,
        Request::builder()
            .uri(format!("/api/endpoint/{PUBLIC_SLUG}/access"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let access: Value = serde_json::from_slice(&body).expect("an access document");
    assert_eq!(access["subject"], json!({ "type": "role", "id": "public" }));
    assert_eq!(
        access["permissions"][0]["resource"]["type"],
        json!("AirQualityObserved")
    );
    assert_eq!(
        access["permissions"][0]["attributes"],
        json!(["location", "pm10", "pm25"])
    );

    // A write that touches an attribute outside the grant is refused whole (GW17).
    let refused = station(Some((
        "operatorPhone",
        json!({ "type": "Property", "value": "+421 900 000 000" }),
    )));
    let (status, _) = call(&app, post(PUBLIC_SLUG, "/entities", &refused)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An operation outside the grant is refused, and the body says nothing about the rule
    // (GW6).
    let (status, body) = call(
        &app,
        Request::builder()
            .method("PATCH")
            .uri(format!(
                "/api/endpoint/{PUBLIC_SLUG}/ngsi-ld/v1/entities/{STATION}"
            ))
            .header("content-type", "application/json")
            .body(Body::from(json!({ "pm10": 1.0 }).to_string()))
            .expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let problem: Value = serde_json::from_slice(&body).expect("problem+json");
    assert_eq!(problem["status"], json!(403));
    assert!(problem["type"]
        .as_str()
        .is_some_and(|t| t.ends_with("forbidden")));
    // CIM 009 clause 6.3.3 carries `detail`; GW1 and R20 say it names no rule: not the
    // policy, not its assignee, not the attribute that was outside the grant.
    assert!(
        problem["detail"].is_string(),
        "clause 6.3.3: detail is carried"
    );
    let rendered = body_text(&body);
    for named in ["public-air", "did:web", "pm10", "assigner", "assignee"] {
        assert!(
            !rendered.contains(named),
            "the body names the rule: {rendered}"
        );
    }

    // A write into another organization's URN space is a bad request (PF-10, PF-42).
    let mut foreign = station(None);
    foreign["id"] = json!("urn:ngsi-ld:AirQualityObserved:zilina.sk:ovzdusie:station-01");
    let (status, body) = call(&app, post(PUBLIC_SLUG, "/entities", &foreign)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let problem: Value = serde_json::from_slice(&body).expect("problem+json");
    // CIM 009 clause 5.5.3: the type is an ETSI one, so a client that tells BadRequestData
    // from InvalidRequest learns the same thing whether the gateway or the broker refused.
    assert_eq!(
        problem["type"],
        json!("https://uri.etsi.org/ngsi-ld/errors/BadRequestData")
    );

    // An endpoint the anonymous caller is not the audience of answers 401, not 403: it is
    // a missing identity, not a refused one (EP-14).
    let (status, _) = call(&app, get(MEMBERS_SLUG, "/entities?type=AirQualityObserved")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A slug nobody issued is a miss, and so is a path off the NGSI-LD resource tree
    // (EP-03).
    let (status, _) = call(&app, get("nosuchslugnosuchslugnosuchsl", "/entities")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, get(PUBLIC_SLUG, "/admin/shutdown")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Delete, and it is gone.
    let (status, body) = call(
        &app,
        Request::builder()
            .method("DELETE")
            .uri(format!(
                "/api/endpoint/{PUBLIC_SLUG}/ngsi-ld/v1/entities/{STATION}"
            ))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "delete: {}",
        String::from_utf8_lossy(&body)
    );

    let (status, _) = call(&app, get(PUBLIC_SLUG, &format!("/entities/{STATION}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The probes the deployment uses answer before any endpoint exists (OPS-12).
#[tokio::test]
async fn the_health_probes_need_no_endpoints_and_no_broker() {
    let app = router(std::sync::Arc::new(Gateway::new(
        Broker::new("http://127.0.0.1:1"),
        Box::new(PolicyPdp),
        ORG,
    )));
    for probe in ["/healthz", "/livez"] {
        let (status, body) = call(&app, get_raw(probe)).await;
        assert_eq!(status, StatusCode::OK, "{probe}");
        assert_eq!(body, b"ok");
    }
}

fn get_raw(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a request")
}

/// PF-45, PF-46: a workload calls with an audience-bound `client_credentials` token, the
/// gateway maps `azp` to the `ServiceAccount` the repository declares, and the grants of
/// that account — not of the token — decide.
#[tokio::test(flavor = "multi_thread")]
async fn a_service_account_writes_only_what_its_own_manifest_grants() {
    let Some(antares) = Antares::start() else {
        return;
    };
    let realm = common::Realm::new();
    let accounts = service_accounts();

    let writer = Endpoint {
        roles: Default::default(),
        slug: WRITER_SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        base_path: format!("/api/endpoint/{WRITER_SLUG}"),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: Vec::new(),
        view_mapping: None,
        policies: vec![serde_norway::from_str(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: serviceAccount, id: writer }
operations: [createEntity, retrieveEntity, queryEntity, deleteEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, pm25, location]
"#,
        )
        .expect("the grant parses")],
    };
    let gateway = Gateway::new(Broker::new(antares.url()), Box::new(PolicyPdp), ORG)
        .serve([writer, endpoint(PUBLIC_SLUG, Audience::Public)])
        .authenticate(
            std::sync::Arc::new(realm.verifier()),
            accounts,
            Some("https://2.28.67.127.sslip.io".to_owned()),
        );
    let app = router(std::sync::Arc::new(gateway));

    let granted = realm.workload_token("ovzdusie-writer", json!(WRITER_SLUG));
    let (status, body) = call(
        &app,
        authorized(post(WRITER_SLUG, "/entities", &station(None)), &granted),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the granted account writes: {}",
        String::from_utf8_lossy(&body)
    );

    // The same token, the same endpoint, a different account: no policy names it, so it
    // has no grants at all.
    let outsider = realm.workload_token("ovzdusie-outsider", json!(WRITER_SLUG));
    let (status, _) = call(
        &app,
        authorized(post(WRITER_SLUG, "/entities", &station(None)), &outsider),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an account with no grant writes nothing"
    );

    // A token from a Keycloak client no manifest names is valid and still worthless.
    let stranger = realm.workload_token("ovzdusie-never-declared", json!(WRITER_SLUG));
    let (status, _) = call(
        &app,
        authorized(
            get(WRITER_SLUG, "/entities?type=AirQualityObserved"),
            &stranger,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The right realm, the right account, the wrong resource: 401, because the token was
    // never issued for this endpoint (RFC 8707).
    let elsewhere = realm.workload_token("ovzdusie-writer", json!(PUBLIC_SLUG));
    let (status, body) = call(
        &app,
        authorized(
            get(WRITER_SLUG, "/entities?type=AirQualityObserved"),
            &elsewhere,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let problem: Value = serde_json::from_slice(&body).expect("problem+json");
    assert!(problem["type"]
        .as_str()
        .is_some_and(|t| t.ends_with("unauthorized")));

    // The full RFC 8707 resource URI names the same endpoint and is accepted too.
    let by_uri = realm.workload_token(
        "ovzdusie-writer",
        json!(format!(
            "https://2.28.67.127.sslip.io/api/endpoint/{WRITER_SLUG}"
        )),
    );
    let (status, _) = call(
        &app,
        authorized(
            get(WRITER_SLUG, "/entities?type=AirQualityObserved"),
            &by_uri,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // No token at all on an endpoint that is not public.
    let (status, _) = call(&app, get(WRITER_SLUG, "/entities?type=AirQualityObserved")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // An expired token is refused before any of it is believed.
    let stale = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "service-account-writer",
        "aud": WRITER_SLUG,
        "azp": "ovzdusie-writer",
        "exp": common::in_seconds(-3600),
    }));
    let (status, _) = call(
        &app,
        authorized(
            get(WRITER_SLUG, "/entities?type=AirQualityObserved"),
            &stale,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Clean up, so a rerun starts from the same place.
    let _ = call(
        &app,
        authorized(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/api/endpoint/{WRITER_SLUG}/ngsi-ld/v1/entities/{STATION}"
                ))
                .body(Body::empty())
                .expect("a request"),
            &granted,
        ),
    )
    .await;
}

/// Two accounts in one project: the client id is `{project}-{name}`, derived from the
/// manifests rather than written down twice (Architecture/12 section 3).
fn service_accounts() -> context_gateway::auth::accounts::ServiceAccounts {
    let dir = std::env::temp_dir().join("gateway-antares-accounts");
    let _ = std::fs::remove_dir_all(&dir);
    let accounts_dir = dir.join("projects/ovzdusie/access/serviceaccounts");
    std::fs::create_dir_all(&accounts_dir).expect("a repository");
    for name in ["writer", "outsider"] {
        std::fs::write(
            accounts_dir.join(format!("{name}.yaml")),
            format!(
                r#"apiVersion: joinedcontext.com/v1alpha1
kind: ServiceAccount
metadata:
  name: {name}
  namespace: ovzdusie
spec:
  owner:
    user: demo.steward
  purpose: "integration test account"
  roles:
    - role: space-writer
      scope:
        contextSpace: ovzdusie
  credentials:
    - kind: oauth-client
      name: default
"#
            ),
        )
        .expect("the manifest is written");
    }
    let repo = jcctl::loader::Repository::load(&dir).expect("the repository loads");
    let accounts = context_gateway::auth::accounts::accounts_of(&repo);
    assert_eq!(accounts.len(), 2);
    std::fs::remove_dir_all(&dir).expect("clean up");
    accounts
}

/// The same request, carrying a bearer token.
fn authorized(request: Request<Body>, token: &str) -> Request<Body> {
    let (mut parts, body) = request.into_parts();
    parts.headers.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}")).expect("a header value"),
    );
    Request::from_parts(parts, body)
}

fn body_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}
