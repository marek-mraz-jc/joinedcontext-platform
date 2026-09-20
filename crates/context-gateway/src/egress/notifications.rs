//! Notifications leave through the gateway, not around it (T-0156, R46, GW27).
//!
//! A subscription is a standing query whose answers arrive later and elsewhere. The broker
//! holds no policy, so a notification delivered straight from the broker to a webhook would
//! be the one answer on the platform nobody projected.
//!
//! Two halves close it. At creation the subscription is narrowed to the caller's grants and
//! its `notification.endpoint.uri` is rewritten to point back at this gateway, carrying the
//! caller's own endpoint in `to`. At delivery the gateway reads the stored subscription,
//! projects the entities to what that subscription was narrowed to, re-checks its `q`
//! against the stored state, and forwards what is left to `to`.
//!
//! The stored subscription is the authority for both the projection and the target. That is
//! what makes the delivery path safe to leave unauthenticated, which it has to be: the
//! broker calling it holds no token of its own. The granted areas travel in that same
//! rewritten URI, because `geoQ` holds one geometry and a caller may hold several grants
//! (T-0428, GW11).

use crate::app::Gateway;
use crate::pdp::evaluator::{conjoin, narrow, Constraints};
use crate::pdp::{geo, projection};
use crate::proxy::Broker;
use crate::query;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::ProblemDetails;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;

/// The path under an endpoint that a rewritten notification endpoint points at.
pub const EGRESS_PATH: &str = "/egress/notifications";

/// The parameter carrying the endpoint the subscriber actually asked for.
const TARGET: &str = "to";

/// The parameter carrying one granted area, repeated once per grant (GW11).
const AREA: &str = "area";

/// The tenant header the gateway pins on the internal hop (GW20).
const TENANT: &str = "NGSILD-Tenant";

/// The largest notification the gateway will project. A notification carries a page of
/// entities, so it is bounded the same way a read is.
const MAX_NOTIFICATION: usize = 8 * 1024 * 1024;

/// Narrows a subscription to the caller's grants and routes its delivery through the
/// gateway (GW27, R46).
///
/// `is_create` separates the two shapes this is called with. A `POST` carries the whole
/// subscription, so what it leaves out has to be filled in from the grants. A `PATCH`
/// carries a fragment, and what it leaves out was already narrowed when it was stored:
/// filling that in from the grants would replace the subscriber's own selection with the
/// widest one the grants allow.
pub fn narrow_subscription(
    subscription: &mut Value,
    constraints: &Constraints,
    endpoint: &Endpoint,
    base_url: &str,
    private_hosts: &[String],
    is_create: bool,
) -> Result<(), Box<ProblemDetails>> {
    let Some(members) = subscription.as_object_mut() else {
        return Err(Box::new(
            ProblemDetails::bad_request().with_detail("the subscription is not an object"),
        ));
    };

    // A scope grant needs nothing here: `policy_filter` folds each policy's scopes into that
    // policy's own `q` term before the constraints are built (R13), so they arrive with the
    // `q` that `narrow_filter` conjoins below and no `scopeQ` is ever stored.
    narrow_selectors(members, constraints, is_create)?;
    narrow_filter(members, constraints, is_create);
    narrow_names(
        members,
        "watchedAttributes",
        constraints,
        &endpoint.hidden_attributes,
    )?;

    let Some(notification) = members
        .get_mut("notification")
        .and_then(Value::as_object_mut)
    else {
        return match is_create {
            true => Err(Box::new(
                ProblemDetails::bad_request()
                    .with_detail("a subscription needs a notification endpoint"),
            )),
            false => Ok(()),
        };
    };
    narrow_names(
        notification,
        "attributes",
        constraints,
        &endpoint.hidden_attributes,
    )?;
    if !constraints.attrs.is_empty() && !notification.contains_key("attributes") {
        // The grant is a whitelist, so the notification carries that whitelist rather than
        // whatever the entity happens to hold when it fires (R9).
        notification.insert(
            "attributes".to_owned(),
            Value::Array(names(&constraints.attrs)),
        );
    }

    let Some(endpoint_of) = notification
        .get_mut("endpoint")
        .and_then(Value::as_object_mut)
    else {
        return match is_create {
            true => Err(Box::new(
                ProblemDetails::bad_request()
                    .with_detail("a subscription needs a notification endpoint"),
            )),
            false => Ok(()),
        };
    };
    let Some(uri) = endpoint_of.get("uri").and_then(Value::as_str) else {
        return match is_create {
            true => Err(Box::new(
                ProblemDetails::bad_request().with_detail("the notification endpoint names no uri"),
            )),
            false => Ok(()),
        };
    };
    if let Some(problem) = refused_at_creation(uri, private_hosts) {
        return Err(Box::new(problem));
    }
    let routed = route(
        uri,
        base_url,
        &endpoint.base_path,
        &granted_areas(constraints),
    )?;
    endpoint_of.insert("uri".to_owned(), Value::String(routed));
    Ok(())
}

