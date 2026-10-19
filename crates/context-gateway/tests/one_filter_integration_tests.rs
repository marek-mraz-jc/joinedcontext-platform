//! T-4755: one entity filter and one subscription filter, whatever the surface.
//!
//! The broker's view and its `joinedcontext` engine decide what a caller reads (ADR-N-050), and
//! its `deliver_as` decides what a subscriber is sent. This test holds every surface of the
//! gateway to that one decision: for each Policy shape (type only, an attribute hidden, a row
//! filter `q`, a bounded area) NGSI-LD, OData, OGC Features, the CSV file, MCP and the WebSocket
//! fan-out name the same entities with the same attributes. The WebSocket is the subscription
//! channel: a hidden attribute or row never reaches its subscriber.
//!
//! Antares runs in Docker with the surfaces dev mounts; the test loads what the Portal's view
//! writer would (`PUT /q/schemas`, `/q/views`, `/q/policy`), and is skipped without Docker.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};

/// The broker dev runs (deployment `components/context-broker/images.yaml`, `dev-8`).
const IMAGE: &str = "ghcr.io/marek-mraz-jc/antares-broker-jc@sha256:36594b141f5ad188cefeacc7eb5292892804248595b8603663826188abc02b62";
const SPACE: &str = "ovzdusie";
const ORG: &str = "banskabystrica.sk";
const TYPE: &str = "AirQualityObserved";
const ATTRIBUTES: [&str; 4] = ["location", "operatorPhone", "pm10", "pm25"];
/// Around Banská Bystrica: st-1 and st-2 lie in it, st-3 (Bratislava) does not.
const AREA: &str = "georel=within;geometry=Polygon;coordinates=[[[18.9,48.6],[19.4,48.6],[19.4,48.9],[18.9,48.9],[18.9,48.6]]]";

/// One Policy shape: what it grants, as the gateway's Policy and as the broker's rule, and what
/// every surface must then show.
struct Shape {
    name: &'static str,
    slug: &'static str,
    /// `propertyNames`; `None` grants the whole entity.
    shows: Option<&'static [&'static str]>,
    q: Option<&'static str>,
    geo_q: Option<&'static str>,
    ids: &'static [&'static str],
}

impl Shape {
    fn attributes(&self) -> BTreeSet<String> {
        self.shows
            .unwrap_or(&ATTRIBUTES)
            .iter()
            .map(|a| (*a).to_owned())
            .collect()
    }

    /// What every surface must answer: each granted id with the granted attributes.
    fn expected(&self) -> BTreeMap<String, BTreeSet<String>> {
        self.ids
            .iter()
            .map(|id| ((*id).to_owned(), self.attributes()))
            .collect()
    }

    fn policy(&self) -> PolicySpec {
        let mut yaml = format!(
            "contextSpaceRef: {SPACE}\nassigner: did:web:{ORG}\nassignee: {{ kind: role, id: public }}\n\
             operations: [queryEntity, retrieveEntity, createSubscription, retrieveSubscription, \
             updateSubscription, deleteSubscription]\ninformation:\n  - entities:\n      - type: {TYPE}\n"
        );
        if let Some(shows) = self.shows {
            yaml += &format!("    propertyNames: [{}]\n", shows.join(", "));
        }
        if let Some(q) = self.q {
            yaml += &format!("q: '{q}'\n");
        }
        if let Some(geo_q) = self.geo_q {
            yaml += &format!("geoQ: '{geo_q}'\n");
        }
        serde_norway::from_str(&yaml).expect("the policy parses")
    }

    /// The same grant in the rule document the Portal writes (`reconciler/views.rs`).
    fn rule(&self) -> Value {
        let mut information = json!({"entities": [{"type": TYPE}]});
        if let Some(shows) = self.shows {
            information["propertyNames"] = json!(shows);
        }
        let mut rule = json!({
            "id": format!("{SPACE}/{}", self.name),
            "effect": "permit",
            "assignee": {"kind": "role", "id": "public"},
            "operations": ["5.7.1", "5.7.2", "5.8.1", "5.8.2", "5.8.3", "5.8.4", "5.8.5"],
            "information": [information],
        });
        if let Some(q) = self.q {
            rule["q"] = json!(q);
        }
        if let Some(geo_q) = self.geo_q {
            rule["geoQ"] = json!(geo_q);
        }
        rule
    }

