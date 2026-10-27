//! The tenant is a conclusion, never a request header (T-0145, GW20, EP-21, EP-22, SP-05).
//!
//! A client can put anything in a header. Everything a client says about which tenant it
//! is in, who it is, or what it may see is therefore removed before the request is looked
//! at, and the values the broker sees are put there by the gateway from the endpoint it
//! resolved and the token it verified.
//!
//! The stripping is unconditional and happens first, before routing, authentication or
//! policy evaluation, so no later stage can accidentally read a client-supplied value.

use crate::auth::subject::Subject;
use axum::extract::Request;
use axum::http::header::{InvalidHeaderValue, AUTHORIZATION, COOKIE};
use axum::http::{HeaderMap, HeaderName, HeaderValue};

/// The header that pins the broker's tenant. Only the gateway ever sets it (GW20).
pub const TENANT: HeaderName = HeaderName::from_static("ngsild-tenant");

/// Headers a client must never be able to set, because a downstream component would
/// believe them (Deployment/10 section 4, GW25); every name under [`IDENTITY_PREFIX`] is
/// removed besides these.
pub const FORGEABLE: &[&str] = &[
    "ngsild-tenant",
    "x-userinfo",
    "x-access-token",
    "x-allowed-scope-ids",
    "x-endpoint-slug",
    "x-consumer-identity",
];

/// The prefix of every identity header the broker's `joinedcontext` engine believes (T-3641):
/// only the gateway sets one, so a client's is removed whatever its name, including one no
/// code reads yet (T-3816).
pub const IDENTITY_PREFIX: &str = "x-jc-";

/// The prefix of the provenance headers the broker stores with a write (ADR-N-054 2.3, DM-112):
/// only the gateway sets one, so a client's is removed whatever its name.
pub const PROVENANCE_PREFIX: &str = "jc-provenance-";
/// The prefix of the Linkerd mesh headers (T-5869, F-SEC1-028): the proxy in front of the broker
/// sets `l5d-client-id` from the caller's mTLS certificate and the broker believes it (Antares
/// `provenance.rs`), so a client's copy never crosses the gateway, whatever its name.
pub const MESH_PREFIX: &str = "l5d-";
/// The path a write came through: `pipeline`, `ui` or `api`.
pub const PROVENANCE_CHANNEL: HeaderName = HeaderName::from_static("jc-provenance-channel");
/// Who wrote, as the audit log names them (`user:…`, `serviceAccount:…`, a DID), percent-encoded.
pub const PROVENANCE_ACTOR: HeaderName = HeaderName::from_static("jc-provenance-actor");
/// The Organization of the Project the caller was admitted under (ADR-N-054 2.3), percent-encoded.
pub const PROVENANCE_ORGANIZATION: HeaderName =
    HeaderName::from_static("jc-provenance-organization");
/// What kind of principal wrote: `person`, `serviceAccount`, `pipeline` or `agent` (ADR-N-054 2.3).
pub const PROVENANCE_ACTOR_KIND: HeaderName = HeaderName::from_static("jc-provenance-actor-kind");
/// The Keycloak client the verified token was issued to (`azp`), percent-encoded: the MCP client,
/// CLI or App a person's write came through (ADR-N-054 2.3, DM-111). Provenance only: the
/// broker decides on `x-jc-client`, which names a workload alone.
pub const PROVENANCE_CLIENT: HeaderName = HeaderName::from_static("jc-provenance-client");
/// `changes` on a write through an Endpoint with `spec.audit: changes` (ADR-N-054 2.8, DM-136):
/// only the gateway may set it, so it shares the provenance prefix the edge and `drop_identity`
/// remove.
pub const AUDIT: HeaderName = HeaderName::from_static("jc-provenance-audit");
/// The grant of `versions=all` (ADR-N-054 2.4): only the gateway may set it.
pub const READ_VERSIONS: &str = "jc-read-versions";
/// Why a write was made, in the caller's words (DM-113): the one provenance header a caller sets.
pub const REASON: HeaderName = HeaderName::from_static("jc-reason");
/// The id of the request, which the broker stores in each row's provenance (ADR-N-054 2.3).
pub const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
/// The longest request id the broker stores (Antares `provenance.rs`).
const MAX_REQUEST_ID: usize = 128;
/// The Keycloak client of the Portal's UI: a person's write through it is channel `ui`.
pub const PORTAL_UI_CLIENT: &str = "portal-ui";

