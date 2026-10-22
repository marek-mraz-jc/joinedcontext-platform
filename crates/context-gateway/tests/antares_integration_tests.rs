//! The gateway in front of a real broker (T-0005).
//!
//! Everything else about the gateway is tested against its own types. This one starts
//! Antares in Docker and drives the whole path — resolve, decide, guard, forward, project
//! — because the interesting failures are the ones at the seam: a header the broker does
//! not get, a query parameter it does not understand, a status the gateway swallows.
//!
//! Since T-3643 the NGSI-LD surface of an Endpoint is the broker's view of the same slug, so
//! the broker is the platform's build (`-jc`: views and the `joinedcontext` engine) and each
//! test loads what the Portal's view writer would: one view per Endpoint and the space's rule
//! document (portal `reconciler/views.rs`).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::view_mapping::ViewMapping;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::process::Command;
use tower::ServiceExt;

/// Antares dev dfe4502 by image-jc.yml run 38152236779: the first -jc build whose engine reads a
/// rule document's per-view bindings (T-3702).
const IMAGE: &str = "ghcr.io/marek-mraz-jc/antares-broker-jc:dfe4502@sha256:54e81850572cd4200a79c94f7a3f9105f06255afa8495f65c4cfa458a0afbfd1";
const SPACE: &str = "ovzdusie";
const ORG: &str = "banskabystrica.sk";
const PUBLIC_SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const MEMBERS_SLUG: &str = "t9x2wqvn7mzc4hd6bkp3rjs5ga";
const WRITER_SLUG: &str = "p3mq8vzt5xkc2nhw7brj4gd6sy";
/// The dev seed's shape: a public grant that narrows nothing (T-0379).
const OPEN_SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
/// An Endpoint serving the space in another model through a view mapping (G18, T-3799).
const MAPPED_SLUG: &str = "w8ck3nzq6vtm2xh5brj7pgd4sf";
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
                "-e",
                // `access` mounts /x/views/{view}/access, where the gateway's access documents
                // come from since T-3649 (T-4112); dev serves it as the joinedcontext image does.
                "ANTARES_API_SURFACES=admin,views,mcp,access",
                "-e",
                "ANTARES_POLICY=joinedcontext",
                "-e",
                "ANTARES_POLICY_SUBJECT_HEADERS=x-jc-subject,x-jc-groups,x-jc-roles,x-jc-client,x-jc-restricted",
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

    /// Loads the views and the space's rule document, as the Portal's view writer does
    /// (`PUT /q/views/{id}`, `PUT /q/policy/{tenant}`). The container publishes on loopback
    /// only, so `/q` runs without the admin secret here.
    async fn load(&self, views: &[Value], rules: Value) {
        let client = reqwest::Client::new();
        let mut puts: Vec<(String, Value)> = views
            .iter()
            .map(|view| {
                (
                    format!("/q/views/{}", view["id"].as_str().expect("an id")),
                    view.clone(),
                )
            })
            .collect();
        puts.push((format!("/q/policy/{SPACE}"), with_gateway(rules)));
        let client = &client;
        for (path, body) in puts {
            let response = client
                .put(format!("{}{path}", self.url()))
                .json(&body)
                .send()
                .await
                .expect("the broker answers");
            let status = response.status();
            assert!(
                status.is_success(),
                "{path}: {status} {}",
                response.text().await.unwrap_or_default()
            );
        }
    }

    /// Writes an entity at the broker's root on the `seeder` account, past every view: what a
    /// pipeline stored that a caller of the view may not see all of.
    async fn seed(&self, entity: &Value) {
        let response = reqwest::Client::new()
            .post(format!("{}/ngsi-ld/v1/entities", self.url()))
            .header("ngsild-tenant", SPACE)
            .header("x-jc-client", "seeder")
            .json(entity)
            .send()
            .await
            .expect("the broker answers");
        assert_eq!(response.status(), reqwest::StatusCode::CREATED, "seed");
    }
}

