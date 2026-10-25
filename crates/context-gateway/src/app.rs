//! The gateway itself and its router: what every surface shares, and the guards every request
//! passes on its way to one (T-0005, EP-01, GW1). Each surface's routes and handlers live in its
//! own module and are merged here, so adding or deleting a surface is one `merge` line (T-4472).

use crate::auth::accounts::ServiceAccounts;
use crate::auth::dataspace_token::Agreements;
use crate::auth::subject::{audiences_of, HUB_AUDIENCE, HUB_PATH};
use crate::auth::token::{self, Claims, Verifier};
use crate::federation::{Federations, Member};
use crate::handlers::{
    catalog, files, mappings, ngsi_ld, ogc, openapi_doc, schema_routes, space_routes, view_access,
    ws,
};
use crate::middleware::in_flight;
use crate::middleware::rate_limit::{self, RateLimiter};
use crate::pdp::Pdp;
use crate::proxy::Broker;
use crate::resolver::{Endpoint, SlugResolver, Space};
use crate::{mcp, oauth, telemetry};
use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{HeaderValue, Response};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use jc_core::kinds::RateLimits;
use jc_core::ProblemDetails;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
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
    /// Exchanges a caller's token for the member brokers of a federated read (T-3999); absent
    /// answers such a read `503`, never forwards it as anyone else.
    pub exchanger: Option<Arc<crate::federation::exchange::Exchanger>>,
    /// The service accounts a token's `azp` can name; swapped whole when the repository
    /// changes, so a withdrawn credential stops resolving without a restart (R48).
    pub(crate) accounts: ArcSwap<ServiceAccounts>,
    /// The registrations of every space, swapped whole like the accounts are. Empty is the
    /// ordinary case: a space with no registration federates nothing (EP-70).
    federation: ArcSwap<Federations>,
    /// The data space agreements, swapped whole with the rest: a terminated agreement stops
    /// authorising reads in the same reconcile that withdraws its compiled grants (DS-12).
    pub(crate) agreements: ArcSwap<Agreements>,
    /// The named MCP servers, swapped whole with the rest (ADR-N-043, EP-92).
    servers: ArcSwap<crate::mcp::server::McpServers>,
    /// The Projects that name their publisher, for their own catalogues (EP-127).
    publishers: ArcSwap<crate::store::Publishers>,
    /// Each Project's Organization and the audited Endpoints (ADR-N-054 2.3, 2.8).
    provenance: ArcSwap<crate::store::Provenance>,
    /// The Mappings each Endpoint publishes beside its model, by slug (EP-53).
    mappings: ArcSwap<crate::handlers::mappings::Mappings>,
    /// The MQTT edge brokers, swapped whole with the rest (ADR-N-048, PL-75).
    brokers: ArcSwap<crate::mqtt::Brokers>,
    /// The gateway's public base URL, when the deployment names one.
    pub public_url: Option<String>,
    /// The stateless authorization server for anonymous MCP (ADR-N-052 §2.2, EP-129); absent
    /// serves no `/oauth/*` and takes no token of its own.
    pub oauth: Option<Arc<oauth::Issuer>>,
    /// Hosts inside the platform's networks a notification may be delivered to (T-1302).
    pub private_hosts: Vec<String>,
    /// One token bucket per endpoint and caller (EP-20).
    pub rate_limiter: RateLimiter,
    /// The creates each form Endpoint has taken today (EP-97).
    pub creates: crate::creates::DailyCreates,
    /// The WebSocket connections each caller holds open (EP-107).
    pub ws_open: crate::handlers::ws::Open,
    /// Each Endpoint's WebSocket fan-out: one broker subscription, its entities, its clients'
    /// filters (ADR-N-049, EP-114).
    pub hubs: crate::fanout::socket::Hubs,
    /// What each caller reached a moment ago, by token (T-4528).
    pub(crate) reaches: crate::mcp::hub::Reaches,
    /// How long a WebSocket opened with a token lasts (API/07 §3, T-4132); an anonymous one is
    /// bounded only by the broker's own lifetime.
    pub ws_token_lifetime: std::time::Duration,
    /// How long a fan-out hub keeps its upstream after its last client left (ADR-N-049 §2.1).
    pub ws_linger: std::time::Duration,
    /// Whether a write waits for the Organization's verified domain (PF-41, T-2572).
    pub domain_gate: Arc<crate::domain_gate::DomainGate>,
    /// Where a person's own connectors are read, `/api/mcp/me/{name}` (EP-123); `None` serves
    /// no personal address.
    pub personal: Option<Arc<crate::mcp::personal::Personal>>,
    /// `jc-assistant`'s search for `search_documents` (EP-133); `None` lists the tool nowhere.
    pub documents: Option<Arc<crate::mcp::documents::Documents>>,
    /// The per-subject bucket every hub request spends on top of the Endpoint's own, so a burst
    /// spread over many Endpoints is still one subject's burst (ADR-N-025 section 3, EP-87).
    pub hub_rate_limit: RateLimits,
    /// What one client address without a token may spend a minute over every MCP surface
    /// together (T-4168).
    pub anonymous_mcp_rate_limit: RateLimits,
    /// Where each space's personal-data key is mounted, one file per space
    /// (`JC_GATEWAY_PERSONAL_KEYS`, DM-121); `None` refuses every write a model hashes.
    pub personal_keys: Option<std::path::PathBuf>,
    /// The largest request body read, from the Organization's `spec.limits.gateway`
    /// (ADR-N-035); swapped with the tables when the repository changes.
    max_request_body: AtomicUsize,
    /// The DCAT profiles the Organization serves its records in (T-4499), as
    /// [`jc_dcat::profile::Offered::bits`].
    dcat_profiles: AtomicU8,
    /// The KiB of write bodies the gateway holds at once (T-4077); see [`WRITE_BUDGET_MIB`].
    writes: Arc<tokio::sync::Semaphore>,
    /// The budget `writes` was made with, in KiB: what one write may take at most.
    write_budget_kib: u32,
    write_wait: std::time::Duration,
    /// The requests and `file.csv` downloads in flight, in total and per caller (T-4782).
    pub(crate) in_flight: crate::middleware::in_flight::InFlight,
    /// The anonymous `file.csv` answers and the space type lists a download reuses (T-5629).
    pub(crate) downloads: crate::handlers::download_cache::Downloads,
    pub(crate) held_types: crate::handlers::download_cache::HeldTypes,
}

