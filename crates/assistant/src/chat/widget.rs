//! The widget a site places in an iframe (API/05 §1.6, T-3058, AG-114): one HTML page per public
//! deployment, its script and its style from this host, framed only by the deployment's
//! `allowedOrigins`. The page sets no cookie and holds the conversation itself; every limit is
//! the chat route's.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::json;

use super::ChatState;
use crate::worker::Deployment;

/// The answer's renderer first (T-3325), then the widget that uses it, as one script.
const SCRIPT: &str = concat!(
    include_str!("../../widget/render.js"),
    include_str!("../../widget/widget.js")
);
const STYLE: &str = include_str!("../../widget/widget.css");

pub fn router(state: Arc<ChatState>) -> Router {
    Router::new()
        .route("/d/widget.js", get(script))
        .route("/d/widget.css", get(style))
        .route("/d/{public_id}/widget", get(page))
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

/// The page of one public deployment.
pub fn html(deployment: &Deployment) -> String {
    let spec = &deployment.spec;
    let config = json!({
        "publicId": spec.public_id,
        "title": spec.public_id,
        "greeting": spec.theme.as_ref().and_then(|t| t.greeting.clone()),
        "color": spec.theme.as_ref().and_then(|t| t.primary_color.clone()),
        "connectors": spec.connectors.iter().map(|c| c.endpoint.clone()).collect::<Vec<_>>(),
    });
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n<title>{title}</title>\n\
         <link rel=\"stylesheet\" href=\"/d/widget.css\">\n</head>\n<body>\n\
         <main id=\"jc-chat\" class=\"jc-chat\" data-config=\"{config}\"></main>\n\
         <script src=\"/d/widget.js\" defer></script>\n</body>\n</html>\n",
        title = escaped(&spec.public_id),
        config = escaped(&config.to_string()),
    )
}

async fn page(State(state): State<Arc<ChatState>>, Path(public_id): Path<String>) -> Response {
    let deployment = state
        .snapshot
        .read()
        .ok()
        .and_then(|snapshot| snapshot.public(&public_id).cloned());
    let Some(deployment) = deployment else {
        return (
            StatusCode::NOT_FOUND,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            "No assistant is published under this id.",
        )
            .into_response();
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        html(&deployment),
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

    #[test]
    fn the_configuration_cannot_break_out_of_its_attribute() {
        assert_eq!(
            escaped("\"><script>x</script>"),
            "&quot;&gt;&lt;script&gt;x&lt;/script&gt;"
        );
        assert_eq!(escaped("a&b'"), "a&amp;b&#39;");
    }
}
