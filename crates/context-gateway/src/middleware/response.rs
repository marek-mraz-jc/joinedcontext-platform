//! What the gateway concluded internally never leaves in a header the caller did not ask
//! for (SP-05, R22).
//!
//! The counterpart of [`super::tenancy::strip_client_headers`]: that one removes what a
//! client said on the way in, this one removes what the gateway said on the way out. One
//! layer rather than a line at every place that builds an answer, so a handler added later
//! cannot forget it.

use crate::app::Gateway;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

// The schema documents the broker's LinkML surface of a view serves (plan 5.4, T-3646): the
// names, file names and media types the `describedby` links, the `schema/` routes, the MCP
// schema tool and the bundle agree on. The documents themselves are the broker's.

/// The media type of a JSON Schema document (EP-49).
pub const JSON_SCHEMA: &str = "application/schema+json";
/// The media type of a JSON-LD `@context` (EP-49).
pub const JSON_LD: &str = "application/ld+json";

/// The media type of a Turtle document (EP-49).
pub const TURTLE: &str = "text/turtle";
/// The media type the OWL rendering is negotiated by, a Turtle profile (EP-49).
pub const TURTLE_OWL: &str = "text/turtle; profile=\"owl\"";
/// The media type of the LinkML source (EP-49).
pub const YAML: &str = "text/yaml";
/// The media type of the human documentation (EP-49).
pub const MARKDOWN: &str = "text/markdown";

/// Which schema document was asked for (EP-49).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Artifact {
    /// The JSON Schema draft-07 of the model, `model.schema.json` (DM-03).
    JsonSchema,
    /// The JSON-LD `@context` of the model (DM-05).
    Context,
    /// The SHACL shapes, rendered from the projection (T-0284).
    Shacl,
    /// The OWL ontology, rendered from the projection (T-0284).
    Owl,
    /// The RDFS rendering of the projection (T-0284).
    Rdf,
    /// The LinkML source of the projection (T-0284).
    LinkMl,
    /// The human documentation of the projection (T-0284).
    Markdown,
    /// The RDF Data Cube structure of the projection, published only where a granted class is
    /// a Data Structure Definition (DM-60, T-1187).
    Qb,
}

impl Artifact {
    /// The media type the artifact is served as.
    pub fn media_type(self) -> &'static str {
        match self {
            Artifact::JsonSchema => JSON_SCHEMA,
            Artifact::Context => JSON_LD,
            Artifact::Shacl | Artifact::Rdf | Artifact::Qb => TURTLE,
            Artifact::Owl => TURTLE_OWL,
            Artifact::LinkMl => YAML,
            Artifact::Markdown => MARKDOWN,
        }
    }

    /// Whether the artifact is a JSON document rather than a rendered text one.
    pub fn is_json(self) -> bool {
        matches!(self, Artifact::JsonSchema | Artifact::Context)
    }

    /// The file name this artifact is published under (API/02 section 7a).
    pub fn file_name(self) -> &'static str {
        match self {
            Artifact::JsonSchema => "model.schema.json",
            Artifact::Context => "context.jsonld",
            Artifact::Shacl => "model.shacl.ttl",
            Artifact::Owl => "model.owl.ttl",
            Artifact::Rdf => "model.rdf.ttl",
            Artifact::LinkMl => "model.linkml.yaml",
            Artifact::Markdown => "model.md",
            Artifact::Qb => "model.qb.ttl",
        }
    }

    /// The name this artifact is asked for by on the agent surface (`format`), which is the
    /// same word in the MCP tool, in the `schema://` resource URI and in the summary's
    /// catalogue, so an agent never has to translate one spelling into another (EP-46, AG-84).
    pub fn format_name(self) -> &'static str {
        match self {
            Artifact::JsonSchema => "json-schema",
            Artifact::Context => "context",
            Artifact::Shacl => "shacl",
            Artifact::Owl => "owl",
            Artifact::Rdf => "rdf",
            Artifact::LinkMl => "linkml",
            Artifact::Markdown => "markdown",
            Artifact::Qb => "qb",
        }
    }

    /// The artifact one [`Self::format_name`] names, or `None` for a word this surface does
    /// not serve. Exact names only: the path aliases of [`artifact_of`] are the REST route's.
    pub fn from_format(name: &str) -> Option<Artifact> {
        Artifact::ALL
            .into_iter()
            .chain([Artifact::Qb])
            .find(|artifact| artifact.format_name() == name)
    }

    /// The seven documents an endpoint publishes about every major (EP-46); the index lists
    /// [`Artifact::Qb`] beside them where the projection holds a Data Structure Definition.
    pub const ALL: [Artifact; 7] = [
        Artifact::LinkMl,
        Artifact::JsonSchema,
        Artifact::Context,
        Artifact::Shacl,
        Artifact::Owl,
        Artifact::Rdf,
        Artifact::Markdown,
    ];
}