    fn view(&self) -> Value {
        json!({
            "id": self.slug,
            "tenant": SPACE,
            "selector": [TYPE],
            "rules": [{"types": [TYPE], "attributes": self.attributes()}],
            "operations": ["query_entities", "retrieve_entity", "create_subscription",
                           "retrieve_subscription", "update_subscription", "delete_subscription",
                           "query_subscriptions"],
        })
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint {
            declared_types: None,
            roles: Default::default(),
            slug: self.slug.to_owned(),
            title: BTreeMap::new(),
            description: BTreeMap::new(),
            base_path: format!("/api/endpoint/{}", self.slug),
            space: SPACE.to_owned(),
            project: SPACE.to_owned(),
            audience: Audience::Public,
            allowed_projects: Vec::new(),
            representations: vec![
                Representation::NgsiLd,
                Representation::Mcp,
                Representation::Csv,
                Representation::OgcFeatures,
                Representation::Odata,
                Representation::WebSocket,
            ],
            rate_limit: None,
            creates: None,
            file_limits: None,
            hidden_attributes: Default::default(),
            projection: None,
            models: Vec::new(),
            view_mapping: None,
            catalog: None,
            policy_names: Vec::new(),
            policies: vec![self.policy()],
        }
    }
}

const SHAPES: [Shape; 4] = [
    Shape {
        name: "types",
        slug: "t4755typeonly2xk7vbn3tdw6h",
        shows: None,
        q: None,
        geo_q: None,
        ids: &["st-1", "st-2", "st-3"],
    },
    Shape {
        name: "hidden",
        slug: "t4755hiddenzq2xk7vbn3tdw6h",
        shows: Some(&["location", "pm10"]),
        q: None,
        geo_q: None,
        ids: &["st-1", "st-2", "st-3"],
    },
    Shape {
        name: "rows",
        slug: "t4755rowsmzq2xk7vbn3tdw6hc",
        shows: None,
        q: Some("pm10>30"),
        geo_q: None,
        ids: &["st-1", "st-3"],
    },
    Shape {
        name: "area",
        slug: "t4755areamzq2xk7vbn3tdw6hc",
        shows: None,
        q: None,
        geo_q: Some(AREA),
        ids: &["st-1", "st-2"],
    },
];

fn urn(local: &str) -> String {
    format!("urn:ngsi-ld:{TYPE}:{ORG}:{SPACE}:{local}")
}

/// The local part of an entity id, so a table reads `st-1`.
fn local(id: &str) -> String {
    id.rsplit(':').next().unwrap_or(id).to_owned()
}

fn station(local: &str, pm10: f64, at: [f64; 2]) -> Value {
    json!({
        "id": urn(local),
        "type": TYPE,
        "pm10": {"type": "Property", "value": pm10},
        "pm25": {"type": "Property", "value": pm10 / 3.0},
        "operatorPhone": {"type": "Property", "value": format!("+421 900 000 {local}")},
        "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": at}},
    })
}

/// pm10 by station, st-2 always under the row filter's bound.
const STATIONS: [(&str, f64, [f64; 2]); 3] = [
    ("st-1", 34.2, [19.15, 48.73]),
    ("st-2", 18.0, [19.16, 48.74]),
    ("st-3", 41.0, [17.10, 48.15]),
];

/// A broker container that stops when the test ends, however it ends.
struct Antares {
    id: String,
    url: String,
}

impl Drop for Antares {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.id]).output();
    }
}