/// The delivery path: the broker's notification, projected and forwarded (R46).
pub async fn deliver(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    request: Request,
) -> Response<Body> {
    let Some(endpoint) = gateway.resolver.resolve(&slug) else {
        return ProblemDetails::not_found().into_response();
    };

    let (parts, body) = request.into_parts();
    let Ok(sent) = axum::body::to_bytes(body, MAX_NOTIFICATION).await else {
        return ProblemDetails::bad_request()
            .with_detail("the notification is unreadable or larger than the gateway accepts")
            .into_response();
    };
    let Ok(mut notification) = serde_json::from_slice::<Value>(&sent) else {
        return ProblemDetails::bad_request()
            .with_detail("the notification is not JSON")
            .into_response();
    };
    let Some(subscription_id) = notification
        .get("subscriptionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return ProblemDetails::bad_request()
            .with_detail("a notification names the subscription it belongs to")
            .into_response();
    };

    // The stored subscription decides what may be sent and where. A forged delivery can
    // therefore neither widen the projection nor choose the target.
    let stored = match read_subscription(&gateway.broker, &endpoint.space, &subscription_id).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return ProblemDetails::not_found().into_response(),
        Err(problem) => return problem.into_response(),
    };
    let Some((target, areas)) = delivery_of(&stored) else {
        tracing::warn!(
            subscription = %subscription_id,
            "a delivery names a subscription whose notification endpoint does not route \
             through this gateway"
        );
        return ProblemDetails::not_found().into_response();
    };

    // `data` is a list of entities, as CIM 009 says a notification carries. Anything else is not a
    // notification this gateway delivers: the per-type filter below reads an array, so a bare
    // object used to be projected attribute by attribute and then forwarded whole — an entity of a
    // type this endpoint does not serve, leaving the platform by the shape of its wrapper (T-2284,
    // EP-26). No broker sends that; a forged delivery does.
    if notification
        .get("data")
        .is_some_and(|data| !data.is_array())
    {
        return ProblemDetails::bad_request()
            .with_detail("a notification's data is a list of entities")
            .into_response();
    }

    let judged = delivery_constraints(&stored, &endpoint);
    if let Some(data) = notification.get_mut("data") {
        // An entity of a type this endpoint does not serve is not delivered, and what is
        // delivered keeps the slots of its own type (EP-26, MP-02, T-1862).
        if let Some(entities) = data.as_array_mut() {
            entities.retain(|entity| projection::permitted(entity, &judged));
        }
        projection::project_by_type(data, &judged);
        if let Some(filter) = stored.get("q").and_then(Value::as_str) {
            if let Err(problem) =
                keep_matching(&gateway.broker, &endpoint.space, filter, data).await
            {
                return problem.into_response();
            }
        }
        // The areas the grants drew, applied the way a read applies them to an answer: an
        // entity the gateway cannot place is not delivered (GW11).
        if let Some((areas, entities)) = geo::Areas::of(&areas, None).zip(data.as_array_mut()) {
            entities.retain(|entity| areas.admits(entity));
        }
    }

    if notification
        .get("data")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        // Everything the broker matched has since stopped matching the narrowed condition,
        // or was projected away entirely. Nothing leaves the platform.
        return StatusCode::NO_CONTENT.into_response();
    }
    if refused_at_delivery(&target, &gateway.private_hosts).await {
        tracing::warn!(
            subscription = %subscription_id,
            "a notification endpoint resolves inside the platform's networks; not delivered"
        );
        return ProblemDetails::forbidden()
            .with_detail("the notification endpoint resolves inside the platform's networks")
            .into_response();
    }
    dispatch(&gateway.broker, &target, &parts.headers, &notification).await
}

