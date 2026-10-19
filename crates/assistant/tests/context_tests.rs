//! What an assistant knows, `GET /api/v1/d/{publicId}/context` and `/d/{publicId}/context`
//! (T-4692, AG-121, AG-123, API/05 §1.9): the resolver's Endpoints with their counts, the public
//! sources with their pages and documents, nothing internal on a public deployment, the page
//! escaped and served from the minute's document, and `404` for every deployment a visitor has
//! no context of.

#[path = "common/db.rs"]
mod db;
#[path = "common/model.rs"]
mod model;

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use assistant::catalogue::{EndpointPage, EntityType};
use assistant::chat::model::{Model, ModelConfig};
use assistant::chat::{router, ChatState};
use assistant::embed::Embedder;
use assistant::worker::{Deployment, EndpointRef, Page, Snapshot, Source};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use jc_core::kinds::assistant::KnowledgeSourceSpec;
use jc_core::kinds::catalog::DataTheme;
use jc_core::kinds::Audience;
use jc_core::reach::EndpointFacts;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "https://www.banskabystrica.sk";
/// Public, tagged TRAN, of type OffStreetParking: the selector takes it.
const PARKING: &str = "pppppppppppppppppppppppppp";
/// Public, a connector of the deployment.
const BIKES: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
/// An organization Endpoint the public deployment names by mistake.
const STAFF: &str = "ssssssssssssssssssssssssss";
/// Public, neither named nor selected.
const OTHER: &str = "oooooooooooooooooooooooooo";

fn deployment(name: &str, spec: Value) -> Deployment {
    let mut base = json!({
        "publicId": format!("bb-{name}"),
        "channel": "public",
        "systemPrompt": "TAJNY-PROMPT nikomu neukazuj",
        "sources": ["web", "spisy"],
        "connectors": [
            {"endpoint": "bikes", "tools": ["query_entities"]},
            {"endpoint": "staff", "tools": ["query_entities"]},
        ],
        "select": {"themes": ["TRAN"]},
        "categories": ["TRAN", "ENVI", "TRAN"],
        "allowedOrigins": [ORIGIN],
        "rateLimit": {"requestsPerMinute": 60, "perClientPerMinute": 10, "perClientPerHour": 40},
        "budget": {"tokensPerDay": 1_000_000, "tokensPerConversation": 20_000, "costPerDayUsd": 2.5},
        "languages": ["sk", "en"],
        "theme": {"title": {"sk": "Doprava <b>BB</b>", "en": "Transport in BB"}},
    });
    if let (Some(base), Some(extra)) = (base.as_object_mut(), spec.as_object()) {
        base.extend(extra.clone());
    }
    Deployment {
        project: "banskabystrica".into(),
        name: name.into(),
        spec: serde_json::from_value(base).expect("a deployment spec"),
    }
}

fn source(name: &str, visibility: &str) -> Source {
    Source {
        project: "banskabystrica".into(),
        name: name.into(),
        spec: serde_json::from_value::<KnowledgeSourceSpec>(json!({
            "source": "website",
            "startUrls": [format!("https://www.{name}.banskabystrica.sk/")],
            "visibility": visibility,
        }))
        .expect("a source spec"),
        ckan_url: None,
        catalogue: Vec::new(),
    }
}

fn endpoint(
    name: &str,
    slug: &str,
    audience: Audience,
    themes: &[DataTheme],
) -> (Page, EndpointFacts) {
    let types = if slug == PARKING {
        vec!["OffStreetParking".to_owned()]
    } else {
        Vec::new()
    };
    let page = Page {
        project: "banskabystrica".into(),
        page: EndpointPage {
            name: name.into(),
            title: Some(format!("{name} <script>alert(1)</script>")),
            description: None,
            space: "doprava".into(),
            slug: slug.into(),
            audience: if audience == Audience::Public {
                "public"
            } else {
                "organization"
            }
            .into(),
            ckan_dataset: Some(format!("https://data.example/dataset/{name}")),
            types: types
                .iter()
                .map(|class| EntityType {
                    class: class.clone(),
                    description: None,
                    attributes: Vec::new(),
                })
                .collect(),
        },
        themes: themes.to_vec(),
    };
    let facts = EndpointFacts {
        project: "banskabystrica".into(),
        name: name.into(),
        slug: slug.into(),
        space: "doprava".into(),
        audience,
        types: types.into_iter().collect(),
        domains: Default::default(),
        themes: themes.iter().copied().collect(),
    };
    (page, facts)
}