/// The MiB of write bodies in the gateway at once unless the deployment names another
/// (`JC_GATEWAY_WRITE_BUDGET_MIB`, T-4077). A write is parsed and judged several times on its way
/// (grants, units, the model, the relationships), so its peak is a multiple of its body: dev's
/// gateway, 365 MiB at rest under a 512 MiB limit, was OOMKilled by a burst of about 160 MiB
/// while a slow broker held pages of 1000 entities. A write waits for its share in arrival order,
/// before its body is read, and one that waits [`WRITE_WAIT`] in vain is answered 503 with
/// `Retry-After`, which a pipeline retries.
pub const WRITE_BUDGET_MIB: usize = 16;

/// The seconds a write over the budget is told to wait.
pub(crate) const WRITE_RETRY_SECONDS: u32 = 1;

/// How long a write waits for its share of the write budget before it is answered 503: well
/// under the 30 s a pipeline's client waits for an answer (T-4123).
pub const WRITE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The hub's per-subject quota unless the deployment names another: ten Endpoint calls a
/// second, sustained, is more than one person's agent makes and less than a scraper wants.
// ponytail: a constant, set per gateway with `limit_hub_to`; an env var when an operator
// measures a reason to tune it.
pub const HUB_RATE_LIMIT: RateLimits = RateLimits {
    requests_per_minute: 600,
    burst: Some(60),
};