/// Whether an address is inside the platform's own networks (T-1302): loopback, private
/// (RFC 1918, IPv6 unique local), link-local (the cloud metadata address), unspecified,
/// broadcast, `0.0.0.0/8` and shared address space (RFC 6598), an IPv4 one mapped into IPv6 too.
fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [first, second, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || first == 0
                || (first == 100 && (second & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_internal(IpAddr::V4(v4)))
        }
    }
}

/// The host and port of an HTTP or HTTPS URL: no user info, no brackets, lower case.
fn host_of(url: &str) -> Option<(String, u16)> {
    let (origin, _) = split(url)?;
    let (scheme, authority) = origin.split_once("://")?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']')?;
            (host, after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let port = match port {
        Some(port) => port.parse().ok()?,
        None if scheme == "https" => 443,
        None => 80,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some((host, port))
}

/// Why a notification endpoint may not be stored, read off the URI alone: a name for this
/// machine or an address inside the platform's networks, unless the installation named it.
/// A name is resolved at delivery, where `refused_at_delivery` checks where it points then.
fn refused_at_creation(uri: &str, private_hosts: &[String]) -> Option<ProblemDetails> {
    if !(uri.starts_with("http://") || uri.starts_with("https://")) {
        // `route` says why, with its own wording.
        return None;
    }
    let Some((host, _)) = host_of(uri) else {
        return Some(
            ProblemDetails::bad_request().with_detail("the notification endpoint names no host"),
        );
    };
    if private_hosts.contains(&host) {
        return None;
    }
    let internal = host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(is_internal);
    internal.then(|| {
        ProblemDetails::bad_request()
            .with_detail("a notification endpoint may not point inside the platform's own networks")
    })
}

/// Whether the host a delivery goes to resolves inside the platform's networks now: a public
/// name can be pointed inward after the subscription was stored (T-1302). A name that does not
/// resolve is left to the delivery, which fails on it anyway.
// ponytail: checks the resolution, then the client resolves again; a name that flips between
// the two lookups gets through. Pin the checked address into the connector if that matters.
async fn refused_at_delivery(target: &str, private_hosts: &[String]) -> bool {
    let Some((host, port)) = host_of(target) else {
        return true;
    };
    if private_hosts.contains(&host) {
        return false;
    }
    let address = match host.contains(':') {
        true => format!("[{host}]:{port}"),
        false => format!("{host}:{port}"),
    };
    match tokio::net::lookup_host(address).await {
        Ok(mut addresses) => addresses.any(|address| is_internal(address.ip())),
        Err(_) => false,
    }
}

/// Rewrites one notification endpoint to point back at this gateway, carrying the areas the
/// delivery path filters with (R46, GW11).
fn route(
    uri: &str,
    base_url: &str,
    base_path: &str,
    areas: &[String],
) -> Result<String, Box<ProblemDetails>> {
    if !(uri.starts_with("http://") || uri.starts_with("https://")) {
        return Err(Box::new(ProblemDetails::bad_request().with_detail(
            "a notification endpoint the gateway delivers to has to be an HTTP or HTTPS URL",
        )));
    }
    if uri.contains(EGRESS_PATH) {
        return Err(Box::new(ProblemDetails::bad_request().with_detail(
            "the notification endpoint already points at the gateway's egress path",
        )));
    }
    if base_url.is_empty() {
        return Err(Box::new(unsupported(
            "this gateway does not know an address to route a notification back through \
             itself; set JC_GATEWAY_EGRESS_URL or JC_GATEWAY_PUBLIC_URL",
        )));
    }
    let mut routed = format!(
        "{base_url}{base_path}{EGRESS_PATH}?{TARGET}={}",
        query::encode(uri)
    );
    for area in areas {
        routed.push_str(&format!("&{AREA}={}", query::encode(area)));
    }
    Ok(routed)
}

/// The granted areas a delivery is filtered against (GW11).
///
/// `geo_grants` where the intersection left the areas to the gateway, and otherwise the
/// `geoQ` the broker would have been given, which is then the grant's own area: a
/// subscription write carries no `geoQ` in its query string, because a subscription's own
/// geometry lives in its body. Where a request did carry one, filtering against it is
/// narrower than the grant, so this stays fail-closed either way.
fn granted_areas(constraints: &Constraints) -> Vec<String> {
    match constraints.geo_grants.is_empty() {
        false => constraints.geo_grants.clone(),
        true => constraints.geo_q.clone().into_iter().collect(),
    }
}

/// The endpoint the subscriber asked for and the areas its grants drew, read back out of a
/// stored subscription.
///
/// `None` for a subscription this gateway did not route, which is the answer for one
/// created directly on the broker: the gateway delivers nothing it did not narrow.
fn delivery_of(stored: &Value) -> Option<(String, Vec<String>)> {
    let uri = stored
        .get("notification")?
        .get("endpoint")?
        .get("uri")?
        .as_str()?;
    let (path, query) = uri.split_once('?')?;
    if !path.ends_with(EGRESS_PATH) {
        return None;
    }
    let params = query::parse(query);
    let target = query::first(&params, TARGET)?.to_owned();
    if !(target.starts_with("http://") || target.starts_with("https://")) {
        return None;
    }
    let areas = params
        .iter()
        .filter(|(name, _)| name == AREA)
        .map(|(_, area)| area.clone())
        .collect();
    Some((target, areas))
}

/// The attributes a stored subscription was narrowed to; empty is a subscription over
/// whatever the entity carries.
fn attributes_of(stored: &Value) -> BTreeSet<String> {
    stored
        .get("notification")
        .and_then(|notification| notification.get("attributes"))
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The types a stored subscription selects; empty is a subscription over whatever the grants
/// cover.
fn types_of(stored: &Value) -> BTreeSet<String> {
    stored
        .get("entities")
        .and_then(Value::as_array)
        .map(|selectors| {
            selectors
                .iter()
                .filter_map(|selector| selector.get("type").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The constraint set one delivery is judged by, per type (MP-02, T-1862).
///
/// A subscription carries one attribute list, so a grant over two classes with different slots is
/// stored as the union of both: `User: [age]` and `Vehicle: [weight]` become `[age, weight]`. That
/// union applied to every entity delivers a `Vehicle`'s `age` to a subscriber who may only read a
/// `User`'s. The endpoint's own projection is therefore read again at delivery and the union is
/// split back into the slots of each class, exactly as a read is projected.
fn delivery_constraints(stored: &Value, endpoint: &Endpoint) -> Constraints {
    let granted = attributes_of(stored);
    let mut constraints = Constraints {
        attrs: granted.clone(),
        hidden: endpoint.hidden_attributes.clone(),
        types: types_of(stored),
        ..Default::default()
    };
    let Some(projection) = &endpoint.projection else {
        return constraints;
    };
    let classes: BTreeSet<String> = projection
        .classes
        .iter()
        .map(|class| class.name.clone())
        .collect();
    constraints.attrs_by_type = classes
        .iter()
        .map(|class| {
            let slots = projection.attributes_of(class).unwrap_or_default();
            let allowed = match granted.is_empty() {
                true => slots,
                false => narrow(&granted, &slots),
            };
            (class.clone(), allowed)
        })
        .collect();
    // A type the projection does not name is not this endpoint's to deliver, whatever the
    // subscription says: the same rule the read path applies to an answer (EP-26).
    constraints.types = match constraints.types.is_empty() {
        true => classes,
        false => narrow(&constraints.types, &classes),
    };
    constraints
}

/// Drops the entities the subscription's own filter no longer matches (R46).
///
/// The filter is evaluated by the broker, on the state it holds now, because a notification
/// says what changed and not whether the entity still satisfies the condition the grant was
/// narrowed with.
async fn keep_matching(
    broker: &Broker,
    space: &str,
    filter: &str,
    data: &mut Value,
) -> Result<(), Box<ProblemDetails>> {
    let Some(entities) = data.as_array_mut() else {
        return Ok(());
    };
    let ids: Vec<String> = entities
        .iter()
        .filter_map(|entity| entity.get("id").and_then(Value::as_str))
        .map(query::encode)
        .collect();
    if ids.is_empty() {
        return Ok(());
    }

    let target = format!(
        "/ngsi-ld/v1/entities?id={}&q={}",
        ids.join(","),
        query::encode(filter)
    );
    let (status, answer) = read(broker, space, &target).await?;
    if !(200..300).contains(&status) {
        tracing::error!(
            status,
            "the broker refused the condition re-check of a notification"
        );
        return Err(Box::new(ProblemDetails::new(
            502,
            "upstream-unavailable",
            "Broker Unavailable",
        )));
    }

    let matching: BTreeSet<&str> = answer
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entity| entity.get("id").and_then(Value::as_str))
        .collect();
    entities.retain(|entity| {
        entity
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| matching.contains(id))
    });
    Ok(())
}

/// Reads one stored subscription, or `None` when the broker does not hold it.
async fn read_subscription(
    broker: &Broker,
    space: &str,
    id: &str,
) -> Result<Option<Value>, Box<ProblemDetails>> {
    let target = format!("/ngsi-ld/v1/subscriptions/{}", query::encode(id));
    let (status, body) = read(broker, space, &target).await?;
    match status {
        200..=299 => Ok(Some(body)),
        404 => Ok(None),
        status => {
            tracing::error!(
                status,
                "the broker did not hand back the stored subscription"
            );
            Err(Box::new(ProblemDetails::new(
                502,
                "upstream-unavailable",
                "Broker Unavailable",
            )))
        }
    }
}

/// One read the gateway makes on its own account, with the tenant pinned from the endpoint.
async fn read(
    broker: &Broker,
    space: &str,
    target: &str,
) -> Result<(u16, Value), Box<ProblemDetails>> {
    let mut headers = HeaderMap::new();
    if let Ok(tenant) = HeaderValue::from_str(space) {
        headers.insert(TENANT, tenant);
    }
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

    let answer = broker
        .send(Method::GET, target, headers, Body::empty())
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error)))?;
    let status = answer.status().as_u16();
    let Ok(bytes) = axum::body::to_bytes(answer.into_body(), MAX_NOTIFICATION).await else {
        return Err(Box::new(ProblemDetails::internal()));
    };
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

/// Sends the projected notification to the endpoint the subscriber asked for.
///
/// The broker's own headers are relayed, so the `receiverInfo` a secured subscription
/// carries reaches the receiver that expects it (ADR 009, R27).
async fn dispatch(
    broker: &Broker,
    target: &str,
    from: &HeaderMap,
    notification: &Value,
) -> Response<Body> {
    let Some((origin, path)) = split(target) else {
        tracing::error!("a stored subscription carries an endpoint the gateway cannot address");
        return ProblemDetails::internal().into_response();
    };
    let Ok(body) = serde_json::to_vec(notification) else {
        return ProblemDetails::internal().into_response();
    };

    let mut headers = from.clone();
    // The projected notification is a different length and a different message from the one
    // the broker sent, and it is addressed to somebody else.
    headers.remove(CONTENT_LENGTH);
    headers.remove(HOST);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(body.len() as u64));

    // The gateway's own client, aimed at the subscriber: one connection pool for every
    // delivery, and the trust anchors the deployment configured (R46).
    match broker
        .aimed_at(origin)
        .send(Method::POST, &path, headers, Body::from(body))
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            tracing::warn!(%error, "a notification could not be delivered");
            ProblemDetails::from(error).into_response()
        }
    }
}

