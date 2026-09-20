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
use crate::handlers::{endpoint_surface, schema, space_surface};
use crate::middleware::rate_limit::{self, RateLimiter};
use crate::pdp::evaluator::{self, Constraints, Subject, Verdict};
use crate::pdp::{conditional, write_guard};
use crate::pdp::{geo, projection, temporal, vocabulary, Pdp};
use crate::proxy::{self, Broker};
use crate::resolver::{Endpoint, Model, SlugResolver, Space};
use crate::translators::{cql2, geojson, ogc, sta, tabular, view_mapping, zip_export};
use crate::{egress, handlers, mcp, middleware::tenancy, operations, query, telemetry};
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
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

/// The largest request or response body the gateway will hold in memory.
///
/// A write has to be inspected whole before any of it is applied and a read has to be
/// projected before any of it is sent, so neither can stream today.
// ponytail: one buffer, one limit. Streaming the untouched representations is a separate
// task, and a limit is a better answer than an out-of-memory kill either way.
const MAX_BODY: usize = 8 * 1024 * 1024;

// The header that tells a caller their answer was narrowed by policy, and the layer that
// removes it again when they did not ask (R22).
use crate::middleware::response::{RESULTS_COUNT, RESULTS_RESTRICTED};

/// The methods a view endpoint answers at all (RFC 9110 section 9.2.1).
const SAFE: &[Method] = &[Method::GET, Method::HEAD, Method::OPTIONS];

/// How many entities a file representation asks the broker for at a time (EP-44).
///
/// Large enough that a normal download is one or two round trips, small enough that one
/// page fits comfortably inside `MAX_BODY` whatever the entities look like.
const PAGE: usize = 1_000;

/// The most broker JSON one `file.*` download may read before it is refused (T-0810).
///
/// The row ceiling bounds how many entities a download holds, not how large they are, and the
/// gateway holds the JSON, the flattened table and the file at once. This is the memory the
/// shared enforcement point will spend on one caller; an endpoint's own `maxFileBytes` bounds
/// the file, which is a different and usually smaller number.
const MAX_COLLECTED: u64 = 64 * 1024 * 1024;