/// Removes every header under [`IDENTITY_PREFIX`], [`PROVENANCE_PREFIX`] and [`MESH_PREFIX`], and
/// [`READ_VERSIONS`], each with all its values.
fn drop_identity(headers: &mut HeaderMap) {
    let names: Vec<HeaderName> = headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            name.starts_with(IDENTITY_PREFIX)
                || name.starts_with(PROVENANCE_PREFIX)
                || name.starts_with(MESH_PREFIX)
                || name == READ_VERSIONS
        })
        .cloned()
        .collect();
    for name in names {
        while headers.remove(&name).is_some() {}
    }
}

/// The user the gateway verified, as the broker's `joinedcontext` engine matches a `user`
/// assignee against it: a person's Keycloak id (PF-126), a client's own service-account user
/// by name (T-3641).
pub const SUBJECT: HeaderName = HeaderName::from_static("x-jc-subject");
/// The groups of the verified caller, each percent-encoded, comma separated.
pub const GROUPS: HeaderName = HeaderName::from_static("x-jc-groups");
/// The roles of the verified caller, each percent-encoded, comma separated; `public` for an anonymous one.
pub const ROLES: HeaderName = HeaderName::from_static("x-jc-roles");
/// The service account of the verified caller.
pub const CLIENT: HeaderName = HeaderName::from_static("x-jc-client");
/// `true` when the caller asked to be told about narrowing (O-3 (b), R22).
pub const RESTRICTED: HeaderName = HeaderName::from_static("x-jc-restricted");
/// The data-space participant a transfer token names (DS-02, map K38, T-3800).
pub const DID: HeaderName = HeaderName::from_static("x-jc-did");

/// The service account the gateway's own hops to the broker run as (T-3767): its Keycloak
/// client. The rule document the Portal writes grants it the reads and the unlinking those hops
/// make; a caller never carries it (see [`pin_caller`]).
pub const GATEWAY_CLIENT: &str = "context-gateway";

/// Removes every header a client could use to forge identity or tenancy (EP-21, GW20).
///
/// Removes each name entirely, not just its first value: a repeated header would otherwise
/// leave a copy behind for whoever reads the last one.
pub fn strip_client_headers(request: &mut Request) {
    let headers = request.headers_mut();
    for name in FORGEABLE {
        while headers.remove(*name).is_some() {}
    }
    drop_identity(headers);
}

/// Pins the tenant for the internal hop to the broker (EP-22, GW20).
///
/// Called after the slug resolved, with the space the endpoint names, never with anything
/// derived from the request. A space name is a DNS-1123 label, so it is always a legal
/// header value; a name that is not is a reconciler bug and pins nothing rather than
/// pinning something wrong.
pub fn pin_tenant(request: &mut Request, space: &str) -> Result<(), InvalidHeaderValue> {
    let value = HeaderValue::from_str(space)?;
    request.headers_mut().insert(TENANT, value);
    Ok(())
}

