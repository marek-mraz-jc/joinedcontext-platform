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
use crate::handlers::files::{file_csv, file_geojson, file_json, file_xlsx, file_zip, space_dump};
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
use crate::translators::{geojson, view_mapping};
use crate::{egress, mcp, middleware::tenancy, operations, query, telemetry};
use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{ACCEPT, ALLOW, AUTHORIZATION, CONTENT_LENGTH, IF_MATCH};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::Router;
use jc_core::kinds::{Audience, Operation, RateLimits, Representation};
use jc_core::ProblemDetails;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The largest broker answer the gateway will hold in memory. A request body is held to the
/// Organization's own limit instead ([`Gateway::max_request_body`], ADR-N-035).
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
    /// The key a subscription's subscriber is sealed with, so each delivery is decided again
    /// for them (GW27, T-2383). Absent refuses every subscription that routes a delivery.
    pub delivery_key: Option<Arc<crate::egress::subject::DeliveryKey>>,
    /// One token bucket per endpoint and caller (EP-20).
    pub rate_limiter: RateLimiter,
    /// The questions a destructive MCP tool is waiting on an answer to (AG-08, T-0849).
    pub elicitations: crate::mcp::elicitation::Elicitations,
    /// Whether a write waits for the Organization's verified domain (PF-41, T-2572).
    pub domain_gate: Arc<crate::domain_gate::DomainGate>,
    /// The per-subject bucket every hub request spends on top of the Endpoint's own, so a burst
    /// spread over many Endpoints is still one subject's burst (ADR-N-025 section 3, EP-87).
    pub hub_rate_limit: RateLimits,
    /// The largest request body read, from the Organization's `spec.limits.gateway`
    /// (ADR-N-035); swapped with the tables when the repository changes.
    max_request_body: AtomicUsize,
}

/// The hub's per-subject quota unless the deployment names another: ten Endpoint calls a
/// second, sustained, is more than one person's agent makes and less than a scraper wants.
// ponytail: a constant, set per gateway with `limit_hub_to`; an env var when an operator
// measures a reason to tune it.
pub const HUB_RATE_LIMIT: RateLimits = RateLimits {
    requests_per_minute: 600,
    burst: Some(60),
};

/// What a tool call re-enters the NGSI-LD path with: the caller's own `Authorization` header,
/// and the door the call came in by (EP-26, ADR-N-025 section 4).
#[derive(Debug, Clone)]
pub struct Credential {
    /// The header as the caller sent it; `None` for an anonymous caller.
    pub authorization: Option<HeaderValue>,
    /// Where the call entered.
    pub door: Door,
}

