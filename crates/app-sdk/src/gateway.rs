//! Reads from the Context Gateway, as the caller (AP-147): the host adds the caller's token, so an
//! App reads what the person it serves may read and nothing more. A component has no environment
//! to learn where the gateway runs; it calls the fixed origin `http://gateway` and the host sends
//! the call on. Every other origin is refused before a connection is made.

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
                "your account may not read the data this needs",
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

/// The body of a `GET` of `path` on the gateway, `Accept: application/json`; a status outside
/// 2xx is [`Error::Status`].
pub fn get(path: &str) -> Result<Vec<u8>, Error> {
    let path = checked(path)?;
    send(path)
}

/// A `GET` of `path` read as `T`.
pub fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, Error> {
    let body = get(path)?;
    serde_json::from_slice(&body).map_err(|err| Error::Decode(err.to_string()))
}

fn send(path: &str) -> Result<Vec<u8>, Error> {
    use wasip2::http::outgoing_handler;
    use wasip2::http::types::{Fields, IncomingBody, OutgoingBody, OutgoingRequest, Scheme};

    let headers = Fields::new();
    let _ = headers.append("accept", b"application/json");
    let request = OutgoingRequest::new(headers);
    let _ = request.set_scheme(Some(&Scheme::Http));
    let _ = request.set_authority(Some("gateway"));
    request
        .set_path_with_query(Some(path))
        .map_err(|()| Error::Invalid("the host refused the path".into()))?;
    if let Ok(body) = request.body() {
        let _ = OutgoingBody::finish(body, None);
    }
    let future = outgoing_handler::handle(request, None)
        .map_err(|code| Error::Unavailable(format!("{code:?}")))?;
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
