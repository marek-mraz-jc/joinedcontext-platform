//! The HTTP surface: what a request passes on its way to the broker (T-0005, EP-01, GW1).
//!
//! One handler serves the whole ETSI resource tree, because every request on it goes
//! through the same six steps and skipping one of them for a "simple" path is how a
//! gateway leaks:
//!
//! 1. remove everything the client claimed about its identity or tenancy (GW20),
//! 2. resolve the slug, or answer 404 without saying whether it exists (EP-03),
//! 3. name the operation, or answer 404: an unmapped path is not an NGSI-LD operation,
//! 4. ask the PDP, and refuse on anything but a rewrite (GW1),
//! 5. check a write payload whole, and narrow a read's query to the grants (GW11, GW17),
//! 6. forward with the tenant pinned, then project the answer back down (R9, R22).

use crate::auth::accounts::ServiceAccounts;
use crate::auth::dataspace_token::{self, Agreements};
use crate::auth::token::{self, Claims, Verifier};
use crate::federation::{Federations, Member};
use crate::handlers::access_routes::{access, access_check};
use crate::handlers::files::{file_csv, file_geojson, file_json, file_xlsx, file_zip};
use crate::handlers::ogc::ogc_features;
use crate::handlers::schema_routes::{
    schema_artifact, schema_index, space_schema_artifact, space_schema_index,
};
use crate::handlers::space_routes::{space_catalog, space_record};
use crate::handlers::sta::sensorthings;
use crate::handlers::{endpoint_surface, schema, space_surface};
use crate::middleware::rate_limit::{self, RateLimiter};
use crate::pdp::evaluator::{self, Constraints, Subject, Verdict};
use crate::pdp::{conditional, write_guard};
use crate::pdp::{geo, projection, temporal, vocabulary, Pdp};
use crate::proxy::{self, Broker};
use crate::resolver::{Endpoint, SlugResolver, Space};
use crate::translators::view_mapping;
use crate::{egress, mcp, middleware::tenancy, operations, query, telemetry};
use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{ACCEPT, ALLOW, AUTHORIZATION, CONTENT_LENGTH, IF_MATCH};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::Router;
use jc_core::kinds::{Audience, Operation, Representation};
use jc_core::ProblemDetails;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;

/// The largest request or response body the gateway will hold in memory.
///
/// A write has to be inspected whole before any of it is applied and a read has to be
/// projected before any of it is sent, so neither can stream today.
// ponytail: one buffer, one limit. Streaming the untouched representations is a separate
// task, and a limit is a better answer than an out-of-memory kill either way.
pub(crate) const MAX_BODY: usize = 8 * 1024 * 1024;

// The header that tells a caller their answer was narrowed by policy, and the layer that
// removes it again when they did not ask (R22).
use crate::middleware::response::{RESULTS_COUNT, RESULTS_RESTRICTED};

/// The methods a view endpoint answers at all (RFC 9110 section 9.2.1).
const SAFE: &[Method] = &[Method::GET, Method::HEAD, Method::OPTIONS];

/// How many entities a file representation asks the broker for at a time (EP-44).
///
/// Large enough that a normal download is one or two round trips, small enough that one
/// page fits comfortably inside `MAX_BODY` whatever the entities look like.
pub(crate) const PAGE: usize = 1_000;

/// The most broker JSON one `file.*` download may read before it is refused (T-0810).
///
/// The row ceiling bounds how many entities a download holds, not how large they are, and the
/// gateway holds the JSON, the flattened table and the file at once. This is the memory the
/// shared enforcement point will spend on one caller; an endpoint's own `maxFileBytes` bounds
/// the file, which is a different and usually smaller number.
pub(crate) const MAX_COLLECTED: u64 = 64 * 1024 * 1024;

/// The deepest offset a paging parameter may name (T-0809).
///
/// A hundred pages is further than any client of this surface pages, and it is the ceiling on
/// what one request can make the shared broker read: the temporal path turns the offset into
/// `lastN`, and nothing in an endpoint's configuration bounds that.
pub(crate) const MAX_SKIP: usize = 100 * PAGE;

/// Everything the surface needs, built once at start-up.
pub struct Gateway {
    /// The endpoint table, replaced whole when the reconciler changes an endpoint.
    pub resolver: SlugResolver,
    /// The broker every endpoint forwards to.
    pub broker: Broker,
    /// What decides.
    pub pdp: Box<dyn Pdp>,
    /// The organization's verified domain, the middle segment of every entity URN.
    pub org_domain: String,
    /// The realm's token verifier; absent means no token is accepted at all (PF-46).
    pub verifier: Option<Arc<Verifier>>,
    /// The service accounts a token's `azp` can name; swapped whole when the repository
    /// changes, so a withdrawn credential stops resolving without a restart (R48).
    accounts: ArcSwap<ServiceAccounts>,
    /// The registrations of every space, swapped whole like the accounts are. Empty is the
    /// ordinary case: a space with no registration federates nothing (EP-70).
    federation: ArcSwap<Federations>,
    /// The data space agreements, swapped whole with the rest: a terminated agreement stops
    /// authorising reads in the same reconcile that withdraws its compiled grants (DS-12).
    agreements: ArcSwap<Agreements>,
    /// The gateway's public base URL, when the deployment names one.
    pub public_url: Option<String>,
    /// The base a rewritten notification endpoint carries, when it is not the public one.
    pub egress_url: Option<String>,
    /// Hosts inside the platform's networks a notification may be delivered to (T-1302).
    pub private_hosts: Vec<String>,
    /// One token bucket per endpoint and caller (EP-20).
    pub rate_limiter: RateLimiter,
    /// The questions a destructive MCP tool is waiting on an answer to (AG-08, T-0849).
    pub elicitations: crate::mcp::elicitation::Elicitations,
    /// Whether a write waits for the Organization's verified domain (PF-41, T-2572).
    pub domain_gate: Arc<crate::domain_gate::DomainGate>,
}

impl Gateway {
    /// A gateway with an empty endpoint table.
    pub fn new(broker: Broker, pdp: Box<dyn Pdp>, org_domain: impl Into<String>) -> Self {
        Self {
            resolver: SlugResolver::new(),
            broker,
            pdp,
            org_domain: org_domain.into(),
            verifier: None,
            accounts: ArcSwap::from_pointee(ServiceAccounts::new()),
            federation: ArcSwap::from_pointee(Federations::new()),
            agreements: ArcSwap::from_pointee(Agreements::new()),
            public_url: None,
            egress_url: None,
            private_hosts: Vec::new(),
            rate_limiter: RateLimiter::new(),
            elicitations: crate::mcp::elicitation::Elicitations::new(),
            domain_gate: Arc::new(crate::domain_gate::DomainGate::new(
                crate::domain_gate::Mode::Report,
            )),
        }
    }

    /// Accepts tokens from one realm, and maps their `azp` to these accounts (PF-46).
    pub fn authenticate(
        mut self,
        verifier: Arc<Verifier>,
        accounts: ServiceAccounts,
        public_url: Option<String>,
    ) -> Self {
        self.verifier = Some(verifier);
        self.accounts.store(Arc::new(accounts));
        self.public_url = public_url;
        self
    }

    /// Hands the broker `egress_url` as the base of every rewritten notification endpoint,
    /// instead of the public URL (R46).
    ///
    /// The two are separate because they answer to different readers. The public URL is what
    /// a caller's token may name as its audience (PF-45); this is what the broker dials to
    /// deliver, and pointing it at the in-cluster Service is what keeps that hop off the
    /// public edge, where it would arrive indistinguishable from any request off the internet.
    pub fn deliver_through(mut self, egress_url: Option<String>) -> Self {
        self.egress_url = egress_url;
        self
    }

    /// Refuses writes by this gate (PF-41, Architecture/03 §3): `report` refuses nothing,
    /// `enforce` refuses a write unless the Organization's domain is verified.
    pub fn gate_writes_on(mut self, gate: Arc<crate::domain_gate::DomainGate>) -> Self {
        self.domain_gate = gate;
        self
    }

    /// Lets notifications reach these hosts although they sit inside the platform's own
    /// networks: an installation's in-cluster subscribers, named one by one (T-1302).
    pub fn deliver_privately_to(mut self, hosts: Vec<String>) -> Self {
        self.private_hosts = hosts;
        self
    }

    /// Every value that names this endpoint as an RFC 8707 resource.
    fn audiences_for(&self, endpoint: &Endpoint) -> Vec<String> {
        audiences_of(
            &endpoint.slug,
            &endpoint.base_path,
            self.public_url.as_deref(),
        )
    }