/// The MCP quota of one client address without a token, over the hub, the named servers and
/// every Endpoint's and Context Space's MCP together (T-4168): two calls a second, sustained,
/// with room for an agent's opening burst of `initialize`, `tools/list` and a few reads.
// ponytail: a constant, set per gateway with `limit_anonymous_mcp_to`; an env var when an
// operator measures a reason to tune it.
pub const ANONYMOUS_MCP_RATE_LIMIT: RateLimits = RateLimits {
    requests_per_minute: 120,
    burst: Some(30),
};

impl Gateway {
    /// A gateway with an empty endpoint table.
    pub fn new(broker: Broker, pdp: Box<dyn Pdp>, org_domain: impl Into<String>) -> Self {
        Self {
            resolver: SlugResolver::new(),
            broker,
            pdp,
            org_domain: org_domain.into(),
            verifier: None,
            exchanger: None,
            accounts: ArcSwap::from_pointee(ServiceAccounts::new()),
            federation: ArcSwap::from_pointee(Federations::new()),
            agreements: ArcSwap::from_pointee(Agreements::new()),
            servers: ArcSwap::from_pointee(Vec::new()),
            publishers: ArcSwap::from_pointee(Default::default()),
            provenance: ArcSwap::from_pointee(Default::default()),
            mappings: ArcSwap::from_pointee(Default::default()),
            brokers: ArcSwap::from_pointee(crate::mqtt::Brokers::new()),
            public_url: None,
            oauth: None,
            private_hosts: Vec::new(),
            rate_limiter: RateLimiter::new(),
            creates: Default::default(),
            ws_open: Default::default(),
            hubs: Default::default(),
            reaches: Default::default(),
            ws_token_lifetime: crate::handlers::ws::TOKEN_LIFETIME,
            ws_linger: crate::fanout::socket::LINGER,
            domain_gate: Arc::new(crate::domain_gate::DomainGate::new(
                crate::domain_gate::Mode::Report,
            )),
            personal: None,
            documents: None,
            personal_keys: None,
            hub_rate_limit: HUB_RATE_LIMIT,
            anonymous_mcp_rate_limit: ANONYMOUS_MCP_RATE_LIMIT,
            max_request_body: AtomicUsize::new(crate::store::Limits::default().max_request_body),
            dcat_profiles: AtomicU8::new(0),
            writes: Arc::new(tokio::sync::Semaphore::new(WRITE_BUDGET_MIB * 1024)),
            write_budget_kib: (WRITE_BUDGET_MIB * 1024) as u32,
            write_wait: WRITE_WAIT,
            in_flight: Default::default(),
            downloads: Default::default(),
            held_types: Default::default(),
        }
    }

    /// Works on at most `total` requests at once, `per_caller` of them one caller's, and builds
    /// at most `downloads` `file.csv` answers at once (T-4782).
    pub fn limit_in_flight(mut self, total: usize, per_caller: usize, downloads: usize) -> Self {
        self.in_flight = in_flight::InFlight::new(total, per_caller, downloads);
        self
    }

    /// Holds at most `mib` MiB of write bodies at once (T-4077).
    pub fn budget_writes(mut self, mib: usize) -> Self {
        let kib = u32::try_from(mib.saturating_mul(1024))
            .unwrap_or(u32::MAX)
            .max(1);
        self.writes = Arc::new(tokio::sync::Semaphore::new(kib as usize));
        self.write_budget_kib = kib;
        self
    }

    /// Reads each space's personal-data key from `dir/{space}` (DM-121).
    pub fn personal_keys_in(mut self, dir: std::path::PathBuf) -> Self {
        self.personal_keys = Some(dir);
        self
    }

    /// How long a write waits for its share before it is answered 503 (T-4123).
    pub fn wait_for_writes(mut self, wait: std::time::Duration) -> Self {
        self.write_wait = wait;
        self
    }

