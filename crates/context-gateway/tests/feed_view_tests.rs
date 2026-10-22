//! The feed outputs of an Endpoint are the broker's `antares-feed` surface of its view (EP-150,
//! EP-151, T-5203): `/api/endpoint/{slug}/feed/v1/{name}/{atom.xml|rss.xml|feed.json}` goes to
//! `/x/views/{slug}/feed/v1/{name}/...` with the query as sent and the tenant and caller pinned.
//! These tests hold what the gateway adds: the route per format, the links and the `tag:` ids
//! on the public host, read-only, and no feed at all on an Endpoint without an output. The
//! 401/403 of a non-public Endpoint is the route sweep's row (route_authz_sweep_tests.rs).

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, FeedOutput, PolicySpec, Representation};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "f4y7pq2mzt6vhx3nbwrs5cjd8f";
const SPACE: &str = "podujatia";
const DOMAIN: &str = "bbsk.sk";
const PUBLIC: &str = "https://data.example.sk";

type Hops = Arc<Mutex<Vec<(String, Option<String>, HeaderMap)>>>;

/// A broker answering each feed file with its media type, naming itself by its `Host` in the
/// links, the `tag:` ids and the author, as Antares does.
async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let recorder = Arc::clone(&recorder);
        async move {
            let authority = request.headers()["host"].to_str().expect("a host").to_owned();
            let host = authority.split(':').next().unwrap_or_default().to_owned();
            let path = request.uri().path().to_owned();
            recorder.lock().expect("the hop log").push((
                path.clone(),
                request.uri().query().map(str::to_owned),
                request.headers().clone(),
            ));
            let this = format!("http://{authority}{path}");
            let (media, body) = if path.ends_with("/rss.xml") {
                ("application/rss+xml; charset=utf-8", format!(
                    "<rss version=\"2.0\"><channel><link>{this}</link><item><guid isPermaLink=\"false\">tag:{host},2026:{SLUG}/urn:ngsi-ld:Event:1</guid></item></channel></rss>"
                ))
            } else if path.ends_with("/feed.json") {
                ("application/feed+json; charset=utf-8", format!(
                    r#"{{"version":"https://jsonfeed.org/version/1.1","feed_url":"{this}","authors":[{{"name":"{host}"}}],"items":[{{"id":"tag:{host},2026:{SLUG}/urn:ngsi-ld:Event:1"}}]}}"#
                ))
            } else {
                ("application/atom+xml; charset=utf-8", format!(
                    "<feed xmlns=\"http://www.w3.org/2005/Atom\"><id>tag:{host},2026:{SLUG}/feed/kultura</id><author><name>{host}</name></author><generator>Antares</generator><link rel=\"self\" href=\"{this}\"/><link rel=\"next\" href=\"{this}?offset=50\"/></feed>"
                ))
            };
            ([("content-type", media)], body).into_response()
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

fn endpoint(feeds: Vec<FeedOutput>) -> Endpoint {
    let grant: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\nassigner: did:web:{DOMAIN}\nassignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n  - entities: [{{ type: Event }}]\n"
    ))
    .expect("the policy spec parses");
    let mut representations = vec![Representation::NgsiLd];
    if !feeds.is_empty() {
        representations.push(Representation::Feed);
    }
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        base_path: format!("/api/endpoint/{SLUG}"),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations,
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds,
        calendars: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant],
    }
}

fn kultura() -> Vec<FeedOutput> {
    vec![serde_norway::from_str(
        "name: kultura\ntitle: { sk: Kultúra }\nsources: [{ type: Event, preset: event }]\n",
    )
    .expect("the output parses")]
}

async fn call(
    upstream: &str,
    endpoint: Endpoint,
    method: Method,
    uri: &str,
) -> (StatusCode, HeaderMap, String) {
    let mut gateway =
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint]);
    gateway.public_url = Some(PUBLIC.to_owned());
    let response = router(Arc::new(gateway))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Each format goes to the view's feed output as sent, keeps its media type, and comes back
/// with its links, ids and author on the public host: no document names the broker.
#[tokio::test]
async fn each_format_is_the_views_feed_on_the_public_host() {
    let (upstream, hops) = broker().await;
    let broker_host = upstream
        .trim_start_matches("http://")
        .split(':')
        .next()
        .unwrap_or_default()
        .to_owned();
    for (file, media) in [
        ("atom.xml", "application/atom+xml"),
        ("rss.xml", "application/rss+xml"),
        ("feed.json", "application/feed+json"),
    ] {
        let uri = format!("/api/endpoint/{SLUG}/feed/v1/kultura/{file}?lang=sk&limit=5");
        let (status, headers, body) = call(&upstream, endpoint(kultura()), Method::GET, &uri).await;
        assert_eq!(status, StatusCode::OK, "{file}: {body}");
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap_or_default()
                .starts_with(media),
            "{file}: {headers:?}"
        );
        assert!(
            body.contains(&format!(
                "{PUBLIC}/api/endpoint/{SLUG}/feed/v1/kultura/{file}"
            )),
            "{file}: {body}"
        );
        assert!(body.contains("tag:data.example.sk,2026:"), "{file}: {body}");
        assert!(
            !body.contains(&broker_host),
            "{file} names the broker: {body}"
        );
        assert!(
            !body.contains("/x/views/"),
            "{file} names the view mount: {body}"
        );
    }
    let hops = hops.lock().expect("the hop log").clone();
    assert_eq!(hops.len(), 3);
    assert_eq!(
        hops[0].0,
        format!("/x/views/{SLUG}/feed/v1/kultura/atom.xml")
    );
    assert_eq!(hops[0].1.as_deref(), Some("lang=sk&limit=5"));
    assert_eq!(hops[0].2["ngsild-tenant"], SPACE);
}

/// EP-150: an Endpoint without a feed output has no feed route to it: the gateway answers 404
/// and the broker is not asked.
#[tokio::test]
async fn an_endpoint_without_an_output_has_no_feed() {
    let (upstream, hops) = broker().await;
    let (status, _, body) = call(
        &upstream,
        endpoint(Vec::new()),
        Method::GET,
        &format!("/api/endpoint/{SLUG}/feed/v1/kultura/atom.xml"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(hops.lock().expect("the hop log").is_empty());
}

/// EP-39: a feed is read-only.
#[tokio::test]
async fn a_write_is_refused_before_the_broker() {
    let (upstream, hops) = broker().await;
    let uri = format!("/api/endpoint/{SLUG}/feed/v1/kultura/atom.xml");
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let (status, _, _) = call(&upstream, endpoint(kultura()), method.clone(), &uri).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
    assert!(hops.lock().expect("the hop log").is_empty());
}