/// Pins the tenant and the verified caller for the hop to the broker, and drops the caller's
/// own credentials from it (T-3641, map G15, K36).
///
/// The broker is the audience of neither the caller's token nor their cookie. It decides on the
/// `x-jc-*` headers instead, and reads a request carrying none of them as the cluster's own, so
/// `x-jc-roles` is always set, to `public` at least for an anonymous caller. Every value and
/// list member is percent-encoded (T-3770), so a name with a diacritic or a comma reaches the
/// broker whole. A caller under a data-space agreement is its participant's DID alone
/// (`x-jc-did`, DS-02): no role, no account, no opt-in, so a broker that does not read the
/// header yet takes the hop as the cluster's own and refuses it, never as the public's.
pub fn pin_caller(
    request: &mut Request,
    space: &str,
    subject: &Subject,
) -> Result<(), InvalidHeaderValue> {
    pin_tenant(request, space)?;
    let asked = super::response::asked_about_narrowing(request);
    let headers = request.headers_mut();
    for name in [AUTHORIZATION, COOKIE] {
        while headers.remove(&name).is_some() {}
    }
    drop_identity(headers);
    let (channel, actor) = provenance(subject);
    headers.insert(PROVENANCE_CHANNEL, HeaderValue::from_static(channel));
    one(headers, PROVENANCE_ACTOR, Some(&actor));
    one(
        headers,
        PROVENANCE_ORGANIZATION,
        subject.organization.as_deref(),
    );
    if let Some(kind) = actor_kind(subject) {
        headers.insert(PROVENANCE_ACTOR_KIND, HeaderValue::from_static(kind));
    }
    request_id(headers);
    if subject.agreement.is_some() {
        one(headers, DID, subject.did.as_deref());
        return Ok(());
    }
    one(headers, SUBJECT, subject.user_principal());
    let named = subject.client.as_deref();
    one(
        headers,
        PROVENANCE_CLIENT,
        named.filter(|c| *c != GATEWAY_CLIENT),
    );
    // Only the gateway's own hops speak as the gateway, never a token of its client. A project's
    // account is `{project}/{name}`, the form a Policy assignee names it by (PL-92): the broker
    // compares the header with the assignee as written, and a bare name would let two projects'
    // same-named accounts share each other's grants (F-SEC1-003, T-5860).
    let client = subject
        .service_account
        .as_deref()
        .filter(|c| *c != GATEWAY_CLIENT)
        .map(|name| match subject.service_account_project.as_deref() {
            Some(project) => format!("{project}/{name}"),
            None => name.to_owned(),
        });
    one(headers, CLIENT, client.as_deref());
    list(headers, GROUPS, &subject.groups, false);
    list(headers, ROLES, &subject.roles, true);
    if asked {
        headers.insert(RESTRICTED, HeaderValue::from_static("true"));
    }
    Ok(())
}

/// The channel and the actor of a write by `subject` (ADR-N-054 2.3, DM-111): a Pipeline's
/// derived account is `pipeline`, a person signed in to the Portal's UI is `ui`, everyone else
/// `api`; the actor is the verified principal, never anything the request says.
pub fn provenance(subject: &Subject) -> (&'static str, String) {
    let channel = match (&subject.user, &subject.service_account) {
        (None, Some(account)) if jc_core::kinds::pipeline_identity::is_derived(account) => {
            "pipeline"
        }
        (Some(_), _) if subject.client.as_deref() == Some(PORTAL_UI_CLIENT) => "ui",
        _ => "api",
    };
    (channel, crate::auth::subject::principal_of(subject))
}

/// Keeps the edge's `X-Request-Id` when the broker would store it, else sets a fresh one, so each
/// written row names the request that wrote it; a correlation id, never a grant.
fn request_id(headers: &mut HeaderMap) {
    let mut ids = headers.get_all(REQUEST_ID).iter();
    let kept = match (ids.next(), ids.next()) {
        (Some(id), None) => {
            let id = id.as_bytes();
            (1..=MAX_REQUEST_ID).contains(&id.len())
                && id
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        }
        _ => false,
    };
    if kept {
        return;
    }
    while headers.remove(REQUEST_ID).is_some() {}
    if let Some(id) = crate::creates::uuid_v4()
        .ok()
        .and_then(|id| HeaderValue::from_str(&id).ok())
    {
        headers.insert(REQUEST_ID, id);
    }
}

/// The kind of principal `subject` is (ADR-N-054 2.3): a Pipeline's derived account is
/// `pipeline`, another account `serviceAccount`, a person's token another client exchanged (an
/// assistant) `agent`, a person `person`. An anonymous caller and a data-space participant name
/// none; an MCP call is made `agent` by [`as_agent`].
pub fn actor_kind(subject: &Subject) -> Option<&'static str> {
    match (&subject.user, &subject.service_account, &subject.via) {
        (Some(_), _, Some(_)) => Some("agent"),
        (Some(_), _, None) => Some("person"),
        (None, Some(account), _) if jc_core::kinds::pipeline_identity::is_derived(account) => {
            Some("pipeline")
        }
        (None, Some(_), _) => Some("serviceAccount"),
        (None, None, _) => None,
    }
}

/// Marks a hop [`pin_caller`] pinned as an AI agent's: an MCP tool call acts for the person or
/// client its token names, who stays the actor (ADR-N-054 2.3).
pub fn as_agent(headers: &mut HeaderMap) {
    if headers.contains_key(PROVENANCE_ACTOR_KIND) {
        headers.insert(PROVENANCE_ACTOR_KIND, HeaderValue::from_static("agent"));
    }
}