/// The app, the gateway and the realm it talks to, and the database to drop at the end.
async fn app(test: &str) -> (axum::Router, MockServer, MockServer, Database) {
    let (admin, pool, name) = db::database(test).await;
    let database = (admin, pool.clone(), name);
    let mut tx = assistant::project_scope(&pool, "banskabystrica")
        .await
        .expect("scope");
    for (source, visibility) in [("web", "public"), ("spisy", "internal")] {
        let site: i64 = sqlx::query_scalar("INSERT INTO sites (organization, project, source, visibility, last_crawl) VALUES ('bb', 'banskabystrica', $1, $2, '2026-10-18T03:00:00Z') RETURNING id")
            .bind(source).bind(visibility).fetch_one(&mut *tx).await.expect("site");
        for (n, status) in [(1, "fetched"), (2, "fetched"), (3, "failed")] {
            sqlx::query("INSERT INTO pages (site_id, project, url, depth, status) VALUES ($1, 'banskabystrica', $2, 0, $3)")
                .bind(site).bind(format!("https://www.{source}.banskabystrica.sk/{n}")).bind(status)
                .execute(&mut *tx).await.expect("page");
        }
        sqlx::query("INSERT INTO documents (site_id, project, url, status) VALUES ($1, 'banskabystrica', $2, 'fetched')")
            .bind(site).bind(format!("https://www.{source}.banskabystrica.sk/a.pdf"))
            .execute(&mut *tx).await.expect("document");
    }
    tx.commit().await.expect("commit");

    // The gateway counts the parking Endpoint's one type as an anonymous caller.
    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/endpoint/{PARKING}/ngsi-ld/v1/entities")))
        .and(query_param("type", "OffStreetParking"))
        .and(query_param("count", "true"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("NGSILD-Results-Count", "41")
                .set_body_json(json!([])),
        )
        .mount(&gateway)
        .await;
    let realm = MockServer::start().await;

    let mut snapshot = Snapshot {
        sources: vec![source("web", "public"), source("spisy", "internal")],
        deployments: vec![
            deployment("doprava", json!({})),
            deployment(
                "interne",
                json!({"channel": "internal", "rateLimit": null, "budget": null}),
            ),
            deployment("vypnuty", json!({"enabled": false})),
        ],
        ..Snapshot::default()
    };
    for (name, slug, audience, themes) in [
        ("parking", PARKING, Audience::Public, vec![DataTheme::Tran]),
        ("bikes", BIKES, Audience::Public, vec![]),
        (
            "staff",
            STAFF,
            Audience::Organization,
            vec![DataTheme::Tran],
        ),
        ("other", OTHER, Audience::Public, vec![DataTheme::Envi]),
    ] {
        let (page, facts) = endpoint(name, slug, audience, &themes);
        snapshot.endpoints.insert(
            ("banskabystrica".into(), name.into()),
            EndpointRef {
                slug: slug.into(),
                audience: page.page.audience.clone(),
                title: None,
                space: String::new(),
            },
        );
        snapshot
            .facts
            .insert(("banskabystrica".into(), name.into()), facts);
        snapshot.pages.insert(slug.into(), page);
    }

    let http = reqwest::Client::new();
    let state = Arc::new(ChatState {
        pool,
        embedder: Embedder::load(&model::model_dir(), 1).expect("the pinned model loads"),
        model: Model::new(
            ModelConfig {
                proxy: "http://127.0.0.1:9".into(),
                token_url: format!("{}/token", realm.uri()),
                client_id: "jc-assistant".into(),
                credential: jc_core::ClientCredential::Secret(jc_core::Secret::new("s3cret")),
                model: "test/model".into(),
            },
            http.clone(),
        ),
        http,
        gateway: gateway.uri(),
        functions: None,
        portal_client: "portal-api".into(),
        public_origin: Some("https://assistant.example".into()),
        snapshot: RwLock::new(Arc::new(snapshot)),
        limits: tokio::sync::Mutex::default(),
        questions: Arc::new(tokio::sync::Semaphore::new(assistant::chat::MAX_QUESTIONS)),
        answers: Default::default(),
        listings: Default::default(),
        endpoint_base: Some(url::Url::parse("https://api.example.sk/").expect("a url")),
        viz: None,
        organization: "banskabystrica".into(),
        contexts: Default::default(),
    });
    assistant::chat::context::count(Arc::clone(&state)).await;
    (router(state), gateway, realm, database)
}

type Database = (sqlx::PgPool, sqlx::PgPool, String);

async fn drop((admin, pool, name): Database) {
    db::drop_database(admin, pool, &name).await;
}

async fn get(
    app: &axum::Router,
    uri: &str,
    origin: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut request = Request::builder().uri(uri);
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("an answer");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn the_context_lists_the_resolved_endpoints_and_the_public_sources() {
    let (app, _gateway, _realm, database) = app("context_lists").await;
    let (status, headers, body) =
        get(&app, "/api/v1/d/bb-doprava/context?lang=en", Some(ORIGIN)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some(ORIGIN)
    );
    let context: Value = serde_json::from_str(&body).expect("JSON");
    let slugs: Vec<&str> = context["endpoints"]
        .as_array()
        .expect("endpoints")
        .iter()
        .filter_map(|e| e["slug"].as_str())
        .collect();
    assert_eq!(
        slugs,
        [BIKES, PARKING],
        "the connector and the selector's TRAN Endpoint, by slug"
    );
    let parking = &context["endpoints"][1];
    assert_eq!(parking["entities"], 41);
    assert_eq!(parking["types"], json!(["OffStreetParking"]));
    assert_eq!(parking["themes"], json!(["TRAN"]));
    assert_eq!(parking["publisher"], "banskabystrica");
    assert_eq!(
        context["endpoints"][0]["entities"],
        Value::Null,
        "no type, no count"
    );
    assert_eq!(
        context["categories"],
        json!([
            {"code": "ENVI", "label": DataTheme::Envi.label_in("en")},
            {"code": "TRAN", "label": DataTheme::Tran.label_in("en")}
        ]),
        "the table's order, each once"
    );
    assert_eq!(context["title"]["en"], "Transport in BB");
    assert_eq!(context["limits"]["perClientPerHour"], 40);
    let sources = context["sources"].as_array().expect("sources");
    assert_eq!(sources.len(), 1, "the internal source never: {sources:?}");
    assert_eq!(sources[0]["name"], "web");
    assert_eq!(sources[0]["pages"], 2, "fetched pages alone");
    assert_eq!(sources[0]["documents"], 1);
    assert_eq!(sources[0]["crawled"], "2026-10-18T03:00:00Z");
    assert_eq!(sources[0]["sites"], json!(["web.banskabystrica.sk"]));
    for secret in [
        "TAJNY-PROMPT",
        "2.5",
        "costPerDayUsd",
        "spisy",
        STAFF,
        OTHER,
    ] {
        assert!(!body.contains(secret), "{secret} in {body}");
    }

    let (_, headers, _) = get(
        &app,
        "/api/v1/d/bb-doprava/context",
        Some("https://evil.example"),
    )
    .await;
    assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    drop(database).await;
}

#[tokio::test]
async fn the_page_is_escaped_holds_nothing_internal_and_comes_from_the_minute_s_document() {
    let (app, gateway, _realm, database) = app("context_page").await;
    let (status, headers, page) = get(&app, "/d/bb-doprava/context", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    let policy = headers
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        policy.contains("default-src 'none'") && policy.contains("frame-ancestors 'none'"),
        "{policy}"
    );
    assert!(page.contains("<html lang=\"sk\">"));
    assert!(page.contains("Doprava &lt;b&gt;BB&lt;/b&gt;"), "{page}");
    assert!(
        !page.contains("<script>") && !page.contains("<b>"),
        "{page}"
    );
    for internal in ["staff", "spisy", "TAJNY-PROMPT", "other"] {
        assert!(!page.contains(internal), "{internal} in {page}");
    }
    assert!(
        page.contains("<a href=\"https://data.example/dataset/parking\">"),
        "{page}"
    );

    let counted = gateway.received_requests().await.unwrap_or_default().len();
    let started = Instant::now();
    let (status, _, again) = get(&app, "/d/bb-doprava/context", None).await;
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
    assert_eq!((status, again), (StatusCode::OK, page));
    assert_eq!(
        gateway.received_requests().await.unwrap_or_default().len(),
        counted,
        "the minute's document asks the gateway nothing"
    );
    drop(database).await;
}

#[tokio::test]
async fn an_internal_disabled_or_unknown_deployment_has_no_public_context() {
    let (app, _gateway, _realm, database) = app("context_missing").await;
    for uri in [
        "/api/v1/d/bb-interne/context",
        "/api/v1/d/bb-vypnuty/context",
        "/api/v1/d/nikto/context",
        "/d/bb-interne/context",
    ] {
        let (status, _, body) = get(&app, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
    }
    drop(database).await;
}

/// T-4694: a copy of the widget on another host reads the page's configuration from the chat's
/// host: the deployment's own origins only, with the MCP connector a visitor adds to their own
/// assistant, and nothing a visitor should not read.
#[tokio::test]
async fn the_configuration_answers_the_deployment_s_origins_with_its_connector() {
    let (app, _gateway, _realm, database) = app("context_config").await;
    let (status, headers, body) = get(&app, "/api/v1/d/bb-doprava/config", Some(ORIGIN)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], ORIGIN);
    let config: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(config["publicId"], "bb-doprava");
    assert_eq!(config["mcp"], "https://api.example.sk/api/mcp/d/bb-doprava");
    assert_eq!(config["frameOrigins"], json!([ORIGIN]));
    assert!(
        !body.contains("systemPrompt") && !body.contains("staff"),
        "{body}"
    );

    let (status, headers, _) = get(
        &app,
        "/api/v1/d/bb-doprava/config",
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "another site's page is refused"
    );
    assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    let (status, _, _) = get(&app, "/api/v1/d/nikto/config", Some(ORIGIN)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    drop(database).await;
}
