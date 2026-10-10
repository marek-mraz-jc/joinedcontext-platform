//! The App's own Endpoint on the Context Gateway, read with the caller's token (AP-147). The
//! component names neither the gateway's address nor its Endpoint: it asks for
//! `http://gateway/ngsi-ld/v1/…`, and the host sends that to the App's own Endpoint with the
//! caller's token, refusing anything else. What the caller may not read, the gateway refuses.
//!
//! ```no_run
//! let alerts = jc_app_sdk::gateway::get(&format!(
//!     "/ngsi-ld/v1/entities?type=Alert&limit=100&options=keyValues&attrs={}",
//!     jc_app_sdk::gateway::encode("location,validFrom"),
//! ));
//! ```

use serde::de::DeserializeOwned;

/// The origin the host maps to the App's own Endpoint.
pub const ORIGIN: &str = "gateway";

/// The gateway's answer: its status and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Answer {
    /// The body as JSON when the status is 2xx; otherwise a sentence with the status and the
    /// gateway's own `detail` or `title`, when it gave one.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, String> {
        if !(200..300).contains(&self.status) {
            return Err(refusal(self.status, &self.body));
        }
        serde_json::from_slice(&self.body).map_err(|err| {
            format!("the gateway answered something that is not the expected JSON: {err}")
        })
    }
}

/// What a non-2xx answer says, as a sentence a person can act on.
pub fn refusal(status: u16, body: &[u8]) -> String {
    let said = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            ["detail", "title"]
                .iter()
                .find_map(|k| v.get(*k).and_then(|d| d.as_str()).map(str::to_owned))
        });
    let what = match status {
        401 => "the gateway wants a login for this data",
        403 => "the gateway does not let this caller read this data",
        404 => "the gateway found nothing at this address",
        429 => "the gateway is asking for fewer requests; try again shortly",
        500..=599 => "the gateway could not answer right now; try again shortly",
        _ => "the gateway refused the request",
    };
    match said {
        Some(said) => format!("{what} ({status}: {said})"),
        None => format!("{what} ({status})"),
    }
}

/// `path` must start with `/ngsi-ld/v1/` (the host refuses anything else); a query is part of it.
pub fn check(path: &str) -> Result<(), String> {
    if path.starts_with("/ngsi-ld/v1/") || path == "/ngsi-ld/v1" {
        Ok(())
    } else {
        Err(format!(
            "a gateway path starts with /ngsi-ld/v1/, not `{path}`"
        ))
    }
}

/// Percent-encodes a query value: everything but `A-Z a-z 0-9 - _ . ~`.
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

/// One GET of the App's own Endpoint, `path` below it (`/ngsi-ld/v1/entities?type=…`), asking for
/// JSON. An error is a sentence: the host's refusal, or why no answer came.
pub fn get(path: &str) -> Result<Answer, String> {
    check(path)?;
    send(path)
}

#[cfg(target_arch = "wasm32")]
fn send(path: &str) -> Result<Answer, String> {
    use wasip2::http::outgoing_handler;
    use wasip2::http::types::{Fields, IncomingBody, OutgoingRequest, Scheme};

    /// How much of an answer one read asks for; the host caps the whole answer (AP-146).
    const READ_CHUNK: u64 = 64 * 1024;

    let headers = Fields::new();
    headers
        .append("accept", b"application/json")
        .map_err(|err| format!("a request header: {err:?}"))?;
    let request = OutgoingRequest::new(headers);
    let _ = request.set_scheme(Some(&Scheme::Http));
    let _ = request.set_authority(Some(ORIGIN));
    request
        .set_path_with_query(Some(path))
        .map_err(|()| format!("`{path}` is not a path the gateway can be asked for"))?;
    let future = outgoing_handler::handle(request, None)
        .map_err(|code| format!("the host refused the call to the gateway: {code:?}"))?;
    future.subscribe().block();
    let response = match future.get() {
        Some(Ok(Ok(response))) => response,
        Some(Ok(Err(code))) => return Err(format!("the gateway could not be reached: {code:?}")),
        _ => return Err("the gateway gave no answer".into()),
    };
    let status = response.status();
    let mut body = Vec::new();
    let incoming = response
        .consume()
        .map_err(|()| "the gateway's answer has no body".to_owned())?;
    let stream = incoming
        .stream()
        .map_err(|()| "the gateway's answer could not be read".to_owned())?;
    loop {
        match stream.blocking_read(READ_CHUNK) {
            Ok(chunk) => body.extend_from_slice(&chunk),
            Err(wasip2::io::streams::StreamError::Closed) => break,
            Err(err) => return Err(format!("the gateway's answer broke off: {err:?}")),
        }
    }
    drop(stream);
    let _ = IncomingBody::finish(incoming);
    Ok(Answer { status, body })
}

/// Off wasm32 there is no host: an App's native unit tests test what it does with an answer.
#[cfg(not(target_arch = "wasm32"))]
fn send(_: &str) -> Result<Answer, String> {
    Err("the gateway is reached only from inside the host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_outside_ngsi_ld_is_refused_before_any_call() {
        assert!(check("/ngsi-ld/v1/entities?type=Alert").is_ok());
        assert!(check("/api/endpoint/x/ngsi-ld/v1/entities").is_err());
        assert!(check("http://elsewhere/ngsi-ld/v1/").is_err());
        assert!(get("/other").unwrap_err().contains("/ngsi-ld/v1/"));
    }

    #[test]
    fn a_value_is_encoded_for_a_query() {
        assert_eq!(encode("a,b c/é"), "a%2Cb%20c%2F%C3%A9");
        assert_eq!(encode("Alert-1_.~"), "Alert-1_.~");
    }

    #[test]
    fn an_answer_reads_as_json_or_says_why_not() {
        let ok = Answer {
            status: 200,
            body: br#"[{"id":"a"}]"#.to_vec(),
        };
        assert_eq!(ok.json::<serde_json::Value>().unwrap()[0]["id"], "a");
        let denied = Answer {
            status: 403,
            body: br#"{"title":"Forbidden","detail":"no policy grants Alert"}"#.to_vec(),
        };
        let why = denied.json::<serde_json::Value>().unwrap_err();
        assert!(
            why.contains("does not let") && why.contains("no policy grants Alert"),
            "{why}"
        );
        let broken = Answer {
            status: 200,
            body: b"<html>".to_vec(),
        };
        assert!(broken
            .json::<serde_json::Value>()
            .unwrap_err()
            .contains("not the expected JSON"));
        assert_eq!(
            refusal(502, b""),
            "the gateway could not answer right now; try again shortly (502)"
        );
    }
}