/// A view of the space's one type showing `attributes`, serving `operations` (matrix row ids).
fn view(slug: &str, attributes: &[&str], operations: &[&str]) -> Value {
    json!({
        "id": slug,
        "tenant": SPACE,
        "selector": ["AirQualityObserved"],
        "rules": [{"types": ["AirQualityObserved"], "attributes": attributes}],
        "operations": operations,
    })
}

/// The rule document's policy for a grant: its id, assignee, clauses and information.
fn rule(id: &str, assignee: Value, clauses: &[&str], information: Option<Value>) -> Value {
    let mut rule = json!({
        "id": format!("{SPACE}/{id}"),
        "effect": "permit",
        "assignee": assignee,
        "operations": clauses,
    });
    if let Some(information) = information {
        rule["information"] = information;
    }
    rule
}

/// The gateway's own hops (T-3767, and the download's type list: T-3790, T-3791), which the Portal
/// adds to every rule document.
fn with_gateway(mut rules: Value) -> Value {
    if let Some(policies) = rules["policies"].as_array_mut() {
        policies.push(json!({
            "id": "@context-gateway",
            "effect": "permit",
            "assignee": {"kind": "serviceAccount", "id": "context-gateway"},
            "operations": ["5.6.4", "5.6.5", "5.6.6", "5.7.1", "5.7.2", "5.7.5", "5.8.3"],
        }));
    }
    rules
}

/// What `public_grant` grants, as the writer puts it into the broker.
fn public_rules() -> (Vec<Value>, Value) {
    let information = json!([{
        "entities": [{
            "type": "AirQualityObserved",
            "idPattern": "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:.*$"
        }],
        "propertyNames": ["pm10", "pm25", "location"]
    }]);
    let views = vec![
        view(
            PUBLIC_SLUG,
            &["location", "pm10", "pm25"],
            &[
                "batch_upsert",
                "create_entity",
                "delete_entity",
                "query_entities",
                "retrieve_entity",
                "temporal_retrieve",
            ],
        ),
        // The space surface `/cs/{space}` (plan 5.1): every class whole, the engine narrows.
        json!({
            "id": format!("cs-{SPACE}"),
            "tenant": SPACE,
            "selector": ["*"],
            "rules": [{"types": ["*"], "attributes": ["*"]}],
            "operations": ["query_entities", "retrieve_entity"],
        }),
        // A grant naming no type shows every type whole: the `*` form (ADR-0023 rule 12).
        json!({
            "id": OPEN_SLUG,
            "tenant": SPACE,
            "selector": ["*"],
            "rules": [{"types": ["*"], "attributes": ["*"]}],
            "operations": ["query_entities", "retrieve_entity"],
        }),
    ];
    let public = json!({"kind": "role", "id": "public"});
    let rules = json!({
        "policies": [
            rule(
                "public-air",
                public.clone(),
                &["5.7.2", "5.7.1", "5.6.1", "5.6.6", "5.7.3", "5.6.8"],
                Some(information),
            ),
            rule("open", public, &["5.7.1", "5.7.2"], None),
            // What the test writes past every view, as a pipeline's account would.
            rule("seeder", json!({"kind": "serviceAccount", "id": "seeder"}), &["5.6.1"], None),
        ],
        "views": {
            PUBLIC_SLUG: [format!("{SPACE}/public-air")],
            OPEN_SLUG: [format!("{SPACE}/open")],
        },
    });
    (views, rules)
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
operations: [queryEntity, retrieveEntity, createEntity, deleteEntity, retrieveTemporal, upsertBatch]
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
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        base_path: format!("/api/endpoint/{slug}"),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::Mcp,
            Representation::Csv,
        ],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
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
    let space = context_gateway::resolver::Space {
        endpoint: std::sync::Arc::new(Endpoint {
            slug: SPACE.to_owned(),
            base_path: format!("/cs/{SPACE}"),
            policies: vec![public_grant(), unrestricted_grant()],
            ..endpoint(PUBLIC_SLUG, Audience::Public)
        }),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    };
    let gateway = Gateway::new(Broker::new(broker_url), Box::new(PolicyPdp), ORG)
        .serve([
            endpoint(PUBLIC_SLUG, Audience::Public),
            endpoint(MEMBERS_SLUG, Audience::Organization),
            open,
        ])
        .serve_spaces([space]);
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