impl Antares {
    /// `None` without Docker: a skipped test, since the assertion is about the gateway.
    // ponytail: a second copy of `antares_integration_tests`'s start, for dev's surfaces and
    // image; one harness in `common` when a third file needs a broker.
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
                "ANTARES_SCHEMA_MAX_BYTES=1048576",
                "-e",
                // As dev mounts them (deployment `context-broker/values/broker/base-values`).
                "ANTARES_API_SURFACES=admin,views,mcp,access,ws,ws-views,ogc,linkml,odata,sta",
                "-e",
                "ANTARES_POLICY=joinedcontext",
                "-e",
                "ANTARES_POLICY_SUBJECT_HEADERS=x-jc-subject,x-jc-groups,x-jc-roles,x-jc-client,x-jc-restricted,x-jc-did",
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
        let port: u16 = String::from_utf8_lossy(&published.stdout)
            .lines()
            .next()
            .and_then(|line| line.rsplit(':').next()?.trim().parse().ok())
            .expect("a published port");
        let broker = Self {
            id,
            url: format!("http://127.0.0.1:{port}"),
        };
        for _ in 0..120 {
            let probe = Command::new("docker")
                .args(["exec", &broker.id, "/antares", "--health"])
                .output();
            if probe.is_ok_and(|out| out.status.success()) {
                return Some(broker);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!("the broker never became healthy");
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: &Value, as_seeder: bool) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.url))
            .json(body);
        if as_seeder {
            request = request
                .header("ngsild-tenant", SPACE)
                .header("x-jc-client", "seeder");
        }
        let answer = request.send().await.expect("the broker answers");
        let status = answer.status();
        assert!(
            status.is_success(),
            "{path}: {status} {}",
            answer.text().await.unwrap_or_default()
        );
    }

    /// The space's schema, one view per shape, the rule document and the three stations.
    async fn load(&self) {
        use reqwest::Method;
        let schema = json!({
            "id": "https://uri.etsi.org/ngsi-ld/default-context", "name": SPACE,
            "prefixes": {"d": {"prefix_prefix": "d",
                               "prefix_reference": "https://uri.etsi.org/ngsi-ld/default-context/"}},
            "default_prefix": "d",
            "classes": {TYPE: {"slots": ATTRIBUTES}},
            "slots": {"pm10": {"range": "float"}, "pm25": {"range": "float"},
                      "operatorPhone": {"range": "string"}, "location": {"range": "string"}},
        });
        self.send(Method::PUT, &format!("/q/schemas/{SPACE}"), &schema, false)
            .await;
        let mut policies: Vec<Value> = SHAPES.iter().map(Shape::rule).collect();
        policies.push(json!({
            "id": format!("{SPACE}/seeder"), "effect": "permit",
            "assignee": {"kind": "serviceAccount", "id": "seeder"},
            "operations": ["5.6.1", "5.6.2", "5.6.3", "5.6.4"],
        }));
        let views: BTreeMap<&str, Vec<String>> = SHAPES
            .iter()
            .map(|s| (s.slug, vec![format!("{SPACE}/{}", s.name)]))
            .collect();
        for shape in &SHAPES {
            self.send(
                Method::PUT,
                &format!("/q/views/{}", shape.slug),
                &shape.view(),
                false,
            )
            .await;
        }
        self.send(
            Method::PUT,
            &format!("/q/policy/{SPACE}"),
            &json!({"policies": policies, "views": views}),
            false,
        )
        .await;
        for (name, pm10, at) in STATIONS {
            self.send(
                Method::POST,
                "/ngsi-ld/v1/entities",
                &station(name, pm10, at),
                true,
            )
            .await;
        }
    }

    /// A new pm10 for every station, on the same side of the row filter's bound as before.
    async fn change(&self, round: u32) {
        for (name, pm10, _) in STATIONS {
            let value = pm10 + f64::from(round) / 10.0;
            self.send(
                reqwest::Method::PATCH,
                &format!("/ngsi-ld/v1/entities/{}/attrs", urn(name)),
                &json!({"pm10": {"type": "Property", "value": value}}),
                true,
            )
            .await;
        }
    }
}

/// The gateway on a socket of its own, so the WebSocket can upgrade.
async fn serve(broker: &str) -> String {
    let gateway = Gateway::new(Broker::new(broker), Box::new(PolicyPdp), ORG)
        .serve(SHAPES.iter().map(Shape::endpoint));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(std::sync::Arc::new(gateway))).await;
    });
    format!("127.0.0.1:{}", address.port())
}

type Seen = BTreeMap<String, BTreeSet<String>>;

/// The id and the attribute names of each NGSI-LD entity, without the structural members.
fn seen<'a>(entities: impl IntoIterator<Item = &'a Value>) -> Seen {
    entities
        .into_iter()
        .filter_map(|e| {
            let id = local(e.get("id")?.as_str()?);
            let names = e
                .as_object()?
                .keys()
                .filter(|k| !["id", "type", "@context", "_types"].contains(&k.as_str()))
                .filter(|k| !k.starts_with('@'))
                .cloned()
                .collect();
            Some((id, names))
        })
        .collect()
}

async fn get(at: &str, path: &str) -> String {
    let answer = reqwest::Client::new()
        .get(format!("http://{at}{path}"))
        .header("accept", "application/json")
        .send()
        .await
        .expect("the gateway answers");
    let status = answer.status();
    let body = answer.text().await.expect("a body");
    assert!(status.is_success(), "{path}: {status} {body}");
    body
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

async fn ngsi_ld(at: &str, slug: &str) -> Seen {
    let body = json_of(
        &get(
            at,
            &format!("/api/endpoint/{slug}/ngsi-ld/v1/entities?type={TYPE}"),
        )
        .await,
    );
    seen(body.as_array().into_iter().flatten())
}

async fn odata(at: &str, slug: &str) -> Seen {
    let body = json_of(&get(at, &format!("/api/endpoint/{slug}/odata/v4/{TYPE}")).await);
    seen(body["value"].as_array().into_iter().flatten())
}

/// A Feature's properties, and `location` for its geometry.
async fn ogc(at: &str, slug: &str) -> Seen {
    let path = format!("/api/endpoint/{slug}/ogc/features/collections/{TYPE}/items");
    let body = json_of(&get(at, &path).await);
    body["features"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let mut names: BTreeSet<String> =
                f["properties"].as_object()?.keys().cloned().collect();
            if !f["geometry"].is_null() {
                names.insert("location".to_owned());
            }
            Some((local(f["id"].as_str()?), names))
        })
        .collect()
}

