//! The Portal's administration paths of jc-assistant (T-3057, AG-113, API/05 §3): only the
//! Portal's service account, the page tree level by level, an administrator's exclusion that
//! removes passages and outlives the next crawl, and a recrawl queued once.

#[path = "common/db.rs"]
mod db;
#[path = "common/model.rs"]
mod model;

use std::sync::{Arc, RwLock};

use assistant::chat::model::{Model, ModelConfig};
use assistant::chat::{router, ChatState};
use assistant::crawl::Sink;
use assistant::embed::Embedder;
use assistant::extract::Indexer;
use assistant::worker::{Snapshot, Source};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use jc_core::kinds::assistant::{KnowledgeSourceSpec, PdfPolicy, SourceType, Visibility};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PORTAL_TOKEN: &str = "the-portals-token";
const BASE: &str = "/internal/v1/projects/hronov/knowledge";

struct World {
    admin: PgPool,
    pool: PgPool,
    name: String,
    app: axum::Router,
    /// Held so the realm answers for as long as the test runs.
    _realm: MockServer,
    root: i64,
    child: i64,
    grandchild: i64,
    document: i64,
}

fn source_spec() -> KnowledgeSourceSpec {
    KnowledgeSourceSpec {
        source: SourceType::Website,
        start_urls: vec!["https://hronov.example/".into()],
        ckan_instance_ref: None,
        context_spaces: Vec::new(),
        sitemap: true,
        include: vec![],
        exclude: vec![],
        max_depth: 3,
        max_pages: 100,
        pdf: PdfPolicy::default(),
        off_domain_documents: false,
        schedule: None,
        languages: vec!["sk".into()],
        visibility: Visibility::Public,
    }
}