/// The deepest offset a paging parameter may name (T-0809).
///
/// A hundred pages is further than any client of this surface pages, and it is the ceiling on
/// what one request can make the shared broker read: the temporal path turns the offset into
/// `lastN`, and nothing in an endpoint's configuration bounds that.
const MAX_SKIP: usize = 100 * PAGE;

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
        .layer(axum::middleware::from_fn(
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
fn authenticate(
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
fn admit(
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
fn unauthorized(gateway: &Gateway, slug: &str) -> Response<Body> {
    let metadata = format!(
        "{}/api/endpoint/{slug}/.well-known/oauth-protected-resource",
        gateway.base_url()
    );
    let mut response = ProblemDetails::new(401, "unauthorized", "Unauthorized")
        .with_detail("this endpoint needs an access token; its authorization server is named by the resource metadata")
        .into_response();
    if let Ok(challenge) =
        HeaderValue::from_str(&format!("Bearer resource_metadata=\"{metadata}\""))
    {
        response
            .headers_mut()
            .insert(axum::http::header::WWW_AUTHENTICATE, challenge);
    }
    response
}

/// What the caller may do here, from the same PDP that enforces it (T-0163, EP-55).
async fn access(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok());
    let now = crate::pdp::now();

    // The same grants in whichever language the caller reads (EP-56, EP-57, EP-58). The
    // document is computed once per representation from the same PDP answer; a second
    // implementation of "what may this caller do" is the thing EP-60 forbids.
    match access_format(accept) {
        AccessFormat::AuthZen => {
            json_response(&handlers::access::permissions(&subject, &endpoint, now))
        }
        AccessFormat::Odrl => {
            let document = handlers::access_odrl::policy(
                &subject,
                &endpoint,
                now,
                gateway.base_url(),
                sha256_hex,
            );
            typed_json_response(&document, handlers::access_odrl::ODRL_JSON)
        }
        AccessFormat::Turtle => {
            let document = handlers::access_odrl::policy(
                &subject,
                &endpoint,
                now,
                gateway.base_url(),
                sha256_hex,
            );
            text_response(
                handlers::access_odrl::turtle(&document),
                handlers::access_odrl::TURTLE,
            )
        }
        AccessFormat::GrantAst => {
            let document = handlers::access_ucast::grant_ast(&subject, &endpoint, now);
            typed_json_response(&document, handlers::access_ucast::GRANT_AST_JSON)
        }
    }
}

/// Which representation of the access surface the caller asked for (EP-56, EP-57, EP-58).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessFormat {
    /// The AuthZEN resource-search document, and the default.
    AuthZen,
    /// ODRL 2.2 in the `ngsi-ld:` profile, as JSON-LD.
    Odrl,
    /// The same ODRL policy as RDF.
    Turtle,
    /// The residual as a UCAST condition tree.
    GrantAst,
}

/// The first representation the `Accept` header names that this surface serves.
///
/// Read in the order the caller wrote them, so a client that prefers Turtle and will settle
/// for JSON gets Turtle. Anything else, including no header at all, is the default document:
/// the access surface always has an answer, and a 406 here would tell a caller nothing it
/// could act on.
fn access_format(accept: Option<&str>) -> AccessFormat {
    let Some(accept) = accept else {
        return AccessFormat::AuthZen;
    };
    for offer in accept.split(',') {
        match offer.split(';').next().unwrap_or_default().trim() {
            handlers::access_odrl::ODRL_JSON => return AccessFormat::Odrl,
            handlers::access_odrl::TURTLE => return AccessFormat::Turtle,
            handlers::access_ucast::GRANT_AST_JSON => return AccessFormat::GrantAst,
            "application/json" | "application/ld+json" => return AccessFormat::AuthZen,
            _ => {}
        }
    }
    AccessFormat::AuthZen
}

/// One prospective request, answered yes or no (T-0163, R51).
async fn access_check(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return ProblemDetails::bad_request().into_response();
    };
    let Ok(query) = serde_json::from_slice::<Value>(&bytes) else {
        return ProblemDetails::bad_request()
            .with_detail("request body is not JSON")
            .into_response();
    };
    let Some(action) = query
        .get("action")
        .and_then(|action| action.get("name"))
        .and_then(Value::as_str)
    else {
        return ProblemDetails::bad_request()
            .with_detail("action.name is required")
            .into_response();
    };

    json_response(&handlers::access::check(
        &subject,
        &endpoint,
        action,
        query
            .get("resource")
            .and_then(|resource| resource.get("type"))
            .and_then(Value::as_str),
        crate::pdp::now(),
    ))
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

/// The catalogue of what this endpoint publishes about its data (T-0162, EP-46).
async fn schema_index(
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
    let document = schema::index(&endpoint, &visible, sha256_hex);
    revalidated(&document, "application/json", request.headers())
}

/// One schema document of one major version, projected to the grant (T-0162, EP-47, EP-49).
async fn schema_artifact(
    State(gateway): State<Arc<Gateway>>,
    Path((slug, version, artifact)): Path<(String, String, String)>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    // `schema/v2/...`: the major of the model, never its full version (DM-22).
    let Some(major) = version
        .strip_prefix('v')
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return ProblemDetails::not_found().into_response();
    };
    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*/*");
    let Some(wanted) = schema::artifact_of(&artifact, accept) else {
        return ProblemDetails::not_found().into_response();
    };

    let models: Vec<&Model> = endpoint
        .models
        .iter()
        .filter(|model| model.major == major)
        .collect();
    if models.is_empty() {
        return ProblemDetails::not_found().into_response();
    }
    let visible = schema::visible(&subject, &endpoint, crate::pdp::now());
    if !wanted.is_json() {
        // SHACL, OWL, RDF, LinkML and Markdown are rendered from the projected model rather
        // than served from a committed file, so no formalism can carry a slot the grant
        // forbids (T-0284, EP-47).
        let body = schema::render(&models, wanted, &visible);
        return revalidated_text(body.into_bytes(), wanted.media_type(), request.headers());
    }

    let mut redacted = Vec::new();
    let document = match wanted {
        schema::Artifact::JsonSchema => schema::json_schema(&models, &visible, &mut redacted),
        _ => schema::context(&models, &visible, &mut redacted),
    };
    revalidated(&document, wanted.media_type(), request.headers())
}

/// A schema document with the strong `ETag` a client revalidates against (EP-51).
///
/// The document is a projection of the policy set, so it is never immutable: a grant that
/// changes changes the schema, and a client holding a stale copy has to find out. What it
/// gets instead is a digest of exactly the bytes it holds, and a 304 whenever they still
/// match.
fn revalidated(document: &Value, media_type: &str, headers: &HeaderMap) -> Response<Body> {
    let Ok(bytes) = serde_json::to_vec(document) else {
        tracing::error!("a schema document does not serialize");
        return ProblemDetails::internal().into_response();
    };
    revalidated_text(bytes, media_type, headers)
}

/// The same, for a document that is already the bytes the caller receives (T-0284).
fn revalidated_text(bytes: Vec<u8>, media_type: &str, headers: &HeaderMap) -> Response<Body> {
    let etag = format!("\"{}\"", sha256_hex(&bytes));

    let mut response = if matches_etag(headers, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        match HeaderValue::from_str(media_type) {
            Ok(content_type) => (
                [(axum::http::header::CONTENT_TYPE, content_type)],
                Body::from(bytes),
            )
                .into_response(),
            Err(_) => {
                tracing::error!(media_type, "a schema media type is not a header value");
                return ProblemDetails::internal().into_response();
            }
        }
    };
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(axum::http::header::ETAG, value);
    }
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    response
}

/// Whether `If-None-Match` names the document the gateway just built (RFC 9110 13.1.2).
fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    let Some(presented) = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    presented
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == etag)
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
fn broker_speaks_json(request: &mut Request) {
    request
        .headers_mut()
        .insert(ACCEPT, HeaderValue::from_static("application/json"));
}

/// The same data as a `FeatureCollection` (T-0158, EP-09, EP-10).
async fn file_geojson(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    broker_speaks_json(&mut request);
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::GeoJson),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let params = query::parse(request.uri().query().unwrap_or_default());
    // A download is the whole dataset, paged out of the broker and held to this endpoint's
    // ceilings, exactly like `file.csv` (EP-44, T-1700). One broker query instead would answer
    // a page and call it a file: a truncated FeatureCollection is indistinguishable from a
    // complete one, and the caller acts on half the data.
    let limits = tabular::Limits::of(endpoint.file_limits.as_ref());
    let (entities, restricted) = match paged_entities(
        &gateway,
        &endpoint,
        &subject,
        &params,
        &mut request,
        &limits,
    )
    .await
    {
        Ok(answer) => answer,
        Err(problem) => return *problem,
    };

    match geojson::feature_collection(&entities) {
        Ok(collection) => {
            // The byte ceiling is the endpoint's own, measured on what would be sent (EP-44).
            if serde_json::to_vec(&collection)
                .is_ok_and(|bytes| bytes.len() as u64 > limits.max_bytes)
            {
                return too_large();
            }
            let mut response = json_response(&collection);
            response.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static(geojson::MEDIA_TYPE),
            );
            if restricted {
                response
                    .headers_mut()
                    .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
            }
            response
        }
        Err(untranslatable) => ProblemDetails::bad_request()
            .with_detail(untranslatable.to_string())
            .into_response(),
    }
}

/// Every entity type the pinned tenant holds, as the broker's own `EntityTypeList` reports it.
///
/// Used only as the selector of last resort for a file download. It can only narrow: the PDP
/// has already decided what this caller may see, and every constraint it produced is applied
/// either upstream or on the way back. An empty list means an empty space, which is an empty
/// file rather than an error.
async fn dataset_types(
    gateway: &Gateway,
    headers: &HeaderMap,
) -> Result<Vec<String>, Box<Response<Body>>> {
    let answer = gateway
        .broker
        .send(
            axum::http::Method::GET,
            "/ngsi-ld/v1/types",
            headers.clone(),
            Body::empty(),
        )
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;

    let (parts, body) = answer.into_parts();
    if !parts.status.is_success() {
        return Err(Box::new(Response::from_parts(parts, body)));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    let list: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    Ok(list["typeList"]
        .as_array()
        .map(|types| {
            types
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// Queries the entities behind an endpoint through the PDP, projected (GW11, R9).
///
/// This is the read half of the `file.*` downloads, shared by every representation that is a
/// different rendering of the same dataset. The NGSI-LD surface does not come through here:
/// it forwards the caller's own query, and an unselected one stays the `400` the
/// specification asks for.
async fn query_entities(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    params: &[(String, String)],
    request: &mut Request,
) -> Result<(Value, bool), Box<Response<Body>>> {
    let verdict = gateway.pdp.decide(
        subject,
        Operation::QueryEntity,
        &query::requested(params),
        endpoint,
    );
    let Verdict::Rewrite(constraints) = verdict else {
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    };
    if constraints.empty {
        return Ok((Value::Array(Vec::new()), true));
    }
    tenancy::pin_tenant(request, &endpoint.space)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    // Nothing the caller sent and nothing the grants added selects anything, and a query that
    // selects nothing is a `400` upstream (CIM 009 5.7.2). A download is not a query though:
    // asking for `file.geojson` is asking for the dataset, so the selector is every type the
    // space holds (EP-09, EP-07). It can only narrow, never widen: the PDP has already spoken.
    let fallback = if query::selects(&constraints) {
        Vec::new()
    } else {
        let types = dataset_types(gateway, request.headers()).await?;
        // A space holding nothing is an empty file, not a `400` and not a second broker call.
        if types.is_empty() {
            return Ok((Value::Array(Vec::new()), constraints.restricted));
        }
        types
    };

    let target = format!(
        "/ngsi-ld/v1/entities?{}",
        query::upstream(params, &constraints, &fallback)
    );
    let answer = gateway
        .broker
        .send(
            axum::http::Method::GET,
            &target,
            request.headers().clone(),
            Body::empty(),
        )
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;

    let (parts, body) = answer.into_parts();
    if !parts.status.is_success() {
        return Err(Box::new(Response::from_parts(parts, body)));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    let mut entities: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    let areas = geo::Areas::of(&constraints.geo_grants, constraints.geo_caller.as_deref());
    if let Value::Array(list) = &mut entities {
        list.retain(|entity| {
            projection::permitted(entity, &constraints)
                && areas.as_ref().is_none_or(|areas| areas.admits(entity))
        });
    }
    projection::project_by_type(&mut entities, &constraints);
    Ok((entities, constraints.restricted))
}

/// The history of one entity, decided and projected exactly as its current state is (T-0438).
///
/// The temporal tree is a second upstream path, so it goes through the same four steps the
/// entity path goes through and in the same order: the PDP decides, the tenant is pinned, the
/// grants' own window and attribute set are what is forwarded, and what comes back is filtered
/// by the id patterns and the geo grants and then projected. Skipping any of them would make
/// the history a way around the projection the instant answer applies (EP-07, R9).
///
/// Two differences from [`query_entities`], both of them the temporal representation's own.
/// The answer is one entity whose attributes are arrays of instances rather than a list of
/// entities, so a caller who may not read it gets `404` rather than an empty page. And the
/// grants' windows are applied to the instances after the projection, because the window the
/// broker was given is the hull of several grants and what falls in the gaps between them was
/// never granted (GW26).
async fn query_temporal(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    urn: &str,
    params: &[(String, String)],
    request: &mut Request,
) -> Result<(Value, bool), Box<Response<Body>>> {
    let verdict = gateway.pdp.decide(
        subject,
        Operation::RetrieveTemporal,
        &query::requested(params),
        endpoint,
    );
    let Verdict::Rewrite(constraints) = verdict else {
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    };
    // A window no grant reaches is genuinely no history, and it is answered here rather than
    // asked of the broker without one (GW26).
    if constraints.empty {
        return Ok((Value::Null, true));
    }
    tenancy::pin_tenant(request, &endpoint.space)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    let target = format!(
        "/ngsi-ld/v1/temporal/entities/{}?{}",
        query::encode(urn),
        query::upstream(params, &constraints, &[])
    );
    let answer = gateway
        .broker
        .send(
            axum::http::Method::GET,
            &target,
            request.headers().clone(),
            Body::empty(),
        )
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;

    let (parts, body) = answer.into_parts();
    // An entity with no history and one the broker refuses both mean "no observations here",
    // and the caller learns nothing from the difference (R20).
    if !parts.status.is_success() {
        return Ok((Value::Null, constraints.restricted));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    let mut entity: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    // The temporal query form answers a list even when it holds one entity.
    if let Some(first) = entity.as_array().and_then(|list| list.first()).cloned() {
        entity = first;
    }

    let areas = geo::Areas::of(&constraints.geo_grants, constraints.geo_caller.as_deref());
    if !projection::permitted(&entity, &constraints)
        || !areas.as_ref().is_none_or(|areas| areas.admits(&entity))
    {
        return Ok((Value::Null, constraints.restricted));
    }
    projection::project_by_type(&mut entity, &constraints);
    temporal::keep_windows(&mut entity, &constraints.temporal_windows);
    Ok((entity, constraints.restricted))
}

/// Resolving a space name and establishing the caller (SP-06, SP-11).
///
/// A space the caller may not reach and a space that does not exist answer the same 404,
/// so a probe over the space namespace learns nothing either way (R20).
fn admit_space(
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
fn discoverable(gateway: &Gateway, space: &Space, subject: &Subject) -> bool {
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

/// The catalog of spaces this caller may discover (SP-11).
async fn space_catalog(State(gateway): State<Arc<Gateway>>, request: Request) -> Response<Body> {
    let headers = request.headers();
    // A token is minted for one resource, so a token that names space A does not verify
    // against space B. That is not an error here, it is the narrowing: a space whose
    // authentication the caller cannot satisfy is a space they cannot discover.
    let visible: Vec<Arc<Space>> = gateway
        .resolver
        .spaces()
        .into_iter()
        .filter(|space| {
            authenticate(&gateway, &space.endpoint, headers)
                .is_ok_and(|subject| discoverable(&gateway, space, &subject))
        })
        .collect();

    let catalog = space_surface::catalog(&visible, gateway.base_url());
    let mut response = json_response(&catalog);
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(space_surface::JSON_LD),
    );
    response
}

/// The DCAT-AP record of one space, in the representation the caller asked for (SP-10).
async fn space_record(
    State(gateway): State<Arc<Gateway>>,
    Path(name): Path<String>,
    request: Request,
) -> Response<Body> {
    // `admit_space` has already answered 404 for a space this caller cannot discover, which is
    // the same answer a name that was never created gets (SP-06, SP-11).
    let (space, _subject) = match admit_space(&gateway, &name, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok());
    let base = gateway.base_url();
    let format = space_surface::negotiate(accept);
    let body = match format {
        space_surface::Format::JsonLd => {
            serde_json::to_string(&space_surface::dataset(&space, base)).unwrap_or_default()
        }
        space_surface::Format::Turtle => space_surface::dataset_turtle(&space, base),
        space_surface::Format::Html => space_surface::dataset_html(&space, base),
    };
    match HeaderValue::from_str(format.media_type()) {
        Ok(media) => ([(axum::http::header::CONTENT_TYPE, media)], body).into_response(),
        Err(_) => ProblemDetails::internal().into_response(),
    }
}

/// `file.csv`: the whole answer as one flat table (EP-08, EP-44, EP-45).
async fn file_csv(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    request: Request,
) -> Response<Body> {
    tabular_download(gateway, slug, request, Representation::Csv).await
}

/// `file.xlsx`: the same rows as `file.csv`, in a workbook (EP-08, EP-44).
async fn file_xlsx(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    request: Request,
) -> Response<Body> {
    tabular_download(gateway, slug, request, Representation::Xlsx).await
}

/// The two tabular representations, which differ only in how the same table is written.
async fn tabular_download(
    gateway: Arc<Gateway>,
    slug: String,
    mut request: Request,
    representation: Representation,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    broker_speaks_json(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, Some(representation), request.headers())
    {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let params = query::parse(request.uri().query().unwrap_or_default());
    let limits = tabular::Limits::of(endpoint.file_limits.as_ref());
    let (entities, restricted) = match paged_entities(
        &gateway,
        &endpoint,
        &subject,
        &params,
        &mut request,
        &limits,
    )
    .await
    {
        Ok(answer) => answer,
        Err(problem) => return *problem,
    };

    let mut table = match tabular::table(&entities, &limits) {
        Ok(table) => table,
        Err(_) => return too_large(),
    };
    if query::first(&params, "humanHeaders") == Some("true") {
        tabular::humanize(&mut table);
    }

    let (body, media, extension) = match representation {
        Representation::Xlsx => {
            let metadata = workbook_metadata(&endpoint, &params, table.len());
            match tabular::xlsx(&table, &metadata, &limits) {
                Ok(bytes) => (Body::from(bytes), tabular::XLSX_MEDIA_TYPE, "xlsx"),
                Err(tabular::XlsxError::TooLarge) => return too_large(),
                Err(error) => {
                    tracing::error!(%error, "the workbook does not serialize");
                    return ProblemDetails::internal().into_response();
                }
            }
        }
        _ => match tabular::csv(&table, &limits) {
            Ok(text) => (Body::from(text), tabular::CSV_MEDIA_TYPE, "csv"),
            Err(_) => return too_large(),
        },
    };

    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if let Ok(media) = HeaderValue::from_str(media) {
        headers.insert(axum::http::header::CONTENT_TYPE, media);
    }
    // The slug is base32 and the extension is one of two literals, so the filename needs
    // no quoting beyond the quotes themselves (EP-43).
    if let Ok(disposition) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}.{extension}\"",
        endpoint.slug
    )) {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, disposition);
    }
    if restricted {
        headers.insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// `file.zip`: one query in every shape, with the schemas and the catalogue record (T-0161,
/// EP-41, EP-43, EP-44, EP-51).
///
/// The bundle is assembled from the same projected answer the other file representations use, so
/// it can only ever carry what the caller was already allowed to download one format at a time.
/// Its schema directory is rendered by the code that serves `schema/`, and its catalogue record
/// is the endpoint's own, so a bundle cannot describe the data differently from the endpoint it
/// came out of.
async fn file_zip(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    broker_speaks_json(&mut request);
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::Zip),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let params = query::parse(request.uri().query().unwrap_or_default());
    let limits = tabular::Limits::of(endpoint.file_limits.as_ref());
    let (entities, restricted) = match paged_entities(
        &gateway,
        &endpoint,
        &subject,
        &params,
        &mut request,
        &limits,
    )
    .await
    {
        Ok(answer) => answer,
        Err(problem) => return *problem,
    };

    let visible = schema::visible(&subject, &endpoint, crate::pdp::now());
    let schemas = schema_directory(&endpoint, &visible);
    let index = schema::index(&endpoint, &visible, sha256_hex);
    let space = gateway.resolver.resolve_space(&endpoint.space);
    let dcat = endpoint_surface::dataset(&endpoint, space.as_deref(), &index, gateway.base_url());

    let exported_at = crate::pdp::now().to_rfc3339();
    let query = params
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let manifest = zip_export::Bundle {
        slug: &endpoint.slug,
        space: &endpoint.space,
        query: &query,
        exported_at: &exported_at,
    };

    let archive = match zip_export::bundle(&entities, &schemas, &dcat, &manifest, &limits) {
        Ok(archive) => archive,
        Err(zip_export::BundleError::TooLarge(_)) => return too_large(),
        Err(error) => {
            tracing::error!(%error, "the bundle does not serialize");
            return ProblemDetails::internal().into_response();
        }
    };

    let mut response = Response::new(Body::from(archive));
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(zip_export::MEDIA_TYPE),
    );
    // The slug is base32 and the date is digits, so the filename needs no quoting beyond the
    // quotes themselves (EP-43).
    if let Ok(disposition) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        zip_export::file_name(&endpoint.slug, &exported_at)
    )) {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, disposition);
    }
    if restricted {
        headers.insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// Every schema document the endpoint publishes, as the bundle stores them (EP-51).
///
/// One directory per major version, seven artifacts in each, each rendered from the caller's own
/// projection by the same functions the `schema/` surface calls (EP-47).
fn schema_directory(endpoint: &Endpoint, visible: &schema::Visible) -> Vec<(String, Vec<u8>)> {
    let mut majors: Vec<u32> = endpoint.models.iter().map(|model| model.major).collect();
    majors.sort_unstable();
    majors.dedup();

    let mut documents = Vec::new();
    for major in majors {
        let models: Vec<&Model> = endpoint
            .models
            .iter()
            .filter(|model| model.major == major)
            .collect();
        for artifact in schema::Artifact::ALL {
            let mut redacted = Vec::new();
            let body = match artifact {
                schema::Artifact::JsonSchema => {
                    serde_json::to_vec_pretty(&schema::json_schema(&models, visible, &mut redacted))
                }
                schema::Artifact::Context => {
                    serde_json::to_vec_pretty(&schema::context(&models, visible, &mut redacted))
                }
                other => Ok(schema::render(&models, other, visible).into_bytes()),
            };
            if let Ok(body) = body {
                documents.push((format!("v{major}/{}", artifact.file_name()), body));
            }
        }
    }
    documents
}

/// What the `metadata` sheet of a workbook says about the download that produced it.
fn workbook_metadata(
    endpoint: &Endpoint,
    params: &[(String, String)],
    rows: usize,
) -> Vec<(String, String)> {
    vec![
        ("space".to_owned(), endpoint.space.clone()),
        ("endpoint".to_owned(), endpoint.slug.clone()),
        ("exportedAt".to_owned(), crate::pdp::now().to_rfc3339()),
        (
            "query".to_owned(),
            params
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("&"),
        ),
        ("rows".to_owned(), rows.to_string()),
    ]
}

/// The answer is bigger than this endpoint allows one download to be (EP-44).
fn too_large() -> Response<Body> {
    let mut response = ProblemDetails::new(413, "payload-too-large", "Payload Too Large")
        .with_detail(tabular::TooLarge.to_string())
        .into_response();
    *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    response
}

/// Every entity the query reaches, read from the broker one page at a time (EP-44).
///
/// A file representation answers with the whole result set rather than one broker page,
/// so it pages until the broker runs out or the endpoint's row ceiling is reached. The
/// ceiling is a refusal and not a truncation: a short CSV looks exactly like a complete
/// one, and a caller who cannot tell will act on half the data.
async fn paged_entities(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    params: &[(String, String)],
    request: &mut Request,
    limits: &tabular::Limits,
) -> Result<(Value, bool), Box<Response<Body>>> {
    let verdict = gateway.pdp.decide(
        subject,
        Operation::QueryEntity,
        &query::requested(params),
        endpoint,
    );
    let Verdict::Rewrite(constraints) = verdict else {
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    };
    if constraints.empty {
        return Ok((Value::Array(Vec::new()), true));
    }
    tenancy::pin_tenant(request, &endpoint.space)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    // The caller's own window is a ceiling on the download, not the page size: paging is
    // the gateway's business and the caller asked for a file, not for a page of one.
    let wanted = query::first(params, "limit")
        .and_then(|limit| limit.parse::<u64>().ok())
        .unwrap_or(u64::from(limits.max_rows))
        .min(u64::from(limits.max_rows));
    let windowless: Vec<(String, String)> = params
        .iter()
        .filter(|(name, _)| name != "limit" && name != "offset")
        .cloned()
        .collect();
    // The same selector of last resort as `query_entities`: a download names a dataset.
    let fallback = if query::selects(&constraints) {
        Vec::new()
    } else {
        let types = dataset_types(gateway, request.headers()).await?;
        if types.is_empty() {
            return Ok((Value::Array(Vec::new()), constraints.restricted));
        }
        types
    };
    let narrowed = query::upstream(&windowless, &constraints, &fallback);
    let areas = geo::Areas::of(&constraints.geo_grants, constraints.geo_caller.as_deref());

    let mut collected: Vec<Value> = Vec::new();
    let mut held = 0u64;
    let mut offset = 0usize;
    loop {
        let target = format!("/ngsi-ld/v1/entities?{narrowed}&limit={PAGE}&offset={offset}");
        let answer = gateway
            .broker
            .send(
                axum::http::Method::GET,
                &target,
                request.headers().clone(),
                Body::empty(),
            )
            .await
            .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;

        let (parts, body) = answer.into_parts();
        if !parts.status.is_success() {
            return Err(Box::new(Response::from_parts(parts, body)));
        }
        let bytes = axum::body::to_bytes(body, MAX_BODY)
            .await
            .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
        let page: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
        let page = match page {
            Value::Array(entities) => entities,
            entity if entity.is_object() => vec![entity],
            _ => Vec::new(),
        };

        let fetched = page.len();
        // What the broker sent, whether or not the entity survives the grants: it was read,
        // parsed and held either way, and this is a ceiling on the gateway's memory rather
        // than a statement about the file (T-0810). A download past it is refused, because a
        // 413 is a better answer than the 500 an out-of-memory gateway gives everyone.
        held += bytes.len() as u64;
        collected.extend(page.into_iter().filter(|entity| {
            projection::permitted(entity, &constraints)
                && areas.as_ref().is_none_or(|areas| areas.admits(entity))
        }));
        if collected.len() as u64 > wanted || held > MAX_COLLECTED {
            return Err(Box::new(too_large()));
        }
        // A short page is the last page; a full one may not be.
        if fetched < PAGE {
            break;
        }
        offset += PAGE;
    }

    let mut entities = Value::Array(collected);
    projection::project_by_type(&mut entities, &constraints);
    Ok((entities, constraints.restricted))
}

/// A JSON body under a media type of its own, for the representations that have one.
fn typed_json_response(payload: &Value, media_type: &'static str) -> Response<Body> {
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
fn text_response(body: String, media_type: &'static str) -> Response<Body> {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static(media_type),
        )],
        body,
    )
        .into_response()
}

/// Every OGC API - Features request of one endpoint (T-0159, EP-29, EP-30, EP-31, EP-32, EP-39).
///
/// One handler over the whole Core resource tree, for the same reason the NGSI-LD surface has
/// one: the four steps before the branch — strip, resolve, refuse a write, translate the
/// parameters — are the steps that make the representation safe, and a second entry point is
/// a second place to forget one of them.
async fn ogc_features(
    State(gateway): State<Arc<Gateway>>,
    Path(params): Path<HashMap<String, String>>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let slug = params.get("slug").cloned().unwrap_or_default();
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::OgcFeatures),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    // EP-39: this representation has no write half at all. `OPTIONS` is answered rather than
    // refused, because that is how a client is supposed to discover the fact.
    let method = request.method().clone();
    if method == Method::OPTIONS {
        return read_only(StatusCode::NO_CONTENT);
    }
    if ![Method::GET, Method::HEAD].contains(&method) {
        return read_only(StatusCode::METHOD_NOT_ALLOWED);
    }

    let base = format!("{}{}", gateway.base_url(), endpoint.base_path);
    let raw_query = request.uri().query().unwrap_or_default().to_owned();
    let caller = query::parse(&raw_query);
    if let Some(wanted) = query::first(&caller, "crs") {
        if let Err(problem) = ogc::crs(wanted) {
            return bad_parameter(&problem);
        }
    }

    let segments: Vec<&str> = params
        .get("rest")
        .map(|rest| rest.split('/').filter(|part| !part.is_empty()).collect())
        .unwrap_or_default();

    match segments.as_slice() {
        [] => {
            let space = gateway.resolver.resolve_space(&endpoint.space);
            let (title, description) = ogc_titles(space.as_deref(), &endpoint, request.headers());
            typed_json_response(&ogc::landing(&base, &title, &description), ogc::JSON)
        }
        ["conformance"] => typed_json_response(&ogc::conformance(), ogc::JSON),
        // EP-40: the description is generated from what this caller may actually see, so the
        // collections it lists are the collections the rest of the document leads to.
        ["api"] => {
            let space = gateway.resolver.resolve_space(&endpoint.space);
            let (title, description) = ogc_titles(space.as_deref(), &endpoint, request.headers());
            match ogc_sample(&gateway, &endpoint, &subject, &mut request, &[]).await {
                Ok(sampled) => {
                    let types: Vec<String> = sampled.keys().cloned().collect();
                    typed_json_response(
                        &ogc::api_document(&base, &title, &description, &types),
                        ogc::OPENAPI,
                    )
                }
                Err(problem) => *problem,
            }
        }
        ["collections"] => {
            let space = gateway.resolver.resolve_space(&endpoint.space);
            let (_, description) = ogc_titles(space.as_deref(), &endpoint, request.headers());
            match ogc_sample(&gateway, &endpoint, &subject, &mut request, &[]).await {
                Ok(sampled) => {
                    let types: Vec<String> = sampled.keys().cloned().collect();
                    typed_json_response(&ogc::collections(&base, &types, &description), ogc::JSON)
                }
                Err(problem) => *problem,
            }
        }
        ["collections", name] => {
            let space = gateway.resolver.resolve_space(&endpoint.space);
            let (_, description) = ogc_titles(space.as_deref(), &endpoint, request.headers());
            let name = (*name).to_owned();
            match ogc_sample(
                &gateway,
                &endpoint,
                &subject,
                &mut request,
                std::slice::from_ref(&name),
            )
            .await
            {
                // EP-31: a type with no geometry is not a Feature Collection, and an ungranted
                // one is indistinguishable from it (R20).
                Ok(sampled) => match sampled.get(&name) {
                    Some(extent) => typed_json_response(
                        &ogc::collection(&base, &name, &description, Some(extent)),
                        ogc::JSON,
                    ),
                    None => ProblemDetails::not_found().into_response(),
                },
                Err(problem) => *problem,
            }
        }
        ["collections", name, "items"] => {
            ogc_items(
                &gateway,
                &endpoint,
                &subject,
                &mut request,
                name,
                &base,
                &raw_query,
            )
            .await
        }
        ["collections", name, "items", id] => {
            ogc_item(&gateway, &endpoint, &subject, &mut request, name, id, &base).await
        }
        _ => ProblemDetails::not_found().into_response(),
    }
}

/// One page of one collection (EP-33, EP-34, EP-36, EP-37, EP-38).
async fn ogc_items(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
    name: &str,
    base: &str,
    raw_query: &str,
) -> Response<Body> {
    let caller = query::parse(raw_query);
    let limit = query::first(&caller, "limit")
        .and_then(|limit| limit.parse::<usize>().ok())
        .unwrap_or(ogc::DEFAULT_LIMIT)
        .clamp(1, PAGE);
    let offset = query::first(&caller, "next")
        .and_then(ogc::offset_of)
        .unwrap_or_default();

    let mut upstream = vec![
        ("type".to_owned(), name.to_owned()),
        ("limit".to_owned(), limit.to_string()),
        ("offset".to_owned(), offset.to_string()),
    ];
    for (parameter, translate) in [
        (
            "bbox",
            ogc::bbox as fn(&str) -> Result<Vec<(String, String)>, ogc::ParamError>,
        ),
        ("datetime", ogc::datetime),
    ] {
        if let Some(raw) = query::first(&caller, parameter) {
            match translate(raw) {
                Ok(translated) => upstream.extend(translated),
                Err(problem) => return bad_parameter(&problem),
            }
        }
    }

    // EP-35: the CQL2 filter, compiled into the same three NGSI-LD parameters. A predicate the
    // subset does not cover is a `400` naming the operator, never a filter half applied.
    if let Some(raw) = query::first(&caller, "filter") {
        if let Some(lang) = query::first(&caller, "filter-lang") {
            if lang != cql2::LANG {
                return bad_parameter(&ogc::ParamError {
                    parameter: "filter-lang",
                    detail: format!("this endpoint reads {} only", cql2::LANG),
                });
            }
        }
        let compiled = match cql2::compile(raw) {
            Ok(compiled) => compiled,
            Err(problem) => return bad_parameter(&problem),
        };
        // NGSI-LD carries one `geoQ` and one `temporalQ`, so a filter that brings its own
        // cannot be combined with the parameter that means the same thing. Applying both
        // would drop one of them, and dropping one returns more than the caller asked for.
        for (from_filter, parameter) in [
            (!compiled.geo.is_empty(), "bbox"),
            (!compiled.temporal.is_empty(), "datetime"),
        ] {
            if from_filter && query::first(&caller, parameter).is_some() {
                return bad_parameter(&ogc::ParamError {
                    parameter: "filter",
                    detail: format!(
                        "{parameter} and a filter predicate of the same kind cannot both be \
                         applied; write the whole condition in one of them"
                    ),
                });
            }
        }
        upstream.extend(compiled.geo);
        upstream.extend(compiled.temporal);
        if let Some(q) = compiled.q {
            upstream.push(("q".to_owned(), q));
        }
    }

    let (entities, restricted) =
        match query_entities(gateway, endpoint, subject, &upstream, request).await {
            Ok(answer) => answer,
            Err(problem) => return *problem,
        };

    let timestamp = crate::pdp::now().to_rfc3339();
    let page = ogc::items(base, name, &entities, limit, offset, raw_query, &timestamp);
    let mut response = typed_json_response(&page, ogc::GEOJSON);
    response
        .headers_mut()
        .insert("content-crs", HeaderValue::from_static(ogc::CRS84));
    if restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// One feature by its entity URN (EP-33).
async fn ogc_item(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
    name: &str,
    id: &str,
    base: &str,
) -> Response<Body> {
    let upstream = vec![
        ("type".to_owned(), name.to_owned()),
        ("id".to_owned(), id.to_owned()),
        ("limit".to_owned(), "1".to_owned()),
    ];
    let (entities, restricted) =
        match query_entities(gateway, endpoint, subject, &upstream, request).await {
            Ok(answer) => answer,
            Err(problem) => return *problem,
        };

    // An entity that does not exist, one the grants withhold and one with no geometry all
    // answer the same 404, so a probe over the URN space learns nothing (R20, EP-33).
    let Some(feature) = entities
        .as_array()
        .and_then(|list| list.first())
        .and_then(|entity| ogc::feature(base, name, entity))
    else {
        return ProblemDetails::not_found().into_response();
    };
    let mut response = typed_json_response(&feature, ogc::GEOJSON);
    response
        .headers_mut()
        .insert("content-crs", HeaderValue::from_static(ogc::CRS84));
    if restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// The collections of an endpoint and what each of them spans, from one bounded page (EP-31,
/// EP-32).
///
/// The manifest cannot answer either question: whether a type carries a geometry and what its
/// data spans are properties of the data. Both come from one projected page, so a GIS client's
/// discovery costs one broker call and not one per type.
// ponytail: a page, not a scan. A type whose geometries all sit past `PAGE` entities is not
// listed; the upgrade path is the cached extent query EP-32 names with its `maxAgeSeconds`.
async fn ogc_sample(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
    wanted: &[String],
) -> Result<BTreeMap<String, ogc::Extent>, Box<Response<Body>>> {
    let visible = schema::visible(subject, endpoint, crate::pdp::now());
    let mut types = schema::visible_types(endpoint, &visible);
    if !wanted.is_empty() {
        types.retain(|name| wanted.contains(name));
        // A type the models do not declare is still a type the space may hold, and the PDP
        // decides either way; the models only ever narrow the discovery list.
        if types.is_empty() {
            types = wanted.to_vec();
        }
    }

    let mut upstream = vec![("limit".to_owned(), PAGE.to_string())];
    if !types.is_empty() {
        upstream.push(("type".to_owned(), types.join(",")));
    }
    let (entities, _) = query_entities(gateway, endpoint, subject, &upstream, request).await?;

    let mut grouped: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for entity in entities.as_array().into_iter().flatten() {
        if let Some(name) = entity.get("type").and_then(Value::as_str) {
            grouped
                .entry(name.to_owned())
                .or_default()
                .push(entity.clone());
        }
    }
    Ok(grouped
        .into_iter()
        .filter_map(|(name, group)| {
            let extent = ogc::extent_of(&Value::Array(group));
            // EP-31: only a type that actually carries a geometry is a Feature Collection.
            extent.bbox.is_some().then_some((name, extent))
        })
        .collect())
}

/// The endpoint's title and description in the caller's language (EP-32).
fn ogc_titles(space: Option<&Space>, endpoint: &Endpoint, headers: &HeaderMap) -> (String, String) {
    let accept = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok());
    let Some(space) = space else {
        return (endpoint.space.clone(), String::new());
    };
    let locale = space.default_locale.as_deref();
    let title = ogc::localized(&space.title, accept, locale);
    let description = ogc::localized(&space.description, accept, locale);
    (
        if title.is_empty() {
            endpoint.space.clone()
        } else {
            title.to_owned()
        },
        description.to_owned(),
    )
}

/// A parameter the representation cannot honour, naming it so the client can fix it (EP-34).
fn bad_parameter(problem: &ogc::ParamError) -> Response<Body> {
    let mut body = ProblemDetails::bad_request()
        .with_detail(problem.detail.clone())
        .into_response();
    *body.status_mut() = StatusCode::BAD_REQUEST;
    let _ = HeaderValue::from_str(problem.parameter)
        .map(|value| body.headers_mut().insert("x-parameter", value));
    body
}

/// The answer of a read-only representation to a method it does not have (EP-13, EP-39).
fn read_only(status: StatusCode) -> Response<Body> {
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

/// Every SensorThings v1.1 request of one endpoint (T-0160, EP-12, EP-13, TS-08).
///
/// The Sensing profile's four linked entity sets are four views of one projected entity page,
/// so the handler fetches once and translates, rather than treating each set as its own query.
/// That is what keeps a `Datastream` and the `Thing` it belongs to from disagreeing about what
/// the caller may see (EP-06, EP-07).
async fn sensorthings(
    State(gateway): State<Arc<Gateway>>,
    Path(params): Path<HashMap<String, String>>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let slug = params.get("slug").cloned().unwrap_or_default();
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::Sta),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    // EP-13: the representation is read-only, so every non-safe method is refused before a
    // path is parsed and before the broker is touched.
    let method = request.method().clone();
    if method == Method::OPTIONS {
        return read_only(StatusCode::NO_CONTENT);
    }
    if ![Method::GET, Method::HEAD].contains(&method) {
        return read_only(StatusCode::METHOD_NOT_ALLOWED);
    }

    let base = format!("{}{}", gateway.base_url(), endpoint.base_path);
    let caller = query::parse(request.uri().query().unwrap_or_default());
    let (top, skip, counted) = match sta_paging(&caller) {
        Ok(paging) => paging,
        Err(problem) => return bad_parameter(&problem),
    };
    let expand: Vec<&str> = query::first(&caller, "$expand")
        .map(|raw| raw.split(',').map(str::trim).collect())
        .unwrap_or_default();

    let segments = sta_segments(params.get("rest").map(String::as_str).unwrap_or_default());
    let addressed: Vec<(&str, Option<String>)> =
        segments.iter().map(|segment| sta_set(segment)).collect();

    // The service document is the only answer that needs no data at all.
    let Some((set, key)) = addressed.first() else {
        return typed_json_response(&sta::service_document(&base), sta::MEDIA_TYPE);
    };
    let child = addressed.get(1).map(|(set, _)| *set);
    if addressed.len() > 2 || (child.is_some() && key.is_none()) {
        return ProblemDetails::not_found().into_response();
    }
    // EP-13: a set the platform holds nothing for is empty rather than missing, so a
    // conformance suite can walk it; a set that is not in the profile at all is a 404.
    if sta::EMPTY_SETS.contains(set) && key.is_none() {
        return typed_json_response(
            &sta::collection(Vec::new(), counted.then_some(0), None),
            sta::MEDIA_TYPE,
        );
    }
    if !sta::SETS.contains(set) {
        return ProblemDetails::not_found().into_response();
    }

    // The one set that is a series rather than an instant, and so the one that comes from the
    // temporal tree instead of the entity page (T-0438, EP-12). Answered before the entity
    // query below, because a client charting a week must not be handed the single point the
    // current state carries, and because the entity query would be a second broker call for
    // an answer it cannot give.
    if let (Some("Datastreams"), Some(key), Some("Observations")) =
        (Some(*set), key.as_deref(), child)
    {
        return sta_series(
            &gateway,
            &endpoint,
            &subject,
            &mut request,
            &caller,
            &base,
            key,
        )
        .await;
    }

    let mut upstream = vec![
        ("limit".to_owned(), top.to_string()),
        ("offset".to_owned(), skip.to_string()),
    ];
    if let Some(key) = key {
        // Every id in this profile carries the entity URN in front of it, so addressing any
        // one of them is one entity query. An id that is not shaped that way names nothing.
        let Some(urn) = sta::urn_of(key) else {
            return ProblemDetails::not_found().into_response();
        };
        upstream.push(("id".to_owned(), urn.to_owned()));
    }
    if let Some(filter) = query::first(&caller, "$filter") {
        match sta::filter_to_q(filter) {
            Ok(q) => upstream.push(("q".to_owned(), q)),
            Err(problem) => {
                return bad_parameter(&ogc::ParamError {
                    parameter: "$filter",
                    detail: problem.0,
                })
            }
        }
    }

    let (entities, restricted) =
        match query_entities(&gateway, &endpoint, &subject, &upstream, &mut request).await {
            Ok(answer) => answer,
            Err(problem) => return *problem,
        };
    let page: Vec<&Value> = entities.as_array().into_iter().flatten().collect();

    let answer = match (*set, key.as_deref(), child) {
        ("Things", None, _) => sta::collection(
            page.iter()
                .filter_map(|entity| sta::thing(&base, entity, &expand))
                .collect(),
            counted.then_some(page.len()),
            None,
        ),
        ("Things", Some(_), None) => {
            match page
                .first()
                .and_then(|entity| sta::thing(&base, entity, &expand))
            {
                Some(thing) => thing,
                None => return ProblemDetails::not_found().into_response(),
            }
        }
        ("Things", Some(_), Some("Locations")) => {
            let items = page.first().and_then(|entity| sta::location(&base, entity));
            sta::collection(items.into_iter().collect(), counted.then_some(0), None)
        }
        ("Things", Some(_), Some("Datastreams")) => {
            let items = page
                .first()
                .map(|entity| sta::datastreams(&base, entity))
                .unwrap_or_default();
            sta::collection(items.clone(), counted.then_some(items.len()), None)
        }
        ("Locations", None, _) => {
            let items: Vec<Value> = page
                .iter()
                .filter_map(|entity| sta::location(&base, entity))
                .collect();
            sta::collection(items.clone(), counted.then_some(items.len()), None)
        }
        ("Datastreams" | "Observations" | "ObservedProperties", None, _) => {
            let items: Vec<Value> = page
                .iter()
                .flat_map(|entity| sta_items(set, &base, entity))
                .collect();
            sta::collection(items.clone(), counted.then_some(items.len()), None)
        }
        (set @ ("Datastreams" | "Observations" | "ObservedProperties"), Some(key), None) => {
            let Some(item) = page
                .first()
                .into_iter()
                .flat_map(|entity| sta_items(set, &base, entity))
                .find(|item| item["@iot.id"].as_str() == Some(key))
            else {
                return ProblemDetails::not_found().into_response();
            };
            item
        }
        _ => return ProblemDetails::not_found().into_response(),
    };

    let mut response = typed_json_response(&answer, sta::MEDIA_TYPE);
    if restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// The `Observations` of one `Datastream`: the history of one attribute (T-0438, EP-12).
///
/// The series is bounded before it is buffered, not after. `lastN` is what the broker is asked
/// for and it is the page the caller asked for plus what they skipped, so a datastream holding
/// a year of minutes costs one page either way; the same ceiling the rest of the
/// representation uses (`$top`, clamped to the gateway's own) is what bounds it.
async fn sta_series(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
    caller: &[(String, String)],
    base: &str,
    key: &str,
) -> Response<Body> {
    // A key that is not `{urn}/{attribute}` names no datastream, so it has no observations
    // rather than every entity's.
    let Some((urn, attribute)) = sta::split_stream_id(key) else {
        return ProblemDetails::not_found().into_response();
    };
    let (top, skip, counted) = match sta_paging(caller) {
        Ok(paging) => paging,
        Err(problem) => return bad_parameter(&problem),
    };

    // One more instance than the page needs, so a full page can tell that there is another
    // one without a second query. Without the peek a `$top` request could never offer
    // `@iot.nextLink`, because the ceiling and the answer would always be the same size.
    let ceiling = top.saturating_add(skip);
    // `$skip` becomes `lastN` here, and one request asks the broker for at most
    // `LAST_N_CAP` instances (GW26). A deeper page would be cut to the cap and answer with the
    // wrong rows, so it is refused by name; a `$filter` window reaches older observations.
    let deepest = (query::LAST_N_CAP as usize).saturating_sub(1);
    if ceiling > deepest {
        return bad_parameter(&ogc::ParamError {
            parameter: "$skip",
            detail: format!(
                "$top and $skip together reach at most {deepest} observations back; narrow the series with $filter on phenomenonTime"
            ),
        });
    }
    let mut upstream = vec![
        ("attrs".to_owned(), attribute.to_owned()),
        ("lastN".to_owned(), ceiling.saturating_add(1).to_string()),
    ];
    if let Some(filter) = query::first(caller, "$filter") {
        match sta::series_filter(filter) {
            Ok(series) => {
                upstream.extend(series.window);
                if let Some(q) = series.q {
                    upstream.push(("q".to_owned(), q));
                }
            }
            Err(problem) => {
                return bad_parameter(&ogc::ParamError {
                    parameter: "$filter",
                    detail: problem.0,
                })
            }
        }
    }

    let (entity, restricted) =
        match query_temporal(gateway, endpoint, subject, urn, &upstream, request).await {
            Ok(answer) => answer,
            Err(problem) => return *problem,
        };

    let mut items = sta::temporal_observations(base, &entity, attribute);
    if sta::newest_first(query::first(caller, "$orderby")) {
        // The broker's order is its own; `$orderby` is the client's, applied to the page it
        // is given rather than asked of a parameter NGSI-LD does not have.
        items.sort_by(|left, right| {
            right["phenomenonTime"]
                .as_str()
                .cmp(&left["phenomenonTime"].as_str())
        });
    }
    let total = items.len();
    let page: Vec<Value> = items.into_iter().skip(skip).take(top).collect();
    let next = (total > skip + page.len()).then(|| {
        format!(
            "{}/Observations?$top={top}&$skip={}",
            sta::datastream_link(base, key),
            skip + top
        )
    });

    // `@iot.count` is the total, and the total is only known when the broker returned less
    // than the ceiling: a series that hit it may hold more instants than were asked for, and
    // a count that is really the ceiling is a wrong number on somebody's chart.
    let count = counted.then_some(total).filter(|total| *total <= ceiling);
    let answer = sta::collection(page, count, next);
    let mut response = typed_json_response(&answer, sta::MEDIA_TYPE);
    if restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// `$top`, `$skip` and `$count`: the page an STA request asked for, bounded by the gateway's
/// own ceiling. One place, because the entity page and the temporal series have to agree on it.
fn sta_paging(caller: &[(String, String)]) -> Result<(usize, usize, bool), ogc::ParamError> {
    let top = query::first(caller, "$top")
        .and_then(|top| top.parse::<usize>().ok())
        .unwrap_or(sta::DEFAULT_TOP)
        .clamp(1, PAGE);
    let skip = match query::first(caller, "$skip") {
        None => 0,
        // An offset is what the caller has already read, so it bounds what the gateway asks
        // the broker for: on a Datastream's Observations it becomes `lastN`, one history
        // instance per skipped item (T-0809). Past the ceiling it is a refusal rather than a
        // clamp, because a silently clamped offset would answer a page the caller did not ask
        // for and page through the same rows forever.
        Some(raw) => match raw.parse::<usize>() {
            Ok(skip) if skip <= MAX_SKIP => skip,
            _ => {
                return Err(ogc::ParamError {
                    parameter: "$skip",
                    detail: format!("$skip must be a whole number of items, at most {MAX_SKIP}"),
                })
            }
        },
    };
    let counted = query::first(caller, "$count").is_some_and(|value| value == "true");
    Ok((top, skip, counted))
}

/// The items of one entity for one derived set.
fn sta_items(set: &str, base: &str, entity: &Value) -> Vec<Value> {
    match set {
        "Datastreams" => sta::datastreams(base, entity),
        "Observations" => sta::observations(base, entity),
        _ => sta::observed_properties(base, entity),
    }
}

/// The path segments of an STA resource path, without splitting inside a key literal.
///
/// A `Datastream` id is `{urn}/{attribute}`, so the slash inside the quotes is part of the
/// name and not a step down the tree. Splitting naively is how `Datastreams('a/b')/Observations`
/// becomes three segments that address nothing.
fn sta_segments(rest: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in rest.chars() {
        match character {
            '\'' => {
                quoted = !quoted;
                current.push(character);
            }
            '/' if !quoted => {
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// One segment as the set it names and the key it addresses, if any.
fn sta_set(segment: &str) -> (&str, Option<String>) {
    match segment.split_once('(') {
        Some((set, rest)) => {
            let key = rest.strip_suffix(')').unwrap_or(rest);
            let key = key.strip_prefix('\'').unwrap_or(key);
            let key = key.strip_suffix('\'').unwrap_or(key);
            (set, Some(key.replace("''", "'")))
        }
        None => (segment, None),
    }
}

/// A JSON body, serialized once.
fn json_response(payload: &Value) -> Response<Body> {
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
