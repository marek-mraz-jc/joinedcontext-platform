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
use jc_core::kinds::assistant::{
    KnowledgeSourceSpec, PdfPolicy, SourceType, UploadLimits, Visibility,
};
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
        organizations: Vec::new(),
        upload: None,
        sitemap: true,
        wordpress_types: vec![],
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
    // T-4174: two verdicts up, one down with a note, one down without.
    sqlx::query("INSERT INTO feedback (project, deployment, conversation, turn, verdict, note) VALUES \
        ('hronov', 'obcania', gen_random_uuid(), 1, 'up', NULL), ('hronov', 'obcania', gen_random_uuid(), 1, 'up', NULL), \
        ('hronov', 'obcania', gen_random_uuid(), 2, 'down', 'Chýbali otváracie hodiny.'), ('hronov', 'obcania', gen_random_uuid(), 1, 'down', NULL)")
        .execute(&mut *tx).await.expect("feedback");
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
        sources: vec![
            Source {
                project: "hronov".into(),
                name: "web".into(),
                spec: source_spec(),
                ckan_url: None,
                catalogue: Vec::new(),
            },
            // T-4184: the files an administrator uploads, at most two of 64 KiB.
            Source {
                project: "hronov".into(),
                name: "subory".into(),
                spec: KnowledgeSourceSpec {
                    source: SourceType::Upload,
                    start_urls: Vec::new(),
                    upload: Some(UploadLimits {
                        max_bytes: 64 * 1024,
                        max_files: 2,
                    }),
                    ..source_spec()
                },
                ckan_url: None,
                catalogue: Vec::new(),
            },
        ],
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
                credential: jc_core::ClientCredential::Secret(jc_core::Secret::new("s3cret")),
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
        answers: Default::default(),
        endpoint_base: None,
        organization: "hronov".into(),
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

/// A token the Portal's own client holds for another audience (the gateway's): an
/// administrator of the assistant only by name.
const OTHER_AUDIENCE_TOKEN: &str = "gateway-audience-token";

/// Every `/internal` route of the router with its method, from the source that declares it, so
/// a route added without a row in the sweep below turns the sweep red.
fn declared_internal_routes() -> Vec<(String, String)> {
    let mut found = Vec::new();
    for source in [
        include_str!("../src/chat/admin.rs"),
        include_str!("../src/chat/mod.rs"),
    ] {
        for (at, _) in source.match_indices("\"/internal/") {
            let rest = &source[at + 1..];
            let route = &rest[..rest.find('"').expect("a closed literal")];
            let method = ["get(", "post(", "put(", "patch(", "delete("]
                .into_iter()
                .filter_map(|m| rest.find(m).map(|i| (i, m)))
                .min()
                .expect("a method after the path")
                .1;
            found.push((
                method.trim_end_matches('(').to_uppercase(),
                route.to_owned(),
            ));
        }
    }
    found.sort();
    found
}