async fn world(test: &str) -> World {
    let (admin, pool, name) = db::database(test).await;
    // A tree of three pages, a document under the child, a passage on each, all of `web`.
    let mut tx = assistant::project_scope(&pool, "hronov")
        .await
        .expect("scope");
    let site: i64 = sqlx::query_scalar("INSERT INTO sites (organization, project, source, visibility, last_crawl) VALUES ('hronov.example', 'hronov', 'web', 'public', '2026-10-06T03:00:00Z') RETURNING id")
        .fetch_one(&mut *tx).await.expect("site");
    let page = |url: &'static str, parent: Option<i64>, depth: i16| {
        sqlx::query_scalar::<_, i64>("INSERT INTO pages (site_id, project, url, parent_id, depth, status, content_hash, etag) VALUES ($1, 'hronov', $2, $3, $4, 'fetched', 'h', 'e') RETURNING id")
            .bind(site).bind(url).bind(parent).bind(depth)
    };
    let root = page("https://hronov.example/", None, 0)
        .fetch_one(&mut *tx)
        .await
        .expect("root");
    let child = page("https://hronov.example/odpad", Some(root), 1)
        .fetch_one(&mut *tx)
        .await
        .expect("child");
    let grandchild = page("https://hronov.example/odpad/zber", Some(child), 2)
        .fetch_one(&mut *tx)
        .await
        .expect("grandchild");
    let document: i64 = sqlx::query_scalar("INSERT INTO documents (site_id, project, page_id, url, mime, bytes, pages, status, content_hash) VALUES ($1, 'hronov', $2, 'https://hronov.example/vzn.pdf', 'application/pdf', 1000, 3, 'fetched', 'h') RETURNING id")
        .bind(site).bind(child).fetch_one(&mut *tx).await.expect("document");
    sqlx::query("INSERT INTO links (from_page, project, to_url, kind) VALUES ($1, 'hronov', 'https://hronov.example/odpad', 'page'), ($1, 'hronov', 'https://hronov.example/vzn.pdf', 'document')")
        .bind(root).execute(&mut *tx).await.expect("links");
    for (owner_page, owner_document, text) in [
        (Some(root), None, "Vitajte v meste."),
        (Some(child), None, "Odpad vyvážame v utorok."),
        (Some(grandchild), None, "Zber je ráno."),
        (None, Some(document), "Všeobecne záväzné nariadenie."),
    ] {
        sqlx::query("INSERT INTO chunks (site_id, project, page_id, document_id, ordinal, url, text, lang, visibility) VALUES ($1, 'hronov', $2, $3, 0, 'u', $4, 'sk', 'public')")
            .bind(site).bind(owner_page).bind(owner_document).bind(text).execute(&mut *tx).await.expect("chunk");
    }
    sqlx::query("INSERT INTO usage (project, deployment, day, requests, tokens_in, tokens_out) VALUES ('hronov', 'obcania', current_date, 3, 900, 100)")
        .execute(&mut *tx).await.expect("usage");
    tx.commit().await.expect("commit");

    let realm = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token/introspect")).and(body_string_contains(format!("token={PORTAL_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"active": true, "azp": "portal-api", "username": "service-account-portal-api", "aud": ["jc-assistant"]})))
        .mount(&realm).await;
    Mock::given(method("POST"))
        .and(path("/token/introspect"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"active": false})))
        .with_priority(10)
        .mount(&realm)
        .await;
    let realm_uri = realm.uri();

    let http = reqwest::Client::new();
    let snapshot = Snapshot {
        sources: vec![Source {
            project: "hronov".into(),
            name: "web".into(),
            spec: source_spec(),
            ckan_url: None,
            catalogue: Vec::new(),
        }],
        ..Snapshot::default()
    };
    let state = Arc::new(ChatState {
        pool: pool.clone(),
        embedder: Embedder::load(&model::model_dir(), 1).expect("the pinned model loads"),
        model: Model::new(
            ModelConfig {
                proxy: "http://127.0.0.1:9".into(),
                token_url: format!("{realm_uri}/token"),
                client_id: "jc-assistant".into(),
                client_secret: "s3cret".into(),
                model: "test/model".into(),
            },
            http.clone(),
        ),
        http,
        gateway: "http://127.0.0.1:9".into(),
        functions: None,
        portal_client: "portal-api".into(),
        public_origin: Some("https://assistant.example".into()),
        snapshot: RwLock::new(Arc::new(snapshot)),
        limits: tokio::sync::Mutex::default(),
        questions: std::sync::Arc::new(tokio::sync::Semaphore::new(assistant::chat::MAX_QUESTIONS)),
    });
    World {
        admin,
        pool,
        name,
        app: router(state),
        _realm: realm,
        root,
        child,
        grandchild,
        document,
    }
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .expect("a request");
    let response = app.clone().oneshot(request).await.expect("an answer");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn passages_left(pool: &PgPool) -> i64 {
    let mut tx = assistant::project_scope(pool, "hronov")
        .await
        .expect("scope");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    tx.rollback().await.expect("rollback");
    n
}