    /// Replaces the federation table, the same way the accounts are replaced (EP-70).
    pub fn replace_federation(&self, federations: Federations) {
        self.federation.store(Arc::new(federations));
    }

    /// Replaces the agreement table (DS-12).
    ///
    /// Called by the reaper with the rest, which is what makes "revokes outstanding transfer
    /// tokens" true without anything having to find those tokens: they name an agreement the
    /// gateway no longer serves.
    pub fn replace_agreements(&self, agreements: Agreements) {
        self.agreements.store(Arc::new(agreements));
    }

    /// The registrations of one space, for a surface that lists what an endpoint federates.
    /// Names only: an address never leaves the platform (EP-71).
    pub fn members_of(&self, project: &str, space: &str) -> Vec<Member> {
        self.federation.load().members(project, space).to_vec()
    }

    /// The registrations of one space that ask for the caller's own identity (PF-48).
    fn caller_identity_members(&self, project: &str, space: &str) -> Vec<String> {
        self.federation
            .load()
            .needing_caller_identity(project, space)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Replaces the service account table (R48, PF-46).
    ///
    /// Called by the reaper when the repository changed: `azp` values the repository no
    /// longer names stop resolving from the next request on.
    pub fn replace_accounts(&self, accounts: ServiceAccounts) {
        self.accounts.store(Arc::new(accounts));
    }

    /// Replaces the endpoint table (EP-19).
    pub fn serve(self, endpoints: impl IntoIterator<Item = Endpoint>) -> Self {
        self.resolver.replace(endpoints);
        self
    }

    /// Replaces the space table, which is the `/cs/{space}` surface (SP-01, EP-19).
    pub fn serve_spaces(self, spaces: impl IntoIterator<Item = Space>) -> Self {
        self.resolver.replace_spaces(spaces);
        self
    }

    /// The gateway's public base URL, or the empty string when none is configured.
    pub(crate) fn base_url(&self) -> &str {
        self.public_url.as_deref().unwrap_or_default()
    }

    /// The base a rewritten notification endpoint carries: the egress URL where the
    /// deployment names one, and the public URL otherwise (R46).
    fn egress_base(&self) -> &str {
        self.egress_url
            .as_deref()
            .unwrap_or_else(|| self.base_url())
    }
}

/// The router: two probes and the endpoint surface.
pub fn router(gateway: Arc<Gateway>) -> Router {
    // The outermost layer resolves the endpoint of a path itself, to write the `describedby`
    // link no handler can then forget (EP-50).
    let gateway_for_scrub = Arc::clone(&gateway);
    // The recorder belongs to the surface rather than to `main`: without it every
    // `metrics::` call in the process is a no-op, and a test that builds a router would
    // measure nothing while looking like it measured zero (OPS-16).
    telemetry::install();
    Router::new()
        // Both spellings, because EP-01 writes the base URL with the trailing slash and
        // every client that stores a base URL drops it.
        .route("/api/endpoint/{slug}", get(endpoint_record))
        .route("/api/endpoint/{slug}/", get(endpoint_record))
        .route("/api/endpoint/{slug}/ngsi-ld/v1/{*rest}", any(ngsi_ld))
        .route("/api/endpoint/{slug}/access", get(access))
        .route("/api/endpoint/{slug}/mcp", post(mcp_message))
        .route("/api/endpoint/{slug}/access/check", post(access_check))
        // Where a rewritten notification endpoint points, and the only surface the
        // broker calls rather than answers (R46).
        .route(
            "/api/endpoint/{slug}/egress/notifications",
            post(egress::notifications::deliver),
        )
        .route("/api/endpoint/{slug}/file.geojson", get(file_geojson))
        .route("/api/endpoint/{slug}/file.json", get(file_json))
        .route("/api/endpoint/{slug}/file.csv", get(file_csv))
        .route("/api/endpoint/{slug}/file.xlsx", get(file_xlsx))
        .route("/api/endpoint/{slug}/file.zip", get(file_zip))
        // The OGC surface is one handler over its own resource tree: three spellings of the
        // landing page, because a GIS client stores whichever one it was given (EP-29).
        .route("/api/endpoint/{slug}/ogc/features", any(ogc_features))
        .route("/api/endpoint/{slug}/ogc/features/", any(ogc_features))
        // The SensorThings surface, the same shape: one handler over its own resource tree.
        .route("/api/endpoint/{slug}/sta/v1.1", any(sensorthings))
        .route("/api/endpoint/{slug}/sta/v1.1/", any(sensorthings))
        .route("/api/endpoint/{slug}/sta/v1.1/{*rest}", any(sensorthings))
        .route(
            "/api/endpoint/{slug}/ogc/features/{*rest}",
            any(ogc_features),
        )
        .route("/cs", get(space_catalog))
        .route("/cs/{space}", get(space_record))
        .route("/cs/{space}/ngsi-ld/v1/{*rest}", any(space_ngsi_ld))
        .route("/cs/{space}/mcp", post(space_mcp_message))
        // SP-04: `schema/` is a child of every space, and the record advertises it. The
        // same two routes as the endpoint surface, because it is the same surface named
        // by its space (SP-03).
        .route("/cs/{space}/schema/index.json", get(space_schema_index))
        .route(
            "/cs/{space}/schema/{version}/{artifact}",
            get(space_schema_artifact),
        )
        .route(
            "/api/endpoint/{slug}/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route("/api/endpoint/{slug}/schema/index.json", get(schema_index))
        .route(
            "/api/endpoint/{slug}/schema/{version}/{artifact}",
            get(schema_artifact),
        )
        // Every endpoint surface passes the limiter; the two probes do not, so a full
        // bucket can never make a pod look unhealthy (EP-20).
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&gateway),
            rate_limit::enforce,
        ))
        .route("/healthz", get(ok))
        .route("/livez", get(ok))
        // OPS-16: what `components/monitoring` scrapes, on the port it already names. Outside
        // every guard above, because it is reachable only from inside the cluster and carries
        // no request of anyone's; and outermost of the two layers, so a request refused by the
        // rate limiter is still counted.
        .route("/metrics", get(telemetry::metrics))
        .with_state(gateway)
        .fallback(missing)
        // Outside every handler, so nothing the gateway concluded for itself — the tenant,
        // and the narrowing signal nobody asked for — leaves in a header (SP-05, R22).
        .layer(axum::middleware::from_fn_with_state(
            gateway_for_scrub,
            crate::middleware::response::scrub,
        ))
        .layer(axum::middleware::from_fn(telemetry::record))
}

async fn ok() -> &'static str {
    "ok"
}

/// Anything off the surface answers the same problem document as an unknown slug.
async fn missing() -> Response<Body> {
    ProblemDetails::not_found().into_response()
}

/// The ETSI resource tree under an endpoint slug, with every refusal rendered the way an
/// NGSI-LD client reads it.
async fn ngsi_ld(
    State(gateway): State<Arc<Gateway>>,
    Path((slug, _rest)): Path<(String, String)>,
    mut request: Request,
) -> Response<Body> {
    // Before routing, before authentication, before anything reads a header (EP-21).
    tenancy::strip_client_headers(&mut request);
    let admitted = admit(
        &gateway,
        &slug,
        Some(Representation::NgsiLd),
        request.headers(),
    );
    let prefix = format!("/api/endpoint/{slug}/ngsi-ld/v1");
    as_ngsi_ld_error(serve_ngsi_ld(&gateway, admitted, &prefix, request).await).await
}

/// The same tree under a context space name (SP-03).
///
/// A stock NGSI-LD client pointed at `/cs/{space}` works unmodified, and it works through
/// the same handler an endpoint slug goes through: the only difference between the two
/// surfaces is the name in the path and the audience the token has to carry (SP-01, R15).
async fn space_ngsi_ld(
    State(gateway): State<Arc<Gateway>>,
    Path((space, _rest)): Path<(String, String)>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let admitted = admit_space(
        &gateway,
        &space,
        Some(Representation::NgsiLd),
        request.headers(),
    )
    .map(|(space, subject)| (Arc::clone(&space.endpoint), subject));
    let prefix = format!("/cs/{space}/ngsi-ld/v1");
    as_ngsi_ld_error(serve_ngsi_ld(&gateway, admitted, &prefix, request).await).await
}

