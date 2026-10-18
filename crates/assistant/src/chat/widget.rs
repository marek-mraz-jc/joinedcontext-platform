//! The public chat (API/05 §1.6, T-3058, AG-114, T-4138): one HTML page per public deployment,
//! served full page at `/d/{publicId}` and as the panel `/d/{publicId}/widget` that a site frames
//! or the launcher `/d/{publicId}/embed.js` opens. Script and style come from this host, the
//! pages are framed only by the deployment's `allowedOrigins`, no page sets a cookie and the
//! conversation lives in the page; every limit is the chat route's.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::json;

use super::ChatState;
use crate::worker::{Deployment, Source};

/// The field labels the renderer reads (T-4661), the answer's renderer (T-3325), then the widget
/// that uses it, as one script.
const SCRIPT: &str = concat!(
    "var JC_FIELDS = ",
    include_str!("../../../jc-core/data/field-order.json"),
    ";\n",
    include_str!("../../widget/render.js"),
    include_str!("../../widget/widget.js")
);
const STYLE: &str = include_str!("../../widget/widget.css");
/// The launcher a site includes; it reads its deployment from its own address.
const LAUNCHER: &str = include_str!("../../widget/embed.js");

pub fn router(state: Arc<ChatState>) -> Router {
    Router::new()
        .route("/d/widget.js", get(script))
        .route("/d/widget.css", get(style))
        .route("/d/{public_id}", get(full_page))
        .route("/d/{public_id}/widget", get(page))
        .route("/d/{public_id}/embed.js", get(launcher))
        .with_state(state)
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=300"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

async fn script() -> Response {
    asset(SCRIPT, "text/javascript; charset=utf-8")
}

async fn style() -> Response {
    asset(STYLE, "text/css; charset=utf-8")
}

/// `<`, `>`, `&` and quotes as entities: the configuration rides in an attribute.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The page's policy: its own host for scripts, styles and requests, framed by the deployment's
/// origins alone. jc-core holds each origin to `https://host[:port]`, so none can widen it.
pub fn policy(deployment: &Deployment) -> String {
    let ancestors = if deployment.spec.allowed_origins.is_empty() {
        "'none'".to_owned()
    } else {
        deployment.spec.allowed_origins.join(" ")
    };
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-ancestors {ancestors}"
    )
}

/// Hosts every deployment's answers may link to, beside its own (T-4149): the national open
/// data portal the city's and the region's datasets are listed on.
const LINK_HOSTS: &[&str] = &["data.gov.sk"];