/// AG-113: nobody but the Portal's service account reads or steers anything.
#[tokio::test]
async fn only_the_portals_service_account_is_answered() {
    let w = world("adminauth").await;
    for token in [None, Some("somebody-else"), Some("")] {
        for (method, uri) in [
            ("GET", format!("{BASE}/sources")),
            ("GET", format!("{BASE}/sources/web/pages")),
            ("POST", format!("{BASE}/sources/web/recrawl")),
        ] {
            let (status, _) = call(&w.app, method, &uri, token, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} {token:?}");
        }
    }
    let (status, _) = call(
        &w.app,
        "POST",
        &format!("{BASE}/sources/web/inclusion"),
        Some("somebody-else"),
        Some(json!({"pages": [w.root], "included": false})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        passages_left(&w.pool).await,
        4,
        "a refused call changed nothing"
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// The inventory: the sources, the tree a level at a time, the documents, a page's links, a
/// page's passages, and the usage of a deployment.
#[tokio::test]
async fn the_inventory_reads_level_by_level() {
    let w = world("admininventory").await;
    let t = Some(PORTAL_TOKEN);
    let (status, sources) = call(&w.app, "GET", &format!("{BASE}/sources"), t, None).await;
    assert_eq!(status, StatusCode::OK);
    let web = &sources["items"][0];
    assert_eq!(
        (
            web["source"].clone(),
            web["pages"].clone(),
            web["documents"].clone(),
            web["passages"].clone()
        ),
        (json!("web"), json!(3), json!(1), json!(4))
    );
    assert_eq!(web["lastCrawl"], "2026-10-06T03:00:00Z");

    let (_, roots) = call(&w.app, "GET", &format!("{BASE}/sources/web/pages"), t, None).await;
    let roots = roots["items"].as_array().expect("items").clone();
    assert_eq!(roots.len(), 1);
    assert_eq!(
        (
            roots[0]["id"].clone(),
            roots[0]["children"].clone(),
            roots[0]["included"].clone()
        ),
        (json!(w.root), json!(1), json!(true))
    );
    let (_, level) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/web/pages?parent={}", w.child),
        t,
        None,
    )
    .await;
    assert_eq!(level["items"][0]["id"], json!(w.grandchild));
    assert_eq!(level["items"][0]["depth"], 2);

    let (_, documents) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/web/documents"),
        t,
        None,
    )
    .await;
    assert_eq!(documents["items"][0]["pageId"], json!(w.child));
    assert_eq!(documents["items"][0]["pages"], 3);
    let (_, links) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/web/pages/{}/links", w.root),
        t,
        None,
    )
    .await;
    assert_eq!(links["items"].as_array().map(Vec::len), Some(2));
    let (_, passages) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/web/passages?page={}", w.child),
        t,
        None,
    )
    .await;
    assert_eq!(passages["items"][0]["text"], "Odpad vyvážame v utorok.");
    let (_, of_document) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/web/passages?document={}", w.document),
        t,
        None,
    )
    .await;
    assert_eq!(
        of_document["items"][0]["text"],
        "Všeobecne záväzné nariadenie."
    );
    let (status, _) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/web/passages?page=1&document=2"),
        t,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, usage) = call(
        &w.app,
        "GET",
        "/internal/v1/projects/hronov/knowledge/deployments/obcania/usage",
        t,
        None,
    )
    .await;
    assert_eq!(usage["items"][0]["requests"], 3);

    // Another project, an unknown source and a page of no source of this project are not found.
    for uri in [
        "/internal/v1/projects/lipno/knowledge/sources/web/pages".to_owned(),
        format!("{BASE}/sources/nikde/pages"),
        format!("{BASE}/sources/web/pages/999999/links"),
    ] {
        let (status, _) = call(&w.app, "GET", &uri, t, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-113: excluding a subtree removes the passages of its pages and their documents at once,
/// says an administrator did it, and the indexer leaves those pages alone at the next crawl;
/// including again clears their validators so that crawl indexes them.
#[tokio::test]
async fn an_administrators_exclusion_removes_passages_and_outlives_the_next_crawl() {
    let w = world("adminexclude").await;
    let t = Some(PORTAL_TOKEN);
    let (status, counts) = call(
        &w.app,
        "POST",
        &format!("{BASE}/sources/web/inclusion"),
        t,
        Some(json!({"pages": [w.child], "subtree": true, "included": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{counts}");
    assert_eq!(
        counts,
        json!({"pages": 2, "documents": 1, "passagesRemoved": 3})
    );
    assert_eq!(
        passages_left(&w.pool).await,
        1,
        "only the root's passage is left"
    );
    let (_, level) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/web/pages?parent={}", w.root),
        t,
        None,
    )
    .await;
    assert_eq!(level["items"][0]["excludedBy"], "administrator");
    assert_eq!(level["items"][0]["included"], false);

    // The next crawl hands the page to the indexer again: it is not indexed.
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let site: i64 = sqlx::query_scalar("SELECT id FROM sites")
        .fetch_one(&mut *tx)
        .await
        .expect("site");
    tx.rollback().await.expect("rollback");
    let mut indexer = Indexer::new(w.pool.clone(), "hronov", site, "public", 10);
    indexer.page(w.child, "https://hronov.example/odpad", Some("sk"), "<html><body><p>Odpad vyvážame v utorok a vo štvrtok, nádoby vyložte do šiestej.</p></body></html>").await;
    assert_eq!(indexer.passages, 0);
    assert_eq!(passages_left(&w.pool).await, 1);

    let (_, counts) = call(
        &w.app,
        "POST",
        &format!("{BASE}/sources/web/inclusion"),
        t,
        Some(json!({"pages": [w.child], "subtree": false, "included": true})),
    )
    .await;
    assert_eq!(counts["pages"], 1);
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let (hash, etag, excluded): (Option<String>, Option<String>, bool) =
        sqlx::query_as("SELECT content_hash, etag, excluded_by_admin FROM pages WHERE id = $1")
            .bind(w.child)
            .fetch_one(&mut *tx)
            .await
            .expect("page");
    tx.rollback().await.expect("rollback");
    assert_eq!(
        (hash, etag, excluded),
        (None, None, false),
        "the next crawl fetches and indexes it"
    );
    indexer.page(w.child, "https://hronov.example/odpad", Some("sk"), "<html><body><p>Odpad vyvážame v utorok a vo štvrtok, nádoby vyložte do šiestej.</p></body></html>").await;
    assert!(indexer.passages >= 1);

    for body in [
        json!({"included": false}),
        json!({"pages": vec![1; 501], "included": false}),
        json!({"pages": [1], "included": false, "extra": 1}),
    ] {
        let (status, _) = call(
            &w.app,
            "POST",
            &format!("{BASE}/sources/web/inclusion"),
            t,
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// A recrawl is queued once; a second while it waits is a conflict, and a source the manifests
/// do not declare is not found.
#[tokio::test]
async fn a_recrawl_is_queued_once() {
    let w = world("adminrecrawl").await;
    let t = Some(PORTAL_TOKEN);
    let (status, job) = call(
        &w.app,
        "POST",
        &format!("{BASE}/sources/web/recrawl"),
        t,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert!(job["job"].as_i64().is_some());
    let (status, _) = call(
        &w.app,
        "POST",
        &format!("{BASE}/sources/web/recrawl"),
        t,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &w.app,
        "POST",
        &format!("{BASE}/sources/nikde/recrawl"),
        t,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-113: ids of another source of the project are not this source's to change, and including
/// what was never excluded changes nothing it should not.
#[tokio::test]
async fn an_inclusion_touches_this_sources_items_alone() {
    let w = world("adminforeign").await;
    let t = Some(PORTAL_TOKEN);
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let other_site: i64 = sqlx::query_scalar("INSERT INTO sites (organization, project, source, visibility) VALUES ('hronov.example', 'hronov', 'other', 'public') RETURNING id")
        .fetch_one(&mut *tx).await.expect("site");
    let foreign: i64 = sqlx::query_scalar("INSERT INTO pages (site_id, project, url, depth, status) VALUES ($1, 'hronov', 'https://other.example/', 0, 'fetched') RETURNING id")
        .bind(other_site).fetch_one(&mut *tx).await.expect("page");
    tx.commit().await.expect("commit");

    let (status, counts) = call(
        &w.app,
        "POST",
        &format!("{BASE}/sources/web/inclusion"),
        t,
        Some(json!({"pages": [foreign], "included": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        counts,
        json!({"pages": 0, "documents": 0, "passagesRemoved": 0})
    );
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let excluded: bool = sqlx::query_scalar("SELECT excluded_by_admin FROM pages WHERE id = $1")
        .bind(foreign)
        .fetch_one(&mut *tx)
        .await
        .expect("page");
    tx.rollback().await.expect("rollback");
    assert!(!excluded);
    assert_eq!(passages_left(&w.pool).await, 4);
    db::drop_database(w.admin, w.pool, &w.name).await;
}
