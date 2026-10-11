//! The tenant is a conclusion, never a request header (T-0145, GW20, EP-21, EP-22, SP-05).
//!
//! A client can put anything in a header. Everything a client says about which tenant it
//! is in, who it is, or what it may see is therefore removed before the request is looked
//! at, and the values the broker sees are put there by the gateway from the endpoint it
//! resolved and the token it verified.
//!
//! The stripping is unconditional and happens first, before routing, authentication or
//! policy evaluation, so no later stage can accidentally read a client-supplied value.

use crate::pdp::evaluator::Subject;
use axum::extract::Request;
use axum::http::header::{InvalidHeaderValue, AUTHORIZATION, COOKIE};
use axum::http::{HeaderMap, HeaderName, HeaderValue};

/// The header that pins the broker's tenant. Only the gateway ever sets it (GW20).
pub const TENANT: HeaderName = HeaderName::from_static("ngsild-tenant");

/// Headers a client must never be able to set, because a downstream component would
/// believe them (Deployment/10 section 4, GW25).
pub const FORGEABLE: &[&str] = &[
    "ngsild-tenant",
    "x-userinfo",
    "x-access-token",
    "x-allowed-scope-ids",
    "x-endpoint-slug",
    "x-consumer-identity",
    "x-jc-subject",
    "x-jc-groups",
    "x-jc-roles",
    "x-jc-client",
    "x-jc-restricted",
];

/// The user the gateway verified, as the broker's `joinedcontext` engine reads it (T-3641).
pub const SUBJECT: HeaderName = HeaderName::from_static("x-jc-subject");
/// The groups of the verified caller, each percent-encoded, comma separated.
pub const GROUPS: HeaderName = HeaderName::from_static("x-jc-groups");
/// The roles of the verified caller, each percent-encoded, comma separated; `public` for an anonymous one.
pub const ROLES: HeaderName = HeaderName::from_static("x-jc-roles");
/// The service account of the verified caller.
pub const CLIENT: HeaderName = HeaderName::from_static("x-jc-client");
/// `true` when the caller asked to be told about narrowing (O-3 (b), R22).
pub const RESTRICTED: HeaderName = HeaderName::from_static("x-jc-restricted");

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
/// broker whole.
pub fn pin_caller(
    request: &mut Request,
    space: &str,
    subject: &Subject,
) -> Result<(), InvalidHeaderValue> {
    pin_tenant(request, space)?;
    let asked = super::response::asked_about_narrowing(request);
    let headers = request.headers_mut();
    for name in [
        AUTHORIZATION,
        COOKIE,
        SUBJECT,
        GROUPS,
        ROLES,
        CLIENT,
        RESTRICTED,
    ] {
        while headers.remove(&name).is_some() {}
    }
    one(headers, SUBJECT, subject.user.as_deref());
    // Only the gateway's own hops speak as the gateway, never a token of its client.
    let client = subject.service_account.as_deref();
    one(headers, CLIENT, client.filter(|c| *c != GATEWAY_CLIENT));
    list(headers, GROUPS, &subject.groups, false);
    list(headers, ROLES, &subject.roles, true);
    if asked {
        headers.insert(RESTRICTED, HeaderValue::from_static("true"));
    }
    Ok(())
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
    for name in [
        AUTHORIZATION,
        COOKIE,
        SUBJECT,
        GROUPS,
        ROLES,
        CLIENT,
        RESTRICTED,
    ] {
        while headers.remove(&name).is_some() {}
    }
    headers.insert(CLIENT, HeaderValue::from_static(GATEWAY_CLIENT));
    headers.insert(ROLES, HeaderValue::from_static(""));
}