/// Names the artifact behind one path segment, or `None` when the surface has no such
/// document at all.
///
/// `model` is the negotiated name: `Accept` picks the formalism (EP-49).
pub fn artifact_of(segment: &str, accept: &str) -> Option<Artifact> {
    match segment {
        "json-schema" | "model.schema.json" => Some(Artifact::JsonSchema),
        "context.jsonld" | "context" => Some(Artifact::Context),
        "model.shacl.ttl" | "shacl" => Some(Artifact::Shacl),
        "model.owl.ttl" | "owl" => Some(Artifact::Owl),
        "model.rdf.ttl" | "rdf" => Some(Artifact::Rdf),
        "model.linkml.yaml" | "linkml" => Some(Artifact::LinkMl),
        "model.md" | "docs" => Some(Artifact::Markdown),
        "model.qb.ttl" | "qb" => Some(Artifact::Qb),
        "model" => Some(negotiated(accept)),
        _ => None,
    }
}

/// The formalism an `Accept` header asks `schema/v{major}/model` for (EP-49).
///
/// `text/turtle` alone is the SHACL, which is what a validator wants; the OWL is the same
/// media type with the `owl` profile, so the two share a name without sharing an answer.
fn negotiated(accept: &str) -> Artifact {
    if accept.contains("turtle") {
        if accept.contains("owl") {
            Artifact::Owl
        } else if accept.contains("rdf") {
            Artifact::Rdf
        } else {
            Artifact::Shacl
        }
    } else if accept.contains("yaml") {
        Artifact::LinkMl
    } else if accept.contains("markdown") {
        Artifact::Markdown
    } else if accept.contains("ld+json") {
        Artifact::Context
    } else {
        Artifact::JsonSchema
    }
}

/// The narrowing signal. Opt-in: a caller who sends it on the request is told when an answer
/// was narrowed, and a caller who does not ask is answered as if the result were simply what
/// it is (R22, GW12). It tells a prober that something was there to hide.
pub const RESULTS_RESTRICTED: HeaderName = HeaderName::from_static("ngsild-results-restricted");

/// Whether the request asked to be told about narrowing (R22).
pub fn asked_about_narrowing(request: &Request) -> bool {
    request
        .headers()
        .get_all(&RESULTS_RESTRICTED)
        .iter()
        .any(|value| value.as_bytes().eq_ignore_ascii_case(b"true"))
}

/// Removes the tenant from every answer, the narrowing signal from every answer nobody asked for
/// it in, says that no answer of this gateway belongs in a shared cache except an anonymous read
/// of data when the deployment allows one (`shareable_read`), and points every answer that
/// carries data at the schema it is described by.
pub async fn scrub(State(gateway): State<Arc<Gateway>>, request: Request, next: Next) -> Response {
    let asked = asked_about_narrowing(&request);
    // Read before the handler runs, because the path is what names the endpoint and the
    // request is moved into it.
    let described = describedby(&gateway, request.uri().path());
    let deprecated = deprecation_headers(&gateway, request.uri().path());
    let shareable = shareable_read(&gateway, &request);
    let mut response = next.run(request).await;
    let status = response.status();
    let carries_data = status.is_success();
    let headers = response.headers_mut();
    // EP-50, EP-147: only on an answer that carries data. A refusal that named a schema or a
    // sunset would tell a caller who was refused that the endpoint exists (EP-03, R20).
    if carries_data {
        for link in described {
            headers.append(axum::http::header::LINK, link);
        }
        for (name, value) in deprecated {
            headers.append(name, value);
        }
    }
    // The tenant is an internal name and a probe for which spaces exist. It is pinned on the
    // hop to the broker and it never comes back out, on either surface (SP-05).
    while headers.remove(&super::tenancy::TENANT).is_some() {}
    if !asked {
        while headers.remove(&RESULTS_RESTRICTED).is_some() {}
    }
    keep_out_of_shared_caches(headers);
    if shareable && status == StatusCode::OK {
        share_for(headers, gateway.public_max_age);
    }
    response
}

/// Whether the answer to `request` is the same for every caller who could send it, so a shared
/// cache may keep it for [`Gateway::public_max_age`] seconds (T-5900): a `GET` or `HEAD` of data
/// with neither `Authorization` nor `Cookie`. Without a token the gateway decides as the
/// Endpoint's anonymous subject (`auth::subject::anonymous_on`), the same subject for every such
/// caller; the headers that still shape the answer stay in `Vary`, so a cache keyed on them
/// cannot mix two of them. Off while `public_max_age` is 0, the default.
///
/// Deny by default: a request carrying any header outside [`SHAREABLE_REQUEST_HEADERS`] is kept
/// private, because the gateway forwards headers that shape the answer (`Link` names the
/// `@context` a compacted answer uses, `Prefer`, `NGSILD-*`) and a shared cache keyed on the URL
/// would replay that answer to a plain read of the same URL.
fn shareable_read(gateway: &Gateway, request: &Request) -> bool {
    gateway.public_max_age > 0
        && matches!(*request.method(), Method::GET | Method::HEAD)
        && request.headers().keys().all(|name| {
            SHAREABLE_REQUEST_HEADERS.contains(&name.as_str())
                || name.as_str().starts_with(super::tenancy::MESH_PREFIX)
        })
        && data_endpoint(gateway, request.uri().path()).is_some()
}

