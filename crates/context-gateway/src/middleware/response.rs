//! What the gateway concluded internally never leaves in a header the caller did not ask
//! for (SP-05, R22).
//!
//! The counterpart of [`super::tenancy::strip_client_headers`]: that one removes what a
//! client said on the way in, this one removes what the gateway said on the way out. One
//! layer rather than a line at every place that builds an answer, so a handler added later
//! cannot forget it.

use crate::app::Gateway;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue};
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

/// What a caller has to know to read the answer they got (CIM 009 clause 6.3.11).
///
/// Sent to everybody, unlike [`RESULTS_RESTRICTED`]: it names types the caller may read and says
/// why they were not queried, which is the difference between an empty list a developer can fix
/// and one they bisect with more filters until it answers (T-1862).
pub const WARNING: HeaderName = HeaderName::from_static("ngsild-warning");

/// The total a broker reports for a query that asked to be counted (CIM 009 clause 6.3.13).
///
/// The gateway never writes it: it is the broker's number, over the entities the broker saw. It
/// is removed from an answer the gateway narrowed, because a total over what was withheld is the
/// same disclosure as the withheld entities (R22, T-2131).
pub const RESULTS_COUNT: HeaderName = HeaderName::from_static("ngsild-results-count");

/// Whether the request asked to be told about narrowing (R22).
pub fn asked_about_narrowing(request: &Request) -> bool {
    request
        .headers()
        .get_all(&RESULTS_RESTRICTED)
        .iter()
        .any(|value| value.as_bytes().eq_ignore_ascii_case(b"true"))
}

/// Removes the tenant from every answer, the narrowing signal from every answer nobody asked for
/// it in, says that no answer of this gateway belongs in a shared cache, and points every answer
/// that carries data at the schema it is described by.
pub async fn scrub(State(gateway): State<Arc<Gateway>>, request: Request, next: Next) -> Response {
    let asked = asked_about_narrowing(&request);
    // Read before the handler runs, because the path is what names the endpoint and the
    // request is moved into it.
    let described = describedby(&gateway, request.uri().path());
    let mut response = next.run(request).await;
    let carries_data = response.status().is_success();
    let headers = response.headers_mut();
    // EP-50: only on an answer that carries data. A refusal that named a schema would tell a
    // caller who was refused that the endpoint exists (EP-03, R20).
    if carries_data {
        for link in described {
            headers.append(axum::http::header::LINK, link);
        }
    }
    // The tenant is an internal name and a probe for which spaces exist. It is pinned on the
    // hop to the broker and it never comes back out, on either surface (SP-05).
    while headers.remove(&super::tenancy::TENANT).is_some() {}
    if !asked {
        while headers.remove(&RESULTS_RESTRICTED).is_some() {}
    }
    keep_out_of_shared_caches(headers);
    response
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
    // a GeoJSON feature and the title of the OGC landing page.
    headers.insert(
        axum::http::header::VARY,
        HeaderValue::from_static(
            "Authorization, Accept, Accept-Language, NGSILD-Results-Restricted",
        ),
    );
}
