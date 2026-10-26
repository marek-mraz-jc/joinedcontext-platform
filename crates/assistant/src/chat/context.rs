//! What an assistant knows (API/05 §1.9, AG-123, ADR-N-055 §2.6, T-4692): `GET
//! /api/v1/d/{publicId}/context` as JSON and `/d/{publicId}/context` as a page, built from the
//! snapshot's resolution of the deployment's Endpoints and from the store, with no model call.
//!
//! Only what a visitor of the channel may read: the resolved Endpoints (public ones alone on an
//! anonymous channel, by the resolver), their entity counts as an anonymous caller counts them,
//! and the `visibility: public` sources. Never the system prompt, the model, the budget in money
//! or a `secretRef`. An `internal`, unknown, disabled or doubly declared `publicId` is `404`.
//! A document is computed at most once a minute per deployment; the language only picks labels.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt;
use jc_core::kinds::assistant::{Localised, SourceType, Visibility};
use jc_core::kinds::catalog::DataTheme;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::ChatState;
use crate::worker::{Deployment, Snapshot};

/// How long one deployment's document is served before it is computed again.
pub const KEEP: Duration = Duration::from_secs(60);
/// The most documents held at once; past it the held ones are dropped and computed again.
const MAX_HELD: usize = 1_000;
/// How long one count through the gateway may take; a slower one is left out, never zero.
const COUNT_DEADLINE: Duration = Duration::from_secs(3);
/// Endpoints counted at once. A count unanswered after 2 s weighs 4 of the gateway's slots
/// (in_flight.rs SLOW_WEIGHT), so 4 slow counts hold half the caller's 32 and a question keeps room.
const COUNT_PARALLEL: usize = 4;

pub fn router(state: Arc<ChatState>) -> Router {
    // `/api/v1/d/{publicId}/context` is the chat router's, beside its preflight.
    Router::new()
        .route("/d/{public_id}/context", get(page))
        .route("/d/context.css", get(style))
        .with_state(state)
}

/// The documents computed in the last minute, by `publicId`, and the entities of each public
/// Endpoint a deployment reads, by slug, as the minute's [`count`] left them.
#[derive(Default)]
pub struct Held {
    documents: HashMap<String, (Instant, Arc<Context>)>,
    counts: HashMap<String, u64>,
}

/// One Endpoint as a visitor sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Shown {
    pub slug: String,
    pub title: Option<String>,
    /// The project that publishes it.
    pub publisher: String,
    pub themes: Vec<DataTheme>,
    pub types: Vec<String>,
    /// Its entities as an anonymous caller counts them; `None` when the count did not come back.
    pub entities: Option<u64>,
    /// When its newest entity changed; `None` until the broker states it per Endpoint.
    pub modified: Option<String>,
    /// Its CKAN dataset page, when it publishes one.
    pub catalogue: Option<String>,
}

/// One public knowledge source.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Read {
    pub name: String,
    pub kind: SourceType,
    /// The hosts it reads: its start pages' and its catalogue's.
    pub sites: Vec<String>,
    pub pages: i64,
    pub documents: i64,
    /// When it was last read, RFC 3339 in UTC; `None` before its first crawl.
    pub crawled: Option<String>,
}

/// What one deployment knows, in every language at once.
#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    pub public_id: String,
    pub title: BTreeMap<String, String>,
    pub languages: Vec<String>,
    pub categories: Vec<DataTheme>,
    pub limits: Value,
    pub endpoints: Vec<Shown>,
    pub sources: Vec<Read>,
    pub omitted: usize,
}

