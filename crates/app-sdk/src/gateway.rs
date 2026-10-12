//! Reads from the Context Gateway, as the caller (AP-147): the host adds the caller's token, so an
//! App reads what the person it serves may read and nothing more. A component has no environment
//! to learn where the gateway runs; it calls the fixed origin `http://gateway` and the host sends
//! the call on, to the App's own Endpoint only: `/api/endpoint/<its slug>/…`, or for short
//! `/ngsi-ld/v1/…`, `/ogc/features/…` and `/schema/…` (AP-147). Every other origin and path is
//! refused before a connection is made.

use serde::de::DeserializeOwned;

use crate::http::Response;

/// Why a read from the gateway did not give an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The path is not one the gateway serves: not absolute, or with `..`, `//`, `\` or `#`.
    Invalid(String),
    /// The gateway answered, with this status and body.
    Status(u16, String),
    /// The host refused the call or the gateway did not answer.
    Unavailable(String),
    /// The answer was not the JSON the App expected.
    Decode(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(why) => write!(f, "not a gateway path: {why}"),
            Error::Status(status, _) => write!(f, "the gateway answered {status}"),
            Error::Unavailable(why) => write!(f, "the gateway is not reachable: {why}"),
            Error::Decode(why) => write!(f, "the gateway's answer: {why}"),
        }
    }
}

impl From<Error> for Response {
    /// The caller's own 401/403 passes through; anything else is the gateway's fault, a 502.
    fn from(err: Error) -> Self {
        match &err {
            Error::Status(401, _) => {
                Response::problem(401, "Unauthorized", "sign in again to read the city's data")
            }
            Error::Status(403, _) => Response::problem(
                403,
                "Forbidden",
                "your account may not read or change this data",
            ),
            Error::Invalid(_) => Response::problem(500, "Internal Server Error", &err.to_string()),
            _ => Response::problem(502, "Bad Gateway", &err.to_string()),
        }
    }
}

/// `path` as a gateway path: absolute, without a dot segment, an empty segment, a backslash or a
/// fragment, so it can only name a resource of the gateway itself.
pub fn checked(path: &str) -> Result<&str, Error> {
    let bare = path.split('?').next().unwrap_or_default();
    if !path.starts_with('/') {
        return Err(Error::Invalid("it does not start with /".into()));
    }
    if bare.contains("//") || path.contains('\\') || path.contains('#') {
        return Err(Error::Invalid("it has //, \\ or #".into()));
    }
    if bare
        .split('/')
        .any(|s| s == "." || s == ".." || s.eq_ignore_ascii_case("%2e%2e"))
    {
        return Err(Error::Invalid("it has a dot segment".into()));
    }
    Ok(path)
}

/// Percent-encodes a query value: everything but `A-Z a-z 0-9 - _ . ~` (T-3351).
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The body of a `GET` of `path` on the gateway, `Accept: application/json`; a status outside
/// 2xx is [`Error::Status`].
pub fn get(path: &str) -> Result<Vec<u8>, Error> {
    let path = checked(path)?;
    send("GET", path, None)
}

/// The path of [`patch_entity`]: `/ngsi-ld/v1/entities/{id}/attrs`, the id encoded.
pub fn entity_attrs_path(id: &str) -> Result<String, Error> {
    if id.is_empty() || id.chars().any(char::is_control) {
        return Err(Error::Invalid(format!("{id:?} is not an Entity id")));
    }
    Ok(format!("/ngsi-ld/v1/entities/{}/attrs", encode(id)))
}

/// Changes Attributes of the Entity `id` through the App's own Endpoint, as the caller (API/07
/// §4): `PATCH /ngsi-ld/v1/entities/{id}/attrs` with `attrs`, a JSON object of NGSI-LD
/// Attributes. The Endpoint's Policy decides; a refusal is [`Error::Status`] with the gateway's
/// `401` or `403`, which [`Response::from`] passes on as such.
pub fn patch_entity(id: &str, attrs: &serde_json::Value) -> Result<(), Error> {
    if !attrs.is_object() {
        return Err(Error::Invalid("the Attributes are a JSON object".into()));
    }
    let path = entity_attrs_path(id)?;
    send("PATCH", checked(&path)?, Some(attrs.to_string().as_bytes())).map(|_| ())
}