fn one(headers: &mut HeaderMap, name: HeaderName, value: Option<&str>) {
    if let Some(value) = value.and_then(|v| HeaderValue::from_str(&encode(v)).ok()) {
        headers.insert(name, value);
    }
}

/// One name as the broker reads it (Antares `addons/joinedcontext-policy/README.md` section 1):
/// percent-encoded UTF-8, the RFC 3986 unreserved characters kept, so a diacritic travels and a
/// comma never splits a list member.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn list<'a>(
    headers: &mut HeaderMap,
    name: HeaderName,
    members: impl IntoIterator<Item = &'a String>,
    always: bool,
) {
    let joined = members
        .into_iter()
        .filter(|member| !member.is_empty())
        .map(|member| encode(member))
        .collect::<Vec<_>>()
        .join(",");
    // Encoded, every member is visible ASCII, which a header always carries.
    if let Ok(value) = HeaderValue::from_str(&joined) {
        if always || !joined.is_empty() {
            headers.insert(name, value);
        }
    }
}

/// Marks a hop the gateway makes on its own account (T-3767): the condition probe of a write,
/// the unlinking of a held delete, the read of a stored Subscription. It speaks as
/// [`GATEWAY_CLIENT`] with no role, so the engine decides it by the gateway's own grant and a
/// hop that forgot this is refused, never served as the cluster's own.
pub fn as_gateway(headers: &mut HeaderMap) {
    for name in [AUTHORIZATION, COOKIE] {
        while headers.remove(&name).is_some() {}
    }
    drop_identity(headers);
    headers.insert(CLIENT, HeaderValue::from_static(GATEWAY_CLIENT));
    headers.insert(ROLES, HeaderValue::from_static(""));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clients_mesh_headers_never_reach_the_broker() {
        let mut request = Request::builder()
            .uri("/ngsi-ld/v1/entities")
            .header(
                "l5d-client-id",
                "context-gateway.jc.serviceaccount.identity.linkerd.cluster.local",
            )
            .header("l5d-dst-override", "broker.other:8080")
            .header("L5D-Remote-Ip", "10.0.0.1")
            .header("jc-reason", "kept")
            .body(axum::body::Body::empty())
            .expect("request");
        strip_client_headers(&mut request);
        let left: Vec<&str> = request.headers().keys().map(HeaderName::as_str).collect();
        assert_eq!(left, ["jc-reason"]);
    }

    #[test]
    fn the_channel_and_actor_come_from_the_verified_subject() {
        let account = |name: &str| Subject {
            service_account: Some(name.to_owned()),
            ..Subject::default()
        };
        assert_eq!(
            provenance(&account("pl-parking")),
            ("pipeline", "serviceAccount:pl-parking".to_owned())
        );
        assert_eq!(provenance(&account("reporting")).0, "api");
        let person = |client: Option<&str>| Subject {
            user: Some("jana".to_owned()),
            client: client.map(str::to_owned),
            ..Subject::default()
        };
        assert_eq!(
            provenance(&person(Some(PORTAL_UI_CLIENT))),
            ("ui", "user:jana".to_owned())
        );
        assert_eq!(provenance(&person(Some("my-app"))).0, "api");
        assert_eq!(provenance(&person(None)).0, "api");
        // a person is never `pipeline`, whatever account their call names
        let mut odd = person(None);
        odd.service_account = Some("pl-x".to_owned());
        assert_eq!(provenance(&odd).0, "api");
        assert_eq!(
            provenance(&Subject::default()),
            ("api", "anonymous".to_owned())
        );
    }

    #[test]
    fn every_hop_names_one_well_formed_request_id() {
        let id = |sent: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in sent {
                if let Ok(value) = HeaderValue::from_str(value) {
                    headers.append(REQUEST_ID, value);
                }
            }
            request_id(&mut headers);
            headers
                .get_all(REQUEST_ID)
                .iter()
                .map(|v| v.to_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(id(&["4f1c-9a07"]), ["4f1c-9a07"]);
        let long = "a".repeat(MAX_REQUEST_ID + 1);
        for sent in [
            &[][..],
            &["a b"],
            &["x;y"],
            &["one", "two"],
            &[long.as_str()],
        ] {
            let fresh = id(sent);
            assert_eq!(fresh.len(), 1, "{sent:?} -> {fresh:?}");
            assert_eq!(fresh.first().map(String::len), Some(36), "{fresh:?}");
        }
    }

    #[test]
    fn the_actor_kind_comes_from_the_verified_subject() {
        let account = |name: &str| Subject {
            service_account: Some(name.to_owned()),
            ..Subject::default()
        };
        let person = Subject {
            user: Some("jana".to_owned()),
            ..Subject::default()
        };
        let assistant = Subject {
            via: Some("assistant".to_owned()),
            ..person.clone()
        };
        assert_eq!(actor_kind(&account("pl-parking")), Some("pipeline"));
        assert_eq!(actor_kind(&account("reporting")), Some("serviceAccount"));
        assert_eq!(actor_kind(&person), Some("person"));
        assert_eq!(actor_kind(&assistant), Some("agent"));
        assert_eq!(actor_kind(&Subject::anonymous()), None);
        let participant = Subject {
            did: Some("did:web:example.org".to_owned()),
            ..Subject::default()
        };
        assert_eq!(actor_kind(&participant), None);
    }

    #[test]
    fn a_persons_write_names_the_client_it_came_through() {
        let mut request = Request::new(axum::body::Body::empty());
        request
            .headers_mut()
            .insert(PROVENANCE_CLIENT, HeaderValue::from_static("forged"));
        let person = Subject {
            user: Some("jana".to_owned()),
            client: Some("claude desktop".to_owned()),
            ..Subject::default()
        };
        pin_caller(&mut request, "air", &person).expect("a usable space");
        let named = request.headers().get_all(PROVENANCE_CLIENT);
        assert_eq!(named.iter().collect::<Vec<_>>(), ["claude%20desktop"]);
        // The client is provenance only: the broker still decides on the person alone.
        assert!(!request.headers().contains_key(CLIENT));
        // The gateway's own client and a participant under an agreement name no client.
        for caller in [
            Subject {
                client: Some(GATEWAY_CLIENT.to_owned()),
                ..Subject::default()
            },
            Subject {
                did: Some("did:web:example.org".to_owned()),
                agreement: Some("a1".to_owned()),
                client: Some("connector".to_owned()),
                ..Subject::default()
            },
        ] {
            let mut hop = Request::new(axum::body::Body::empty());
            pin_caller(&mut hop, "air", &caller).expect("a usable space");
            assert!(!hop.headers().contains_key(PROVENANCE_CLIENT));
        }
    }

    #[test]
    fn a_hop_names_the_organization_and_kind_and_an_mcp_call_is_an_agents() {
        let mut request = Request::new(axum::body::Body::empty());
        request
            .headers_mut()
            .insert(PROVENANCE_ORGANIZATION, HeaderValue::from_static("forged"));
        let person = Subject {
            user: Some("jana".to_owned()),
            organization: Some("Banská Bystrica".to_owned()),
            ..Subject::default()
        };
        pin_caller(&mut request, "air", &person).expect("a usable space");
        let headers = request.headers_mut();
        assert_eq!(
            headers
                .get_all(PROVENANCE_ORGANIZATION)
                .iter()
                .collect::<Vec<_>>(),
            ["Bansk%C3%A1%20Bystrica"]
        );
        assert_eq!(headers[PROVENANCE_ACTOR_KIND], "person");
        as_agent(headers);
        assert_eq!(headers[PROVENANCE_ACTOR_KIND], "agent");
        // An anonymous hop names no kind, and an MCP call does not invent one.
        let mut anonymous = Request::new(axum::body::Body::empty());
        pin_caller(&mut anonymous, "air", &Subject::anonymous()).expect("a usable space");
        as_agent(anonymous.headers_mut());
        assert!(!anonymous.headers().contains_key(PROVENANCE_ACTOR_KIND));
        assert!(!anonymous.headers().contains_key(PROVENANCE_ORGANIZATION));
    }

    #[test]
    fn a_clients_provenance_headers_never_survive() {
        let mut headers = HeaderMap::new();
        for name in [
            "jc-provenance-actor",
            "jc-provenance-organization",
            "jc-provenance-actor-kind",
            "jc-provenance-audit",
            "jc-provenance-x",
            "jc-read-versions",
        ] {
            headers.append(name, HeaderValue::from_static("forged"));
            headers.append(name, HeaderValue::from_static("twice"));
        }
        headers.insert(REASON, HeaderValue::from_static("kept"));
        drop_identity(&mut headers);
        assert_eq!(headers.len(), 1, "{headers:?}");
    }
}
