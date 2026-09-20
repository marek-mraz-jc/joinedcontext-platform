//! The smallest HTTP/1.1 client that can reach Model Tools, and nothing more (DM-32).
//!
//! `jcctl` had no HTTP client and this is the only thing that needs one: one POST of a JSON
//! body to a service the platform runs itself, over plaintext inside the cluster. A general
//! client would be a dependency tree for a request whose peer, port and body shape are all
//! known here, so this is a hundred lines instead. It refuses everything it cannot do
//! correctly rather than guessing: no TLS, no redirects, no chunked responses, no keep-alive.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// How long a connection, a write or a read may take. Generation is CPU-bound Python and a
/// large model takes seconds, so the read budget is the generous one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(120);

/// Largest answer accepted. An artifact set of a big model is a few hundred kilobytes.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// One answer: the status line's code and the body, which is always JSON here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Response body, decoded as UTF-8.
    pub body: String,
}

/// Why a request did not produce an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// The URL is not one this client can use.
    Url(String),
    /// The host did not answer.
    Connect(String),
    /// The connection failed part-way.
    Io(String),
    /// The answer is not something this client will guess at.
    Protocol(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Url(m) => write!(f, "{m}"),
            Self::Connect(m) => write!(f, "cannot reach Model Tools: {m}"),
            Self::Io(m) => write!(f, "the connection to Model Tools failed: {m}"),
            Self::Protocol(m) => write!(f, "Model Tools answered something unusable: {m}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// `http://host[:port]` split into what a connection needs, plus the path prefix.
fn split(url: &str) -> Result<(String, u16, String), HttpError> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        HttpError::Url(format!(
            "'{url}' is not an http:// URL; Model Tools is reached over plaintext inside the \
             cluster, where the mesh carries the TLS"
        ))
    })?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.contains('@') {
        return Err(HttpError::Url(
            "a Model Tools URL carries no credentials; it holds none and needs none".into(),
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .map_err(|_| HttpError::Url(format!("'{port}' is not a port")))?,
        ),
        None => (authority, 8080),
    };
    if host.is_empty() {
        return Err(HttpError::Url(format!("'{url}' names no host")));
    }
    Ok((host.to_owned(), port, path.trim_end_matches('/').to_owned()))
}

/// One request. `body` is a JSON document for a POST, or `None` for a GET.
pub fn request(base: &str, path: &str, body: Option<&str>) -> Result<Response, HttpError> {
    let (host, port, prefix) = split(base)?;
    let target = format!("{prefix}{path}");

    let address = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| HttpError::Connect(format!("{host}:{port} does not resolve ({e})")))?
        .next()
        .ok_or_else(|| HttpError::Connect(format!("{host}:{port} resolves to no address")))?;
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .map_err(|e| HttpError::Connect(format!("{host}:{port} ({e})")))?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
        .map_err(|e| HttpError::Io(e.to_string()))?;

    // `Connection: close` on purpose: the answer is then everything up to the end of the
    // stream, which is the one framing this client does not have to implement.
    let method = if body.is_some() { "POST" } else { "GET" };
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\n\
         User-Agent: jcctl\r\nConnection: close\r\n"
    );
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body.unwrap_or("").as_bytes()))
        .and_then(|()| stream.flush())
        .map_err(|e| HttpError::Io(e.to_string()))?;

    let mut raw = Vec::new();
    stream
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| HttpError::Io(e.to_string()))?;
    if raw.len() > MAX_BODY_BYTES {
        return Err(HttpError::Protocol(format!(
            "an answer larger than {MAX_BODY_BYTES} bytes"
        )));
    }
    parse(&raw)
}

/// The status code and the body of one raw answer.
fn parse(raw: &[u8]) -> Result<Response, HttpError> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .ok_or_else(|| HttpError::Protocol("no header block".into()))?;

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Protocol(format!("'{status_line}' is not a status line")))?;

    // Reading to the end of the stream is the body only when the body is not framed inside
    // it. Model Tools never chunks; refusing is still better than handing back chunk sizes
    // as if they were JSON.
    if lines.any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("transfer-encoding:") && lower.contains("chunked")
    }) {
        return Err(HttpError::Protocol(
            "a chunked response, which this client does not decode".into(),
        ));
    }

    Ok(Response {
        status,
        body: body.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_without_a_port_uses_the_service_port() {
        assert_eq!(
            split("http://model-tools").unwrap(),
            ("model-tools".into(), 8080, String::new())
        );
        assert_eq!(
            split("http://model-tools.jc.svc.cluster.local:9000/tools/").unwrap(),
            (
                "model-tools.jc.svc.cluster.local".into(),
                9000,
                "/tools".into()
            )
        );
    }

    #[test]
    fn https_is_refused_rather_than_silently_downgraded() {
        let err = split("https://model-tools:8080").unwrap_err();
        assert!(matches!(err, HttpError::Url(_)), "{err}");
        assert!(err.to_string().contains("plaintext"));
    }

    #[test]
    fn a_url_carrying_credentials_is_refused() {
        assert!(matches!(
            split("http://user:pass@model-tools:8080").unwrap_err(),
            HttpError::Url(_)
        ));
    }

    #[test]
    fn the_status_and_the_body_come_out_of_the_answer() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"a\":1}";
        assert_eq!(
            parse(raw).unwrap(),
            Response {
                status: 200,
                body: "{\"a\":1}".into()
            }
        );
    }

    #[test]
    fn a_chunked_answer_is_refused_instead_of_being_read_as_json() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1}\r\n0\r\n\r\n";
        assert!(matches!(parse(raw).unwrap_err(), HttpError::Protocol(_)));
    }

    #[test]
    fn an_answer_with_no_header_block_is_not_guessed_at() {
        assert!(matches!(
            parse(b"garbage").unwrap_err(),
            HttpError::Protocol(_)
        ));
    }
}