/// Runs one NGSI-LD request through the very pipeline the HTTP surfaces run (T-0166, SP-16).
///
/// The MCP façade turns a tool call into the NGSI-LD request it stands for and hands it
/// here, so the PDP, the query narrowing, the write guard and the response projection are
/// literally the same code for an agent as for any other client, and discovery can never
/// drift from enforcement.
///
/// `path` is the part under `/ngsi-ld/v1`, already percent-encoded; only the caller's own
/// token crosses over, because a header of the MCP request describes that request and not
/// this one.
pub async fn ngsi_ld_request(
    gateway: Arc<Gateway>,
    endpoint: &Endpoint,
    method: Method,
    path: &str,
    query: &str,
    body: Option<Vec<u8>>,
    authorization: Option<HeaderValue>,
) -> Response<Body> {
    let prefix = format!("{}/ngsi-ld/v1", endpoint.base_path);
    let uri = match query.is_empty() {
        true => format!("{prefix}{path}"),
        false => format!("{prefix}{path}?{query}"),
    };
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(token) = authorization {
        builder = builder.header(AUTHORIZATION, token);
    }
    if body.is_some() {
        builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
    }
    let Ok(request) = builder.body(body.map_or_else(Body::empty, Body::from)) else {
        return ProblemDetails::bad_request()
            .with_detail("the arguments do not form a request this endpoint can serve")
            .into_response();
    };

    // Admitted again from the token alone, so a tool call carries no privilege the same
    // call over HTTP would not have (EP-26). The record says which of the two surfaces it
    // belongs to, so a space instance re-resolves through the space table (SP-14).
    let admitted = match endpoint.base_path.strip_prefix("/cs/") {
        Some(space) => admit_space(
            &gateway,
            space,
            Some(Representation::Mcp),
            request.headers(),
        )
        .map(|(space, subject)| (Arc::clone(&space.endpoint), subject)),
        None => admit(
            &gateway,
            &endpoint.slug,
            Some(Representation::Mcp),
            request.headers(),
        ),
    };
    as_ngsi_ld_error(serve_ngsi_ld(&gateway, admitted, &prefix, request).await).await
}

/// The error types of CIM 009 clause 5.5.2 the gateway's own refusals map to, by status.
///
/// The clause defines no type for 401, 403 (other than the two query-size ones), 415 or
/// 502, so those keep the joinedcontext type URI: a made-up ETSI URI would be a lie a
/// client could act on, and a joinedcontext one is at least documented.
fn ngsi_ld_error_type(status: u16) -> Option<&'static str> {
    Some(match status {
        400 => "https://uri.etsi.org/ngsi-ld/errors/BadRequestData",
        404 => "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound",
        409 => "https://uri.etsi.org/ngsi-ld/errors/AlreadyExists",
        422 => "https://uri.etsi.org/ngsi-ld/errors/OperationNotSupported",
        500 => "https://uri.etsi.org/ngsi-ld/errors/InternalError",
        503 => "https://uri.etsi.org/ngsi-ld/errors/LdContextNotAvailable",
        _ => return None,
    })
}

/// Re-renders a problem document the way CIM 009 clause 5.5.3 wants it (T-0272).
///
/// The clause is explicit: errors on the NGSI-LD surface use `application/json`, not the
/// RFC 7807 media type, and carry `type`, `title` and `detail`. The broker already answers
/// that way; this makes the gateway's own refusals indistinguishable from it, so a client
/// that keys on `type` learns the same thing whichever of the two refused. The Portal API
/// keeps `application/problem+json`: a different surface, a different contract.
async fn as_ngsi_ld_error(response: Response<Body>) -> Response<Body> {
    let is_problem = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(jc_core::PROBLEM_JSON));
    if !is_problem {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return ProblemDetails::internal().into_response();
    };
    let Ok(mut problem) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };

    if let Some(members) = problem.as_object_mut() {
        if let Some(etsi) = ngsi_ld_error_type(parts.status.as_u16()) {
            members.insert("type".to_owned(), Value::String(etsi.to_owned()));
        }
        // Clause 6.3.3: `detail` is one of the three terms an error carries; a refusal that
        // names no reason still says so in a sentence rather than omitting the member.
        if !members.contains_key("detail") {
            let title = members.get("title").cloned().unwrap_or(Value::Null);
            members.insert("detail".to_owned(), title);
        }
    }
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    match serde_json::to_vec(&problem) {
        Ok(rendered) => proxy::with_body(parts, rendered),
        Err(_) => Response::from_parts(parts, Body::from(bytes)),
    }
}