/// The hosts an answer's links may go to (T-4149): the pages it is placed on, its sources' sites
/// and CKAN, and `LINK_HOSTS` (the national open data portal). The widget adds the hosts of each answer's citations.
pub fn link_hosts(deployment: &Deployment, sources: &[Source]) -> Vec<String> {
    let spec = &deployment.spec;
    let mut urls: Vec<&str> = spec.allowed_origins.iter().map(String::as_str).collect();
    // A source of another project, `{project}/{source}`, adds that source's sites (T-4281).
    let named = |s: &&Source| {
        spec.sources.iter().any(|name| match name.split_once('/') {
            Some((project, name)) => s.project == project && s.name == name,
            None => s.project == deployment.project && s.name == *name,
        })
    };
    for source in sources.iter().filter(named) {
        urls.extend(source.spec.start_urls.iter().map(String::as_str));
        urls.extend(source.ckan_url.as_deref());
    }
    let mut hosts: Vec<String> = urls
        .into_iter()
        .filter_map(|u| url::Url::parse(u).ok()?.host_str().map(str::to_owned))
        .map(|h| h.trim_start_matches("www.").to_owned())
        .chain(LINK_HOSTS.iter().map(|h| (*h).to_owned()))
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// What the page needs to know of its deployment; nothing in it is secret.
fn config(deployment: &Deployment, link_hosts: &[String]) -> serde_json::Value {
    let spec = &deployment.spec;
    json!({
        "linkHosts": link_hosts,
        // The page's `frame-ancestors`, already in its header: where its close message may go (T-4654).
        "frameOrigins": spec.allowed_origins,
        "publicId": spec.public_id,
        "title": spec.theme.as_ref().and_then(|t| t.title.clone()),
        "greeting": spec.theme.as_ref().and_then(|t| t.greeting.clone()),
        "color": spec.theme.as_ref().and_then(|t| t.primary_color.clone()),
        // A visitor gets no picker: the assistant chooses among all its connectors, and their
        // Endpoint names stay off an anonymous page (T-4253). An internal channel keeps it.
        "connectors": if spec.channel.is_anonymous() {
            Vec::new()
        } else {
            spec.connectors.iter().map(|c| c.endpoint.clone()).collect::<Vec<_>>()
        },
        "languages": spec.languages,
        "suggestions": spec.suggestions,
        "disclaimer": spec.disclaimer,
        "enabled": spec.enabled,
        "feedback": spec.feedback,
        // How the chat picks its language and whether it shows the toggle (T-4343).
        "widget": spec.widget.unwrap_or_default(),
        "questionChars": spec.budget.as_ref().map(|b| b.question_chars),
    })
}

/// The page of one public deployment: the panel, or with `full` the page of its own.
pub fn html(deployment: &Deployment, link_hosts: &[String], full: bool) -> String {
    let spec = &deployment.spec;
    // The first of `languages` is the one the chat opens in (MF-52, T-4281); Slovak when none.
    let lang = spec.languages.first().map_or("sk", String::as_str);
    format!(
        "<!doctype html>\n<html lang=\"{lang}\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1, interactive-widget=resizes-content\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n<title>{title}</title>\n\
         <link rel=\"stylesheet\" href=\"/d/widget.css\">\n</head>\n<body{class}>\n\
         <main id=\"jc-chat\" class=\"jc-chat\" data-config=\"{config}\"></main>\n\
         <script src=\"/d/widget.js\" defer></script>\n</body>\n</html>\n",
        lang = escaped(lang),
        title = escaped(&spec.public_id),
        class = if full { " class=\"jc-page\"" } else { "" },
        config = escaped(&config(deployment, link_hosts).to_string()),
    )
}

fn published(state: &ChatState, public_id: &str) -> Option<Deployment> {
    state
        .snapshot
        .read()
        .ok()
        .and_then(|snapshot| snapshot.public(public_id).cloned())
}

fn not_published() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "No assistant is published under this id.",
    )
        .into_response()
}

/// What a person reads on an unknown id: the edge's words for an unknown path (T-4200), no
/// script and no deployment named, so a draft and a never-created id answer the same bytes.
const MISSING_PAGE: &str = "<!doctype html>\n<html lang=\"sk\">\n<head>\n<meta charset=\"utf-8\">\n\
     <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
     <title>Stránka neexistuje</title>\n</head>\n<body>\n<main>\n\
     <h1>Táto stránka neexistuje.</h1>\n<p>Asistent odpovedá na stránke, ktorá ho ponúka.</p>\n\
     <p lang=\"en\">This page does not exist. The assistant answers on the site that offers it.</p>\n\
     </main>\n</body>\n</html>\n";

fn missing_page() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; base-uri 'none'; form-action 'none'",
            ),
        ],
        MISSING_PAGE,
    )
        .into_response()
}

/// The launcher of a published deployment; an unknown id gets a 404, so a typo shows in the
/// site's console instead of a button that opens an error.
async fn launcher(State(state): State<Arc<ChatState>>, Path(public_id): Path<String>) -> Response {
    match published(&state, &public_id) {
        Some(_) => asset(LAUNCHER, "text/javascript; charset=utf-8"),
        None => not_published(),
    }
}

async fn page(State(state): State<Arc<ChatState>>, Path(public_id): Path<String>) -> Response {
    serve(&state, &public_id, false)
}

async fn full_page(State(state): State<Arc<ChatState>>, Path(public_id): Path<String>) -> Response {
    serve(&state, &public_id, true)
}