/// A `GET` of `path` read as `T`.
pub fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, Error> {
    let body = get(path)?;
    serde_json::from_slice(&body).map_err(|err| Error::Decode(err.to_string()))
}

/// The `/items` path of `collection` on the Endpoint's OGC API - Features surface (EP-29), with
/// `query`'s parameters encoded in order; a `filter` without a `filter-lang` is CQL2 text.
pub fn features_path(collection: &str, query: &[(&str, &str)]) -> Result<String, Error> {
    let collection = collection_name(collection)?;
    let mut pairs: Vec<String> = query
        .iter()
        .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
        .collect();
    if query.iter().any(|(name, _)| *name == "filter")
        && !query.iter().any(|(name, _)| *name == "filter-lang")
    {
        pairs.push("filter-lang=cql2-text".into());
    }
    let path = format!("/ogc/features/collections/{collection}/items");
    Ok(if pairs.is_empty() {
        path
    } else {
        format!("{path}?{}", pairs.join("&"))
    })
}

/// One page of `collection`'s Features (a GeoJSON `FeatureCollection`) from the App's Endpoint,
/// e.g. `features::<serde_json::Value>("Vehicle", &[("bbox", "24.9,60.1,25.1,60.3"), ("filter",
/// "speed > 50"), ("limit", "100")])`: the Entities the Endpoint's view and Policy admit to the
/// caller, as GeoJSON (EP-29).
pub fn features<T: DeserializeOwned>(collection: &str, query: &[(&str, &str)]) -> Result<T, Error> {
    get_json(&features_path(collection, query)?)
}

/// The Feature `id` (an Entity URN) of `collection`.
pub fn feature<T: DeserializeOwned>(collection: &str, id: &str) -> Result<T, Error> {
    let collection = collection_name(collection)?;
    get_json(&format!(
        "/ogc/features/collections/{collection}/items/{}",
        encode(id)
    ))
}

/// A collection is an Entity type name: a letter, then letters, digits and `_`.
fn collection_name(name: &str) -> Result<&str, Error> {
    let mut chars = name.chars();
    let fine = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if fine {
        Ok(name)
    } else {
        Err(Error::Invalid(format!("{name:?} is not a type name")))
    }
}