/// The request headers a shareable anonymous read may carry: the ones in `Vary` (and in the edge's
/// cache key), and the ones the gateway does not read to build an answer (transport, tracing, the
/// edge's forwarding; the Linkerd `l5d-*` headers too, which never reach the broker). `Authorization`
/// and `Cookie` are not among them.
const SHAREABLE_REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-encoding",
    "accept-language",
    "ngsild-results-restricted",
    "host",
    "user-agent",
    "connection",
    "cache-control",
    "pragma",
    "referer",
    "origin",
    "dnt",
    "te",
    "upgrade-insecure-requests",
    "sec-fetch-dest",
    "sec-fetch-mode",
    "sec-fetch-site",
    "sec-fetch-user",
    "sec-ch-ua",
    "sec-ch-ua-mobile",
    "sec-ch-ua-platform",
    "priority",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-forwarded-proto",
    "x-real-ip",
    "x-request-id",
    "traceparent",
    "tracestate",
];

/// Lets a shared cache keep an anonymous answer `seconds` long: a revoked public grant or a
/// changed value is served stale at most that long, which is why the deployment bounds it
/// (`JC_GATEWAY_PUBLIC_MAX_AGE`, at most [`crate::config::PUBLIC_MAX_AGE_LIMIT`]).
fn share_for(headers: &mut axum::http::HeaderMap, seconds: u32) {
    if let Ok(value) = HeaderValue::from_str(&format!("public, max-age={seconds}")) {
        headers.insert(axum::http::header::CACHE_CONTROL, value);
    }
}

/// The schema documents an answer is described by, as `Link` headers (EP-50, EP-46, EP-49).
///
/// One link per formalism a consumer validates with — the JSON Schema and the SHACL shapes —
/// and one set per model major the endpoint publishes, so an endpoint carrying two majors names
/// four. The target is the schema surface that already serves them, written as a path so it
/// resolves against whatever host the request arrived on rather than against a `Host` header the
/// gateway would have to trust.
///
/// The link says nothing about the caller: it is the same for everyone on one endpoint, which is
/// why `Vary` does not change. What the document behind it holds is projected to the grant when
/// it is fetched (EP-47).
fn describedby(gateway: &Gateway, path: &str) -> Vec<HeaderValue> {
    let Some(endpoint) = data_endpoint(gateway, path) else {
        return Vec::new();
    };
    let base = format!("{}{}", gateway.base_url(), endpoint.base_path);
    let mut majors: Vec<u32> = endpoint.models.iter().map(|model| model.major).collect();
    majors.sort_unstable();
    majors.dedup();

    let mut links = Vec::new();
    for major in majors {
        for artifact in [Artifact::JsonSchema, Artifact::Shacl] {
            let target = format!(
                "<{base}/schema/v{major}/{}>; rel=\"describedby\"; type=\"{}\"",
                artifact.file_name(),
                artifact.media_type()
            );
            if let Ok(value) = HeaderValue::from_str(&target) {
                links.push(value);
            }
        }
    }
    links
}

/// RFC 9745 `Deprecation`.
pub const DEPRECATION: HeaderName = HeaderName::from_static("deprecation");
/// RFC 8594 `Sunset`.
pub const SUNSET: HeaderName = HeaderName::from_static("sunset");

/// The headers that announce an Endpoint's `spec.deprecation` on an answer of `path` (EP-147):
/// the two dates and the author's public link, nothing else. Only the published address
/// carries one; a space's own surface has no manifest of its own to announce it.
fn deprecation_headers(gateway: &Gateway, path: &str) -> Vec<(HeaderName, HeaderValue)> {
    let Some(rest) = path.strip_prefix("/api/endpoint/") else {
        return Vec::new();
    };
    let (slug, rest) = rest.split_once('/').unwrap_or((rest, ""));
    let Some(endpoint) = gateway.resolver.resolve(slug) else {
        return Vec::new();
    };
    match endpoint.deprecation.as_deref() {
        Some(deprecation) if deprecation.covers(surface_of(rest)) => announce(deprecation),
        _ => Vec::new(),
    }
}