async fn serve_ngsi_ld(
    gateway: &Gateway,
    admitted: Result<(Arc<Endpoint>, Subject), Box<Response<Body>>>,
    prefix: &str,
    mut request: Request,
) -> Response<Body> {
    let (endpoint, subject) = match admitted {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let method = request.method().clone();
    let uri = request.uri().clone();
    // The path as it came off the wire: decoding it would corrupt the URN in an entity
    // path, and re-encoding it is not the gateway's business.
    let path = uri
        .path()
        .strip_prefix(prefix)
        .unwrap_or_default()
        .to_owned();
    let params = query::parse(uri.query().unwrap_or_default());
    let details = query::first(&params, "details") == Some("true");

    // EP-54: a view endpoint is read only, and it says so before the request is decided.
    // A write in the target model has no source entity to reconstruct, so refusing it is
    // the whole answer rather than the first half of one.
    if endpoint.view_mapping.is_some() && !SAFE.contains(&method) {
        let mut refusal = view_mapping::read_only().into_response();
        refusal
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("GET, HEAD, OPTIONS"));
        return refusal;
    }

    let Some(operation) = operations::operation_of(&method, &path, details) else {
        return ProblemDetails::not_found().into_response();
    };

    // GW31: a type or q the specification refuses is refused as the caller sent it, before an
    // empty intersection with the grants answers it with nothing.
    if operation == Operation::QueryEntity {
        if let Some(why) = query::malformed(&params) {
            return ProblemDetails::bad_request()
                .with_detail(why)
                .into_response();
        }
        // GW33: a query naming no selector is `400 BadRequestData` (CIM 009 5.7.2.4), an `id`
        // list or an `idPattern` alone included. The grants would have supplied a type and
        // answered it, which is the narrowing this refusal replaces: a grant decides which of
        // the well-formed answers a caller sees, never which status code the surface returns.
        if query::unselected(&params) {
            return ProblemDetails::bad_request()
                .with_detail(
                    "a query names at least one of type, attrs, q or georel; an id list or \
                     idPattern alone is not a selector (CIM 009 5.7.2.4). Ask \
                     /ngsi-ld/v1/types for the types this endpoint serves, or retrieve one \
                     entity at /ngsi-ld/v1/entities/{id}",
                )
                .into_response();
        }
    }

    let verdict = gateway
        .pdp
        .decide(&subject, operation, &query::requested(&params), &endpoint);
    // The audit line names the principal the gateway established, never the one the
    // request claimed (PF-46).
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        principal = %principal_of(&subject),
        // Empty for every caller that is not acting under an agreement; DS-13 is about the
        // records that are.
        agreement = subject.agreement.as_deref().unwrap_or_default(),
        operation = %operation.as_str(),
        allowed = !verdict.is_deny(),
        "decision"
    );
    let Verdict::Rewrite(constraints) = verdict else {
        return refused(operation, &path);
    };

    // PF-41, T-2572: under `domainVerification: enforce` a write waits for the Organization's
    // verified domain. After the decision, so a caller who may not write at all learns only
    // that; before the body is read, so nothing of a refused write reaches the broker. Every
    // NGSI-LD door comes through here, the MCP write tools included. Reads are never refused.
    if operation.is_write() {
        if let Some(refusal) = gateway.domain_gate.refusal(&gateway.org_domain) {
            tracing::info!(slug = %endpoint.slug, %operation, "write refused: the domain is not verified");
            return refusal.into_response();
        }
    }

    // PF-48: a registration asking for `caller` identity needs the caller's own token
    // rewritten for the member's audience, which is an RFC 8693 exchange this platform does
    // not have yet. Forwarding as the hub's own service account instead would answer with data
    // the caller was never granted on that member, so the read stops here.
    //
    // After the decision, not before it: a caller who may not use this endpoint at all learns
    // that it is refused, never that it federates (EP-03, EP-23).
    let waiting = gateway.caller_identity_members(&endpoint.project, &endpoint.space);
    if !waiting.is_empty() {
        tracing::warn!(
            slug = %endpoint.slug,
            space = %endpoint.space,
            registrations = %waiting.join(", "),
            "caller identity is registered but token exchange is not implemented"
        );
        return ProblemDetails::new(501, "federation-identity-unavailable", "Not Implemented")
            .with_detail(
                "a source registered on this space forwards the caller's own token, which \
                 needs an RFC 8693 token exchange this deployment does not have yet; \
                 spec.federation.identity: serviceAccount is served today",
            )
            .into_response();
    }

    // AG-85, T-2299: `geoproperty` and `geometryProperty` each choose the attribute a
    // decision is taken on, so each is judged against the grants that were just read rather
    // than forwarded. Before the broker is asked, and on this one path, so the NGSI-LD
    // surface and the MCP tool refuse in the same words.
    if let Some(why) = query::geo_refusal(&params, &constraints) {
        return ProblemDetails::bad_request()
            .with_detail(why)
            .into_response();
    }

    // The caller asked for a period no grant reaches. The request was well formed and its
    // answer is genuinely nothing, so it is answered here: forwarding it without a
    // temporal window would ask the broker for everything (GW26).
    if constraints.empty {
        return match (
            operations::addressed_entity(&path),
            operations::addressed_vocabulary(&path),
        ) {
            (Some(_), _) | (_, Some(_)) => ProblemDetails::not_found().into_response(),
            _ => empty_list(&constraints),
        };
    }

    // EP-25: discovery is enforcement. A type or attribute this endpoint does not serve is not
    // found, and the broker is not asked at all, so neither the status nor the timing of the
    // answer can be read as a directory of the names the grants withhold (T-2134).
    if let Some(name) = operations::addressed_vocabulary(&path) {
        if !vocabulary::reaches(operation, &query::decode(name), &constraints) {
            tracing::info!(
                slug = %endpoint.slug,
                %operation,
                "a vocabulary name this endpoint does not serve is answered as not found"
            );
            return ProblemDetails::not_found().into_response();
        }
    }

    // The identifier in the path belongs to this organization and this space or the
    // request is malformed, whichever verb carries it (PF-10, PF-42).
    if let Some(raw) = operations::addressed_entity(&path) {
        let id = query::decode(raw);
        if let Err(refusal) =
            write_guard::check_identifier(&id, None, &endpoint.space, &gateway.org_domain)
        {
            return ProblemDetails::from(refusal).into_response();
        }
        // A write to an id outside the grant's types and patterns is refused here, before a
        // body is read (T-0806, GW11, R24).
        if operation.is_write() {
            if let Err(refusal) = write_guard::check_granted_id(&id, &constraints) {
                tracing::info!(slug = %endpoint.slug, %refusal, "write refused");
                return ProblemDetails::from(refusal).into_response();
            }
        } else if write_guard::check_granted_id(&id, &constraints).is_err() {
            // A read of an id the grant does not select is the same miss an unknown id is, and
            // the broker is not asked at all: a retrieve carries no type, so asking would mean
            // trusting the answer, and asking alone would tell the caller the type exists
            // (EP-26, R20, T-2130).
            return ProblemDetails::not_found().into_response();
        }
    }

    if let Err(problem) = tenancy::pin_tenant(&mut request, &endpoint.space) {
        tracing::error!(space = %endpoint.space, %problem, "endpoint names an unusable space");
        return ProblemDetails::internal().into_response();
    }

    let (mut parts, body) = request.into_parts();
    let Ok(sent) = axum::body::to_bytes(body, MAX_BODY).await else {
        return ProblemDetails::bad_request()
            .with_detail("request body is unreadable or larger than the gateway accepts")
            .into_response();
    };

    // A payload the surface does not accept is refused for its media type, before anything
    // tries to parse it (CIM 009 clauses 6.3.2 and 6.3.5). Stricter than forwarding, not
    // looser: the write guard sees every body that gets past this line.
    if operation.is_write() && !sent.is_empty() && !accepted_payload(&parts.headers) {
        return unsupported_media_type().into_response();
    }

    // A subscription is a standing query, not an entity: it is narrowed to the grants and
    // its delivery routed back through the gateway, rather than checked as a write payload
    // whose members would all read as ungranted attributes (GW27, R46).
    let subscribing = matches!(
        operation,
        Operation::CreateSubscription | Operation::UpdateSubscription
    );
    let mut sent = sent.to_vec();
    if subscribing && !sent.is_empty() {
        match narrowed_subscription(
            &sent,
            &constraints,
            &endpoint,
            gateway.egress_base(),
            &gateway.private_hosts,
            operation,
        ) {
            Ok(narrowed) => sent = narrowed,
            Err(problem) => return problem.into_response(),
        }
        parts.headers.remove(CONTENT_LENGTH);
        parts
            .headers
            .insert(CONTENT_LENGTH, HeaderValue::from(sent.len() as u64));
    } else if operation.is_write() && !sent.is_empty() {
        if let Some(problem) =
            refuse_write(&sent, &path, &constraints, &endpoint, &gateway.org_domain)
        {
            return problem.into_response();
        }
    }

    // CIM 009 clause 5.6.9 puts a query's selector in the body, and a filter is a read wherever
    // it is written. The body is narrowed here rather than in the decision point because the
    // decision is taken before a body is read (T-2259, T-1862).
    let mut constraints = constraints;
    // Both POST query forms: `/entityOperations/query` and its temporal sibling, which is the
    // same operation read from a body (CIM 009 clauses 5.6.9 and 5.6.12). A selector in a body
    // may no more widen a read than one in a query string, so neither reaches the broker
    // unnarrowed (AG-85).
    let selects_in_body = operation == Operation::QueryBatch
        || (operation == Operation::QueryTemporal && path == "/temporal/entityOperations/query");
    if selects_in_body && !sent.is_empty() {
        match narrowed_batch_query(&sent, *constraints) {
            Ok((narrowed, decided)) => {
                constraints = decided;
                if constraints.empty {
                    tracing::info!(
                        slug = %endpoint.slug,
                        "a batch query selects on what this endpoint does not serve; \
                         the broker is not asked"
                    );
                    return empty_list(&constraints);
                }
                sent = narrowed;
                parts.headers.remove(CONTENT_LENGTH);
                parts
                    .headers
                    .insert(CONTENT_LENGTH, HeaderValue::from(sent.len() as u64));
            }
            Err(problem) => return problem.into_response(),
        }
    }

    // A grant that decides from the stored entity cannot decide a batch, which names its
    // entities in the payload and not in the path (T-0807).
    if let Some(problem) = conditional::batch_refusal(operation, &constraints) {
        tracing::info!(
            slug = %endpoint.slug,
            %operation,
            "batch write refused: the grant decides from the stored entity"
        );
        return problem.into_response();
    }

    // A grant that decides from the stored entity, or a caller that sent `If-Match`, turns
    // the write into a read, an evaluation and then a conditional write (R45, GW16).
    if conditional::required(operation, &path, &parts.headers, &constraints) {
        match conditional::evaluate(&gateway.broker, &path, &parts.headers, &constraints).await {
            conditional::Precondition::Refuse(problem) => return problem.into_response(),
            conditional::Precondition::Forward(etag) => {
                // The caller's own tag was just checked against the same read, so what goes
                // upstream is the tag the gateway read and nothing the client sent.
                parts.headers.remove(IF_MATCH);
                if let Some(etag) = etag {
                    parts.headers.insert(IF_MATCH, etag);
                }
            }
        }
    }

    let sent_query = if operation.is_write() || vocabulary::describes(operation) {
        // A discovery request takes `details` and nothing else (CIM 009 clause 5.7.10): the
        // grants' `type`, `attrs` and `q` say which entities may be read, and a broker asked for
        // the vocabulary of a type filter would either ignore them or answer something else.
        // The narrowing of these answers happens on their own members instead (T-2134).
        query::passthrough(&params)
    } else {
        query::upstream(&params, &constraints, &[])
    };
    // The caller writes in the target model; the broker knows only the source (DM-51).
    let sent_query = match &endpoint.view_mapping {
        None => sent_query,
        Some(mapping) => match invert_query(mapping, &sent_query) {
            Ok(inverted) => inverted,
            Err(problem) => return problem.into_response(),
        },
    };
    let target = format!("/ngsi-ld/v1{path}?{sent_query}");
    let answer = match gateway
        .broker
        .send(method, &target, parts.headers, Body::from(sent))
        .await
    {
        Ok(answer) => answer,
        Err(error) => return ProblemDetails::from(error).into_response(),
    };

    // The answer is judged in the model it arrives in: a view endpoint's grant names the class the
    // view serves, and the broker answers in the class it stores, exactly as `invert_query`
    // already assumes for the query's own `type` (DM-51, T-2241).
    let judged = match &endpoint.view_mapping {
        None => constraints.clone(),
        Some(mapping) => {
            Box::new(constraints.in_source_model(&mapping.target_class, &mapping.source_class))
        }
    };
    let projected = project_answer(answer, operation, &judged).await;
    match &endpoint.view_mapping {
        None => projected,
        Some(mapping) => translated(projected, mapping).await,
    }
}

