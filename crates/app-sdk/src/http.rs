//! A request in and a response out, over `wasi:http` (AP-143): the body read whole (the host has
//! capped it), JSON in and out, errors as `application/problem+json`, and a router of paths with
//! `{name}` segments.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// One request, as the App sees it: the path below `/apps/{name}`, never the caller's
/// `Authorization` or cookies (the host keeps those).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    pub method: String,
    /// `/api/…`, without the query.
    pub path: String,
    /// The query's pairs, percent-decoded, in order.
    pub query: Vec<(String, String)>,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// The first query value named `name`.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body read as `T`, or the 400 that says why not.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, Response> {
        serde_json::from_slice(&self.body)
            .map_err(|err| Response::problem(400, "Bad Request", &format!("the body: {err}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            body: body.into(),
        }
    }

    pub fn json<T: Serialize>(status: u16, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self::new(status, "application/json", body),
            Err(err) => Self::problem(500, "Internal Server Error", &err.to_string()),
        }
    }

    pub fn text(status: u16, text: &str) -> Self {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            text.as_bytes().to_vec(),
        )
    }

    /// RFC 9457: `{status, title, detail}`.
    pub fn problem(status: u16, title: &str, detail: &str) -> Self {
        let body = serde_json::json!({"status": status, "title": title, "detail": detail});
        Self::new(
            status,
            "application/problem+json",
            body.to_string().into_bytes(),
        )
    }

    pub fn no_content() -> Self {
        Self {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A `jc:app/sql` error as the caller reads it.
    pub fn from_sql(err: crate::sql::Error) -> Self {
        use crate::sql::Error;
        match err {
            Error::Refused(why) => Self::problem(403, "Refused", &why),
            Error::Invalid(why) => Self::problem(400, "Bad Request", &why),
            Error::Quota(why) => Self::problem(507, "Insufficient Storage", &why),
            Error::Timeout => {
                Self::problem(504, "Timeout", "the statement took longer than it may")
            }
            Error::Unavailable(why) => Self::problem(503, "Unavailable", &why),
        }
    }

    /// A `jc:app/blob` error as the caller reads it.
    pub fn from_blob(err: crate::blob::Error) -> Self {
        use crate::blob::Error;
        match err {
            Error::Refused(why) => Self::problem(403, "Refused", &why),
            Error::NotFound => Self::problem(404, "Not Found", "no such file"),
            Error::Quota(why) => Self::problem(507, "Insufficient Storage", &why),
            Error::Unavailable(why) => Self::problem(503, "Unavailable", &why),
        }
    }
}

/// The `{name}` segments a route matched.
pub type Params = BTreeMap<String, String>;

type Handler = fn(&Request, &Params) -> Response;

/// Routes by method and path; `{name}` matches one segment.
#[derive(Default)]
pub struct Router {
    routes: Vec<(String, Vec<String>, Handler)>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(mut self, method: &str, pattern: &str, handler: Handler) -> Self {
        let segments = pattern
            .trim_matches('/')
            .split('/')
            .map(str::to_owned)
            .collect();
        self.routes
            .push((method.to_ascii_uppercase(), segments, handler));
        self
    }

    pub fn get(self, pattern: &str, handler: Handler) -> Self {
        self.route("GET", pattern, handler)
    }
    pub fn post(self, pattern: &str, handler: Handler) -> Self {
        self.route("POST", pattern, handler)
    }
    pub fn put(self, pattern: &str, handler: Handler) -> Self {
        self.route("PUT", pattern, handler)
    }
    pub fn delete(self, pattern: &str, handler: Handler) -> Self {
        self.route("DELETE", pattern, handler)
    }

    fn matches(segments: &[String], path: &str) -> Option<Params> {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.len() != segments.len() {
            return None;
        }
        let mut params = Params::new();
        for (segment, part) in segments.iter().zip(parts) {
            match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(name) if !part.is_empty() => {
                    params.insert(name.to_owned(), decode(part));
                }
                Some(_) => return None,
                None if segment == part => {}
                None => return None,
            }
        }
        Some(params)
    }

    /// The matching route's answer; 405 when the path matches with another method, else 404.
    pub fn handle(&self, request: &Request) -> Response {
        let mut path_known = false;
        for (method, segments, handler) in &self.routes {
            if let Some(params) = Self::matches(segments, &request.path) {
                if *method == request.method {
                    return handler(request, &params);
                }
                path_known = true;
            }
        }
        if path_known {
            Response::problem(
                405,
                "Method Not Allowed",
                "this address takes another method",
            )
        } else {
            Response::problem(404, "Not Found", "no such address")
        }
    }
}