fn send(method: &str, path: &str, body: Option<&[u8]>) -> Result<Vec<u8>, Error> {
    use wasip2::http::outgoing_handler;
    use wasip2::http::types::{
        Fields, IncomingBody, Method, OutgoingBody, OutgoingRequest, Scheme,
    };

    let headers = Fields::new();
    let _ = headers.append("accept", b"application/json");
    if body.is_some() {
        let _ = headers.append("content-type", b"application/json");
    }
    let request = OutgoingRequest::new(headers);
    let _ = request.set_method(&match method {
        "GET" => Method::Get,
        "PATCH" => Method::Patch,
        other => Method::Other(other.into()),
    });
    let _ = request.set_scheme(Some(&Scheme::Http));
    let _ = request.set_authority(Some("gateway"));
    request
        .set_path_with_query(Some(path))
        .map_err(|()| Error::Invalid("the host refused the path".into()))?;
    let outgoing = request
        .body()
        .map_err(|()| Error::Unavailable("the host gave no request body".into()))?;
    // handed over first: the host drains the body while it sends, so a write never waits on a
    // full pipe that nothing reads
    let future = outgoing_handler::handle(request, None)
        .map_err(|code| Error::Unavailable(format!("{code:?}")))?;
    if let Some(bytes) = body {
        let stream = outgoing
            .write()
            .map_err(|()| Error::Unavailable("the host gave no body stream".into()))?;
        for chunk in bytes.chunks(4096) {
            stream
                .blocking_write_and_flush(chunk)
                .map_err(|err| Error::Unavailable(format!("{err:?}")))?;
        }
        drop(stream);
    }
    OutgoingBody::finish(outgoing, None).map_err(|err| Error::Unavailable(format!("{err:?}")))?;
    future.subscribe().block();
    let response = match future.get() {
        Some(Ok(Ok(response))) => response,
        Some(Ok(Err(code))) => return Err(Error::Unavailable(format!("{code:?}"))),
        _ => return Err(Error::Unavailable("no answer".into())),
    };
    let status = response.status();
    let mut bytes = Vec::new();
    if let Ok(body) = response.consume() {
        if let Ok(stream) = body.stream() {
            while let Ok(chunk) = stream.blocking_read(64 * 1024) {
                if chunk.is_empty() {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            drop(stream);
        }
        let _ = IncomingBody::finish(body);
    }
    if (200..300).contains(&status) {
        Ok(bytes)
    } else {
        Err(Error::Status(
            status,
            String::from_utf8_lossy(&bytes).chars().take(500).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entity_edit_names_one_entity_of_the_endpoint() {
        assert_eq!(
            entity_attrs_path("urn:ngsi-ld:Station:7").as_deref(),
            Ok("/ngsi-ld/v1/entities/urn%3Angsi-ld%3AStation%3A7/attrs")
        );
        let escaped = entity_attrs_path("../../x/y?z#").expect("encoded");
        assert!(checked(&escaped).is_ok(), "{escaped}");
        assert_eq!(escaped, "/ngsi-ld/v1/entities/..%2F..%2Fx%2Fy%3Fz%23/attrs");
        assert!(entity_attrs_path("").is_err());
        assert!(entity_attrs_path("a\nb").is_err());
        assert!(matches!(
            patch_entity("urn:x", &serde_json::json!([1])),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn a_path_names_a_resource_of_the_gateway_alone() {
        assert_eq!(
            checked("/api/endpoint/abc/ngsi-ld/v1/entities?type=A&limit=10"),
            Ok("/api/endpoint/abc/ngsi-ld/v1/entities?type=A&limit=10")
        );
        for bad in [
            "",
            "api/x",
            "//evil/x",
            "/a//b",
            "/a/../b",
            "/a/./b",
            "/a/%2E%2E/b",
            "/a\\b",
            "/a#b",
            "http://evil/x",
        ] {
            assert!(checked(bad).is_err(), "{bad}");
        }
        // A query may carry `//`, as a URL value does.
        assert!(checked("/a?q=http://x").is_ok());
    }

    #[test]
    fn a_features_read_names_its_collection_and_encodes_every_parameter() {
        assert_eq!(
            features_path("Vehicle", &[]).as_deref(),
            Ok("/ogc/features/collections/Vehicle/items")
        );
        assert_eq!(
            features_path(
                "Vehicle",
                &[("bbox", "24.9,60.1,25.1,60.3"), ("filter", "name LIKE 'A%'"), ("limit", "10")]
            )
            .as_deref(),
            Ok("/ogc/features/collections/Vehicle/items?bbox=24.9%2C60.1%2C25.1%2C60.3&filter=name%20LIKE%20%27A%25%27&limit=10&filter-lang=cql2-text")
        );
        // a filter language the App names is kept, never doubled
        assert_eq!(
            features_path("Vehicle", &[("filter", "{}"), ("filter-lang", "cql2-json")]).as_deref(),
            Ok("/ogc/features/collections/Vehicle/items?filter=%7B%7D&filter-lang=cql2-json")
        );
        // every path it builds is one the gateway check accepts
        assert!(checked(
            &features_path("V", &[("datetime", "2026-01-01T00:00:00Z/..")]).unwrap_or_default()
        )
        .is_ok());
    }

    #[test]
    fn a_collection_that_is_no_type_name_is_refused_before_any_call() {
        for bad in [
            "",
            "../x",
            "Vehicle/items",
            "1Car",
            "Car?x=1",
            "Car#x",
            "Ca r",
        ] {
            assert!(
                matches!(features_path(bad, &[]), Err(Error::Invalid(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            feature::<serde_json::Value>("../admin", "x"),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn a_value_is_encoded_for_a_query() {
        assert_eq!(encode("a,b c/é"), "a%2Cb%20c%2F%C3%A9");
        assert_eq!(encode("Alert-1_.~"), "Alert-1_.~");
    }

    #[test]
    fn the_callers_refusal_passes_through_and_the_rest_is_the_gateways() {
        assert_eq!(
            Response::from(Error::Status(401, String::new())).status,
            401
        );
        assert_eq!(
            Response::from(Error::Status(403, String::new())).status,
            403
        );
        assert_eq!(
            Response::from(Error::Status(500, String::new())).status,
            502
        );
        assert_eq!(Response::from(Error::Unavailable("x".into())).status, 502);
        assert_eq!(Response::from(Error::Decode("x".into())).status, 502);
    }
}