/// Rewrites the parameters of a query that name attributes of the model (DM-51).
///
/// `type` is not translated per value: the entity type is the mapping's own class pair, so
/// the target class the caller asked for is replaced by the source class wholesale.
fn invert_query(
    mapping: &view_mapping::ViewMapping,
    query: &str,
) -> Result<String, Box<ProblemDetails>> {
    let mut out: Vec<String> = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let Some((name, value)) = pair.split_once('=') else {
            out.push(pair.to_owned());
            continue;
        };
        let decoded = query::decode(value);
        let inverted = match name {
            "attrs" => mapping.invert_attrs(&decoded)?,
            "q" => mapping.invert_q(&decoded)?,
            "geoproperty" => mapping.invert_geo_property(&decoded)?,
            "type" => mapping.source_class.clone(),
            _ => {
                out.push(pair.to_owned());
                continue;
            }
        };
        // An empty `attrs` asks the broker for everything rather than for nothing, so a
        // projection made entirely of locally produced slots drops out instead.
        if inverted.is_empty() && name == "attrs" {
            continue;
        }
        out.push(format!("{name}={}", query::encode(&inverted)));
    }
    Ok(out.join("&"))
}

/// Rebuilds a projected answer in the target model (EP-54).
///
/// After the projection, never before: the grants and the geo and temporal filters are
/// written in the source model's names, which is the only model the repository declares.
async fn translated(answer: Response<Body>, mapping: &view_mapping::ViewMapping) -> Response<Body> {
    let (parts, body) = answer.into_parts();
    if !parts.status.is_success() {
        return Response::from_parts(parts, body);
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        tracing::error!("the broker's answer is larger than the gateway can translate");
        return ProblemDetails::internal().into_response();
    };
    let Ok(mut payload) = serde_json::from_slice::<Value>(&bytes) else {
        return proxy::with_body(parts, bytes.to_vec());
    };
    mapping.translate(&mut payload);
    match serde_json::to_vec(&payload) {
        Ok(bytes) => proxy::with_body(parts, bytes),
        Err(error) => {
            tracing::error!(%error, "the translated answer does not serialize");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The request payload media types the NGSI-LD surface accepts (CIM 009 clause 6.3.5).
const PAYLOAD_TYPES: &[&str] = &[
    "application/json",
    "application/ld+json",
    "application/merge-patch+json",
];

/// Whether the request names a payload media type the surface accepts, parameters aside.
fn accepted_payload(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().trim())
        .is_some_and(|media| {
            PAYLOAD_TYPES
                .iter()
                .any(|accepted| media.eq_ignore_ascii_case(accepted))
        })
}

/// 415, the answer to a payload in a media type the surface does not take (clause 6.3.2).
fn unsupported_media_type() -> ProblemDetails {
    ProblemDetails::new(415, "unsupported-media-type", "Unsupported Media Type").with_detail(
        "the request payload must be application/json, application/ld+json or application/merge-patch+json",
    )
}

/// GW1 and R20: a refused read of one entity is indistinguishable from a miss; everything
/// else is refused openly, without naming the rule that refused it.
fn refused(operation: Operation, path: &str) -> Response<Body> {
    if !operation.is_write() && operations::addressed_entity(path).is_some() {
        ProblemDetails::not_found().into_response()
    } else {
        ProblemDetails::forbidden().into_response()
    }
}

/// Checks a write payload whole and answers with the problem that refuses it, or nothing
/// (GW17, GW18).
///
/// The body of a `PATCH .../attrs/{name}` is the bare attribute value, so it is wrapped
/// back under its name before the grant is consulted; anything else is an entity, or a
/// batch of them.
fn refuse_write(
    body: &[u8],
    path: &str,
    constraints: &Constraints,
    endpoint: &Endpoint,
    org_domain: &str,
) -> Option<ProblemDetails> {
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return Some(ProblemDetails::bad_request().with_detail("request body is not JSON"));
    };
    let payload = match operations::targeted_attribute(path) {
        Some(name) => serde_json::json!({ name: payload }),
        None => payload,
    };

    let entities: Vec<&Value> = match &payload {
        Value::Array(entities) => entities.iter().collect(),
        other => vec![other],
    };
    // An addressed write changes the entity in its path and nothing else (GW17, PF-44).
    let addressed = operations::addressed_entity(path).map(query::decode);
    for entity in entities {
        if let (Some(path_id), Some(object)) = (&addressed, entity.as_object()) {
            let named = object.get("id").or_else(|| object.get("@id"));
            if named.is_some_and(|id| id.as_str() != Some(path_id.as_str())) {
                return Some(
                    ProblemDetails::bad_request()
                        .with_detail("the body names another entity than the path"),
                );
            }
            if let Some(kind) = object.get("type").or_else(|| object.get("@type")) {
                if let Err(refusal) = write_guard::check_identifier(
                    path_id,
                    Some(kind.as_str().unwrap_or_default()),
                    &endpoint.space,
                    org_domain,
                ) {
                    return Some(ProblemDetails::from(refusal));
                }
            }
        }
        // What an Endpoint does not show cannot be changed through it (EP-61, GW17).
        if let Some(hidden) = entity.as_object().and_then(|object| {
            object
                .keys()
                .find(|key| endpoint.hidden_attributes.contains(*key))
        }) {
            let refusal = write_guard::Refusal::AttributeOutsideGrant(hidden.clone());
            tracing::info!(slug = %endpoint.slug, %refusal, "write refused");
            return Some(ProblemDetails::from(refusal));
        }
        // A batch delete is an array of URN strings: each is an identifier and nothing else
        // (T-0806).
        let outcome = if let Some(raw) = entity.as_str() {
            write_guard::check_identifier(raw, None, &endpoint.space, org_domain)
                .and_then(|()| write_guard::check_granted_id(raw, constraints))
        } else if entity.get("id").is_some() || entity.get("@id").is_some() {
            write_guard::check(entity, constraints, &endpoint.space, org_domain)
        } else {
            write_guard::check_fragment(entity, constraints)
        };
        if let Err(refusal) = outcome {
            tracing::info!(slug = %endpoint.slug, %refusal, "write refused");
            return Some(ProblemDetails::from(refusal));
        }
    }
    None
}

/// Narrows a subscription payload to the grants and routes its delivery through the
/// gateway (GW27, R46).
fn narrowed_subscription(
    body: &[u8],
    constraints: &Constraints,
    endpoint: &Endpoint,
    base_url: &str,
    private_hosts: &[String],
    operation: Operation,
) -> Result<Vec<u8>, Box<ProblemDetails>> {
    let mut payload: Value = serde_json::from_slice(body).map_err(|_| {
        Box::new(ProblemDetails::bad_request().with_detail("request body is not JSON"))
    })?;
    egress::notifications::narrow_subscription(
        &mut payload,
        constraints,
        endpoint,
        base_url,
        private_hosts,
        operation == Operation::CreateSubscription,
    )?;
    serde_json::to_vec(&payload).map_err(|error| {
        tracing::error!(%error, "the narrowed subscription does not serialize");
        Box::new(ProblemDetails::internal())
    })
}

/// The body of a batch query, narrowed to the grants, with the decision it leaves behind
/// (T-2259; EP-26, MP-02, R9).
///
/// `POST /entityOperations/query` is the one read whose selector travels in the body: the
/// `type` of each entry, `attrs` and `q` decide which entities come back, and a broker reads
/// them in preference to anything in the query string. So the same three narrowings the query
/// string gets are applied here, and the names the body filters on run through the rule that
/// takes a type out of a query it may not be filtered on. The returned constraints carry
/// `empty` when nothing is left to ask for, which is the `200 []` an attribute that does not
/// exist would answer.
fn narrowed_batch_query(
    body: &[u8],
    constraints: Constraints,
) -> Result<(Vec<u8>, Box<Constraints>), Box<ProblemDetails>> {
    let mut payload: Value = serde_json::from_slice(body).map_err(|_| {
        Box::new(ProblemDetails::bad_request().with_detail("request body is not JSON"))
    })?;
    let asked = evaluator::Request {
        referenced: query::referenced_in_body(&payload),
        ..Default::default()
    };
    let Verdict::Rewrite(mut decided) =
        crate::pdp::drop_types_that_may_not_be_filtered(constraints, &asked)
    else {
        // The rule only ever narrows a REWRITE; a DENY here would mean the decision changed
        // shape between the two calls.
        return Err(Box::new(ProblemDetails::internal()));
    };
    if decided.empty {
        return Ok((body.to_vec(), decided));
    }

    // Each entry selects by type, by id, or by both. An entry naming a type outside the grants
    // is not this caller's to ask for; an entry naming only an id stays, and the answer's own
    // type guard judges what comes back for it (T-2130).
    if let Some(entities) = payload.get_mut("entities").and_then(Value::as_array_mut) {
        let named = entities.len();
        if !decided.types.is_empty() {
            entities.retain(
                |selector| match selector.get("type").and_then(Value::as_str) {
                    Some(asked) => decided.types.contains(projection::term(asked)),
                    None => true,
                },
            );
        }
        if entities.len() < named {
            decided.restricted = true;
        }
        if entities.is_empty() {
            decided.empty = true;
            return Ok((body.to_vec(), decided));
        }
    }

    // The grants' own filter joins the caller's, so a body can only ever narrow further.
    if let Some(q) = &decided.q {
        let caller = payload.get("q").and_then(Value::as_str).map(str::to_owned);
        let joined = evaluator::conjoin(caller.as_deref(), std::slice::from_ref(q));
        if let Some(joined) = joined {
            payload["q"] = Value::String(joined);
        }
    }

    let narrowed = serde_json::to_vec(&payload).map_err(|error| {
        tracing::error!(%error, "the narrowed batch query does not serialize");
        Box::new(ProblemDetails::internal())
    })?;
    Ok((narrowed, decided))
}

/// Cuts the broker's answer down to what the grants cover (R9, R22, R24).
///
/// A single entity the grants do not reach is a miss, not a refusal: the caller must not
/// learn that it exists (R20).
/// The answer to a read whose granted window the caller's request does not reach (GW26).
fn empty_list(constraints: &Constraints) -> Response<Body> {
    let mut response = json_response(&Value::Array(Vec::new()));
    if constraints.restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    warn_about_dropped_types(response.headers_mut(), constraints);
    response
}

/// Says which types this query was not allowed to select on, and why (CIM 009 clause 6.3.11,
/// T-1862 rule 4).
///
/// An answer narrowed by the filter rule is an empty list or a shorter one, and nothing in it says
/// so. A developer reading that guesses, and the way they guess is by filtering harder — which is
/// the oracle the rule closed. So the answer says it out loud, naming only types the caller may
/// read.
fn warn_about_dropped_types(headers: &mut HeaderMap, constraints: &Constraints) {
    if constraints.dropped.is_empty() {
        return;
    }
    let named: Vec<&str> = constraints.dropped.iter().map(String::as_str).collect();
    let warning = format!(
        "199 joinedcontext \"{} {} not queried: this request selects or orders on an attribute \
         they do not serve on this endpoint\"",
        named.join(", "),
        match named.len() {
            1 => "was",
            _ => "were",
        }
    );
    if let Ok(value) = HeaderValue::from_str(&warning) {
        headers.insert(crate::middleware::response::WARNING, value);
    }
}

async fn project_answer(
    answer: Response<Body>,
    operation: Operation,
    constraints: &Constraints,
) -> Response<Body> {
    let (mut parts, body) = answer.into_parts();
    if constraints.restricted {
        parts
            .headers
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    warn_about_dropped_types(&mut parts.headers, constraints);
    if !parts.status.is_success() {
        // A read the grants do not reach is answered with the gateway's own miss, so the
        // broker's wording — which names the id it could not find — cannot be told apart from
        // a refusal (R20, T-2130).
        if parts.status == StatusCode::NOT_FOUND && !operation.is_write() {
            return ProblemDetails::not_found().into_response();
        }
        // What a broker says when it fails is whatever it was holding: an entity id in a
        // message, a fragment of its own query, the name of an attribute this endpoint hides.
        // The caller gets the gateway's own document and the operator gets the text (T-2260).
        if parts.status.is_server_error() {
            let held = axum::body::to_bytes(body, MAX_BODY)
                .await
                .unwrap_or_default();
            tracing::warn!(
                status = parts.status.as_u16(),
                broker = %String::from_utf8_lossy(&held),
                "the broker failed; its own explanation is not passed on"
            );
            return ProblemDetails::new(
                parts.status.as_u16(),
                "broker-failure",
                "The context broker could not answer",
            )
            .with_detail("the context broker behind this endpoint failed to answer this request")
            .into_response();
        }
        // A `4xx` is the caller's own request coming back, and it explains what to send instead.
        return Response::from_parts(parts, body);
    }
    if operation.is_write() {
        return Response::from_parts(parts, body);
    }

    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        tracing::error!("the broker's answer is larger than the gateway can project");
        return ProblemDetails::internal().into_response();
    };
    let Ok(mut payload) = serde_json::from_slice::<Value>(&bytes) else {
        // Not JSON, so there is nothing to project and nothing to leak through a member
        // the gateway did not understand.
        return proxy::with_body(parts, bytes.to_vec());
    };

    // A document about the vocabulary of the space carries an `id` and a `type` like an entity
    // does, and is not one: the entity projection would strip `typeList` and `attributeDetails`
    // — the members it exists for — and the type guard would refuse it for declaring a type no
    // grant names. It is narrowed on its own members instead (EP-26, T-2134).
    if vocabulary::describes(operation) {
        if !vocabulary::narrow(&mut payload, constraints) {
            // The broker answered about a name other than the one the path asked for, and that
            // name is not one this caller reaches. Serving it would let the broker choose what
            // the grants cover (EP-26, T-2131).
            return ProblemDetails::not_found().into_response();
        }
        return match serde_json::to_vec(&payload) {
            Ok(bytes) => proxy::with_body(parts, bytes),
            Err(error) => {
                tracing::error!(%error, "the narrowed vocabulary document does not serialize");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        };
    }

    let areas = geo::Areas::of(&constraints.geo_grants, constraints.geo_caller.as_deref());
    match &mut payload {
        Value::Array(entities) => {
            let answered = entities.len();
            entities.retain(|entity| {
                projection::permitted(entity, constraints)
                    && areas.as_ref().is_none_or(|areas| areas.admits(entity))
            });
            // The broker counted what it answered, this counts what the caller may read, and the
            // difference is the number of entities dropped here: a total that says two where one
            // entity came back is the same oracle the entity itself would have been (R22, T-2131).
            if entities.len() < answered {
                parts.headers.remove(RESULTS_COUNT);
                parts
                    .headers
                    .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
            }
            projection::project_by_type(&mut payload, constraints);
        }
        entity if entity.is_object() && entity.get("id").is_some() => {
            if !projection::permitted(entity, constraints)
                || !areas.as_ref().is_none_or(|areas| areas.admits(entity))
            {
                return ProblemDetails::not_found().into_response();
            }
            projection::project_by_type(&mut payload, constraints);
        }
        // A type list, an attribute list, a problem document: not entities, nothing to
        // project.
        _ => {}
    }
    // The forwarded window is the hull of several grants; what falls in the gaps between
    // them was never granted (GW26).
    temporal::keep_windows(&mut payload, &constraints.temporal_windows);

    match serde_json::to_vec(&payload) {
        Ok(bytes) => proxy::with_body(parts, bytes),
        Err(error) => {
            tracing::error!(%error, "the projected answer does not serialize");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Establishes who is calling, from the verified token or from nobody (PF-45, PF-46).
///
/// The three answers are: a verified principal, the anonymous `public` role on an endpoint
/// that admits it, or 401. There is no fourth answer where a claim the client made is
/// believed without a signature behind it.
pub(crate) fn authenticate(
    gateway: &Gateway,
    endpoint: &Endpoint,
    headers: &HeaderMap,
) -> Result<Subject, Box<ProblemDetails>> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    let raw = match token::bearer(presented) {
        Ok(raw) => raw,
        // No token at all: the anonymous caller, which only a public endpoint admits
        // (EP-16, GW22).
        Err(token::Rejected::NoToken) if endpoint.admits(None) => return Ok(Subject::anonymous()),
        Err(rejected) => return Err(Box::new(rejected.into())),
    };

    // A token was presented and the gateway has no realm to check it against. Believing
    // it would be believing the client.
    let Some(verifier) = &gateway.verifier else {
        tracing::warn!("a token was presented but no realm is configured");
        return Err(Box::new(ProblemDetails::unauthorized()));
    };
    let claims = verifier
        .verify(raw, &gateway.audiences_for(endpoint))
        .map_err(|rejected| Box::new(ProblemDetails::from(rejected)))?;

    subject_of(&claims, endpoint, gateway)
}

/// The audience of a person signed in at the edge (ADR-N-019): the gateway's own name, accepted
/// on every endpoint, because a session cannot name an endpoint approved after the login. The
/// Policy decision stays per endpoint (PF-46).
pub const EDGE_AUDIENCE: &str = "context-gateway";

/// The slug, the public resource URI when the deployment names one, and the edge audience.
fn audiences_of(slug: &str, base_path: &str, public_url: Option<&str>) -> Vec<String> {
    let mut audiences = vec![slug.to_owned()];
    if let Some(base) = public_url {
        audiences.push(format!("{base}{base_path}"));
    }
    audiences.push(EDGE_AUDIENCE.to_owned());
    audiences
}

/// Turns verified claims into the subject the PDP evaluates.
fn subject_of(
    claims: &Claims,
    endpoint: &Endpoint,
    gateway: &Gateway,
) -> Result<Subject, Box<ProblemDetails>> {
    // A data space consumer, before anything looks at `azp`. The connector obtained this
    // token with its own ServiceAccount, and resolving that account would hand the consumer
    // the connector's grants instead of the agreement's (DS-01, DS-02).
    if dataspace_token::presented(claims) {
        return dataspace_token::subject(
            claims,
            gateway.agreements.load().as_ref(),
            endpoint,
            crate::pdp::now(),
        )
        .map_err(|refused| Box::new(ProblemDetails::from(refused)));
    }

    let groups: BTreeSet<String> = claims
        .groups
        .iter()
        .map(|group| group.trim_start_matches('/').to_owned())
        .collect();

    // A workload: `azp` has to name a ServiceAccount this repository declares, or the
    // token is valid and the account is unknown, which is an account with no grants
    // (PF-46).
    if let Some(azp) = claims.azp.as_deref() {
        if let Some(account) = gateway.accounts.load().resolve(azp) {
            if !endpoint.admits(Some(&account.project)) {
                return Err(Box::new(ProblemDetails::forbidden()));
            }
            return Ok(Subject {
                user: None,
                service_account: Some(account.name.clone()),
                roles: roles_on(
                    endpoint.audience,
                    account.roles_in(&account.project, &endpoint.space),
                ),
                groups,
                did: None,
                agreement: None,
            });
        }
        if claims.preferred_username.is_none() {
            tracing::warn!(azp, "token from a client no ServiceAccount manifest names");
            return Err(Box::new(ProblemDetails::forbidden()));
        }
    }

    // A human. The token says they are a member of the organization; a group names the
    // project when the endpoint's audience is a list of them (EP-14, EP-15).
    let Some(user) = claims.preferred_username.clone() else {
        return Err(Box::new(ProblemDetails::unauthorized()));
    };
    let project = std::iter::once(&endpoint.project)
        .chain(endpoint.allowed_projects.iter())
        .find(|project| groups.contains(*project))
        .cloned()
        .unwrap_or_default();
    if !endpoint.admits(Some(&project)) {
        return Err(Box::new(ProblemDetails::forbidden()));
    }

    Ok(Subject {
        user: Some(user),
        service_account: None,
        roles: roles_on(endpoint.audience, claims.roles().iter().cloned()),
        groups,
        did: None,
        agreement: None,
    })
}

/// The roles a caller holds on this endpoint: what the token asserts and, on a public
/// endpoint, the synthetic `public` role as well. A signed-in person is a member of the
/// public too, so a login never grants less than no login does (EP-16, GW22).
fn roles_on(audience: Audience, asserted: impl IntoIterator<Item = String>) -> BTreeSet<String> {
    let mut roles: BTreeSet<String> = asserted.into_iter().collect();
    if audience == Audience::Public {
        roles.insert("public".to_owned());
    }
    roles
}

/// The principal, as one string for the audit log.
fn principal_of(subject: &Subject) -> String {
    match (&subject.user, &subject.service_account, &subject.did) {
        (Some(user), _, _) => format!("user:{user}"),
        (_, Some(account), _) => format!("serviceAccount:{account}"),
        // A data space consumer is its DID and nothing else, so the audit line says so
        // rather than calling a whole agreement anonymous (DS-13).
        (_, _, Some(did)) => did.clone(),
        _ => "anonymous".to_owned(),
    }
}

/// Resolving the slug and establishing the caller: the two steps every surface starts
/// with (EP-03, EP-14, GW20).
///
/// `representation` is the one the surface serves, and `None` for the surfaces that are
/// not a representation of the data at all, like `access`.
pub(crate) fn admit(
    gateway: &Gateway,
    slug: &str,
    representation: Option<Representation>,
    headers: &HeaderMap,
) -> Result<(Arc<Endpoint>, Subject), Box<Response<Body>>> {
    let endpoint = gateway
        .resolver
        .resolve(slug)
        .ok_or_else(|| Box::new(ProblemDetails::not_found().into_response()))?;

    // An endpoint that does not serve the representation is not an endpoint at this URL
    // (EP-05).
    if representation.is_some_and(|wanted| !endpoint.serves(wanted)) {
        return Err(Box::new(ProblemDetails::not_found().into_response()));
    }
    let subject = authenticate(gateway, &endpoint, headers)
        .map_err(|problem| Box::new(problem.into_response()))?;
    Ok((endpoint, subject))
}

/// The endpoint's own MCP instance: one JSON-RPC message in, one out (T-0166, EP-24, SP-14).
///
/// Streamable HTTP without a session: no `GET` stream to open, nothing kept between calls,
/// so a grant withdrawn a second ago is already gone from the next `tools/list` (SP-19).
async fn mcp_message(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);

    // AG-32: a client with no token is told where to get one, instead of being refused in
    // a way it cannot act on. A public MCP instance needs none, so it is served first.
    let public = gateway
        .resolver
        .resolve(&slug)
        .is_some_and(|endpoint| endpoint.audience == Audience::Public);
    if !public && request.headers().get(AUTHORIZATION).is_none() {
        return unauthorized(&gateway, &slug);
    }

    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::Mcp),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    mcp_answer(gateway, endpoint, subject, request).await
}

/// One JSON-RPC message, whichever surface it arrived on.
async fn mcp_answer(
    gateway: Arc<Gateway>,
    endpoint: Arc<Endpoint>,
    subject: Subject,
    request: Request,
) -> Response<Body> {
    let authorization = request.headers().get(AUTHORIZATION).cloned();
    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return mcp::endpoint_facade::parse_error();
    };
    let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
        return mcp::endpoint_facade::parse_error();
    };

    match mcp::endpoint_facade::handle(gateway, endpoint, subject, authorization, message).await {
        Some(answer) => mcp::endpoint_facade::json_response(StatusCode::OK, &answer),
        // A notification is acknowledged and nothing more: there is no state to change.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// The space's own MCP instance, the same façade the endpoint surface runs (SP-14).
async fn space_mcp_message(
    State(gateway): State<Arc<Gateway>>,
    Path(space): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (space, subject) = match admit_space(
        &gateway,
        &space,
        Some(Representation::Mcp),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    mcp_answer(gateway, Arc::clone(&space.endpoint), subject, request).await
}

/// The protected-resource metadata of one MCP route (RFC 9728, AG-32).
///
/// Answered for every slug, whether or not one resolves: the document names the resource
/// URL the caller already typed and the realm the deployment already publishes, so it
/// discloses nothing, and a phone that has to discover its authorization server always
/// can (R20, EP-23).
async fn protected_resource(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
) -> Response<Body> {
    let Some(issuer) = gateway.verifier.as_ref().map(|verifier| verifier.issuer()) else {
        // No realm configured is a deployment that serves public endpoints only; there is
        // no authorization server to point a client at.
        return ProblemDetails::not_found().into_response();
    };
    json_response(&serde_json::json!({
        "resource": format!("{}/api/endpoint/{slug}/mcp", gateway.base_url()),
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
        "resource_documentation": format!("{}/api/endpoint/{slug}/", gateway.base_url()),
    }))
}

/// The `401` a client needs in order to find its authorization server (RFC 9728, AG-32).
///
/// Sent for every unauthenticated call that is not on a public MCP instance, whether the
/// slug resolves or not, so the answer is the same for an endpoint that needs a login and
/// for one that does not exist (R20).
///
/// The slug is the caller's text inside a quoted-string, so only a slug shaped like one is named
/// there: a quote, a backslash or a dot segment would reshape the challenge or the URL a client
/// resolves, and such a slug never resolves anyway (T-2509). It gets the same 401 with no
/// challenge.
fn unauthorized(gateway: &Gateway, slug: &str) -> Response<Body> {
    let mut response = ProblemDetails::new(401, "unauthorized", "Unauthorized")
        .with_detail("this endpoint needs an access token; its authorization server is named by the resource metadata")
        .into_response();
    let shaped = (1..=128).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !shaped {
        return response;
    }
    let metadata = format!(
        "{}/api/endpoint/{slug}/.well-known/oauth-protected-resource",
        gateway.base_url()
    );
    if let Ok(challenge) =
        HeaderValue::from_str(&format!("Bearer resource_metadata=\"{metadata}\""))
    {
        response
            .headers_mut()
            .insert(axum::http::header::WWW_AUTHENTICATE, challenge);
    }
    response
}

/// The DCAT-AP record of one endpoint, in the representation the caller asked for
/// (T-0337, T-0338, EP-27, EP-68, EP-69).
///
/// The record is built on the schema catalogue the endpoint already serves, so the digests
/// it names are the ones `schema/index.json` names and the artifacts' own `ETag`s, computed
/// once. Everything is the granted projection: `admit` has already refused a caller the
/// endpoint does not serve, and `schema::visible` narrows the artifacts to what this one
/// may read.
async fn endpoint_record(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let visible = schema::visible(&subject, &endpoint, crate::pdp::now());
    let index = schema::index(&endpoint, &visible, sha256_hex);
    let space = gateway.resolver.resolve_space(&endpoint.space);
    let space = space.as_deref();

    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok());
    let base = gateway.base_url();
    let format = space_surface::negotiate(accept);
    let body = match format {
        space_surface::Format::JsonLd => {
            serde_json::to_string(&endpoint_surface::dataset(&endpoint, space, &index, base))
                .unwrap_or_default()
        }
        space_surface::Format::Turtle => {
            endpoint_surface::dataset_turtle(&endpoint, space, &index, base)
        }
        space_surface::Format::Html => {
            endpoint_surface::dataset_html(&endpoint, space, &index, base)
        }
    };
    match HeaderValue::from_str(format.media_type()) {
        Ok(media) => ([(axum::http::header::CONTENT_TYPE, media)], body).into_response(),
        Err(_) => ProblemDetails::internal().into_response(),
    }
}

/// The lowercase hex sha256 of a body, which is what every `ETag` here is.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A file route names its format in the path, so the hop to the broker asks for JSON whatever
/// the caller's `Accept` said: forwarded as it came, `Accept: text/csv` on `file.csv` was the
/// broker's `406` (EP-08, EP-44).
pub(crate) fn broker_speaks_json(request: &mut Request) {
    request
        .headers_mut()
        .insert(ACCEPT, HeaderValue::from_static("application/json"));
}

/// Resolving a space name and establishing the caller (SP-06, SP-11).
///
/// A space the caller may not reach and a space that does not exist answer the same 404,
/// so a probe over the space namespace learns nothing either way (R20).
pub(crate) fn admit_space(
    gateway: &Gateway,
    name: &str,
    representation: Option<Representation>,
    headers: &HeaderMap,
) -> Result<(Arc<Space>, Subject), Box<Response<Body>>> {
    // One answer for every way in which this caller may not have this space: it does not exist,
    // it does not serve this representation, the token does not verify, or no grant of the
    // caller's reaches it. Space names are guessable words — `helsinki`, `air-quality` — so a
    // 403 or a 401 where another space gives 404 is an enumeration oracle for what a deployment
    // runs (R20, SP-06, SP-11). A client that has to discover where to authenticate still can:
    // `/.well-known/oauth-protected-resource/…` answers for every name, resolvable or not.
    let missing = || Box::new(ProblemDetails::not_found().into_response());
    let space = gateway.resolver.resolve_space(name).ok_or_else(missing)?;
    if representation.is_some_and(|wanted| !space.endpoint.serves(wanted)) {
        return Err(missing());
    }
    let subject = authenticate(gateway, &space.endpoint, headers).map_err(|_| missing())?;
    if !discoverable(gateway, &space, &subject) {
        return Err(missing());
    }
    Ok((space, subject))
}

/// Whether this caller holds any grant that reaches this space (SP-11).
///
/// The same PDP that enforces a request decides who may see the space exists, so the
/// catalog cannot list a space the data surface would refuse.
pub(crate) fn discoverable(gateway: &Gateway, space: &Space, subject: &Subject) -> bool {
    !gateway
        .pdp
        .decide(
            subject,
            Operation::QueryEntity,
            &query::requested(&[]),
            &space.endpoint,
        )
        .is_deny()
}

/// A JSON body under a media type of its own, for the representations that have one.
pub(crate) fn typed_json_response(payload: &Value, media_type: &'static str) -> Response<Body> {
    match serde_json::to_vec(payload) {
        Ok(bytes) => (
            [(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static(media_type),
            )],
            bytes,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "the answer does not serialize");
            ProblemDetails::internal().into_response()
        }
    }
}

/// A text body under its own media type.
pub(crate) fn text_response(body: String, media_type: &'static str) -> Response<Body> {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static(media_type),
        )],
        body,
    )
        .into_response()
}