/// The surface a path under an Endpoint's address serves; `None` for the record, the schema
/// and access documents, which belong to the Endpoint as a whole.
fn surface_of(rest: &str) -> Option<jc_core::kinds::Representation> {
    use jc_core::kinds::Representation as R;
    [
        ("ngsi-ld/v1", R::NgsiLd),
        ("file.", R::Csv),
        ("ogc/features", R::OgcFeatures),
        ("sta/v1.1", R::Sta),
        ("odata/v4", R::Odata),
        ("ical/v1", R::Ical),
        ("feed/v1", R::Feed),
        ("mcp", R::Mcp),
        ("ws", R::WebSocket),
    ]
    .into_iter()
    .find(|(prefix, _)| rest.starts_with(prefix))
    .map(|(_, surface)| surface)
}

/// `Deprecation: @{since}`, `Sunset: {sunset}` and the `rel="deprecation"` link, each date at
/// midnight UTC. A link is validated when the manifest is (EP-146); one that would still not
/// make a header value is left out rather than sent broken.
fn announce(
    deprecation: &jc_core::kinds::endpoint_deprecation::Deprecation,
) -> Vec<(HeaderName, HeaderValue)> {
    let since = deprecation.since.and_time(chrono::NaiveTime::MIN).and_utc();
    let sunset = deprecation.sunset.format("%a, %d %b %Y 00:00:00 GMT");
    let mut headers = vec![
        (DEPRECATION, format!("@{}", since.timestamp())),
        (SUNSET, sunset.to_string()),
    ];
    if let Some(link) = &deprecation.link {
        headers.push((
            axum::http::header::LINK,
            format!("<{link}>; rel=\"deprecation\"; type=\"text/html\""),
        ));
    }
    headers
        .into_iter()
        .filter_map(|(name, value)| Some((name, HeaderValue::from_str(&value).ok()?)))
        .collect()
}

/// The endpoint whose data this path serves, or `None` where the path serves something else.
///
/// The data representations of EP-50 and nothing else: the NGSI-LD tree, the file downloads, the
/// OGC resource tree and the SensorThings one, on the endpoint surface and on the space surface.
/// The schema documents themselves, the endpoint record, the access surface and the probes are
/// not data and describe themselves.
fn data_endpoint(gateway: &Gateway, path: &str) -> Option<Arc<crate::resolver::Endpoint>> {
    let carries_data = |rest: &str| {
        rest.starts_with("ngsi-ld/v1")
            || rest.starts_with("file.")
            || rest.starts_with("ogc/features")
            || rest.starts_with("sta/v1.1")
    };
    if let Some(rest) = path.strip_prefix("/api/endpoint/") {
        let (slug, rest) = rest.split_once('/')?;
        return carries_data(rest)
            .then(|| gateway.resolver.resolve(slug))
            .flatten();
    }
    if let Some(rest) = path.strip_prefix("/cs/") {
        let (space, rest) = rest.split_once('/')?;
        return carries_data(rest)
            .then(|| gateway.resolver.resolve_space(space))
            .flatten()
            .map(|space| Arc::clone(&space.endpoint));
    }
    None
}

/// Says that every answer here is one caller's (R9, EP-26, T-2261).
///
/// Two callers share a URL and are answered differently, because the answer is the intersection of
/// that URL with their own grants. A shared cache that stored one and replayed it to the other
/// would serve an answer nobody decided — so every answer says whose it is, and on what the
/// difference depends. The edge sets `no-store` on top of this today; the gateway is the
/// enforcement point and does not depend on the edge for it, exactly as it does not depend on the
/// broker for the projection.
///
/// A document the gateway wants revalidated keeps its own `Cache-Control` (the schema artifacts
/// carry `no-cache` with a strong `ETag`, EP-51) and only gains `private` and the `Vary`.
fn keep_out_of_shared_caches(headers: &mut axum::http::HeaderMap) {
    let revalidated = headers
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("no-cache"));
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        match revalidated {
            true => HeaderValue::from_static("private, no-cache"),
            false => HeaderValue::from_static("private, no-store"),
        },
    );
    // `Authorization` is what the answer differs by; the request headers a caller may narrow
    // or shape the answer with are named so a cache keyed on them cannot mix them either.
    // `Accept-Language` is one of them since EP-37: it picks the text of a LanguageProperty in
    // a GeoJSON feature and the title of the OGC landing page. `Accept-Encoding` since T-5847:
    // the compression layer inside this one packs the answer per that header, and its own
    // appended `Vary` would be replaced here. `Link` (the `@context` of a compacted answer) and
    // `Prefer` since T-5900, when an anonymous answer may be shared.
    headers.insert(
        axum::http::header::VARY,
        HeaderValue::from_static(
            "Authorization, Accept, Accept-Language, NGSILD-Results-Restricted, Accept-Encoding, Link, Prefer",
        ),
    );
}