/// AG-113: every `/internal` route refuses all but the Portal's service account, with each
/// method; a refused call changes nothing.
#[tokio::test]
async fn only_the_portals_service_account_is_answered() {
    let w = world("adminauth").await;
    Mock::given(method("POST"))
        .and(path("/token/introspect"))
        .and(body_string_contains(format!("token={OTHER_AUDIENCE_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"active": true, "azp": "portal-api", "username": "service-account-portal-api", "aud": ["context-gateway"]})))
        .mount(&w._realm)
        .await;
    let k = "/internal/v1/projects/{project}/knowledge";
    // (method, declared route, called uri, body)
    let sweep: Vec<(&str, String, String, Option<Value>)> = vec![
        (
            "GET",
            format!("{k}/sources"),
            format!("{BASE}/sources"),
            None,
        ),
        (
            "GET",
            format!("{k}/sources/{{source}}/pages"),
            format!("{BASE}/sources/web/pages?parent={}", w.root),
            None,
        ),
        (
            "GET",
            format!("{k}/sources/{{source}}/documents"),
            format!("{BASE}/sources/web/documents"),
            None,
        ),
        (
            "GET",
            format!("{k}/sources/{{source}}/pages/{{page}}/links"),
            format!("{BASE}/sources/web/pages/{}/links", w.root),
            None,
        ),
        (
            "GET",
            format!("{k}/sources/{{source}}/errors"),
            format!("{BASE}/sources/web/errors"),
            None,
        ),
        (
            "GET",
            format!("{k}/sources/{{source}}/passages"),
            format!("{BASE}/sources/web/passages?page={}", w.child),
            None,
        ),
        (
            "POST",
            format!("{k}/sources/{{source}}/inclusion"),
            format!("{BASE}/sources/web/inclusion"),
            Some(json!({"pages": [w.root], "included": false})),
        ),
        (
            "POST",
            format!("{k}/sources/{{source}}/recrawl"),
            format!("{BASE}/sources/web/recrawl"),
            None,
        ),
        (
            "GET",
            format!("{k}/sources/{{source}}/files"),
            format!("{BASE}/sources/subory/files"),
            None,
        ),
        (
            "POST",
            format!("{k}/sources/{{source}}/files"),
            format!("{BASE}/sources/subory/files?name=a.md"),
            Some(json!("# A")),
        ),
        (
            "DELETE",
            format!("{k}/sources/{{source}}/files/{{id}}"),
            format!("{BASE}/sources/subory/files/1"),
            None,
        ),
        (
            "GET",
            format!("{k}/deployments/{{deployment}}/usage"),
            format!("{BASE}/deployments/obcania/usage"),
            None,
        ),
        (
            "GET",
            format!("{k}/conversations"),
            format!("{BASE}/conversations"),
            None,
        ),
        (
            "GET",
            format!("{k}/conversations/{{conversation}}"),
            format!("{BASE}/conversations/00000000-0000-4000-8000-000000000000"),
            None,
        ),
        (
            "DELETE",
            format!("{k}/conversations/{{conversation}}"),
            format!("{BASE}/conversations/00000000-0000-4000-8000-000000000000"),
            None,
        ),
        (
            "POST",
            format!("{k}/deployments/{{deployment}}/chat"),
            format!("{BASE}/deployments/obcania/chat"),
            Some(json!({"message": "Kedy je zber?"})),
        ),
    ];
    let mut swept: Vec<(String, String)> = sweep
        .iter()
        .map(|(m, route, _, _)| ((*m).to_owned(), route.clone()))
        .collect();
    swept.sort();
    assert_eq!(
        swept,
        declared_internal_routes(),
        "every /internal route is in the sweep"
    );

    for token in [
        None,
        Some("somebody-else"),
        Some(""),
        Some(OTHER_AUDIENCE_TOKEN),
    ] {
        for (method, _, uri, body) in &sweep {
            let (status, _) = call(&w.app, method, uri, token, body.clone()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} {token:?}");
        }
    }
    assert_eq!(
        passages_left(&w.pool).await,
        4,
        "a refused call changed nothing"
    );

    for (method, _, uri, body) in &sweep {
        let (status, _) = call(&w.app, method, uri, Some(PORTAL_TOKEN), body.clone()).await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} answers the Portal"
        );
    }
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
    assert_eq!(
        (
            &usage["items"][0]["thumbsUp"],
            &usage["items"][0]["thumbsDown"],
            &usage["items"][0]["notes"]
        ),
        (&json!(2), &json!(2), &json!(1)),
        "{usage}"
    );

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

/// One stored (uncompressed) ZIP entry per file, with its CRC-32, as an Office file can be.
fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let (mut out, mut directory) = (Vec::new(), Vec::new());
    for (name, data) in entries {
        let offset = u32::try_from(out.len()).expect("small");
        let (crc, size) = (crc32(data), u32::try_from(data.len()).expect("small"));
        let name_len = u16::try_from(name.len()).expect("short");
        let common = |out: &mut Vec<u8>| {
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&name_len.to_le_bytes());
            out.extend_from_slice(&[0, 0]);
        };
        out.extend_from_slice(b"PK\x03\x04");
        common(&mut out);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        directory.extend_from_slice(b"PK\x01\x02\x14\x00");
        common(&mut directory);
        // Comment length, disk, internal and external attributes.
        directory.extend_from_slice(&[0; 10]);
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
    }
    let (start, count) = (
        u32::try_from(out.len()).expect("small"),
        u16::try_from(entries.len()).expect("few"),
    );
    out.extend_from_slice(&directory);
    out.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00");
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&u32::try_from(directory.len()).expect("small").to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

async fn send_file(
    app: &axum::Router,
    source: &str,
    name: &str,
    bytes: Vec<u8>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("{BASE}/sources/{source}/files?name={name}"))
        .header("authorization", format!("Bearer {PORTAL_TOKEN}"))
        // The type is read from the content; what the caller says is not.
        .header("content-type", "application/pdf")
        .body(Body::from(bytes))
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

async fn passages_of(pool: &PgPool, url: &str) -> Vec<(String, String)> {
    let mut tx = assistant::project_scope(pool, "hronov")
        .await
        .expect("scope");
    let rows =
        sqlx::query_as("SELECT text, visibility FROM chunks WHERE url = $1 OR url LIKE $1 || '#%' ORDER BY ordinal")
            .bind(url)
            .fetch_all(&mut *tx)
            .await
            .expect("chunks");
    tx.rollback().await.expect("rollback");
    rows
}

/// T-4184, MF-51, API/05 §3: a Markdown file and a DOCX are typed by their content (whatever
/// the caller names), indexed with the source's visibility, listed, replaced by name and
/// removed with their passages; a binary file, a body past `maxBytes`, a file past `maxFiles`,
/// a name that is a path, and a source that takes no files are each refused.
#[tokio::test]
async fn uploaded_files_are_typed_indexed_listed_replaced_and_removed() {
    let w = world("adminupload").await;
    let guide = "# Parkovacia karta\n\nParkovaciu kartu vybavíte na Námestí SNP 1 v pondelok.\n";
    let (status, md) =
        send_file(&w.app, "subory", "N%C3%A1vod.md", guide.as_bytes().to_vec()).await;
    assert_eq!(status, StatusCode::CREATED, "{md}");
    assert_eq!(
        (md["name"].clone(), md["mime"].clone()),
        (json!("Návod.md"), json!("text/markdown"))
    );
    assert!(md["passages"].as_i64().unwrap_or(0) >= 1, "{md}");
    let passages = passages_of(&w.pool, "upload:subory/Návod.md").await;
    assert!(
        passages
            .iter()
            .any(|(t, v)| t.contains("Námestí SNP 1") && v == "public"),
        "{passages:?}"
    );

    let document = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Zber odpadu je v utorok.</w:t></w:r></w:p></w:body></w:document>"#;
    let types = br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    let docx = stored_zip(&[
        ("[Content_Types].xml", types),
        ("word/document.xml", document),
    ]);
    let (status, word) = send_file(&w.app, "subory", "zber.txt", docx).await;
    assert_eq!(status, StatusCode::CREATED, "{word}");
    assert_eq!(word["mime"], assistant::upload::DOCX);
    let passages = passages_of(&w.pool, "upload:subory/zber.txt").await;
    assert!(
        passages.iter().any(|(t, _)| t.contains("Zber odpadu")),
        "{word} {passages:?}"
    );

    let (status, listed) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/subory/files"),
        Some(PORTAL_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = listed["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(names, ["Návod.md", "zber.txt"]);

    // Full: a third file is refused, the same name again replaces its file.
    let (status, _) = send_file(&w.app, "subory", "tretí.md", b"# Tri".to_vec()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, again) = send_file(
        &w.app,
        "subory",
        "N%C3%A1vod.md",
        b"# Parkovanie\n\nKarta sa vybavuje online.\n".to_vec(),
    )
    .await;
    assert_eq!(
        (status, again["id"].clone()),
        (StatusCode::CREATED, md["id"].clone())
    );
    let passages = passages_of(&w.pool, "upload:subory/Návod.md").await;
    assert!(
        passages.iter().all(|(t, _)| !t.contains("Námestí SNP 1")),
        "{passages:?}"
    );

    for (source, name, bytes, expected) in [
        (
            "subory",
            "obrazok.png",
            b"\x89PNG\r\n\x1a\n\x00\x00".to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "subory",
            "velky.md",
            vec![b'a'; 64 * 1024 + 1],
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "subory",
            "..%2Fetc%2Fpasswd",
            b"# x".to_vec(),
            StatusCode::BAD_REQUEST,
        ),
        ("subory", "", b"# x".to_vec(), StatusCode::BAD_REQUEST),
        ("web", "a.md", b"# x".to_vec(), StatusCode::CONFLICT),
        ("nikde", "a.md", b"# x".to_vec(), StatusCode::NOT_FOUND),
    ] {
        let (status, body) = send_file(&w.app, source, name, bytes).await;
        assert_eq!(status, expected, "{source} {name}: {body}");
    }

    let id = md["id"].as_i64().expect("an id");
    let gone = format!("{BASE}/sources/subory/files/{id}");
    assert_eq!(
        call(&w.app, "DELETE", &gone, Some(PORTAL_TOKEN), None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert!(passages_of(&w.pool, "upload:subory/Návod.md")
        .await
        .is_empty());
    assert_eq!(
        call(&w.app, "DELETE", &gone, Some(PORTAL_TOKEN), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4197: the sources row counts passages embedded and pending (before and after one embed
/// round) and failed reads, names the last run's start and end, and the errors route lists
/// each failed page and file with why, newest first, never another source's.
#[tokio::test]
async fn the_index_status_counts_pending_and_failed_and_lists_why() {
    let w = world("adminstatus").await;
    let row = |listed: &Value| {
        listed["items"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["source"] == "web")
            .cloned()
            .unwrap_or(Value::Null)
    };
    let (status, listed) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources"),
        Some(PORTAL_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let web = row(&listed);
    assert_eq!(
        (
            web["embedded"].clone(),
            web["pending"].clone(),
            web["failed"].clone()
        ),
        (json!(0), json!(4), json!(0)),
        "{web}"
    );
    let (status, errors) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/web/errors"),
        Some(PORTAL_TOKEN),
        None,
    )
    .await;
    assert_eq!((status, errors), (StatusCode::OK, json!({"items": []})));

    // One page and one file fail; one with a reason, one recorded before reasons were kept.
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    sqlx::query("UPDATE pages SET status = 'failed', error = 'HTTP 404' WHERE id = $1")
        .bind(w.grandchild)
        .execute(&mut *tx)
        .await
        .expect("page");
    sqlx::query("UPDATE documents SET status = 'failed' WHERE id = $1")
        .bind(w.document)
        .execute(&mut *tx)
        .await
        .expect("document");
    tx.commit().await.expect("commit");
    assistant::crawl::queue::enqueue(&w.pool, "hronov", "web")
        .await
        .expect("enqueue");
    let job = assistant::crawl::queue::claim(&w.pool, "test")
        .await
        .expect("claim")
        .expect("a job");
    assistant::crawl::queue::finish(&w.pool, job.id)
        .await
        .expect("finish");
    let embedder = Embedder::load(&model::model_dir(), 1).expect("the pinned model loads");
    let embedded = assistant::embed::embed_missing(&w.pool, &embedder, "hronov", 2)
        .await
        .expect("embed");
    assert_eq!(embedded, 2);

    let (_, listed) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources"),
        Some(PORTAL_TOKEN),
        None,
    )
    .await;
    let web = row(&listed);
    assert_eq!(
        (
            web["embedded"].clone(),
            web["pending"].clone(),
            web["failed"].clone()
        ),
        (json!(2), json!(2), json!(2)),
        "{web}"
    );
    assert_eq!(web["job"]["state"], "done", "{web}");
    for at in ["startedAt", "finishedAt"] {
        assert!(
            web["job"][at].as_str().is_some_and(|t| t.ends_with('Z')),
            "{at}: {web}"
        );
    }
    let (status, errors) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/web/errors"),
        Some(PORTAL_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = errors["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2, "{errors}");
    assert!(items.contains(&json!({"url": "https://hronov.example/odpad/zber", "kind": "page", "reason": "HTTP 404"})), "{errors}");
    assert!(items.contains(&json!({"url": "https://hronov.example/vzn.pdf", "kind": "document", "reason": "not recorded"})), "{errors}");

    // The upload source has no failures, a source never crawled is not found.
    let (status, none) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/subory/errors"),
        Some(PORTAL_TOKEN),
        None,
    )
    .await;
    assert!(
        status == StatusCode::NOT_FOUND || none == json!({"items": []}),
        "{status} {none}"
    );
    let (status, _) = call(
        &w.app,
        "GET",
        &format!("{BASE}/sources/nikde/errors"),
        Some(PORTAL_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    db::drop_database(w.admin, w.pool, &w.name).await;
}