/// The language the caller asked for, as they wrote it (EP-37, EP-45).
pub(crate) fn accept_language(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
}

/// The answer of a read-only representation to a method it does not have (EP-13, EP-39).
pub(crate) fn read_only(status: StatusCode) -> Response<Body> {
    let mut response = if status == StatusCode::NO_CONTENT {
        Response::new(Body::empty())
    } else {
        ProblemDetails::new(405, "method-not-allowed", "Method Not Allowed")
            .with_detail("this representation is read-only; write through the NGSI-LD surface")
            .into_response()
    };
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::ALLOW,
        HeaderValue::from_static("GET, HEAD, OPTIONS"),
    );
    response
}

/// A JSON body, serialized once.
pub(crate) fn json_response(payload: &Value) -> Response<Body> {
    match serde_json::to_vec(payload) {
        Ok(bytes) => (
            [(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            bytes,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "the answer does not serialize");
            ProblemDetails::internal().into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_in_caller_on_a_public_endpoint_holds_the_public_role_too() {
        let asserted = || ["steward".to_owned()];
        assert_eq!(
            roles_on(Audience::Public, asserted()),
            BTreeSet::from(["public".to_owned(), "steward".to_owned()])
        );
        assert_eq!(
            roles_on(Audience::Organization, asserted()),
            BTreeSet::from(["steward".to_owned()])
        );
        assert_eq!(
            roles_on(Audience::ProjectList, Vec::new()),
            BTreeSet::new(),
            "no audience but public hands out a role the token did not assert"
        );
    }

    #[test]
    fn a_token_names_the_slug_the_public_resource_or_the_edge_audience() {
        assert_eq!(
            audiences_of(
                "k4y7pq2mzt6vhx3nbwrs5cjd8f",
                "/api/endpoint/k4y7pq2mzt6vhx3nbwrs5cjd8f",
                Some("https://hel.fi")
            ),
            vec![
                "k4y7pq2mzt6vhx3nbwrs5cjd8f".to_owned(),
                "https://hel.fi/api/endpoint/k4y7pq2mzt6vhx3nbwrs5cjd8f".to_owned(),
                EDGE_AUDIENCE.to_owned(),
            ]
        );
        assert_eq!(
            audiences_of(
                "k4y7pq2mzt6vhx3nbwrs5cjd8f",
                "/api/endpoint/k4y7pq2mzt6vhx3nbwrs5cjd8f",
                None
            ),
            vec![
                "k4y7pq2mzt6vhx3nbwrs5cjd8f".to_owned(),
                EDGE_AUDIENCE.to_owned()
            ],
            "no public URL, no resource URI, the edge audience stays"
        );
    }
}