/// Splits an HTTP or HTTPS URL into the origin the proxy takes and the path it forwards.
fn split(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = ["https://", "http://"]
        .into_iter()
        .find_map(|scheme| url.strip_prefix(scheme).map(|rest| (scheme, rest)))?;
    Some(match rest.split_once('/') {
        Some((authority, path)) if !authority.is_empty() => {
            (format!("{scheme}{authority}"), format!("/{path}"))
        }
        Some(_) => return None,
        None if rest.is_empty() => return None,
        None => (format!("{scheme}{rest}"), "/".to_owned()),
    })
}

/// Narrows one array of attribute names in place, when the payload carries it.
fn narrow_names(
    members: &mut Map<String, Value>,
    field: &str,
    constraints: &Constraints,
    hidden: &BTreeSet<String>,
) -> Result<(), Box<ProblemDetails>> {
    let Some(asked) = members.get(field).and_then(Value::as_array) else {
        return Ok(());
    };
    let asked: BTreeSet<String> = asked
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();

    let mut kept = narrow(&asked, &constraints.attrs);
    kept.retain(|name| !hidden.contains(name));
    if kept.is_empty() && !asked.is_empty() {
        return Err(Box::new(ProblemDetails::forbidden().with_detail(format!(
            "no attribute named in `{field}` is granted"
        ))));
    }
    members.insert(field.to_owned(), Value::Array(names(&kept)));
    Ok(())
}