/// The door a request came in by (ADR-N-025 section 4).
///
/// A token whose audience is the hub is accepted only on a call that entered at `/api/mcp`,
/// and only for the Endpoints its `endpoint:{slug}` scopes name: the same token sent to an
/// Endpoint's own URL is refused, so a hub connector is never a key to the REST surfaces.
/// The door is a function argument and never a header, so no client can claim it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    /// `/api/endpoint/{slug}/…` or `/cs/{space}/…`, as every surface before the hub.
    Endpoint,
    /// `/api/mcp`, naming the Endpoint in the `endpoint` argument (EP-87).
    Hub,
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
            delivery_key: None,
            rate_limiter: RateLimiter::new(),
            elicitations: crate::mcp::elicitation::Elicitations::new(),
            domain_gate: Arc::new(crate::domain_gate::DomainGate::new(
                crate::domain_gate::Mode::Report,
            )),
            hub_rate_limit: HUB_RATE_LIMIT,
            max_request_body: AtomicUsize::new(crate::store::Limits::default().max_request_body),
        }
    }

    /// Spends at most `limits` a minute per subject on the MCP hub (ADR-N-025 section 3).
    pub fn limit_hub_to(mut self, limits: RateLimits) -> Self {
        self.hub_rate_limit = limits;
        self
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

    /// Seals each subscription's subscriber with `key` and decides every delivery again for
    /// them (GW27, T-2383).
    pub fn seal_subscribers_with(mut self, key: crate::egress::subject::DeliveryKey) -> Self {
        self.delivery_key = Some(Arc::new(key));
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
    pub(crate) fn audiences_for(&self, endpoint: &Endpoint) -> Vec<String> {
        audiences_of(
            &endpoint.slug,
            &endpoint.base_path,
            self.public_url.as_deref(),
        )
    }

    /// Every value that names the hub as an RFC 8707 resource: its Keycloak client and, when
    /// the deployment names its public URL, `{url}/api/mcp` (ADR-N-025 section 4).
    pub(crate) fn hub_audiences(&self) -> Vec<String> {
        let mut audiences = vec![HUB_AUDIENCE.to_owned()];
        if let Some(base) = self.public_url.as_deref() {
            audiences.push(format!("{base}{HUB_PATH}"));
        }
        audiences
    }

    /// What a token may be bound to on a call to `endpoint` through `door`.
    fn audiences_by(&self, endpoint: &Endpoint, door: Door) -> Vec<String> {
        let mut audiences = self.audiences_for(endpoint);
        if door == Door::Hub {
            audiences.extend(self.hub_audiences());
        }
        audiences
    }

    /// Whether a verified token reaches `endpoint` through the hub (EP-88, PF-45, PF-46): its
    /// audience names the Endpoint itself (its slug, its URL or the edge), or it names the hub
    /// and an `endpoint:{slug}` scope picks this Endpoint. The Endpoint's Policy still decides.
    pub(crate) fn reaches(&self, claims: &Claims, endpoint: &Endpoint) -> bool {
        claims.names_audience(&self.audiences_for(endpoint))
            || (claims.names_audience(&self.hub_audiences())
                && claims.endpoint_scopes().any(|slug| slug == endpoint.slug))
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

    /// Replaces the limits the Organization sets (ADR-N-035): from the next request on.
    pub fn replace_limits(&self, limits: crate::store::Limits) {
        self.max_request_body
            .store(limits.max_request_body, Ordering::Relaxed);
    }

    /// The largest request body this gateway reads now, in bytes.
    pub fn max_request_body(&self) -> usize {
        self.max_request_body.load(Ordering::Relaxed)
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
        // The NGSI-LD access URL the DCAT record advertises, both spellings (EP-90).
        .route("/api/endpoint/{slug}/ngsi-ld/v1", get(ngsi_ld_entry))
        .route("/api/endpoint/{slug}/ngsi-ld/v1/", get(ngsi_ld_entry))
        .route("/api/endpoint/{slug}/ngsi-ld/v1/{*rest}", any(ngsi_ld))
        .route("/api/endpoint/{slug}/access", get(access))
        .route("/api/endpoint/{slug}/mcp", post(mcp_message))
        // One connector over every Endpoint the token reaches (EP-87, ADR-N-025).
        .route(HUB_PATH, post(mcp::hub::message))
        .route(
            "/api/mcp/.well-known/oauth-protected-resource",
            get(mcp::hub::protected_resource),
        )
        .route("/api/endpoint/{slug}/access/check", post(access_check))
        .route(
            "/api/endpoint/{slug}/preview",
            post(crate::handlers::preview::preview),
        )
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
        // The organization's catalogue for harvesters: every public Endpoint's record (EP-84).
        .route("/catalog.jsonld", get(catalog_feed_jsonld))
        .route("/catalog.ttl", get(catalog_feed_turtle))
        .route("/cs", get(space_catalog))
        .route("/cs/{space}", get(space_record))
        .route("/cs/{space}/ngsi-ld/v1/{*rest}", any(space_ngsi_ld))
        .route("/cs/{space}/mcp", post(space_mcp_message))
        // SP-04: `schema/` is a child of every space, and the record advertises it. The
        // same two routes as the endpoint surface, because it is the same surface named
        // by its space (SP-03).
        .route("/cs/{space}/schema/index.json", get(space_schema_index))
        .route("/cs/{space}/dump/", get(space_dump))
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

/// The entry document at the NGSI-LD access URL (EP-90): where the entities are, the types this
/// caller may read with a query for each, and the access and schema documents. The gateway's
/// own answer, not a CIM 009 resource; the types are the schema surface's projection, so it
/// names none the grants withhold.
async fn ngsi_ld_entry(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::NgsiLd),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return as_ngsi_ld_error(*problem).await,
    };
    let root = format!("{}{}", gateway.base_url(), endpoint.base_path);
    let visible = schema::visible(&subject, &endpoint, crate::pdp::now());
    let types: Vec<Value> = schema::visible_types(&endpoint, &visible)
        .into_iter()
        .map(|name| {
            let url = format!("{root}/ngsi-ld/v1/entities?type={}", query::encode(&name));
            serde_json::json!({ "type": name, "query": url })
        })
        .collect();
    json_response(&serde_json::json!({
        "entities": format!("{root}/ngsi-ld/v1/entities"),
        "types": types,
        "access": format!("{root}/access"),
        "schema": format!("{root}/schema/index.json"),
    }))
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
    credential: Credential,
) -> Response<Body> {
    let Credential {
        authorization,
        door,
    } = credential;
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
        None => admit_by(
            &gateway,
            &endpoint.slug,
            Some(Representation::Mcp),
            request.headers(),
            door,
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
pub(crate) async fn as_ngsi_ld_error(response: Response<Body>) -> Response<Body> {
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

pub(crate) async fn serve_ngsi_ld(
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
                     idPattern alone is not a selector (CIM 009 5.7.2.4). The entry at \
                     /ngsi-ld/v1/ lists the types you may query here, or retrieve one \
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
    let limit = gateway.max_request_body();
    let Ok(sent) = axum::body::to_bytes(body, limit).await else {
        return ProblemDetails::bad_request()
            .with_detail(format!(
                "the request body is unreadable or larger than the {} MiB this organization \
                 accepts (spec.limits.gateway.maxRequestBodyMegabytes); send less at once, or \
                 ask an organization admin to raise it in Organization settings",
                limit / (1024 * 1024)
            ))
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
    // The ids a divided batch forwards and the entities it refused, merged into one answer once
    // the broker has spoken (GW18).
    let mut divided: Option<(Vec<String>, Refused)> = None;
    if subscribing && !sent.is_empty() {
        match narrowed_subscription(
            &sent,
            &constraints,
            &endpoint,
            gateway.egress_base(),
            &gateway.private_hosts,
            operation,
            (gateway.delivery_key.as_deref(), &subject),
        ) {
            Ok(narrowed) => sent = narrowed,
            Err(problem) => return problem.into_response(),
        }
        parts.headers.remove(CONTENT_LENGTH);
        parts
            .headers
            .insert(CONTENT_LENGTH, HeaderValue::from(sent.len() as u64));
    } else if operation.is_write() && !sent.is_empty() {
        match judge_write(
            &sent,
            &path,
            operation,
            &constraints,
            &endpoint,
            &gateway.org_domain,
        ) {
            Err(problem) => return problem.into_response(),
            Ok(Judged::Whole) => {}
            // A batch the grants divide: the permitted entities go on, the refused ones are
            // answered beside what the broker makes of the rest (GW18).
            Ok(Judged::Divided { permitted, refused }) => {
                if permitted.is_empty() {
                    return batch_result(Vec::new(), refusal_entries(refused));
                }
                let ids = permitted.iter().filter_map(batch_entry_id).collect();
                sent = match serde_json::to_vec(&permitted) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::error!(%error, "the permitted part of a batch does not serialize");
                        return ProblemDetails::internal().into_response();
                    }
                };
                parts.headers.remove(CONTENT_LENGTH);
                parts
                    .headers
                    .insert(CONTENT_LENGTH, HeaderValue::from(sent.len() as u64));
                divided = Some((ids, refused));
            }
        }
    }

    // A quantity is written in its model's unit or not at all (DM-06, T-2810): a wrong
    // `unitCode` is refused, a missing one filled with the model's code unless the space
    // refuses that. A view endpoint writes in its target model, which the space's rules do not
    // describe, so it is left to the mapping.
    let mut units_filled = Vec::new();
    if operation.is_write() && !subscribing && !sent.is_empty() && endpoint.view_mapping.is_none() {
        if let Some(rules) = endpoint
            .declared_types
            .as_ref()
            .map(|declared| &declared.units)
        {
            match filled_units(&mut sent, &path, rules) {
                Ok(filled) => units_filled = filled,
                Err(problem) => {
                    tracing::info!(slug = %endpoint.slug, "write refused: a quantity is not in its model's unit");
                    return problem.into_response();
                }
            }
            if !units_filled.is_empty() {
                parts.headers.remove(CONTENT_LENGTH);
                parts
                    .headers
                    .insert(CONTENT_LENGTH, HeaderValue::from(sent.len() as u64));
            }
        }
    }

    // A write keeps the relationships of the space's model (DM-70, T-2740): the target's type,
    // one target on a single end, a required end, and a target the writer can read in the space.
    // A batch is answered per entity (207); anything else is refused whole. A view endpoint
    // writes in its target model, which the space's rules do not describe.
    if operation.is_write() && !subscribing && endpoint.view_mapping.is_none() {
        if let Some(rules) = endpoint
            .declared_types
            .as_ref()
            .map(|declared| &declared.relationships)
            .filter(|rules| !rules.is_empty())
        {
            let before = sent.len();
            let held = HeldWrite {
                gateway,
                subject: &subject,
                endpoint: &endpoint,
                headers: &parts.headers,
                path: &path,
                params: &params,
                operation,
                rules,
            };
            if let Err(answer) = held.check(&mut sent, &mut divided).await {
                tracing::info!(slug = %endpoint.slug, "write refused: it breaks a relationship of the space's model");
                return *answer;
            }
            if sent.len() != before {
                parts.headers.remove(CONTENT_LENGTH);
                parts
                    .headers
                    .insert(CONTENT_LENGTH, HeaderValue::from(sent.len() as u64));
            }
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

    // A delete runs the rule of every relationship that points at what it deletes (DM-71,
    // T-2858), after every other check passed, so a delete refused above has changed nothing.
    if matches!(operation, Operation::DeleteEntity | Operation::DeleteBatch)
        && endpoint.view_mapping.is_none()
    {
        if let Some(rules) = endpoint
            .declared_types
            .as_ref()
            .map(|declared| &declared.relationships)
            .filter(|rules| !rules.is_empty())
        {
            let held = HeldDelete {
                gateway,
                subject: &subject,
                endpoint: &endpoint,
                headers: &parts.headers,
                rules,
            };
            let held = if operation == Operation::DeleteEntity {
                match operations::addressed_entity(&path).map(query::decode) {
                    Some(id) => held.entity(&id).await.map(|()| false),
                    None => Ok(false),
                }
            } else {
                held.batch(&mut sent, &mut divided).await
            };
            match held {
                Ok(true) => {
                    parts.headers.remove(CONTENT_LENGTH);
                    parts
                        .headers
                        .insert(CONTENT_LENGTH, HeaderValue::from(sent.len() as u64));
                }
                Ok(false) => {}
                Err(answer) => {
                    tracing::info!(slug = %endpoint.slug, "delete refused: a relationship keeps it");
                    return *answer;
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
    } else if let (Operation::RetrieveTemporal, Some(id)) =
        (operation, operations::addressed_entity(&path))
    {
        // One entity's history takes no `type` (CIM 009 6.19.3.1); its type is judged on the
        // answer below (T-2987).
        query::upstream_by_id(&params, &constraints, &query::decode(id))
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
    // CIM 009 6.3.15: GeoJSON is rendered here, after the projection, from the JSON the broker
    // is asked for: the broker's own Feature is not an entity the grants can judge, and judged
    // as one it was the 404 of a type nobody granted (T-2583).
    let geojson = matches!(
        operation,
        Operation::RetrieveEntity | Operation::QueryEntity
    ) && parts
        .headers
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains(geojson::MEDIA_TYPE));
    let lang = parts
        .headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if geojson {
        parts
            .headers
            .insert(ACCEPT, HeaderValue::from_static("application/json"));
    }
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
    let answer = match &endpoint.view_mapping {
        None => projected,
        Some(mapping) => translated(projected, mapping).await,
    };
    let answer = match divided {
        Some((forwarded, refused)) => merged_batch(answer, forwarded, refused).await,
        None if geojson => as_geojson(answer, operation, lang.as_deref()).await,
        None => answer,
    };
    with_filled_units(answer, &units_filled)
}

/// Ids the existence read of a write asks for at once; a batch naming more is read in chunks.
const TARGETS_PER_READ: usize = 100;

/// One write held against the relationships of the space's model (DM-70, T-2740).
struct HeldWrite<'a> {
    gateway: &'a Gateway,
    subject: &'a Subject,
    endpoint: &'a Endpoint,
    headers: &'a HeaderMap,
    path: &'a str,
    params: &'a [(String, String)],
    operation: Operation,
    rules: &'a crate::relationships::RelationshipRules,
}

impl HeldWrite<'_> {
    /// Refuses a write that breaks a relationship, or divides a batch: the entities that break
    /// one leave `sent` and join the refusals the answer merges (GW18's 207).
    async fn check(
        &self,
        sent: &mut Vec<u8>,
        divided: &mut Option<(Vec<String>, Refused)>,
    ) -> Result<(), Box<Response<Body>>> {
        use crate::relationships::{self, Extent, Shape};
        let refuse = |problem: ProblemDetails| Box::new(problem.into_response());

        let addressed = operations::addressed_entity(self.path).map(query::decode);
        let targeted = operations::targeted_attribute(self.path);
        // Removing a required end outright empties it as surely as a null does.
        if self.operation == Operation::DeleteAttrs {
            if let (Some(id), Some(attribute)) = (addressed.as_deref(), targeted) {
                relationships::deleted_attribute(id, attribute, self.rules)
                    .map_err(|violation| refuse(violation.into()))?;
            }
            return Ok(());
        }
        let extent = match self.operation {
            Operation::CreateEntity | Operation::ReplaceEntity | Operation::CreateBatch => {
                Extent::Whole
            }
            // `options=update` makes an upsert a merge into what is there (CIM 009 5.6.8).
            Operation::UpsertBatch
                if !query::first(self.params, "options").is_some_and(|options| {
                    options.split(',').any(|option| option.trim() == "update")
                }) =>
            {
                Extent::Whole
            }
            Operation::UpsertBatch
            | Operation::UpdateBatch
            | Operation::MergeBatch
            | Operation::MergeEntity
            | Operation::AppendAttrs
            | Operation::UpdateAttrs
            | Operation::ReplaceAttrs
            | Operation::UpdateEntity => Extent::Partial,
            // A temporal write records history, and a delete carries no targets.
            _ => return Ok(()),
        };
        if sent.is_empty() {
            return Ok(());
        }
        let Ok(payload) = serde_json::from_slice::<Value>(sent) else {
            // `judge_write` already refused a body that is not JSON.
            return Ok(());
        };
        let batch = matches!(
            self.operation,
            Operation::CreateBatch
                | Operation::UpsertBatch
                | Operation::UpdateBatch
                | Operation::MergeBatch
        );
        let shape = Shape {
            addressed: addressed.as_deref(),
            targeted,
            extent,
        };
        let (Value::Array(entries), true) = (&payload, batch) else {
            let targets = relationships::targets_of(&payload, shape, self.rules)
                .map_err(|violation| refuse(violation.into()))?;
            let found = self
                .readable(&targets)
                .await
                .map_err(|problem| refuse(*problem))?;
            if let Some(violation) = relationships::first_missing(&targets, &found) {
                return Err(refuse(violation.into()));
            }
            let claims = relationships::claims_of(&payload, shape, self.rules);
            if let Some(violation) = self
                .untaken(&claims, &[])
                .await
                .map_err(|problem| refuse(*problem))?
            {
                return Err(refuse(violation.into()));
            }
            return Ok(());
        };

        // Each entity on its own, then one existence read for the targets of all of them.
        let mut refused: Refused = Vec::new();
        let mut kept: Vec<(usize, relationships::Targets)> = Vec::new();
        for (at, entry) in entries.iter().enumerate() {
            match relationships::targets_of(entry, shape, self.rules) {
                Ok(targets) => kept.push((at, targets)),
                Err(violation) => {
                    refused.push((batch_entry_id(entry).unwrap_or_default(), violation.into()))
                }
            }
        }
        let mut all = relationships::Targets::new();
        for (_, targets) in &kept {
            all.extend(
                targets
                    .iter()
                    .map(|(urn, named)| (urn.clone(), named.clone())),
            );
        }
        let found = self
            .readable(&all)
            .await
            .map_err(|problem| refuse(*problem))?;
        let mut permitted = Vec::new();
        // One-to-one targets in the batch's order: the first entity that names one takes it.
        let mut taken: Vec<relationships::Claim> = Vec::new();
        for (at, targets) in kept {
            let entry = &entries[at];
            if let Some(violation) = relationships::first_missing(&targets, &found) {
                refused.push((batch_entry_id(entry).unwrap_or_default(), violation.into()));
                continue;
            }
            let claims = relationships::claims_of(entry, shape, self.rules);
            match self
                .untaken(&claims, &taken)
                .await
                .map_err(|problem| refuse(*problem))?
            {
                Some(violation) => {
                    refused.push((batch_entry_id(entry).unwrap_or_default(), violation.into()))
                }
                None => {
                    taken.extend(claims);
                    permitted.push(entry.clone());
                }
            }
        }
        if refused.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = permitted.iter().filter_map(batch_entry_id).collect();
        let refused = match divided.take() {
            Some((_, mut earlier)) => {
                earlier.extend(refused);
                earlier
            }
            None => refused,
        };
        if permitted.is_empty() {
            return Err(Box::new(batch_result(Vec::new(), refusal_entries(refused))));
        }
        *sent = serde_json::to_vec(&permitted).map_err(|error| {
            tracing::error!(%error, "the entities a relationship check kept do not serialize");
            refuse(ProblemDetails::internal())
        })?;
        *divided = Some((ids, refused));
        Ok(())
    }

    /// The first one-to-one target of `claims` that another source stores, or that `earlier`
    /// claims of this write already took (DM-70 `target-taken`). A read of the space right
    /// before the write, not a constraint in the store: two writes inside the window between
    /// this read and the broker's write can both take one target (Architecture/11 §1.2, T-2858).
    async fn untaken(
        &self,
        claims: &[crate::relationships::Claim],
        earlier: &[crate::relationships::Claim],
    ) -> Result<Option<crate::relationships::Violation>, Box<ProblemDetails>> {
        let all: Vec<_> = earlier.iter().chain(claims).cloned().collect();
        if let Some(twice) = crate::relationships::claimed_twice(&all) {
            return Ok(Some(twice.taken()));
        }
        let mut asked = BTreeSet::new();
        for claim in claims {
            if !asked.insert((&claim.class, &claim.slot, &claim.object)) {
                continue;
            }
            // Two answers are enough: the holder itself, and anyone else.
            let storing = storing(
                &self.gateway.broker,
                self.headers,
                &claim.class,
                &claim.slot,
                &claim.object,
                2,
                0,
            )
            .await?;
            let other = storing
                .iter()
                .filter_map(|entity| entity.get("id").and_then(Value::as_str))
                .any(|id| Some(id) != claim.holder.as_deref());
            if other {
                return Ok(Some(claim.taken()));
            }
        }
        Ok(None)
    }

    /// The targets the writer can read in the space: one read through this endpoint under the
    /// caller's own read grants, so a target they may not read is as absent as one that is not
    /// there, and a refusal tells them nothing a read would not (DM-71).
    async fn readable(
        &self,
        targets: &crate::relationships::Targets,
    ) -> Result<BTreeSet<String>, Box<ProblemDetails>> {
        let mut found = BTreeSet::new();
        if targets.is_empty() {
            return Ok(found);
        }
        let requested = evaluator::Request {
            types: targets.values().map(|named| named.target.clone()).collect(),
            ..Default::default()
        };
        let Verdict::Rewrite(read) = self.gateway.pdp.decide(
            self.subject,
            Operation::QueryEntity,
            &requested,
            self.endpoint,
        ) else {
            return Ok(found);
        };
        if read.empty {
            return Ok(found);
        }
        // The grant's filters narrow the read; the attributes and the time window would only
        // hide the entity, which is all this read looks for.
        let mut probe = (*read).clone();
        probe.attrs.clear();
        probe.temporal_q = None;
        probe.temporal_windows.clear();
        let narrowed = query::upstream(&[], &probe, &[]);
        let urns: Vec<&String> = targets.keys().collect();
        for chunk in urns.chunks(TARGETS_PER_READ) {
            let ids = chunk
                .iter()
                .map(|urn| query::encode(urn))
                .collect::<Vec<_>>()
                .join(",");
            let answer = conditional::retrieve(
                &self.gateway.broker,
                // Only which ids exist is asked (`pick=id`, CIM 009 4.21): a target with a
                // polygon or a long history is megabytes, and a chunk of them outgrew the
                // probe's cap and answered every write 500 (praha CityDistrict, T-2971).
                &format!(
                    "/ngsi-ld/v1/entities?id={ids}&pick=id&limit={}&{narrowed}",
                    chunk.len()
                ),
                self.headers,
            )
            .await?;
            if !(200..300).contains(&answer.status) {
                tracing::error!(
                    status = answer.status,
                    "the broker refused the read of a write's relationship targets"
                );
                return Err(Box::new(ProblemDetails::new(
                    502,
                    "upstream-unavailable",
                    "Broker Unavailable",
                )));
            }
            found.extend(
                answer
                    .body
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entity| entity.get("id").and_then(Value::as_str))
                    .filter(|id| write_guard::check_granted_id(id, &probe).is_ok())
                    .map(str::to_owned),
            );
        }
        Ok(found)
    }
}

/// The entities of `class` whose stored end `slot` names `object`: the computed end's query
/// (DM-67). Read with the pinned tenant and none of the caller's grants, because a relationship
/// rule holds for the whole space; what the caller may see of the answer is decided by whoever
/// uses it.
async fn storing(
    broker: &Broker,
    headers: &HeaderMap,
    class: &str,
    slot: &str,
    object: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<Value>, Box<ProblemDetails>> {
    let q = query::encode(&format!("{slot}==\"{object}\""));
    let target = format!(
        "/ngsi-ld/v1/entities?type={}&q={q}&attrs={}&limit={limit}&offset={offset}",
        query::encode(class),
        query::encode(slot),
    );
    let answer = conditional::retrieve(broker, &target, headers).await?;
    if !(200..300).contains(&answer.status) {
        tracing::error!(
            status = answer.status,
            "the broker refused the read of the entities that point at a target"
        );
        return Err(Box::new(ProblemDetails::new(
            502,
            "upstream-unavailable",
            "Broker Unavailable",
        )));
    }
    Ok(match answer.body {
        Value::Array(entities) => entities,
        _ => Vec::new(),
    })
}

/// Entities a delete's rules may reach, counting the ones they change and the ones they delete
/// with it; a delete that reaches more is refused before anything changes (DM-71).
const REACHED_PER_DELETE: usize = 500;

/// Entities that point at a target, read per page.
const POINTING_PER_READ: usize = 100;

/// One change a delete's rules make to an entity that points at what it deletes (DM-66).
#[derive(Debug)]
enum Unlinking {
    /// The entity goes with its target (`cascade` on a single end).
    Delete(String),
    /// The whole attribute goes (`set-null` on a single end).
    DeleteAttribute { id: String, slot: String },
    /// One link instance of a many end goes.
    DeleteInstance {
        id: String,
        slot: String,
        dataset: Option<String>,
    },
    /// A link instance whose `object` list named the target keeps its other targets.
    Rewrite {
        id: String,
        slot: String,
        dataset: Option<String>,
        objects: Vec<String>,
    },
}

/// One delete held against the relationships of the space's model (DM-71, T-2858).
///
/// The broker keeps no relationship rules (the owner's decision of 2026-09-25), so the rules run
/// here as the gateway's own writes: every entity that points at what is deleted is read, the
/// delete is refused while one of them keeps it (`restrict`, or a rule that would empty a
/// required end, or an entity the caller may not change), and otherwise the entities are changed
/// or deleted first and the entity itself last. Nothing is changed until every rule has passed;
/// a reference written after the read is the race window Architecture/11 §1.2 names.
struct HeldDelete<'a> {
    gateway: &'a Gateway,
    subject: &'a Subject,
    endpoint: &'a Endpoint,
    headers: &'a HeaderMap,
    rules: &'a crate::relationships::RelationshipRules,
}

impl HeldDelete<'_> {
    /// Runs the rules of a delete of one entity; the entity itself is the forwarded delete's.
    async fn entity(&self, id: &str) -> Result<(), Box<Response<Body>>> {
        let answer = |problem: Box<ProblemDetails>| Box::new(problem.into_response());
        let steps = self.plan(id).await.map_err(answer)?;
        self.apply(id, &steps).await.map_err(answer)
    }

    /// Runs the rules of a batch delete per id: an id the rules keep is refused in the batch's
    /// answer (207), the others go on. Answers whether `sent` was rewritten.
    async fn batch(
        &self,
        sent: &mut Vec<u8>,
        divided: &mut Option<(Vec<String>, Refused)>,
    ) -> Result<bool, Box<Response<Body>>> {
        let answer = |problem: Box<ProblemDetails>| Box::new(problem.into_response());
        let Ok(Value::Array(entries)) = serde_json::from_slice::<Value>(sent) else {
            return Ok(false);
        };
        let mut refused: Refused = Vec::new();
        let mut permitted = Vec::new();
        let mut plans = Vec::new();
        for entry in entries {
            let Some(id) = entry.as_str() else {
                permitted.push(entry);
                continue;
            };
            match self.plan(id).await {
                Ok(steps) => {
                    plans.push((id.to_owned(), steps));
                    permitted.push(entry);
                }
                Err(problem) if problem.status == 409 => refused.push((id.to_owned(), *problem)),
                Err(problem) => return Err(answer(problem)),
            }
        }
        for (id, steps) in &plans {
            self.apply(id, steps).await.map_err(answer)?;
        }
        if refused.is_empty() {
            return Ok(false);
        }
        let ids: Vec<String> = permitted.iter().filter_map(batch_entry_id).collect();
        let refused = match divided.take() {
            Some((_, mut earlier)) => {
                earlier.extend(refused);
                earlier
            }
            None => refused,
        };
        if permitted.is_empty() {
            return Err(Box::new(batch_result(Vec::new(), refusal_entries(refused))));
        }
        *sent = serde_json::to_vec(&permitted).map_err(|error| {
            tracing::error!(%error, "the ids a delete's rules kept do not serialize");
            answer(Box::new(ProblemDetails::internal()))
        })?;
        *divided = Some((ids, refused));
        Ok(true)
    }

    /// What the rules of a delete of `root` change, in the order they are applied: the
    /// attributes and links first, then the entities deleted with it, farthest first, so a stop
    /// part way never leaves a reference to something already gone. Reads only.
    async fn plan(&self, root: &str) -> Result<Vec<Unlinking>, Box<ProblemDetails>> {
        use crate::relationships::{unlink, Unlink};
        let mut edits = Vec::new();
        let mut deleted = Vec::new();
        let mut going = BTreeSet::from([root.to_owned()]);
        let mut queue = std::collections::VecDeque::from([root.to_owned()]);
        while let Some(id) = queue.pop_front() {
            for at in self.rules.referencing_id(&id) {
                let pointing = self.pointing(at.class, at.slot, &id).await?;
                for entity in &pointing {
                    let Some(other) = entity.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    // An entity deleted by this same delete keeps nothing and needs no change.
                    if going.contains(other) {
                        continue;
                    }
                    match unlink(entity, at.slot, at.end, &id) {
                        Unlink::Refused => {
                            return Err(self.kept(at.class, at.slot, &id, &pointing, true));
                        }
                        Unlink::DeleteEntity => {
                            self.may(
                                Operation::DeleteEntity,
                                at.class,
                                other,
                                at.slot,
                                &id,
                                &pointing,
                            )?;
                            going.insert(other.to_owned());
                            queue.push_back(other.to_owned());
                            deleted.push(Unlinking::Delete(other.to_owned()));
                        }
                        Unlink::DeleteAttribute => {
                            self.may(
                                Operation::DeleteAttrs,
                                at.class,
                                other,
                                at.slot,
                                &id,
                                &pointing,
                            )?;
                            edits.push(Unlinking::DeleteAttribute {
                                id: other.to_owned(),
                                slot: at.slot.to_owned(),
                            });
                        }
                        Unlink::Instances { removed, rewritten } => {
                            if !removed.is_empty() {
                                self.may(
                                    Operation::DeleteAttrs,
                                    at.class,
                                    other,
                                    at.slot,
                                    &id,
                                    &pointing,
                                )?;
                            }
                            if !rewritten.is_empty() {
                                self.may(
                                    Operation::UpdateAttrs,
                                    at.class,
                                    other,
                                    at.slot,
                                    &id,
                                    &pointing,
                                )?;
                            }
                            edits.extend(removed.into_iter().map(|dataset| {
                                Unlinking::DeleteInstance {
                                    id: other.to_owned(),
                                    slot: at.slot.to_owned(),
                                    dataset,
                                }
                            }));
                            edits.extend(rewritten.into_iter().map(|(dataset, objects)| {
                                Unlinking::Rewrite {
                                    id: other.to_owned(),
                                    slot: at.slot.to_owned(),
                                    dataset,
                                    objects,
                                }
                            }));
                        }
                    }
                    if going.len() + edits.len() > REACHED_PER_DELETE {
                        return Err(Box::new(too_far(root)));
                    }
                }
            }
        }
        edits.extend(deleted.into_iter().rev());
        Ok(edits)
    }

    /// Every entity of `class` whose end `slot` points at `id`, page by page.
    async fn pointing(
        &self,
        class: &str,
        slot: &str,
        id: &str,
    ) -> Result<Vec<Value>, Box<ProblemDetails>> {
        let mut all = Vec::new();
        loop {
            let page = storing(
                &self.gateway.broker,
                self.headers,
                class,
                slot,
                id,
                POINTING_PER_READ,
                all.len(),
            )
            .await?;
            let last = page.len() < POINTING_PER_READ;
            all.extend(page);
            if last {
                return Ok(all);
            }
            if all.len() > REACHED_PER_DELETE {
                return Err(Box::new(too_far(id)));
            }
        }
    }

    /// Refuses the change unless the caller's own grants allow `operation` on `other`: a rule
    /// never reaches an entity its caller could not have changed by hand. A grant that decides
    /// from the stored entity is not evaluated here and is held as a refusal.
    fn may(
        &self,
        operation: Operation,
        class: &str,
        other: &str,
        slot: &str,
        id: &str,
        pointing: &[Value],
    ) -> Result<(), Box<ProblemDetails>> {
        let requested = evaluator::Request {
            types: BTreeSet::from([class.to_owned()]),
            ..Default::default()
        };
        let allowed =
            match self
                .gateway
                .pdp
                .decide(self.subject, operation, &requested, self.endpoint)
            {
                Verdict::Rewrite(granted) => {
                    !granted.empty
                        && !conditional::state_dependent(&granted)
                        && write_guard::check_granted_id(other, &granted).is_ok()
                }
                _ => false,
            };
        if allowed {
            Ok(())
        } else {
            Err(self.kept(class, slot, id, pointing, false))
        }
    }

    /// 409 for a delete the rules keep (DM-71): the slot and how many point at the target,
    /// their ids only as far as the caller may read them, and never an id when the reason is an
    /// entity the caller may not change.
    fn kept(
        &self,
        class: &str,
        slot: &str,
        id: &str,
        pointing: &[Value],
        by_rule: bool,
    ) -> Box<ProblemDetails> {
        let count = pointing.len();
        let detail = if by_rule {
            format!(
                "{count} `{class}` still point at `{id}` through `{slot}`, and the model's delete \
                 rule keeps it while they do; unlink or delete them first (DM-71)"
            )
        } else {
            format!(
                "{count} `{class}` point at `{id}` through `{slot}`, and the delete rule would \
                 change one you may not change; ask someone who may, or unlink it first (DM-71)"
            )
        };
        let ids: Vec<Value> = if by_rule {
            self.readable_ids(class, pointing)
        } else {
            Vec::new()
        };
        Box::new(
            ProblemDetails::conflict()
                .with_detail(detail)
                .with_extension("rule", Value::String("restrict".to_owned()))
                .with_extension("slot", Value::String(slot.to_owned()))
                .with_extension("count", Value::from(count))
                .with_extension("ids", Value::Array(ids)),
        )
    }

    /// The ids of `pointing` the caller's read grants on this endpoint select.
    fn readable_ids(&self, class: &str, pointing: &[Value]) -> Vec<Value> {
        let requested = evaluator::Request {
            types: BTreeSet::from([class.to_owned()]),
            ..Default::default()
        };
        let Verdict::Rewrite(read) = self.gateway.pdp.decide(
            self.subject,
            Operation::QueryEntity,
            &requested,
            self.endpoint,
        ) else {
            return Vec::new();
        };
        if read.empty || conditional::state_dependent(&read) {
            return Vec::new();
        }
        pointing
            .iter()
            .filter_map(|entity| entity.get("id").and_then(Value::as_str))
            .filter(|id| write_guard::check_granted_id(id, &read).is_ok())
            .map(|id| Value::String(id.to_owned()))
            .collect()
    }

    /// Applies the planned changes in order. A step the broker refuses stops the delete with the
    /// entity still in place, and the answer says how many changes were already made.
    async fn apply(&self, root: &str, steps: &[Unlinking]) -> Result<(), Box<ProblemDetails>> {
        for (done, step) in steps.iter().enumerate() {
            let entity = |id: &str| format!("/ngsi-ld/v1/entities/{}", query::encode(id));
            let attribute =
                |id: &str, slot: &str| format!("{}/attrs/{}", entity(id), query::encode(slot));
            let (method, target, body) = match step {
                Unlinking::Delete(id) => (Method::DELETE, entity(id), None),
                Unlinking::DeleteAttribute { id, slot } => {
                    (Method::DELETE, attribute(id, slot), None)
                }
                Unlinking::DeleteInstance { id, slot, dataset } => {
                    let target = match dataset {
                        Some(dataset) => format!(
                            "{}?datasetId={}",
                            attribute(id, slot),
                            query::encode(dataset)
                        ),
                        None => attribute(id, slot),
                    };
                    (Method::DELETE, target, None)
                }
                Unlinking::Rewrite {
                    id,
                    slot,
                    dataset,
                    objects,
                } => {
                    let mut value =
                        serde_json::json!({ "type": "Relationship", "object": objects });
                    if let Some(dataset) = dataset {
                        value["datasetId"] = Value::String(dataset.clone());
                    }
                    (Method::PATCH, attribute(id, slot), Some(value.to_string()))
                }
            };
            let mut headers = HeaderMap::new();
            if let Some(tenant) = self.headers.get(tenancy::TENANT) {
                headers.insert(tenancy::TENANT, tenant.clone());
            }
            if body.is_some() {
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
            }
            let body = body.map_or_else(Body::empty, Body::from);
            let status = match self
                .gateway
                .broker
                .send(method, &target, headers, body)
                .await
            {
                Ok(answer) => answer.status(),
                Err(error) => {
                    tracing::error!(%error, "the broker did not take a delete rule's change");
                    StatusCode::BAD_GATEWAY
                }
            };
            // An entity another delete already took is gone either way; a link or an attribute
            // that is not found was not removed by this delete, so it stops like any refusal.
            let gone = status == StatusCode::NOT_FOUND && matches!(step, Unlinking::Delete(_));
            if !status.is_success() && !gone {
                tracing::error!(%status, %target, "the broker refused a delete rule's change");
                return Err(Box::new(
                    ProblemDetails::new(502, "upstream-unavailable", "Broker Unavailable")
                        .with_detail(format!(
                            "`{root}` is not deleted: the broker refused change {} of {} its \
                             delete rules make, after {done} were made (DM-71)",
                            done + 1,
                            steps.len()
                        ))
                        .with_extension("changed", Value::from(done)),
                ));
            }
        }
        Ok(())
    }
}

/// 409 for a delete whose rules reach more entities than one delete may change.
fn too_far(id: &str) -> ProblemDetails {
    ProblemDetails::conflict()
        .with_detail(format!(
            "deleting `{id}` would reach more than {REACHED_PER_DELETE} entities through the \
             model's delete rules; delete them in smaller parts first (DM-71)"
        ))
        .with_extension("rule", Value::String("restrict".to_owned()))
}

/// Checks and fills the units of a write body in place (DM-06), answering the quantities it
/// filled. The body is rewritten only when something was filled.
fn filled_units(
    sent: &mut Vec<u8>,
    path: &str,
    rules: &crate::units::UnitRules,
) -> Result<Vec<String>, Box<ProblemDetails>> {
    if rules.is_empty() {
        return Ok(Vec::new());
    }
    let Ok(mut payload) = serde_json::from_slice::<Value>(sent) else {
        // `judge_write` already refused a body that is not JSON; nothing is left to check.
        return Ok(Vec::new());
    };
    let addressed = operations::addressed_entity(path).map(query::decode);
    let shape = crate::units::Shape {
        addressed: addressed.as_deref(),
        targeted: operations::targeted_attribute(path),
    };
    let filled = crate::units::enforce(&mut payload, shape, rules)
        .map_err(|refusal| Box::new(ProblemDetails::from(refusal)))?;
    if !filled.is_empty() {
        *sent = serde_json::to_vec(&payload).map_err(|error| {
            tracing::error!(%error, "a write with its units filled does not serialize");
            Box::new(ProblemDetails::internal())
        })?;
    }
    Ok(filled)
}

/// The answer to a write whose units the gateway filled, saying which (DM-06). A refused
/// write changed nothing, so it says nothing either.
fn with_filled_units(mut answer: Response<Body>, filled: &[String]) -> Response<Body> {
    if filled.is_empty() || !answer.status().is_success() {
        return answer;
    }
    match HeaderValue::from_str(&filled.join(", ")) {
        Ok(value) => {
            answer
                .headers_mut()
                .insert(crate::units::FILLED_HEADER, value);
        }
        Err(_) => {
            tracing::warn!("the filled units do not fit a header; the answer does not name them")
        }
    }
    answer
}

/// A projected NGSI-LD answer as GeoJSON (CIM 009 6.3.15, T-2583): one entity is a `Feature`, a
/// query a `FeatureCollection`. A refusal or a miss is left as it is, so the status is the one
/// the JSON read would have answered.
async fn as_geojson(
    answer: Response<Body>,
    operation: Operation,
    lang: Option<&str>,
) -> Response<Body> {
    let (mut parts, body) = answer.into_parts();
    if !parts.status.is_success() {
        return Response::from_parts(parts, body);
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        tracing::error!("the broker's answer is larger than the gateway can render as GeoJSON");
        return ProblemDetails::internal().into_response();
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        tracing::error!("the projected answer is not JSON and cannot be rendered as GeoJSON");
        return ProblemDetails::internal().into_response();
    };
    let rendered = match (operation, &payload) {
        (Operation::QueryEntity, Value::Array(entities)) => serde_json::json!({
            "type": "FeatureCollection",
            "features": entities.iter().map(|entity| geojson::ngsi_feature(entity, lang)).collect::<Vec<_>>(),
        }),
        _ => geojson::ngsi_feature(&payload, lang),
    };
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(geojson::MEDIA_TYPE),
    );
    match serde_json::to_vec(&rendered) {
        Ok(bytes) => Response::from_parts(parts, Body::from(bytes)),
        Err(error) => {
            tracing::error!(%error, "the GeoJSON answer does not serialize");
            ProblemDetails::internal().into_response()
        }
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

/// The entities of a batch the gateway refused, each with the id it named and the problem.
type Refused = Vec<(String, ProblemDetails)>;

/// What the write guard makes of a write payload (GW17, GW18).
enum Judged {
    /// Every entity it names is permitted: the body goes on as it was sent.
    Whole,
    /// A batch the grants divide: `permitted` goes on, `refused` is answered per entity id.
    Divided {
        permitted: Vec<Value>,
        refused: Refused,
    },
}

/// The batch operations whose entities are judged one by one (GW18). A batch query is a read
/// and is narrowed instead.
fn divides(operation: Operation) -> bool {
    matches!(
        operation,
        Operation::CreateBatch
            | Operation::UpsertBatch
            | Operation::UpdateBatch
            | Operation::MergeBatch
            | Operation::DeleteBatch
    )
}

/// The entity id a batch entry names: the URN itself in a batch delete, the `id` of an entity
/// otherwise.
fn batch_entry_id(entry: &Value) -> Option<String> {
    entry
        .as_str()
        .or_else(|| {
            entry
                .get("id")
                .or_else(|| entry.get("@id"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

/// Checks a write payload and answers with the problem that refuses it whole, or with what
/// goes on (GW17, GW18).
///
/// The body of a `PATCH .../attrs/{name}` is the bare attribute value, so it is wrapped
/// back under its name before the grant is consulted; anything else is an entity, or a
/// batch of them. An entity is judged whole (GW17); a batch is divided per entity, so the
/// entities the grants permit are still applied (GW18), unless one of them is malformed.
fn judge_write(
    body: &[u8],
    path: &str,
    operation: Operation,
    constraints: &Constraints,
    endpoint: &Endpoint,
    org_domain: &str,
) -> Result<Judged, Box<ProblemDetails>> {
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return Err(Box::new(
            ProblemDetails::bad_request().with_detail("request body is not JSON"),
        ));
    };
    let payload = match operations::targeted_attribute(path) {
        Some(name) => serde_json::json!({ name: payload }),
        None => payload,
    };
    // An addressed write changes the entity in its path and nothing else (GW17, PF-44).
    let addressed = operations::addressed_entity(path).map(query::decode);

    let entries = match payload {
        Value::Array(entries) if divides(operation) => entries,
        Value::Array(entities) => {
            for entity in &entities {
                refuse_entity(
                    entity,
                    addressed.as_deref(),
                    constraints,
                    endpoint,
                    org_domain,
                )?;
            }
            return Ok(Judged::Whole);
        }
        other => {
            refuse_entity(
                &other,
                addressed.as_deref(),
                constraints,
                endpoint,
                org_domain,
            )?;
            return Ok(Judged::Whole);
        }
    };
    // Every refusal of a batch is answered under the id it names, so an entry that names none
    // leaves nothing to answer it under: the request is malformed as a whole.
    if entries.iter().any(|entry| batch_entry_id(entry).is_none()) {
        return Err(Box::new(ProblemDetails::bad_request().with_detail(
            "every entry of a batch operation names its entity by a string id",
        )));
    }
    let mut permitted = Vec::with_capacity(entries.len());
    let mut refused = Vec::new();
    for entry in entries {
        match refuse_entity(&entry, None, constraints, endpoint, org_domain) {
            Ok(()) => permitted.push(entry),
            // A malformed or foreign id is no grant decision but a malformed write, and a batch
            // half applied around it is the one outcome nobody asked for (PF-42, T-1697).
            Err(problem) if problem.status == StatusCode::BAD_REQUEST.as_u16() => {
                return Err(problem);
            }
            Err(problem) => {
                refused.push((batch_entry_id(&entry).unwrap_or_default(), *problem));
            }
        }
    }
    Ok(if refused.is_empty() {
        Judged::Whole
    } else {
        Judged::Divided { permitted, refused }
    })
}

/// Checks one entity of a write, or the fragment of an addressed one, against the grants and
/// the endpoint, and answers with the problem that refuses it (GW17).
fn refuse_entity(
    entity: &Value,
    addressed: Option<&str>,
    constraints: &Constraints,
    endpoint: &Endpoint,
    org_domain: &str,
) -> Result<(), Box<ProblemDetails>> {
    if let Some(problem) = undeclared_type(entity, endpoint) {
        return Err(Box::new(problem));
    }
    if let (Some(path_id), Some(object)) = (addressed, entity.as_object()) {
        let named = object.get("id").or_else(|| object.get("@id"));
        if named.is_some_and(|id| id.as_str() != Some(path_id)) {
            return Err(Box::new(
                ProblemDetails::bad_request()
                    .with_detail("the body names another entity than the path"),
            ));
        }
        if let Some(kind) = object.get("type").or_else(|| object.get("@type")) {
            write_guard::check_identifier(
                path_id,
                Some(kind.as_str().unwrap_or_default()),
                &endpoint.space,
                org_domain,
            )
            .map_err(|refusal| Box::new(ProblemDetails::from(refusal)))?;
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
        return Err(Box::new(ProblemDetails::from(refusal)));
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
    outcome.map_err(|refusal| {
        tracing::info!(slug = %endpoint.slug, %refusal, "write refused");
        Box::new(ProblemDetails::from(refusal))
    })
}

/// The `errors` members of a `BatchOperationResult` for the entities the gateway refused.
fn refusal_entries(refused: Refused) -> Vec<Value> {
    refused
        .into_iter()
        .map(|(id, problem)| serde_json::json!({ "entityId": id, "error": problem }))
        .collect()
}

/// A `207 Multi-Status` carrying a `BatchOperationResult` (CIM 009 clause 5.2.16, GW18).
fn batch_result(success: Vec<Value>, errors: Vec<Value>) -> Response<Body> {
    let body = serde_json::json!({ "success": success, "errors": errors });
    let mut answer = (StatusCode::MULTI_STATUS, body.to_string()).into_response();
    answer.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    answer
}

/// The broker's answer to the permitted part of a divided batch, with the gateway's refusals
/// beside it, as one `BatchOperationResult` (GW18).
///
/// A `207` from the broker is extended; a success without an id list means every forwarded id
/// was applied; a refusal of the whole forwarded part is answered for each of its entities, in
/// the words the gateway already chose for it (a broker failure is never passed on verbatim).
async fn merged_batch(
    answer: Response<Body>,
    forwarded: Vec<String>,
    refused: Refused,
) -> Response<Body> {
    let (parts, body) = answer.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        tracing::error!("the broker's answer to a batch is larger than the gateway can merge");
        return ProblemDetails::internal().into_response();
    };
    let payload = serde_json::from_slice::<Value>(&bytes).ok();
    let (success, mut errors) = match (parts.status, payload) {
        (StatusCode::MULTI_STATUS, Some(Value::Object(mut result))) => {
            let mut members = |name: &str| match result.remove(name) {
                Some(Value::Array(values)) => values,
                _ => Vec::new(),
            };
            (members("success"), members("errors"))
        }
        (status, Some(Value::Array(ids))) if status.is_success() => (ids, Vec::new()),
        (status, _) if status.is_success() => (
            forwarded.into_iter().map(Value::String).collect(),
            Vec::new(),
        ),
        (status, payload) => {
            let problem = payload.filter(Value::is_object).unwrap_or_else(|| {
                serde_json::json!(ProblemDetails::new(
                    status.as_u16(),
                    "broker-failure",
                    "The context broker could not answer",
                ))
            });
            let errors = forwarded
                .into_iter()
                .map(|id| serde_json::json!({ "entityId": id, "error": problem }))
                .collect();
            (Vec::new(), errors)
        }
    };
    errors.extend(refusal_entries(refused));
    batch_result(success, errors)
}

/// 400 naming the type, when an entity's type is not a class of the space's one model (DM-61).
///
/// The model is published on the schema surface, so naming the missing type tells the caller
/// nothing a grant hides; it is the one thing they need to fix the write. A space that names no
/// model yet is not narrowed here.
fn undeclared_type(entity: &Value, endpoint: &Endpoint) -> Option<ProblemDetails> {
    let declared = endpoint.declared_types.as_ref()?;
    let object = entity.as_object()?;
    let types: Vec<&str> = match object.get("type").or_else(|| object.get("@type"))? {
        Value::String(one) => vec![one.as_str()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => return Some(ProblemDetails::bad_request().with_detail("entity type is not a string")),
    };
    let missing = types.into_iter().find(|kind| !declared.declares(kind))?;
    tracing::info!(slug = %endpoint.slug, entity_type = missing, model = %declared.model, "write refused: type not in the model");
    Some(ProblemDetails::bad_request().with_detail(format!(
        "entity type `{missing}` is not a class of the space's data model `{}` (DM-61)",
        declared.model
    )))
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
    (key, subject): (Option<&egress::subject::DeliveryKey>, &Subject),
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
    egress::notifications::seal_route(&mut payload, &endpoint.base_path, key, subject)?;
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
    authenticate_by(gateway, endpoint, headers, Door::Endpoint)
}

/// [`authenticate`], for a request that came in by `door` (ADR-N-025 section 4).
pub(crate) fn authenticate_by(
    gateway: &Gateway,
    endpoint: &Endpoint,
    headers: &HeaderMap,
    door: Door,
) -> Result<Subject, Box<ProblemDetails>> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    let raw = match token::bearer(presented) {
        Ok(raw) => raw,
        // No token at all: the anonymous caller, which only a public endpoint admits
        // (EP-16, GW22).
        Err(token::Rejected::NoToken) if endpoint.admits(None) => {
            let mut subject = Subject::anonymous();
            with_endpoint_roles(&mut subject, endpoint, &[]);
            return Ok(subject);
        }
        Err(rejected) => return Err(Box::new(rejected.into())),
    };

    // A token was presented and the gateway has no realm to check it against. Believing
    // it would be believing the client.
    let Some(verifier) = &gateway.verifier else {
        tracing::warn!("a token was presented but no realm is configured");
        return Err(Box::new(ProblemDetails::unauthorized()));
    };
    let claims = verifier
        .verify(raw, &gateway.audiences_by(endpoint, door))
        .map_err(|rejected| Box::new(ProblemDetails::from(rejected)))?;
    // A hub token whose scopes do not pick this Endpoint does not reach it, and learns no more
    // than it would about a slug that does not exist (EP-88, SP-20).
    if door == Door::Hub && !gateway.reaches(&claims, endpoint) {
        return Err(Box::new(ProblemDetails::not_found()));
    }
    subject_from(gateway, endpoint, &claims)
}

/// The subject a verified token is on `endpoint`: who it names, admitted by the Endpoint's
/// audience, with the Endpoint's own roles (PF-46, EP-14, AP-97).
pub(crate) fn subject_from(
    gateway: &Gateway,
    endpoint: &Endpoint,
    claims: &Claims,
) -> Result<Subject, Box<ProblemDetails>> {
    // An App's client reaches the Endpoints its App reads and no other, whatever audience its
    // token names (AP-113): the same answer as a token bound to another resource.
    if let Some(azp) = claims.azp.as_deref() {
        if !gateway.accounts.load().admits_on(azp, &endpoint.slug) {
            tracing::info!(azp, slug = %endpoint.slug, "an App client's token on an Endpoint its App does not read");
            return Err(Box::new(ProblemDetails::unauthorized()));
        }
    }
    let client_roles = endpoint
        .roles
        .app_client
        .as_deref()
        .map(|client| claims.client_roles(client))
        .unwrap_or_default();
    let mut subject = subject_of(claims, endpoint, gateway)?;
    with_endpoint_roles(&mut subject, endpoint, &client_roles);
    Ok(subject)
}

/// The roles an Endpoint gives exist on requests through it alone (AP-97). One that a token or
/// an account asserts is dropped whatever its source, so no realm role, no ServiceAccount
/// template and no other endpoint can carry an application's grant; then this endpoint's own
/// are added for the caller it admitted: on an App's Endpoint, from `client_roles`, the roles
/// the token of that App's own client carries (ADR-N-030).
fn with_endpoint_roles(subject: &mut Subject, endpoint: &Endpoint, client_roles: &[String]) {
    subject
        .roles
        .retain(|role| !role.starts_with(jc_core::kinds::ENDPOINT_ROLE_PREFIX));
    let held: Vec<String> = endpoint
        .roles
        .held_by(subject.user.as_deref(), &subject.groups, client_roles)
        .map(str::to_owned)
        .collect();
    subject.roles.extend(held);
}

/// The audience of a person signed in at the edge (ADR-N-019): the gateway's own name, accepted
/// on every endpoint, because a session cannot name an endpoint approved after the login. The
/// Policy decision stays per endpoint (PF-46).
pub const EDGE_AUDIENCE: &str = "context-gateway";

/// The hub's Keycloak client, which is also the audience of the tokens it obtains (ADR-N-025
/// section 4, EP-88).
pub const HUB_AUDIENCE: &str = "mcp-hub";

/// Where the hub answers (EP-87).
pub const HUB_PATH: &str = "/api/mcp";

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
    let mut via = None;
    if let Some(azp) = claims.azp.as_deref() {
        let accounts = gateway.accounts.load();
        let account = accounts.resolve(azp);
        // A person's token the account's client exchanged (ADR-N-038, AG-95): the subject is
        // a person, not the client's own service-account user. Decided as that person when the
        // account declares delegation, and refused when it does not, so no client can carry a
        // person's rights, or its own rights under a person's name, without saying so.
        let for_a_person = claims
            .preferred_username
            .as_deref()
            .is_some_and(|user| !user.eq_ignore_ascii_case(&format!("service-account-{azp}")));
        match account {
            Some(account) if for_a_person && account.delegates => {
                via = Some(account.name.clone());
            }
            Some(_) if for_a_person => {
                tracing::warn!(
                    azp,
                    "a person's token from a client whose account delegates nothing"
                );
                return Err(Box::new(ProblemDetails::forbidden()));
            }
            _ => {}
        }
        if let Some(account) = account.filter(|_| via.is_none()) {
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
                // A workload's grants are its manifest's (PF-35, T-2545): a group the token
                // claims is Keycloak membership nobody reviewed, so it reaches no group Policy.
                groups: BTreeSet::new(),
                did: None,
                agreement: None,
                via: None,
            });
        }
        if via.is_none() && claims.preferred_username.is_none() {
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
        via,
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
pub(crate) fn principal_of(subject: &Subject) -> String {
    match (&subject.user, &subject.service_account, &subject.did) {
        (Some(user), _, _) => match &subject.via {
            Some(account) => format!("user:{user} via serviceAccount:{account}"),
            None => format!("user:{user}"),
        },
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
    admit_by(gateway, slug, representation, headers, Door::Endpoint)
}

/// [`admit`], for a request that came in by `door` (ADR-N-025 section 4).
pub(crate) fn admit_by(
    gateway: &Gateway,
    slug: &str,
    representation: Option<Representation>,
    headers: &HeaderMap,
    door: Door,
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
    let subject = authenticate_by(gateway, &endpoint, headers, door)
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

    let credential = Credential {
        authorization,
        door: Door::Endpoint,
    };
    match mcp::endpoint_facade::handle(gateway, endpoint, subject, credential, message).await {
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

/// The records of every public Endpoint as the anonymous caller reads each one, which is all
/// the feed may carry (EP-84, EP-69): no token is read, so no grant widens what it lists.
fn public_records(gateway: &Gateway) -> Vec<serde_json::Value> {
    let anonymous = Subject::anonymous();
    let now = crate::pdp::now();
    gateway
        .resolver
        .endpoints()
        .into_iter()
        .filter(|endpoint| endpoint.audience == Audience::Public)
        .map(|endpoint| {
            let visible = schema::visible(&anonymous, &endpoint, now);
            let index = schema::index(&endpoint, &visible, sha256_hex);
            let space = gateway.resolver.resolve_space(&endpoint.space);
            endpoint_surface::dataset(&endpoint, space.as_deref(), &index, gateway.base_url())
        })
        .collect()
}

fn catalog_feed(gateway: &Gateway) -> serde_json::Value {
    endpoint_surface::feed(
        public_records(gateway),
        gateway.base_url(),
        &gateway.org_domain,
    )
}

/// `GET /catalog.jsonld`: the organization's DCAT-AP catalogue as JSON-LD (EP-84).
async fn catalog_feed_jsonld(State(gateway): State<Arc<Gateway>>) -> Response<Body> {
    let body = serde_json::to_string(&catalog_feed(&gateway)).unwrap_or_default();
    (
        [(axum::http::header::CONTENT_TYPE, "application/ld+json")],
        body,
    )
        .into_response()
}

/// `GET /catalog.ttl`: the same catalogue as Turtle (EP-84).
async fn catalog_feed_turtle(State(gateway): State<Arc<Gateway>>) -> Response<Body> {
    let body = endpoint_surface::feed_turtle(&catalog_feed(&gateway));
    ([(axum::http::header::CONTENT_TYPE, "text/turtle")], body).into_response()
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

    /// AG-95: the audit line of a delegated call names the person and the account it came
    /// through; an ordinary person or workload is named alone.
    #[test]
    fn the_audit_principal_names_the_account_a_persons_call_came_through() {
        let person = Subject {
            user: Some("jana".to_owned()),
            ..Subject::default()
        };
        let delegated = Subject {
            via: Some("agent-proxy".to_owned()),
            ..person.clone()
        };
        let workload = Subject {
            service_account: Some("agent-proxy".to_owned()),
            ..Subject::default()
        };
        assert_eq!(principal_of(&person), "user:jana");
        assert_eq!(
            principal_of(&delegated),
            "user:jana via serviceAccount:agent-proxy"
        );
        assert_eq!(principal_of(&workload), "serviceAccount:agent-proxy");
    }

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