/// One JSON-RPC request to an Endpoint's MCP through the gateway, as its answer.
async fn mcp(app: &axum::Router, slug: &str, method: &str, params: Value) -> Value {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{slug}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string(),
        ))
        .expect("a request");
    let (status, body) = call(app, request).await;
    assert_eq!(status, StatusCode::OK, "{method}: {}", body_text(&body));
    serde_json::from_slice(&body).expect("a JSON-RPC answer")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_whole_path_through_the_gateway_to_a_real_broker() {
    let Some(antares) = Antares::start() else {
        return;
    };
    let (views, rules) = public_rules();
    antares.load(&views, rules).await;
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

    // DEMO step 4: the same data as a file, with exactly the attributes the grant allows
    // (EP-09, EP-07).
    let (status, body) = call(
        &app,
        Request::builder()
            .uri(format!("/api/endpoint/{PUBLIC_SLUG}/file.csv"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "csv: {}", body_text(&body));
    assert_eq!(ids_of("file.csv", &body), [STATION]);
    assert!(body_text(&body).contains("34.2"), "{}", body_text(&body));
    assert!(!body_text(&body).contains("operatorPhone"));

    // T-0379: the same bare download on the endpoint whose grant narrows nothing, which is
    // the shape the dev cluster seeds. Against a real broker this was a 400 under CIM 009
    // 5.7.2, because the gateway forwarded a query that selected nothing at all.
    let (status, body) = call(
        &app,
        Request::builder()
            .uri(format!("/api/endpoint/{OPEN_SLUG}/file.csv"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bare csv on an unrestricted grant: {}",
        body_text(&body)
    );
    assert_eq!(
        ids_of("file.csv", &body).len(),
        1,
        "the dataset is the selection: {}",
        body_text(&body)
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
    // The view serves no merge here, so the broker answers 405 with `Allow` (views.md 4).
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let problem: Value = serde_json::from_slice(&body).expect("problem+json");
    assert_eq!(problem["status"], json!(405));
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

    // An id that is no NGSI-LD URN is a bad request (PF-43). A URN naming another organization
    // is not: it is an id like any other, and this caller's grant decides it (ADR-N-041).
    let mut named_elsewhere = station(None);
    named_elsewhere["id"] = json!("urn:ngsi-ld:AirQualityObserved:zilina.sk:ovzdusie:station-01");
    let (status, _) = call(&app, post(PUBLIC_SLUG, "/entities", &named_elsewhere)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the public grant writes nothing, whatever the URN says"
    );
    let mut malformed = station(None);
    malformed["id"] = json!("urn:ngsi-ld:AirQualityObserved");
    let (status, body) = call(&app, post(PUBLIC_SLUG, "/entities", &malformed)).await;
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

    // Plan 5.2, M3 and M9: `pick` and `join` through the slug are the view's.
    let (status, body) = call(
        &app,
        get(
            PUBLIC_SLUG,
            "/entities?type=AirQualityObserved&pick=id,pm10",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pick: {}", body_text(&body));
    let picked: Value = serde_json::from_slice(&body).expect("a list");
    assert_eq!(picked[0]["pm10"]["value"], json!(34.2));
    assert!(picked[0].get("pm25").is_none(), "{picked}");
    let (status, _) = call(
        &app,
        get(
            PUBLIC_SLUG,
            "/entities?type=AirQualityObserved&join=inline&joinLevel=2",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // M8: the temporal read of the same entity, through the view.
    let (status, body) = call(
        &app,
        get(PUBLIC_SLUG, &format!("/temporal/entities/{STATION}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "temporal: {}", body_text(&body));

    // M4, M6: the space surface is the `cs-{space}` view, decided by every Policy of the space.
    let (status, body) = call(
        &app,
        Request::builder()
            .uri(format!("/cs/{SPACE}/ngsi-ld/v1/entities/{STATION}"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "space: {}", body_text(&body));

    // M1: what a pipeline stored beyond the grant is cut, and a caller who asks is told so under
    // the specification's header; one who does not ask is not (R22, O-3 (b)).
    let seeded = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-seeded";
    let mut stored = station(Some((
        "operatorPhone",
        json!({ "type": "Property", "value": "+421 900 000 000" }),
    )));
    stored["id"] = json!(seeded);
    if let Some(members) = stored.as_object_mut() {
        members.remove("@context");
    }
    antares.seed(&stored).await;
    let query = || {
        get(
            PUBLIC_SLUG,
            &format!("/entities?type=AirQualityObserved&id={seeded}"),
        )
    };
    let mut asked = query();
    asked.headers_mut().insert(
        "ngsild-results-restricted",
        "true".parse().expect("a value"),
    );
    let response = app
        .clone()
        .oneshot(asked)
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("ngsild-results-restricted")
            .map(|v| v.as_bytes()),
        Some(&b"true"[..])
    );
    assert!(response
        .headers()
        .get("antares-results-restricted")
        .is_none());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    assert!(!body_text(&bytes).contains("+421"), "{}", body_text(&bytes));
    let response = app
        .clone()
        .oneshot(query())
        .await
        .expect("the gateway answers");
    assert!(response
        .headers()
        .get("ngsild-results-restricted")
        .is_none());

    // CIM 009 6.3.15: GeoJSON on the NGSI-LD tree is the broker's rendering of the same narrowed
    // answer, so what the view cuts is cut there too.
    let mut geo = query();
    geo.headers_mut()
        .insert("accept", "application/geo+json".parse().expect("a value"));
    let response = app.clone().oneshot(geo).await.expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").map(|v| v.as_bytes()),
        Some(&b"application/geo+json"[..])
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let collection: Value = serde_json::from_slice(&bytes).expect("a FeatureCollection");
    assert_eq!(collection["type"], json!("FeatureCollection"));
    assert_eq!(collection["features"][0]["id"], json!(seeded));
    assert!(!body_text(&bytes).contains("+421"), "{collection}");

    // M17: a batch upsert is decided member by member, and the refused one comes back in the
    // 207 under the broker's own type (O-8).
    let fine = station(None);
    let mut beyond = station(Some((
        "operatorPhone",
        json!({ "type": "Property", "value": "x" }),
    )));
    beyond["id"] = json!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-02");
    let (status, body) = call(
        &app,
        post(
            PUBLIC_SLUG,
            "/entityOperations/upsert",
            &json!([fine, beyond]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS, "{}", body_text(&body));
    let result: Value = serde_json::from_slice(&body).expect("a BatchOperationResult");
    assert_eq!(result["success"], json!([STATION]));
    assert_eq!(
        result["errors"][0]["error"]["type"],
        json!("urn:antares:error:AccessDenied")
    );

    // M7: a write the engine refuses is 403 under the broker's type, whatever the id names.
    let (status, body) = call(&app, post(PUBLIC_SLUG, "/entities", &named_elsewhere)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let problem: Value = serde_json::from_slice(&body).expect("problem+json");
    assert_eq!(problem["type"], json!("urn:antares:error:AccessDenied"));
    assert_eq!(
        problem["code"],
        json!("forbidden"),
        "the Portal's key for its hint"
    );

    // T-3647: the Endpoint's MCP is the broker's view under the verified caller. The tool
    // list is the view's plus the two the gateway answers; a result is narrowed like the
    // NGSI-LD read (the seeded phone stays out); a type no grant names and a type nobody has
    // answer alike; an argument naming a tenant is refused (carried over from the façade's
    // `mcp_read_security_tests`).
    let listed = mcp(&app, PUBLIC_SLUG, "tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for wanted in [
        "query_entities",
        "get_entity",
        "describe_access",
        "describe_schema",
    ] {
        assert!(names.contains(&wanted), "{wanted}: {listed}");
    }
    let queried = mcp(
        &app,
        PUBLIC_SLUG,
        "tools/call",
        json!({ "name": "query_entities", "arguments": { "type": ["AirQualityObserved"], "count": true } }),
    )
    .await;
    let found = &queried["result"]["structuredContent"]["entities"];
    assert!(
        found
            .as_array()
            .is_some_and(|entities| entities.iter().any(|entity| entity["id"] == json!(seeded))),
        "{queried}"
    );
    assert!(!queried.to_string().contains("+421"), "{queried}");
    let fetched = mcp(
        &app,
        PUBLIC_SLUG,
        "tools/call",
        json!({ "name": "get_entity", "arguments": { "id": seeded } }),
    )
    .await;
    assert_eq!(fetched["result"]["isError"], json!(false), "{fetched}");
    assert!(!fetched.to_string().contains("operatorPhone"), "{fetched}");
    let hidden = |name: &str| json!({ "name": "query_entities", "arguments": { "type": [name] } });
    let denied = mcp(&app, PUBLIC_SLUG, "tools/call", hidden("Depot")).await;
    let absent = mcp(&app, PUBLIC_SLUG, "tools/call", hidden("Ghost")).await;
    assert_eq!(denied["result"]["isError"], json!(true), "{denied}");
    assert_eq!(
        denied["result"], absent["result"],
        "a hidden and an absent type answer alike"
    );
    let aimed = mcp(
        &app,
        PUBLIC_SLUG,
        "tools/call",
        json!({ "name": "query_entities", "arguments": { "type": ["AirQualityObserved"], "tenant": "doprava" } }),
    )
    .await;
    assert_eq!(aimed["result"]["isError"], json!(true), "{aimed}");

    // A delete that names no reason is refused before the broker, and the entity stays
    // (ADR-N-054 2.3, DM-113).
    let delete = |reason: Option<&str>| {
        let mut request = Request::builder().method("DELETE").uri(format!(
            "/api/endpoint/{PUBLIC_SLUG}/ngsi-ld/v1/entities/{STATION}"
        ));
        if let Some(reason) = reason {
            request = request.header("jc-reason", reason);
        }
        request.body(Body::empty()).expect("a request")
    };
    let (status, _) = call(&app, delete(None)).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a delete without JC-Reason"
    );
    let (status, _) = call(&app, get(PUBLIC_SLUG, &format!("/entities/{STATION}"))).await;
    assert_eq!(status, StatusCode::OK, "the refused delete left the entity");

    // Delete with a reason, and it is gone.
    let (status, body) = call(&app, delete(Some("station%20retired"))).await;
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
/// T-3799: an Endpoint with a view mapping is its broker view. The Portal writes the Mapping IR
/// into the view of the slug (T-3671); the broker inverts the target-model query into the source
/// model and rebuilds the answer (T-3669), and a write is its 405, never the source model's.
#[tokio::test(flavor = "multi_thread")]
async fn a_mapped_endpoint_reads_in_the_target_model_through_its_view() {
    let Some(antares) = Antares::start() else {
        return;
    };
    let ir = json!({
        "version": 2,
        "sourceClass": "AirQualityObserved",
        "targetClass": "AirQuality",
        "slots": [{ "target": "particulates", "source": "pm10", "kind": "rename", "filterable": true }]
    });
    let (mut views, mut rules) = public_rules();
    let mut mapped = view(
        MAPPED_SLUG,
        &["pm10"],
        &["query_entities", "retrieve_entity"],
    );
    mapped["mapping"] = ir.clone();
    views.push(mapped);
    rules["views"][MAPPED_SLUG] = json!([format!("{SPACE}/public-air")]);
    antares.load(&views, rules).await;
    let mut stored = station(None);
    if let Some(members) = stored.as_object_mut() {
        members.remove("@context");
    }
    antares.seed(&stored).await;
    let endpoint = Endpoint {
        view_mapping: Some(std::sync::Arc::new(
            ViewMapping::parse(&ir).expect("the IR parses"),
        )),
        ..endpoint(MAPPED_SLUG, Audience::Public)
    };
    let app = router(std::sync::Arc::new(
        Gateway::new(Broker::new(antares.url()), Box::new(PolicyPdp), ORG).serve([endpoint]),
    ));

    let (status, body) = call(&app, get(MAPPED_SLUG, "/entities?type=AirQuality")).await;
    let text = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, StatusCode::OK, "read: {text}");
    let entities: Value = serde_json::from_slice(&body).expect("a list");
    assert_eq!(entities[0]["id"], json!(STATION), "{text}");
    assert_eq!(entities[0]["type"], json!("AirQuality"), "{text}");
    assert_eq!(entities[0]["particulates"]["value"], json!(34.2), "{text}");
    assert!(!text.contains("pm10") && !text.contains("pm25"), "{text}");

    for (q, found) in [("particulates%3E30", 1), ("particulates%3E40", 0)] {
        let (status, body) = call(
            &app,
            get(MAPPED_SLUG, &format!("/entities?type=AirQuality&q={q}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{q}");
        let entities: Value = serde_json::from_slice(&body).expect("a list");
        assert_eq!(entities.as_array().map(Vec::len), Some(found), "{q}");
    }

    let (status, _) = call(&app, post(MAPPED_SLUG, "/entities", &station(None))).await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "a mapped view takes no write"
    );
}

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
    antares
        .load(
            &[view(
                WRITER_SLUG,
                &["location", "pm10", "pm25"],
                &[
                    "create_entity",
                    "delete_entity",
                    "query_entities",
                    "retrieve_entity",
                ],
            )],
            json!({
                "policies": [rule(
                    "writer",
                    json!({"kind": "serviceAccount", "id": "writer"}),
                    &["5.6.1", "5.7.1", "5.7.2", "5.6.6"],
                    Some(json!([{
                        "entities": [{"type": "AirQualityObserved"}],
                        "propertyNames": ["pm10", "pm25", "location"]
                    }])),
                )],
                "views": { WRITER_SLUG: [format!("{SPACE}/writer")] },
            }),
        )
        .await;
    let realm = common::Realm::new();
    let accounts = service_accounts();

    let writer = Endpoint {
        declared_types: None,
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
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
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
                // Refused without a reason (T-4666), so a rerun would not start clean.
                .header("jc-reason", "cleanup")
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
    let repo = jc_manifest::loader::Repository::load(&dir).expect("the repository loads");
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

/// T-3801 (G6, T-3632, T-3812): a projection's `temporalQ` reaches the broker as the window of the
/// view's permit, as the Portal's writer puts it, and a temporal read through the Endpoint
/// returns only the instances inside it; the gateway clamps nothing itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_projection_window_narrows_a_temporal_read_through_the_view() {
    let Some(antares) = Antares::start() else {
        return;
    };
    let public = json!({"kind": "role", "id": "public"});
    let information = json!([{
        "entities": [{"type": "AirQualityObserved"}],
        "propertyNames": ["pm10"]
    }]);
    let window = "timerel=after;timeAt=2026-01-01T00:00:00Z";
    let mut recent = rule(
        &format!("public-air@{WRITER_SLUG}"),
        public,
        &["5.7.3"],
        Some(information),
    );
    recent["temporalQ"] = json!(window);
    let rules = json!({
        "policies": [
            recent,
            rule("seeder", json!({"kind": "serviceAccount", "id": "seeder"}), &["5.6.1", "5.6.2"], None),
        ],
        "views": { WRITER_SLUG: [format!("{SPACE}/public-air@{WRITER_SLUG}")] },
    });
    antares
        .load(
            &[view(WRITER_SLUG, &["pm10"], &["temporal_retrieve"])],
            rules,
        )
        .await;
    // Two instances of pm10, one each side of the window: a create, then an update.
    let instance =
        |value: f64, at: &str| json!({"type": "Property", "value": value, "observedAt": at});
    antares
        .seed(&json!({
            "id": STATION,
            "type": "AirQualityObserved",
            "pm10": instance(1.0, "2025-06-01T00:00:00Z"),
        }))
        .await;
    let updated = reqwest::Client::new()
        .patch(format!(
            "{}/ngsi-ld/v1/entities/{STATION}/attrs",
            antares.url()
        ))
        .header("ngsild-tenant", SPACE)
        .header("x-jc-client", "seeder")
        .json(&json!({"pm10": instance(2.0, "2026-06-01T00:00:00Z")}))
        .send()
        .await
        .expect("the broker answers");
    let code = updated.status();
    assert!(
        code.is_success(),
        "update: {code} {}",
        updated.text().await.unwrap_or_default()
    );

    let projection = serde_json::from_value(json!({
        "contextSpaceRef": SPACE, "dataModelRef": {"name": "air", "version": "1.0.0"},
        "classes": [{"name": "AirQualityObserved"}],
        "filter": {"temporalQ": window}
    }))
    .expect("a projection");
    let windowed = Endpoint {
        projection: Some(std::sync::Arc::new(projection)),
        ..endpoint(WRITER_SLUG, Audience::Public)
    };
    let app = router(std::sync::Arc::new(
        Gateway::new(Broker::new(antares.url()), Box::new(PolicyPdp), ORG).serve([windowed]),
    ));
    let (status, body) = call(
        &app,
        get(WRITER_SLUG, &format!("/temporal/entities/{STATION}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "temporal: {}", body_text(&body));
    let answer: Value = serde_json::from_slice(&body).expect("a JSON answer");
    let values: Vec<&Value> = answer["pm10"]
        .as_array()
        .map(|instances| instances.iter().map(|i| &i["value"]).collect())
        .unwrap_or_default();
    assert_eq!(
        values,
        [&json!(2.0)],
        "only the instance inside the window: {answer}"
    );
}

/// The ids a surface answered: an NGSI-LD array or a CSV file.
fn ids_of(file: &str, body: &[u8]) -> Vec<String> {
    let mut ids: Vec<String> = match file {
        "file.csv" => {
            let text = body_text(body);
            let mut lines = text.trim_start_matches('\u{feff}').lines();
            let header: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
            // No row is a file with no columns to name.
            let Some(at) = header.iter().position(|h| h.trim_matches('"') == "id") else {
                assert!(lines.next().is_none(), "rows without an id column: {text}");
                return Vec::new();
            };
            lines
                .filter(|line| !line.is_empty())
                .map(|line| {
                    line.split(',')
                        .nth(at)
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_owned()
                })
                .collect()
        }
        _ => {
            let answer: Value = serde_json::from_slice(body).expect("JSON");
            let items = answer.get("features").unwrap_or(&answer);
            items
                .as_array()
                .expect("a list")
                .iter()
                .map(|item| item["id"].as_str().expect("an id").to_owned())
                .collect()
        }
    };
    ids.sort();
    ids
}

/// EP-42 (T-4420): a file download takes the NGSI-LD query parameters under the same Policy
/// intersection as `/entities`. For q, georel, idPattern and attrs, `file.csv` answers the ids
/// the query answers through the view, and no
/// parameter widens the grant: an attribute outside it is neither returned nor matched, and an
/// entity outside its idPattern is not counted in.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_download_takes_the_query_under_the_policy_as_entities_does() {
    let Some(antares) = Antares::start() else {
        return;
    };
    let (views, rules) = public_rules();
    antares.load(&views, rules).await;
    let at = |name: &str, pm10: f64, point: [f64; 2]| {
        json!({
            "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:{name}"),
            "type": "AirQualityObserved",
            "pm10": { "type": "Property", "value": pm10 },
            "pm25": { "type": "Property", "value": 5.0 },
            // Stored by the pipeline, outside the grant's propertyNames.
            "operatorPhone": { "type": "Property", "value": "CANARY-PHONE" },
            "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": point } }
        })
    };
    // Banská Bystrica twice, Bratislava once, and one the grant's idPattern leaves out.
    let mut outside = at("x", 99.0, [19.15, 48.73]);
    outside["id"] = json!("urn:ngsi-ld:AirQualityObserved:zvolen.sk:ovzdusie:x");
    for entity in [
        at("station-a1", 34.2, [19.15, 48.73]),
        at("station-a2", 12.0, [19.151, 48.731]),
        at("station-b1", 50.0, [17.11, 48.15]),
        outside,
    ] {
        antares.seed(&entity).await;
    }

    let mut served = endpoint(PUBLIC_SLUG, Audience::Public);
    served.representations = vec![Representation::NgsiLd, Representation::Csv];
    let gateway =
        Gateway::new(Broker::new(antares.url()), Box::new(PolicyPdp), ORG).serve([served]);
    let app = router(std::sync::Arc::new(gateway));
    let id =
        |name: &str| format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:{name}");
    let read = |path: String| {
        Request::builder()
            .method("GET")
            .uri(format!("/api/endpoint/{PUBLIC_SLUG}{path}"))
            .body(Body::empty())
            .expect("a request")
    };

    let cases: [(&str, &str, Option<Vec<String>>); 6] = [
        (
            "q",
            "q=pm10%3E30",
            Some(vec![id("station-a1"), id("station-b1")]),
        ),
        (
            "georel",
            "georel=near%3BmaxDistance%3D%3D2000&geometry=Point&coordinates=%5B19.15%2C48.73%5D",
            Some(vec![id("station-a1"), id("station-a2")]),
        ),
        (
            "idPattern",
            "idPattern=.%2Astation-a.%2A",
            Some(vec![id("station-a1"), id("station-a2")]),
        ),
        (
            "attrs",
            "attrs=pm10",
            Some(vec![id("station-a1"), id("station-a2"), id("station-b1")]),
        ),
        // A parameter that names what the grant leaves out widens nothing.
        ("attrs outside the grant", "attrs=operatorPhone", None),
        (
            "q outside the grant",
            "q=operatorPhone%3D%3D%22CANARY-PHONE%22",
            Some(vec![]),
        ),
    ];
    for (case, query, expected) in cases {
        let (status, body) = call(
            &app,
            read(format!(
                "/ngsi-ld/v1/entities?type=AirQualityObserved&{query}"
            )),
        )
        .await;
        let entities = (status == StatusCode::OK).then(|| ids_of("entities", &body));
        if let Some(expected) = &expected {
            assert_eq!(
                entities.as_ref(),
                Some(expected),
                "{case}: /entities {status} {}",
                body_text(&body)
            );
        }
        assert!(
            !body_text(&body).contains("CANARY"),
            "{case}: /entities carried a denied value"
        );
        let file = "file.csv";
        let (file_status, bytes) = call(
            &app,
            read(format!("/{file}?type=AirQualityObserved&{query}")),
        )
        .await;
        let text = body_text(&bytes);
        assert!(
            !text.contains("CANARY"),
            "{case}: {file} carried a denied value: {text}"
        );
        assert!(
            !text.contains("operatorPhone"),
            "{case}: {file} named a denied attribute: {text}"
        );
        assert!(
            !text.contains("zvolen.sk"),
            "{case}: {file} counted in an entity outside the idPattern"
        );
        if case == "attrs" {
            assert!(
                !text.contains("pm25"),
                "{case}: {file} carried an attribute attrs left out: {text}"
            );
        }
        match &entities {
            Some(entities) => {
                assert_eq!(file_status, StatusCode::OK, "{case}: {file}: {text}");
                assert_eq!(
                    &ids_of(file, &bytes),
                    entities,
                    "{case}: {file} and /entities differ"
                );
            }
            // Where `/entities` refuses the query, the file does not answer it either.
            None => assert!(
                file_status.is_client_error() || ids_of(file, &bytes).is_empty(),
                "{case}: {file} answered what /entities refused ({status}): {text}"
            ),
        }
    }
}
