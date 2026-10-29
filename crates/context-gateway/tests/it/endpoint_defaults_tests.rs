//! An Endpoint whose manifest lists no `enabledRepresentations` serves every standard but
//! SensorThings, which stays opt-in (EP-104, T-3830). The manifests go through the store, as the
//! repository serves them, and the broker's view answers the surfaces it serves (NGSI-LD, OGC,
//! OData); these tests hold what the gateway does before that hop.

use crate::common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use jc_core::kinds::Representation;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const HOST: &str = "https://hel.example.fi";
/// A public Endpoint that lists nothing.
const DEFAULTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
/// A public Endpoint that lists SensorThings.
const WITH_STA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
/// An Endpoint for its own project, listing nothing.
const INTERNAL: &str = "cccccccccccccccccccccccccc";

/// The surfaces the broker's view serves, and SensorThings, as a client asks them.
const NGSI_LD: &str = "ngsi-ld/v1/entities?type=BikeHireDockingStation";
const OGC: &str = "ogc/features/collections";
const ODATA: &str = "odata/v4/";
const STA: &str = "sta/v1.1/Things";

const SPACE: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  \
                     name: helsinki\n  namespace: helsinki\nspec:\n  isSandbox: false\n  \
                     urnSegment: helsinki\n";

fn endpoint(name: &str, slug: &str, audience: &str, extra: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: helsinki\n  slug: {slug}\n  \
         audience: {audience}\n{extra}"
    )
}

fn policy(name: &str, role: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: {name}\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: {{ kind: ContextSpace, name: helsinki }}\n  \
         assigner: did:web:hel.fi\n  assignee: {{ kind: role, id: {role} }}\n  \
         operations: [retrieveOps]\n  information:\n    - entities:\n        - type: BikeHireDockingStation\n"
    )
}

fn repo(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-endpoint-defaults-{test}-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).expect("write");
    write("space.yaml", SPACE);
    write(
        "defaulted.yaml",
        &endpoint("helsinki-bikes", DEFAULTED, "public", ""),
    );
    write(
        "sta.yaml",
        &endpoint(
            "helsinki-sensors",
            WITH_STA,
            "public",
            "  enabledRepresentations: [ngsi-ld, sta]\n",
        ),
    );
    write(
        "internal.yaml",
        &endpoint(
            "helsinki-ops",
            INTERNAL,
            "project-list",
            "  allowedProjects: [hel-mobility]\n",
        ),
    );
    write("public.yaml", &policy("public-bikes", "public"));
    write("staff.yaml", &policy("staff-bikes", "data-steward"));
    dir
}

type Hops = Arc<Mutex<Vec<String>>>;

/// A broker that answers every request `200` and records the path it was asked.
async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder
                .lock()
                .expect("the hop log")
                .push(request.uri().path().to_owned());
            axum::Json(json!({ "value": [] }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, hops)
}

fn app(dir: &Path, upstream: &str, realm: &common::Realm) -> Router {
    let (endpoints, ..) = store::load(dir).expect("the repository loads");
    router(Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), "hel.fi")
            .serve(endpoints)
            .authenticate(
                Arc::new(realm.verifier()),
                ServiceAccounts::new(),
                Some(HOST.to_owned()),
            ),
    ))
}

/// A person of `project`, holding `data-steward`, with a token for `slug`.
fn person(realm: &common::Realm, project: &str, slug: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": format!("someone-of-{project}"),
        "aud": slug,
        "preferred_username": format!("someone-of-{project}"),
        "groups": [format!("/{project}")],
        "realm_access": { "roles": ["data-steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

async fn get(app: Router, slug: &str, surface: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/endpoint/{slug}/{surface}"));
    if let Some(token) = token {
        request = request.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// EP-104: a fresh public Endpoint answers on NGSI-LD, OGC and OData, each through the broker's
/// view, and SensorThings is the 404 of a surface it does not serve.
#[tokio::test]
async fn a_fresh_endpoint_serves_every_standard_but_sta() {
    let realm = common::Realm::new();
    let dir = repo("fresh");
    let (upstream, hops) = broker().await;
    for surface in [NGSI_LD, OGC, ODATA] {
        let (status, body) = get(app(&dir, &upstream, &realm), DEFAULTED, surface, None).await;
        assert_eq!(status, StatusCode::OK, "{surface}: {body}");
    }
    let asked = hops.lock().expect("the hop log").clone();
    for path in [
        "/ngsi-ld/v1/entities",
        "/ogc/features/collections",
        "/odata/v4",
    ] {
        assert!(
            asked
                .iter()
                .any(|hop| hop.starts_with("/x/views/") && hop.contains(path)),
            "{path} reached the broker's view: {asked:?}"
        );
    }
    let (status, _) = get(app(&dir, &upstream, &realm), DEFAULTED, STA, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "STA is opt-in");

    let (endpoints, ..) = store::load(&dir).expect("the repository loads");
    let fresh = endpoints
        .iter()
        .find(|e| e.slug == DEFAULTED)
        .expect("loaded");
    assert!(fresh.serves(Representation::Odata) && !fresh.serves(Representation::Sta));
}

/// EP-104: an Endpoint that lists `sta` serves it as before, and nothing the default adds.
#[tokio::test]
async fn an_endpoint_listing_sta_still_serves_it() {
    let realm = common::Realm::new();
    let dir = repo("sta");
    let (upstream, _) = broker().await;
    let (status, body) = get(app(&dir, &upstream, &realm), WITH_STA, STA, None).await;
    assert_ne!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = get(app(&dir, &upstream, &realm), WITH_STA, ODATA, None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an explicit list keeps its set"
    );
}

/// Turning a standard on widens the formats, never the data: an outsider of an internal
/// Endpoint is refused the same way on every surface, before the broker is asked.
#[tokio::test]
async fn a_caller_without_a_grant_is_refused_on_every_surface() {
    let realm = common::Realm::new();
    let dir = repo("refused");
    let (upstream, hops) = broker().await;
    let outsider = person(&realm, "doprava", INTERNAL);
    for surface in [NGSI_LD, OGC, ODATA] {
        let (status, body) = get(
            app(&dir, &upstream, &realm),
            INTERNAL,
            surface,
            Some(&outsider),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{surface}: {body}");
    }
    assert!(
        hops.lock().expect("the hop log").is_empty(),
        "nothing reached the broker"
    );
}