/// `%XX` and `+` decoded; a broken escape stays as written.
pub fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(byte) => {
                    out.push(byte);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The query string as pairs.
pub fn pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

/// Reads the incoming request, runs `handler`, writes its response (the `app!` macro's body).
pub fn serve(
    incoming: wasip2::http::types::IncomingRequest,
    out: wasip2::http::types::ResponseOutparam,
    handler: fn(Request) -> Response,
) {
    use wasip2::http::types::{
        Fields, IncomingBody, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
    };

    let method = match incoming.method() {
        Method::Get => "GET".to_owned(),
        Method::Post => "POST".to_owned(),
        Method::Put => "PUT".to_owned(),
        Method::Delete => "DELETE".to_owned(),
        Method::Patch => "PATCH".to_owned(),
        Method::Head => "HEAD".to_owned(),
        Method::Options => "OPTIONS".to_owned(),
        Method::Connect => "CONNECT".to_owned(),
        Method::Trace => "TRACE".to_owned(),
        Method::Other(other) => other.to_ascii_uppercase(),
    };
    let target = incoming.path_with_query().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let headers = incoming
        .headers()
        .entries()
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_ascii_lowercase(),
                String::from_utf8_lossy(&value).into_owned(),
            )
        })
        .collect();
    let mut body = Vec::new();
    if let Ok(stream_body) = incoming.consume() {
        if let Ok(stream) = stream_body.stream() {
            while let Ok(chunk) = stream.blocking_read(64 * 1024) {
                if chunk.is_empty() {
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            drop(stream);
        }
        let _ = IncomingBody::finish(stream_body);
    }
    let request = Request {
        method,
        path: path.to_owned(),
        query: pairs(query),
        headers,
        body,
    };
    let response = handler(request);

    let fields = Fields::new();
    for (name, value) in &response.headers {
        let _ = fields.append(name, value.as_bytes());
    }
    let outgoing = OutgoingResponse::new(fields);
    let _ = outgoing.set_status_code(response.status);
    let Ok(body) = outgoing.body() else { return };
    ResponseOutparam::set(out, Ok(outgoing));
    if let Ok(stream) = body.write() {
        for chunk in response.body.chunks(4096) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                break;
            }
        }
        drop(stream);
    }
    let _ = OutgoingBody::finish(body, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, path: &str) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            ..Request::default()
        }
    }

    fn named(_: &Request, params: &Params) -> Response {
        Response::text(200, &format!("note {}", params["id"]))
    }

    fn all(_: &Request, _: &Params) -> Response {
        Response::text(200, "all")
    }

    #[test]
    fn a_route_matches_its_method_and_its_segments() {
        let router = Router::new()
            .get("/api/notes", all)
            .get("/api/notes/{id}", named)
            .delete("/api/notes/{id}", named);
        assert_eq!(router.handle(&request("GET", "/api/notes")).body, b"all");
        assert_eq!(
            router.handle(&request("GET", "/api/notes/a%20b")).body,
            b"note a b"
        );
        assert_eq!(router.handle(&request("POST", "/api/notes/1")).status, 405);
        assert_eq!(router.handle(&request("GET", "/api/notes/1/x")).status, 404);
        assert_eq!(
            router.handle(&request("GET", "/api/notes/")).status,
            200,
            "a trailing slash is the list"
        );
        assert_eq!(router.handle(&request("GET", "/other")).status, 404);
    }

    #[test]
    fn a_query_is_decoded_and_a_broken_escape_stays() {
        assert_eq!(
            pairs("q=a+b&x=%C3%A9&flag&bad=%zz&end=%4"),
            [
                ("q".into(), "a b".into()),
                ("x".into(), "é".into()),
                ("flag".into(), String::new()),
                ("bad".into(), "%zz".into()),
                ("end".into(), "%4".into()),
            ]
        );
    }

    #[test]
    fn a_body_is_read_as_json_or_answered_with_400() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Note {
            body: String,
        }
        let mut good = request("POST", "/api/notes");
        good.body = br#"{"body": "hi"}"#.to_vec();
        assert_eq!(good.json::<Note>(), Ok(Note { body: "hi".into() }));
        good.body = b"{".to_vec();
        let refused = good.json::<Note>().unwrap_err();
        assert_eq!(refused.status, 400);
        assert_eq!(refused.headers[0].1, "application/problem+json");
    }
}