    /// A share of the write budget for a body of `length` bytes, or `None` when the gateway held
    /// all it may for the whole of its wait (T-4077, T-4123). The semaphore is fair: a page that
    /// waits for most of the budget holds back the small writes that come after it, so a steady
    /// stream of small writes never starves it. A body of unknown length counts as the largest
    /// one the organization accepts; one larger than the whole budget takes the whole budget, so
    /// it waits for an idle gateway instead of never fitting.
    pub(crate) async fn write_share(
        &self,
        length: Option<usize>,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let bytes = length.unwrap_or_else(|| self.max_request_body());
        let kib = u32::try_from(bytes.div_ceil(1024))
            .unwrap_or(u32::MAX)
            .clamp(1, self.write_budget_kib);
        let waiting = self.writes.clone().acquire_many_owned(kib);
        tokio::time::timeout(self.write_wait, waiting)
            .await
            .ok()?
            .ok()
    }

    /// Exchanges a caller's token with `exchanger` for a read over a space whose
    /// registrations ask for `identity: caller` (T-3999, ADR-N-047 §2.4).
    pub fn exchange_with(mut self, exchanger: crate::federation::exchange::Exchanger) -> Self {
        self.exchanger = Some(Arc::new(exchanger));
        self
    }

    /// Spends at most `limits` a minute per subject on the MCP hub (ADR-N-025 section 3).
    pub fn limit_hub_to(mut self, limits: RateLimits) -> Self {
        self.hub_rate_limit = limits;
        self
    }