/// The file's rows by id, each with the columns it fills.
async fn csv(at: &str, slug: &str) -> Seen {
    let text = get(at, &format!("/api/endpoint/{slug}/file.csv?type={TYPE}")).await;
    let mut lines = text.lines();
    let header: Vec<String> = lines
        .next()
        .unwrap_or_default()
        .split(',')
        .map(|c| c.trim_matches('"').to_owned())
        .collect();
    let mut table = Seen::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let cells = split_csv(line);
        let mut id = String::new();
        let mut names = BTreeSet::new();
        for (column, cell) in header.iter().zip(cells) {
            // A column is a flattened path (`pm10.value`): its attribute is the first segment.
            match column.split('.').next().unwrap_or_default() {
                "id" => id = local(&cell),
                "type" => {}
                name if !cell.is_empty() => {
                    names.insert(name.to_owned());
                }
                _ => {}
            }
        }
        table.insert(id, names);
    }
    table
}

/// One CSV line's cells, quotes honoured.
fn split_csv(line: &str) -> Vec<String> {
    let (mut cells, mut cell, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => cells.push(std::mem::take(&mut cell)),
            c => cell.push(c),
        }
    }
    cells.push(cell);
    cells
}

async fn mcp(at: &str, slug: &str) -> Seen {
    let answer = reqwest::Client::new()
        .post(format!("http://{at}/api/endpoint/{slug}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": "query_entities", "arguments": {"type": [TYPE]}}}))
        .send()
        .await
        .expect("the gateway answers");
    let body = json_of(&answer.text().await.expect("a body"));
    assert_ne!(body["result"]["isError"], true, "{body}");
    seen(
        body["result"]["structuredContent"]["entities"]
            .as_array()
            .into_iter()
            .flatten(),
    )
}

/// Subscribes over the Endpoint's WebSocket, changes every station and answers what was
/// notified: the latest attributes per id.
async fn websocket(at: &str, slug: &str, broker: &Antares) -> Seen {
    let mut request = format!("ws://{at}/api/endpoint/{slug}/ws")
        .into_client_request()
        .expect("a request");
    request.headers_mut().insert(
        "sec-websocket-protocol",
        "ngsi-ld-ws".parse().expect("a value"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .unwrap_or_else(|e| panic!("{slug}: no upgrade: {e}"));
    let subscribe = json!({"metadata": {"x-op": "x-subscribe", "x-id": "s"},
        "body": {"type": "Subscription", "entities": [{"type": TYPE}]}});
    socket
        .send(Message::text(subscribe.to_string()))
        .await
        .expect("sent");
    let mut notified = Seen::new();
    // The hub's upstream goes live a moment after the first client joins, so the stations change
    // round after round until notifications arrive, and once more for any that come late.
    let mut rounds_with_data = 0;
    for round in 1..=10 {
        broker.change(round).await;
        read_for(&mut socket, Duration::from_secs(2), &mut notified).await;
        if !notified.is_empty() {
            rounds_with_data += 1;
            if rounds_with_data == 2 {
                break;
            }
        }
    }
    notified
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Reads the socket for `long`, keeping the latest attributes each notification names per id.
async fn read_for(socket: &mut Socket, long: Duration, notified: &mut Seen) {
    let until = tokio::time::Instant::now() + long;
    while let Ok(read) = tokio::time::timeout_at(until, socket.next()).await {
        let Some(Ok(Message::Text(text))) = read else {
            panic!("the socket ended: {read:?}");
        };
        let message = json_of(text.as_str());
        match message["metadata"]["x-op"].as_str() {
            Some("x-result") => assert_eq!(message["metadata"]["x-status"], 201, "{message}"),
            Some("x-notification") => {
                notified.extend(seen(
                    message["body"]["data"].as_array().into_iter().flatten(),
                ));
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_surface_shows_the_same_rows_and_attributes_under_each_policy_shape() {
    let Some(broker) = Antares::start() else {
        return;
    };
    broker.load().await;
    let at = serve(&broker.url).await;
    for shape in &SHAPES {
        let expected = shape.expected();
        let surfaces = [
            ("NGSI-LD", ngsi_ld(&at, shape.slug).await),
            ("OData", odata(&at, shape.slug).await),
            ("OGC Features", ogc(&at, shape.slug).await),
            ("CSV", csv(&at, shape.slug).await),
            ("MCP", mcp(&at, shape.slug).await),
            ("WebSocket", websocket(&at, shape.slug, &broker).await),
        ];
        for (surface, got) in surfaces {
            assert_eq!(got, expected, "{} on {surface}", shape.name);
        }
    }
}