impl Context {
    /// The document of API/05 §1.9, the category labels in `lang`.
    pub fn json(&self, lang: &str) -> Value {
        json!({
            "publicId": self.public_id,
            "title": self.title,
            "languages": self.languages,
            "categories": self.categories.iter().map(|code| json!({"code": code, "label": code.label_in(lang)})).collect::<Vec<_>>(),
            "limits": self.limits,
            "endpoints": self.endpoints,
            "sources": self.sources,
            "omitted": self.omitted,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct Asked {
    lang: Option<String>,
}

/// The language of the answer: the asked one when the deployment answers in it, else its first.
fn language<'a>(deployment: &'a Deployment, asked: Option<&'a str>) -> &'a str {
    let languages = &deployment.spec.languages;
    asked
        .filter(|lang| languages.iter().any(|l| l == lang))
        .or_else(|| languages.first().map(String::as_str))
        .unwrap_or("sk")
}

/// The title per language: one string is the deployment's first language's.
fn titles(deployment: &Deployment) -> BTreeMap<String, String> {
    let first = deployment
        .spec
        .languages
        .first()
        .map_or("sk", String::as_str);
    deployment
        .spec
        .theme
        .as_ref()
        .and_then(|theme| theme.title.as_ref())
        .map(Localised::texts)
        .unwrap_or_default()
        .into_iter()
        .map(|(lang, text)| (lang.unwrap_or(first).to_owned(), text.to_owned()))
        .collect()
}

/// The visitor limits of MF-52; the money and the token budget are the operator's alone.
fn limits(deployment: &Deployment) -> Value {
    let spec = &deployment.spec;
    json!({
        "perClientPerHour": spec.rate_limit.as_ref().map(|r| r.per_client_per_hour),
        "perClientPerDay": spec.rate_limit.as_ref().map(|r| r.per_client_per_day),
        "messagesPerConversation": spec.budget.as_ref().map(|b| b.messages_per_conversation),
        "questionChars": spec.budget.as_ref().map(|b| b.question_chars),
    })
}

/// The hosts of `urls`, `www.` dropped, sorted, each once.
fn hosts<'a>(urls: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut hosts: Vec<String> = urls
        .into_iter()
        .filter_map(|u| url::Url::parse(u).ok()?.host_str().map(str::to_owned))
        .map(|h| h.trim_start_matches("www.").to_owned())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Counts, once a minute and outside any request, the entities of every public Endpoint a
/// published deployment reads, as an anonymous caller counts them; a page only reads the
/// result. An Endpoint with a type that did not come back has no total: a part is not its count.
pub async fn count(state: Arc<ChatState>) {
    // The worker starts a round each minute: one still running keeps the gateway alone, so a
    // slow round never doubles the fan-out (T-5781).
    static ROUND: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let Ok(_round) = ROUND.try_lock() else {
        return;
    };
    let Ok(snapshot) = state.snapshot.read().map(|s| Arc::clone(&s)) else {
        return;
    };
    let Ok(gateway) = url::Url::parse(&state.gateway) else {
        return;
    };
    let mut slugs: Vec<&str> = Vec::new();
    for deployment in snapshot
        .deployments
        .iter()
        .filter(|d| d.spec.enabled && d.spec.channel.is_anonymous())
    {
        for resolved in snapshot.reach(deployment).endpoints {
            if let Some((slug, _)) = snapshot.pages.get_key_value(&resolved.slug) {
                if !slugs.contains(&slug.as_str()) {
                    slugs.push(slug);
                }
            }
        }
    }
    let pages: Vec<_> = slugs
        .iter()
        .map(|slug| &snapshot.pages[*slug].page)
        .collect();
    let (counted, shed) = count_round(&state.http, &gateway, &pages).await;
    if let Ok(mut held) = state.contexts.lock() {
        // A shed round counted only some Endpoints: the others keep the last round's totals.
        if shed {
            held.counts.extend(counted);
        } else {
            held.counts = counted;
        }
    }
}

/// Counts `pages` [`COUNT_PARALLEL`] at a time and gives each Endpoint whose every type came
/// back its total, and whether the gateway shed load. Once it does, no Endpoint not yet asked is
/// asked this round (T-5781: a round kept asking a full gateway, 2,416 refusals in one second).
async fn count_round(
    http: &reqwest::Client,
    gateway: &url::Url,
    pages: &[&crate::catalogue::EndpointPage],
) -> (HashMap<String, u64>, bool) {
    let shed = AtomicBool::new(false);
    // A bounded fan-out: the gateway admits 32 requests per caller, and an unbounded one over
    // every Endpoint held them all each minute, so the questions' own reads got 503.
    let asks: Vec<_> = pages
        .iter()
        .map(|page| {
            let shed = &shed;
            async move {
                if shed.load(Ordering::Relaxed) {
                    return None;
                }
                let counts = tokio::time::timeout(
                    COUNT_DEADLINE,
                    crate::catalogue::counts(http, gateway, page),
                )
                .await
                .unwrap_or(Some(BTreeMap::new()));
                let Some(counts) = counts else {
                    shed.store(true, Ordering::Relaxed);
                    return None;
                };
                (!counts.is_empty() && counts.len() == page.types.len())
                    .then(|| (page.slug.clone(), counts.values().sum::<u64>()))
            }
        })
        .collect();
    let counted: Vec<_> = futures_util::stream::iter(asks)
        .buffer_unordered(COUNT_PARALLEL)
        .collect()
        .await;
    (counted.into_iter().flatten().collect(), shed.into_inner())
}

/// The deployment's own document from `snapshot`, the store and the minute's counts.
async fn compute(
    state: &ChatState,
    snapshot: &Snapshot,
    deployment: &Deployment,
) -> Result<Context, crate::Error> {
    let resolution = snapshot.reach(deployment);
    let counts = state
        .contexts
        .lock()
        .map(|held| held.counts.clone())
        .unwrap_or_default();
    let endpoints = resolution
        .endpoints
        .iter()
        .filter_map(|resolved| snapshot.pages.get(&resolved.slug))
        .map(|page| Shown {
            slug: page.page.slug.clone(),
            title: page.page.title.clone(),
            publisher: page.project.clone(),
            themes: page.themes.clone(),
            types: page.page.types.iter().map(|t| t.class.clone()).collect(),
            entities: counts.get(&page.page.slug).copied(),
            modified: None,
            catalogue: page.page.ckan_dataset.clone(),
        })
        .collect();

    let mut sources = Vec::new();
    for source in snapshot.sources_of(deployment) {
        if source.spec.visibility != Visibility::Public {
            continue;
        }
        let mut tx = crate::project_scope(&state.pool, &source.project).await?;
        let (pages, documents, crawled): (i64, i64, Option<String>) = sqlx::query_as(
            "SELECT \
               (SELECT count(*) FROM pages p WHERE p.site_id = s.id AND p.status = 'fetched' AND p.included)::int8, \
               (SELECT count(*) FROM documents d WHERE d.site_id = s.id AND d.status = 'fetched' AND d.included)::int8, \
               to_char(s.last_crawl AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             FROM sites s WHERE s.project = $1 AND s.source = $2",
        )
        .bind(&source.project)
        .bind(&source.name)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or((0, 0, None));
        tx.rollback().await?;
        sources.push(Read {
            name: source.name.clone(),
            kind: source.spec.source,
            sites: hosts(
                source
                    .spec
                    .start_urls
                    .iter()
                    .map(String::as_str)
                    .chain(source.ckan_url.as_deref()),
            ),
            pages,
            documents,
            crawled,
        });
    }

    Ok(Context {
        public_id: deployment.spec.public_id.clone(),
        title: titles(deployment),
        languages: deployment.spec.languages.clone(),
        // In the table's order, each once, whatever order the manifest lists them in.
        categories: DataTheme::ALL
            .into_iter()
            .filter(|theme| deployment.spec.categories.contains(theme))
            .collect(),
        limits: limits(deployment),
        endpoints,
        sources,
        omitted: resolution.omitted.len(),
    })
}

/// The deployment of `public_id` with its document, from the last minute's or computed now.
async fn held(
    state: &ChatState,
    public_id: &str,
) -> Result<Option<(Deployment, Arc<Context>)>, crate::Error> {
    let Ok(snapshot) = state.snapshot.read().map(|s| Arc::clone(&s)) else {
        return Ok(None);
    };
    let Some(deployment) = snapshot
        .public(public_id)
        .filter(|d| d.spec.enabled)
        .cloned()
    else {
        return Ok(None);
    };
    let now = Instant::now();
    if let Ok(held) = state.contexts.lock() {
        if let Some((at, context)) = held.documents.get(public_id) {
            if now.duration_since(*at) < KEEP {
                return Ok(Some((deployment, Arc::clone(context))));
            }
        }
    }
    let context = Arc::new(compute(state, &snapshot, &deployment).await?);
    if let Ok(mut held) = state.contexts.lock() {
        if held.documents.len() >= MAX_HELD {
            held.documents.clear();
        }
        held.documents
            .insert(public_id.to_owned(), (now, Arc::clone(&context)));
    }
    Ok(Some((deployment, context)))
}

fn missing() -> Response {
    jc_core::ProblemDetails::for_status(404)
        .with_detail("No assistant is published under this id.")
        .into_response()
}

fn failed(err: &crate::Error) -> Response {
    tracing::error!(%err, "an assistant's context could not be read");
    jc_core::ProblemDetails::for_status(503)
        .with_detail("What this assistant knows could not be read just now; try again shortly.")
        .into_response()
}

/// `GET /api/v1/d/{publicId}/context`: any page may read it; a browser on one of the
/// deployment's origins gets the CORS header the chat gives it.
pub(super) async fn document(
    State(state): State<Arc<ChatState>>,
    Path(public_id): Path<String>,
    Query(asked): Query<Asked>,
    headers: HeaderMap,
) -> Response {
    let (deployment, context) = match held(&state, &public_id).await {
        Ok(Some(found)) => found,
        Ok(None) => return missing(),
        Err(err) => return failed(&err),
    };
    let lang = language(&deployment, asked.lang.as_deref());
    let mut response = Json(context.json(lang)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    let origin = headers
        .get(header::ORIGIN)
        .filter(|o| {
            o.to_str().is_ok_and(|o| {
                deployment.spec.allowed_origins.iter().any(|a| a == o)
                    || state.public_origin.as_deref() == Some(o)
            })
        })
        .cloned();
    if let Some(origin) = origin {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
}

/// `<`, `>`, `&` and quotes as entities (T-4681): every name on the page is someone's text.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The page's own words, in Slovak or English; every other language reads the English ones.
fn words(lang: &str) -> [&'static str; 14] {
    if matches!(lang, "sk" | "cs") {
        [
            "Čo tento asistent vie",
            "Kategórie",
            "Dátové zdroje",
            "Dataset",
            "Vydavateľ",
            "Typy",
            "Počet záznamov",
            "Webové zdroje a dokumenty",
            "Stránky",
            "Dokumenty",
            "Naposledy načítané",
            "Obmedzenia",
            "Otázok za hodinu",
            "Otázok za deň",
        ]
    } else {
        [
            "What this assistant knows",
            "Categories",
            "Data sources",
            "Dataset",
            "Publisher",
            "Types",
            "Records",
            "Websites and documents",
            "Pages",
            "Documents",
            "Last read",
            "Limits",
            "Questions per hour",
            "Questions per day",
        ]
    }
}

/// The page of `context` in `lang`: headings and tables a screen reader walks, nothing scripted.
pub fn html(context: &Context, lang: &str) -> String {
    let [heading, categories, data, dataset, publisher, types, records, sites, pages, documents, crawled, limits, per_hour, per_day] =
        words(lang);
    let title = context
        .title
        .get(lang)
        .or_else(|| context.title.values().next())
        .map_or(context.public_id.as_str(), String::as_str);
    let none = "—";
    let mut body = format!(
        "<header><h1>{heading}</h1><p class=\"jc-assistant\">{}</p></header>\n",
        escaped(title)
    );
    if !context.categories.is_empty() {
        body.push_str(&format!("<section><h2>{categories}</h2><ul>"));
        for code in &context.categories {
            body.push_str(&format!("<li>{}</li>", escaped(code.label_in(lang))));
        }
        body.push_str("</ul></section>\n");
    }
    if !context.endpoints.is_empty() {
        body.push_str(&format!(
            "<section><h2>{data}</h2><table><thead><tr><th scope=\"col\">{dataset}</th>\
             <th scope=\"col\">{publisher}</th><th scope=\"col\">{types}</th>\
             <th scope=\"col\">{records}</th></tr></thead><tbody>"
        ));
        for e in &context.endpoints {
            let name = escaped(e.title.as_deref().unwrap_or(&e.slug));
            let name = match &e.catalogue {
                Some(url) if url.starts_with("https://") => {
                    format!("<a href=\"{}\">{name}</a>", escaped(url))
                }
                _ => name,
            };
            body.push_str(&format!(
                "<tr><th scope=\"row\">{name}</th><td>{}</td><td>{}</td><td>{}</td></tr>",
                escaped(&e.publisher),
                escaped(&e.types.join(", ")),
                e.entities
                    .map_or_else(|| none.to_owned(), |n| n.to_string()),
            ));
        }
        body.push_str("</tbody></table></section>\n");
    }
    if !context.sources.is_empty() {
        body.push_str(&format!(
            "<section><h2>{sites}</h2><table><thead><tr><th scope=\"col\">{dataset}</th>\
             <th scope=\"col\">{pages}</th><th scope=\"col\">{documents}</th>\
             <th scope=\"col\">{crawled}</th></tr></thead><tbody>"
        ));
        for s in &context.sources {
            let named = if s.sites.is_empty() {
                s.name.clone()
            } else {
                s.sites.join(", ")
            };
            body.push_str(&format!(
                "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td><td>{}</td></tr>",
                escaped(&named),
                s.pages,
                s.documents,
                s.crawled
                    .as_deref()
                    .map_or_else(|| none.to_owned(), escaped),
            ));
        }
        body.push_str("</tbody></table></section>\n");
    }
    let limit = |key: &str| {
        context.limits[key]
            .as_u64()
            .map_or_else(|| none.to_owned(), |n| n.to_string())
    };
    body.push_str(&format!(
        "<section><h2>{limits}</h2><dl><dt>{per_hour}</dt><dd>{}</dd><dt>{per_day}</dt><dd>{}</dd></dl></section>\n",
        limit("perClientPerHour"),
        limit("perClientPerDay"),
    ));
    format!(
        "<!doctype html>\n<html lang=\"{lang}\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n<title>{heading}: {title}</title>\n\
         <link rel=\"stylesheet\" href=\"/d/context.css\">\n</head>\n<body>\n<main>\n{body}</main>\n\
         </body>\n</html>\n",
        lang = escaped(lang),
        title = escaped(title),
    )
}

/// `/d/{publicId}/context`: the same document as a page, framed by nobody.
async fn page(
    State(state): State<Arc<ChatState>>,
    Path(public_id): Path<String>,
    Query(asked): Query<Asked>,
) -> Response {
    let (deployment, context) = match held(&state, &public_id).await {
        Ok(Some(found)) => found,
        Ok(None) => return missing(),
        Err(err) => return failed(&err),
    };
    let lang = language(&deployment, asked.lang.as_deref());
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=60"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
        ],
        html(&context, lang),
    )
        .into_response()
}

async fn style() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        include_str!("../../widget/context.css"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            public_id: "bb-doprava".into(),
            title: BTreeMap::from([("sk".into(), "Doprava <v> BB".into())]),
            languages: vec!["sk".into(), "en".into()],
            categories: vec![DataTheme::Tran],
            limits: json!({"perClientPerHour": 60, "perClientPerDay": null}),
            endpoints: vec![Shown {
                slug: "aaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                title: Some("Parkoviská \"<script>alert(1)</script>\"".into()),
                publisher: "banskabystrica".into(),
                themes: vec![DataTheme::Tran],
                types: vec!["OffStreetParking".into()],
                entities: Some(41),
                modified: None,
                catalogue: Some("javascript:alert(1)".into()),
            }],
            sources: vec![Read {
                name: "web".into(),
                kind: SourceType::Website,
                sites: vec!["banskabystrica.sk".into()],
                pages: 12,
                documents: 3,
                crawled: Some("2026-10-18T21:40:00Z".into()),
            }],
            omitted: 0,
        }
    }

    /// T-4681 rules: every name is escaped and only an https link becomes one.
    #[test]
    fn every_name_on_the_page_is_escaped_and_only_https_links() {
        let page = html(&context(), "sk");
        assert!(!page.contains("<script>"), "{page}");
        assert!(page.contains("&lt;script&gt;"), "{page}");
        assert!(page.contains("Doprava &lt;v&gt; BB"), "{page}");
        assert!(!page.contains("javascript:"), "{page}");
        assert!(
            page.contains("<th scope=\"row\">"),
            "row headers for a screen reader"
        );
        assert!(page.contains("<html lang=\"sk\">"));
        assert!(page.contains("Doprava"), "the TRAN label in Slovak");
    }

    #[test]
    fn the_document_names_codes_with_labels_in_the_asked_language() {
        let document = context().json("en");
        assert_eq!(document["categories"][0]["code"], "TRAN");
        assert_eq!(
            document["categories"][0]["label"],
            DataTheme::Tran.label_in("en")
        );
        assert_eq!(document["endpoints"][0]["entities"], 41);
        assert_eq!(document["sources"][0]["kind"], "website");
        for absent in ["systemPrompt", "model", "budget", "secretRef"] {
            assert!(!document.to_string().contains(absent), "{absent}");
        }
    }

    /// A stub gateway that answers `status` to every count after `pause`, and the most requests
    /// it held at once and in total.
    async fn gateway(
        status: StatusCode,
        pause: Duration,
    ) -> (
        url::Url,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::AtomicUsize;
        let now = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        let (n, m, t) = (Arc::clone(&now), Arc::clone(&most), Arc::clone(&total));
        let app = Router::new().fallback(move || {
            let (n, m, t) = (Arc::clone(&n), Arc::clone(&m), Arc::clone(&t));
            async move {
                t.fetch_add(1, Ordering::SeqCst);
                let held = n.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                m.fetch_max(held, Ordering::SeqCst);
                tokio::time::sleep(pause).await;
                n.fetch_sub(1, Ordering::SeqCst);
                (status, [("NGSILD-Results-Count", "7")])
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = listener.local_addr().expect("its address");
        tokio::spawn(async move { axum::serve(listener, app).await });
        let url = url::Url::parse(&format!("http://{address}/")).expect("url");
        (url, most, total)
    }

    fn endpoints(n: usize, types: usize) -> Vec<crate::catalogue::EndpointPage> {
        (0..n)
            .map(|i| crate::catalogue::EndpointPage {
                name: format!("e{i}"),
                title: None,
                description: None,
                space: "s".into(),
                slug: format!("slug{i}"),
                audience: "public".into(),
                ckan_dataset: None,
                types: (0..types)
                    .map(|t| crate::catalogue::EntityType {
                        class: format!("T{t}"),
                        description: None,
                        attributes: Vec::new(),
                    })
                    .collect(),
                datasets: Vec::new(),
            })
            .collect()
    }

    /// T-5781: a round asks the gateway at most `COUNT_PARALLEL` counts at once.
    #[tokio::test]
    async fn a_round_counts_at_most_count_parallel_at_once() {
        let (url, most, total) = gateway(StatusCode::OK, Duration::from_millis(30)).await;
        let pages = endpoints(12, 2);
        let pages: Vec<_> = pages.iter().collect();
        let (counted, shed) = count_round(&reqwest::Client::new(), &url, &pages).await;
        let most = most.load(Ordering::SeqCst);
        assert!(
            most <= COUNT_PARALLEL,
            "{most} counts at once, past {COUNT_PARALLEL}"
        );
        assert!(most > 1, "the round counts Endpoints side by side");
        assert_eq!(total.load(Ordering::SeqCst), 24);
        assert!(!shed);
        assert_eq!(counted.len(), 12);
        assert_eq!(counted.get("slug3"), Some(&14));
    }

    /// T-5781: once the gateway sheds load, neither the Endpoint's other types nor the Endpoints
    /// not yet asked are asked this round.
    #[tokio::test]
    async fn a_shedding_gateway_ends_the_round() {
        for status in [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let (url, _, total) = gateway(status, Duration::from_millis(5)).await;
            let pages = endpoints(40, 3);
            let pages: Vec<_> = pages.iter().collect();
            let (counted, shed) = count_round(&reqwest::Client::new(), &url, &pages).await;
            let total = total.load(Ordering::SeqCst);
            assert!(
                total <= COUNT_PARALLEL,
                "{status}: {total} counts asked of a shedding gateway"
            );
            assert!(shed, "{status}");
            assert!(counted.is_empty(), "{status}");
        }
    }

    /// A refusal that is not load (an Endpoint a visitor may not read) still counts the other types.
    #[tokio::test]
    async fn a_refused_type_is_left_out_and_the_round_goes_on() {
        let (url, _, total) = gateway(StatusCode::FORBIDDEN, Duration::ZERO).await;
        let pages = endpoints(3, 2);
        let pages: Vec<_> = pages.iter().collect();
        let (counted, shed) = count_round(&reqwest::Client::new(), &url, &pages).await;
        assert_eq!(total.load(Ordering::SeqCst), 6);
        assert!(!shed);
        assert!(counted.is_empty());
    }
}