fn serve(state: &ChatState, public_id: &str, full: bool) -> Response {
    let Some(deployment) = published(state, public_id) else {
        return missing_page();
    };
    let hosts = state
        .snapshot
        .read()
        .map(|snapshot| link_hosts(&deployment, &snapshot.sources))
        .unwrap_or_default();
    let mut response = (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        html(&deployment, &hosts, full),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&policy(&deployment)) {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(spec: serde_json::Value) -> Deployment {
        Deployment {
            project: "banskabystrica".into(),
            name: "verejny".into(),
            spec: serde_json::from_value(spec).expect("a spec"),
        }
    }

    /// T-4138: the page carries what the visitor sees before asking, per language, and nothing
    /// a visitor should not read; the full page and the panel differ only in their layout.
    #[test]
    fn the_page_carries_the_suggestions_disclaimer_and_switch_and_no_secret() {
        let public = deployment(json!({
            "publicId": "bb",
            "channel": "public",
            "sources": ["web"],
            "allowedOrigins": ["https://www.banskabystrica.sk"],
            "systemPrompt": "Interné pokyny pre model.",
            "languages": ["sk", "en"],
            "suggestions": {"sk": ["Kedy je zber odpadu?"], "en": ["When is waste collected?"]},
            "disclaimer": {"sk": "Odpovede píše umelá inteligencia."},
            "theme": {"title": "Banská Bystrica"},
            "rateLimit": {"requestsPerMinute": 60, "perClientPerMinute": 10},
            "budget": {"tokensPerDay": 100000, "tokensPerConversation": 10000, "questionChars": 500},
        }));
        let config = config(&public, &[]);
        // T-4283: the header's name; the monogram is drawn from it, no image.
        assert_eq!(config["title"], json!("Banská Bystrica"));
        assert_eq!(config["languages"], json!(["sk", "en"]));
        assert_eq!(
            config["suggestions"]["en"],
            json!(["When is waste collected?"])
        );
        assert_eq!(
            config["disclaimer"]["sk"],
            json!("Odpovede píše umelá inteligencia.")
        );
        assert_eq!(config["enabled"], json!(true));
        assert_eq!(config["questionChars"], json!(500));
        // T-4343: a deployment that names no widget opens in the browser's language, toggle on.
        assert_eq!(
            config["widget"],
            json!({"language": "browser", "languageToggle": true})
        );
        let mut asking = public.clone();
        asking.spec.widget = Some(jc_core::kinds::ChatWidget {
            language: jc_core::kinds::WidgetLanguage::Ask,
            language_toggle: false,
        });
        assert_eq!(
            super::config(&asking, &[])["widget"],
            json!({"language": "ask", "languageToggle": false})
        );
        let panel = html(&public, &[], false);
        let page = html(&public, &[], true);
        assert!(panel.contains("<html lang=\"sk\">") && panel.contains("<body>"));
        assert!(page.contains("<body class=\"jc-page\">"));
        for html in [&panel, &page] {
            assert!(
                !html.contains("Interné pokyny"),
                "the prompt never reaches the page"
            );
            assert!(html.contains("Kedy je zber odpadu?"));
        }
    }

    /// T-4253: a visitor's page names no connector, so it shows no picker and no Endpoint name;
    /// an internal deployment keeps the picker.
    #[test]
    fn a_public_page_names_no_connector_and_an_internal_one_keeps_them() {
        let spec = |channel: &str| {
            json!({
                "publicId": "bb",
                "channel": channel,
                "sources": ["web"],
                "connectors": [{"endpoint": "banskabystrica/air-quality", "tools": ["query_entities"]}],
            })
        };
        let public = deployment(spec("public"));
        assert_eq!(config(&public, &[])["connectors"], json!([]));
        assert!(!html(&public, &[], false).contains("air-quality"));
        let internal = deployment(spec("internal"));
        assert_eq!(
            config(&internal, &[])["connectors"],
            json!(["banskabystrica/air-quality"])
        );
    }

    #[test]
    fn a_paused_deployment_says_so_and_an_english_one_opens_in_english() {
        let paused = deployment(json!({
            "publicId": "bb",
            "channel": "public",
            "sources": ["web"],
            "languages": ["en", "fi"],
            "enabled": false,
        }));
        assert_eq!(config(&paused, &[])["enabled"], json!(false));
        assert_eq!(config(&paused, &[])["questionChars"], json!(null));
        assert!(html(&paused, &[], true).contains("<html lang=\"en\">"));
    }

    /// T-4149: links go to the deployment's own sites, its sources and the national portal; a
    /// source of another project or not named by the deployment adds nothing.
    #[test]
    fn the_link_hosts_are_the_deployment_s_own() {
        let public = deployment(json!({
            "publicId": "bb",
            "channel": "public",
            "sources": ["web", "data"],
            "allowedOrigins": ["https://www.banskabystrica.sk"],
        }));
        let source =
            |project: &str, name: &str, spec: serde_json::Value, ckan: Option<&str>| Source {
                project: project.into(),
                name: name.into(),
                spec: serde_json::from_value(spec).expect("a source spec"),
                ckan_url: ckan.map(str::to_owned),
                catalogue: Vec::new(),
            };
        let web = json!({"source": "website", "startUrls": ["https://www.bbsk.sk/", "https://mapy.banskabystrica.sk/x"]});
        let ckan = json!({"source": "ckan", "ckanInstanceRef": "otvorene"});
        let hosts = link_hosts(
            &public,
            &[
                source("banskabystrica", "web", web.clone(), None),
                source(
                    "banskabystrica",
                    "data",
                    ckan,
                    Some("https://data.dev.joinedcontext.com"),
                ),
                source(
                    "banskabystrica",
                    "unnamed",
                    json!({"source": "website", "startUrls": ["https://evil.example/"]}),
                    None,
                ),
                source(
                    "helsinki",
                    "web",
                    json!({"source": "website", "startUrls": ["https://hel.fi/"]}),
                    None,
                ),
            ],
        );
        assert_eq!(
            hosts,
            [
                "banskabystrica.sk",
                "bbsk.sk",
                "data.dev.joinedcontext.com",
                "data.gov.sk",
                "mapy.banskabystrica.sk",
            ]
        );
        assert_eq!(config(&public, &hosts)["linkHosts"], json!(hosts));
        // The close message's targets are the page's own frame-ancestors (T-4654).
        assert_eq!(
            config(&public, &hosts)["frameOrigins"],
            json!(public.spec.allowed_origins)
        );
        assert!(!public.spec.allowed_origins.is_empty());
        for origin in &public.spec.allowed_origins {
            assert!(policy(&public).contains(origin.as_str()), "{origin}");
        }
    }

    #[test]
    fn the_configuration_cannot_break_out_of_its_attribute() {
        assert_eq!(
            escaped("\"><script>x</script>"),
            "&quot;&gt;&lt;script&gt;x&lt;/script&gt;"
        );
        assert_eq!(escaped("a&b'"), "a&amp;b&#39;");
    }

    /// T-4241 (T-4162 #15): a person who opens an unknown id reads a page in their words, with
    /// the edge's sentence (T-4200), and nothing that tells one hidden deployment from another.
    #[test]
    fn an_unknown_id_is_a_page_in_words_that_names_no_deployment() {
        let response = missing_page();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let headers = response.headers();
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        assert_eq!(
            headers[header::CONTENT_SECURITY_POLICY],
            "default-src 'none'; base-uri 'none'; form-action 'none'"
        );
        assert!(MISSING_PAGE.contains("<html lang=\"sk\">"));
        assert!(MISSING_PAGE.contains("Táto stránka neexistuje."));
        assert!(MISSING_PAGE.contains("<p lang=\"en\">This page does not exist."));
        assert!(!MISSING_PAGE.contains("<script"));
    }
}
