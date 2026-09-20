//! The tenant is a conclusion, never a request header (T-0145, GW20, EP-21, EP-22, SP-05).
//!
//! A client can put anything in a header. Everything a client says about which tenant it
//! is in, who it is, or what it may see is therefore removed before the request is looked
//! at, and the values the broker sees are put there by the gateway from the endpoint it
//! resolved and the token it verified.
//!
//! The stripping is unconditional and happens first, before routing, authentication or
//! policy evaluation, so no later stage can accidentally read a client-supplied value.

use axum::extract::Request;
use axum::http::header::InvalidHeaderValue;
use axum::http::{HeaderName, HeaderValue};

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
];

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