/// Narrows the subscription's entity selectors to the granted types (GW27).
fn narrow_selectors(
    members: &mut Map<String, Value>,
    constraints: &Constraints,
    is_create: bool,
) -> Result<(), Box<ProblemDetails>> {
    if constraints.types.is_empty() {
        return Ok(());
    }
    match members.get_mut("entities").and_then(Value::as_array_mut) {
        Some(selectors) => {
            selectors.retain(|selector| {
                selector
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|entity_type| constraints.types.contains(entity_type))
            });
            if selectors.is_empty() {
                return Err(Box::new(ProblemDetails::forbidden().with_detail(
                    "no entity type this subscription watches is granted",
                )));
            }
        }
        // A creation that names no type would stand for every type in the tenant, which is
        // wider than the grant. The grant's own types become the selector.
        None if is_create => {
            members.insert(
                "entities".to_owned(),
                Value::Array(
                    constraints
                        .types
                        .iter()
                        .map(|entity_type| serde_json::json!({ "type": entity_type }))
                        .collect(),
                ),
            );
        }
        None => {}
    }
    Ok(())
}

/// Conjoins the subscription's own filter with the grants' (R12, R13, GW27).
fn narrow_filter(members: &mut Map<String, Value>, constraints: &Constraints, is_create: bool) {
    let Some(grants) = constraints.q.clone() else {
        return;
    };
    let own = members.get("q").and_then(Value::as_str).map(str::to_owned);
    // A `PATCH` that says nothing about `q` leaves the stored one alone: it was narrowed
    // when it was stored, and replacing it with the grants alone would drop the
    // subscriber's own filter.
    if own.is_none() && !is_create {
        return;
    }
    // `constraints.q` is already one complete expression, so it is conjoined rather than
    // folded in again as a term. The subscriber's own filter goes through `conjoin` alone,
    // which is what parenthesises it and what drops it when its parentheses do not balance
    // (ADR 006: an unbalanced filter would consume the wrapping and regroup what follows).
    let folded = match conjoin(own.as_deref(), &[]) {
        Some(own) => format!("{own};{grants}"),
        None => grants,
    };
    members.insert("q".to_owned(), Value::String(folded));
}

/// A sorted set as a JSON array of strings.
fn names(set: &BTreeSet<String>) -> Vec<Value> {
    set.iter().map(|name| Value::String(name.clone())).collect()
}

/// The refusal for a subscription this gateway cannot route or narrow faithfully.
fn unsupported(detail: &str) -> ProblemDetails {
    ProblemDetails::new(501, "subscription-not-routable", "Not Implemented").with_detail(detail)
}