    /// Spends at most `limits` a minute per client address without a token over every MCP
    /// surface together (T-4168).
    pub fn limit_anonymous_mcp_to(mut self, limits: RateLimits) -> Self {
        self.anonymous_mcp_rate_limit = limits;
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

    /// Serves the stateless authorization server for anonymous MCP (ADR-N-052 §2.2, EP-129).
    pub fn issue_anonymous_tokens(mut self, issuer: oauth::Issuer) -> Self {
        self.oauth = Some(Arc::new(issuer));
        self
    }

    /// Refuses writes by this gate (PF-41, Architecture/03 §3): `report` refuses nothing,
    /// `enforce` refuses a write unless the Organization's domain is verified.
    pub fn gate_writes_on(mut self, gate: Arc<crate::domain_gate::DomainGate>) -> Self {
        self.domain_gate = gate;
        self
    }

    /// Offers `search_documents` on the hub and the servers that attach knowledge sources,
    /// searched by `documents` (EP-133).
    pub fn search_documents_with(mut self, documents: crate::mcp::documents::Documents) -> Self {
        self.documents = Some(Arc::new(documents));
        self
    }

    /// Serves `/api/mcp/me/{name}` from the connectors `personal` reads (EP-123).
    pub fn read_personal_connectors(mut self, personal: crate::mcp::personal::Personal) -> Self {
        self.personal = Some(Arc::new(personal));
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

    /// The audience of the broker `endpoint`'s space lives on, the one a federated read's
    /// exchanged token names (T-3999, PF-114).
    pub(crate) fn broker_audience(&self, endpoint: &Endpoint) -> String {
        format!(
            "context-broker-{}",
            self.broker.broker_of_view(&endpoint.slug)
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

    /// Every value that names a named server as an RFC 8707 resource: its Keycloak client
    /// `mcp-{project}-{name}` and, when the deployment names its public URL, its address
    /// (ADR-N-043 §2.2).
    pub(crate) fn server_audiences(&self, server: &crate::mcp::server::McpServer) -> Vec<String> {
        let mut audiences = vec![server.client()];
        if let Some(base) = self.public_url.as_deref() {
            audiences.push(format!("{base}{}", server.path()));
        }
        audiences
    }

    /// Replaces the MQTT edge brokers (ADR-N-048): from the next CONNECT on.
    pub fn replace_brokers(&self, brokers: crate::mqtt::Brokers) {
        self.brokers.store(Arc::new(brokers));
    }

    /// The MQTT edge brokers served now.
    pub fn brokers(&self) -> Arc<crate::mqtt::Brokers> {
        self.brokers.load_full()
    }

    /// The service accounts a token's `azp` resolves through now.
    pub fn service_accounts(&self) -> Arc<ServiceAccounts> {
        self.accounts.load_full()
    }

    /// Replaces the Projects that name their publisher (EP-127): from the next request on.
    pub fn replace_publishers(&self, publishers: crate::store::Publishers) {
        self.publishers.store(Arc::new(publishers));
    }

    /// Replaces each Project's Organization and the audited Endpoints: from the next request on.
    pub fn replace_provenance(&self, provenance: crate::store::Provenance) {
        self.provenance.store(Arc::new(provenance));
    }

    /// The Organization `project` belongs to (ADR-N-054 2.3).
    pub(crate) fn organization_of(&self, project: &str) -> Option<String> {
        self.provenance.load().organizations.get(project).cloned()
    }

    /// Whether the Endpoint `slug` keeps an audit trail of its writes (DM-136).
    pub(crate) fn audited(&self, slug: &str) -> bool {
        self.provenance.load().audited.contains(slug)
    }

    /// The publisher of `project`'s catalogue, when the project names one (EP-127).
    pub fn publisher(&self, project: &str) -> Option<Arc<crate::store::ProjectPublisher>> {
        self.publishers.load().get(project).cloned()
    }

    /// Replaces the named MCP servers (ADR-N-043): from the next request on.
    pub fn replace_servers(&self, servers: crate::mcp::server::McpServers) {
        self.servers.store(Arc::new(servers));
    }

    /// Replaces the published Mappings (EP-53); a withdrawn one stops answering at once.
    pub fn replace_mappings(&self, mappings: crate::handlers::mappings::Mappings) {
        self.mappings.store(Arc::new(mappings));
    }

    /// The Mappings the Endpoint `slug` publishes, before any caller's narrowing.
    pub(crate) fn mappings(
        &self,
        slug: &str,
    ) -> Vec<Arc<crate::handlers::mappings::PublishedMapping>> {
        self.mappings.load().get(slug).cloned().unwrap_or_default()
    }

    /// The named server at `/api/mcp/{project}/{name}`, if one is served.
    pub(crate) fn server(
        &self,
        project: &str,
        name: &str,
    ) -> Option<crate::mcp::server::McpServer> {
        self.servers
            .load()
            .iter()
            .find(|server| server.project == project && server.name == name)
            .cloned()
            .or_else(|| {
                (project == jc_core::envelope::ORG_NAMESPACE && name == crate::mcp::server::ORG_ALL)
                    .then(crate::mcp::server::McpServer::org_all)
            })
    }

    /// The project of the ServiceAccount a token's `azp` names, when it acts as itself rather
    /// than for a person it exchanged a token for (PF-46, ADR-N-038).
    pub(crate) fn account_project(&self, azp: &str) -> Option<String> {
        self.accounts
            .load()
            .resolve(azp)
            .filter(|account| !account.delegates)
            .map(|account| account.project.clone())
    }

    /// Whether a verified token reaches `endpoint` through the hub (EP-88, PF-45, PF-46): its
    /// audience names the Endpoint itself (its slug, its URL or the edge), or it names the hub
    /// and an `endpoint:{slug}` scope picks this Endpoint, or `endpoint:*` picks every one the
    /// person consented to hand over (ADR-N-025 4b). The Endpoint's Policy still decides.
    pub(crate) fn reaches(&self, claims: &Claims, endpoint: &Endpoint) -> bool {
        claims.names_audience(&self.audiences_for(endpoint))
            || (claims.names_audience(&self.hub_audiences())
                && claims
                    .endpoint_scopes()
                    .any(|slug| slug == "*" || slug == endpoint.slug))
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

    /// The `Authorization` a hop over `endpoint`'s space carries for its `identity: caller`
    /// registrations (PF-48, PF-113, T-3999): none when it has none or the caller is anonymous,
    /// else the caller's token exchanged for the brokers those members live on. A member outside
    /// this platform never receives one (`501`); an exchange that fails is `502` or `503`, and
    /// nothing is forwarded in its place.
    pub(crate) async fn federation_token(
        &self,
        endpoint: &Endpoint,
        presented: Option<&HeaderValue>,
    ) -> Result<Option<HeaderValue>, Box<Response<Body>>> {
        let federation = self.federation.load();
        let members: Vec<&crate::federation::Member> = federation
            .caller_members(&endpoint.project, &endpoint.space)
            .collect();
        if members.is_empty() {
            return Ok(None);
        }
        let outside: Vec<&str> = members
            .iter()
            .filter(|member| member.view.is_none())
            .map(|member| member.name.as_str())
            .collect();
        if !outside.is_empty() {
            tracing::warn!(
                slug = %endpoint.slug,
                registrations = %outside.join(", "),
                "caller identity on a source outside this platform or not served here"
            );
            return Err(Box::new(
                ProblemDetails::new(501, "federation-identity-unavailable", "Not Implemented")
                    .with_detail(
                        "a source registered on this space asks for the caller's own token, but \
                         it is an address outside this platform or an Endpoint this gateway does \
                         not serve, and the token never leaves the platform; register it with \
                         spec.federation.identity: serviceAccount",
                    )
                    .into_response(),
            ));
        }
        // The anonymous caller is the member's anonymous caller too: nothing to exchange.
        let Some(subject) = presented
            .and_then(|value| value.to_str().ok())
            .and_then(|value| token::bearer(Some(value)).ok())
        else {
            return Ok(None);
        };
        let brokers: std::collections::BTreeSet<String> = members
            .iter()
            .filter_map(|member| member.view.as_deref())
            .map(|view| self.broker.broker_of_view(view))
            .collect();
        let Some(exchanger) = &self.exchanger else {
            tracing::warn!(slug = %endpoint.slug, "a federated read needs a token exchange and none is configured");
            return Err(Box::new(
                crate::federation::exchange::ExchangeError::Unavailable(String::new())
                    .problem()
                    .into_response(),
            ));
        };
        match exchanger.exchange(subject, &brokers).await {
            Ok(exchanged) => HeaderValue::from_str(&format!("Bearer {exchanged}"))
                .map(|mut value| {
                    value.set_sensitive(true);
                    Some(value)
                })
                .map_err(|_| Box::new(ProblemDetails::internal().into_response())),
            Err(error) => {
                tracing::warn!(slug = %endpoint.slug, %error, "token exchange for a federated read failed");
                Err(Box::new(error.problem().into_response()))
            }
        }
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
        self.dcat_profiles
            .store(limits.dcat_profiles.bits(), Ordering::Relaxed);
    }

    /// The DCAT profiles the Organization serves its records in now (T-4499).
    pub fn dcat_profiles(&self) -> jc_dcat::profile::Offered {
        jc_dcat::profile::Offered::from_bits(self.dcat_profiles.load(Ordering::Relaxed))
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
        .merge(catalog::routes())
        .merge(ngsi_ld::routes())
        .merge(view_access::routes())
        .merge(mcp::routes())
        .merge(files::routes())
        .merge(ogc::routes())
        .merge(ws::routes())
        .merge(space_routes::routes())
        .merge(schema_routes::routes())
        .merge(openapi_doc::routes())
        .merge(mappings::routes())
        .merge(oauth::routes())
        // Every endpoint surface passes the limiter; the two probes do not, so a full
        // bucket can never make a pod look unhealthy (EP-20).
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&gateway),
            rate_limit::enforce,
        ))
        // Outside the limiter, so a request the full gateway refuses spends no quota; inside
        // `admit`, so a freshly minted anonymous token is no fresh caller (T-4782).
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&gateway),
            in_flight::enforce,
        ))
        // Outside the limiter: the gateway's own anonymous token is taken off before the
        // limiter picks a bucket, so minting tokens never buys one (ADR-N-052 §2.2.5).
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&gateway),
            oauth::admit,
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
pub(crate) async fn missing() -> Response<Body> {
    ProblemDetails::not_found().into_response()
}
