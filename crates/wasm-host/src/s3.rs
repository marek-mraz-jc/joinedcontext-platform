//! The little of S3 a shard speaks to RustFS (ADR-N-044 §2.4): object requests signed with AWS
//! Signature Version 4 and presigned URLs, path-style, over the workspace's reqwest. Written here
//! rather than taken from `object_store`, whose TLS stack carries a licence the deny list refuses
//! and whose older releases carry advisories; the signing is checked against AWS's own examples.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

/// The SHA-256 of an empty body, which a GET or DELETE signs.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// One bucket on one endpoint, with the shard's own key.
#[derive(Clone)]
pub struct Bucket {
    /// `http(s)://host[:port]`, no path.
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub key_id: String,
    pub secret: String,
    /// The address a browser reaches the store at, when it differs from the shard's own
    /// (in-cluster) one: presigned URLs are signed for it, so they open from the browser (T-3342).
    pub public_endpoint: Option<String>,
    pub http: reqwest::Client,
}

impl std::fmt::Debug for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret never reaches a log.
        f.debug_struct("Bucket")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .finish_non_exhaustive()
    }
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// RFC 3986 percent-encoding as SigV4 wants it: every byte but the unreserved ones, and `/` kept
/// in a path.
pub fn encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `20130524T000000Z` and `20130524`.
fn stamps(at: OffsetDateTime) -> (String, String) {
    let at = at.to_offset(time::UtcOffset::UTC);
    let date = format!("{:04}{:02}{:02}", at.year(), u8::from(at.month()), at.day());
    (
        format!(
            "{date}T{:02}{:02}{:02}Z",
            at.hour(),
            at.minute(),
            at.second()
        ),
        date,
    )
}

/// The `host[:port]` of an endpoint.
fn host_of(endpoint: &str) -> &str {
    endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest)
        .trim_end_matches('/')
}

/// The SigV4 signature of a canonical request, and the scope it was signed for.
fn sign(
    secret: &str,
    region: &str,
    amz_date: &str,
    date: &str,
    canonical: &str,
) -> (String, String) {
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let key = hmac(
        &hmac(
            &hmac(&hmac(format!("AWS4{secret}").as_bytes(), date), region),
            "s3",
        ),
        "aws4_request",
    );
    (hex::encode(hmac(&key, &to_sign)), scope)
}

impl Bucket {
    fn host(&self) -> &str {
        host_of(&self.endpoint)
    }

    /// `/{bucket}/{key}`, encoded; `/{key}` when the endpoint is the bucket's own host.
    fn path(&self, key: &str) -> String {
        if self.bucket.is_empty() {
            format!("/{}", encode(key, true))
        } else {
            format!("/{}/{}", encode(&self.bucket, false), encode(key, true))
        }
    }

    /// The `Authorization` header of one request; `headers` are the extra headers to sign, lower
    /// case, `query` the canonical query string.
    pub fn authorization(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(&str, String)],
        payload_sha256: &str,
        at: OffsetDateTime,
    ) -> (String, String) {
        let (amz_date, date) = stamps(at);
        let mut signed: Vec<(String, String)> = vec![
            ("host".into(), self.host().to_owned()),
            ("x-amz-content-sha256".into(), payload_sha256.to_owned()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        signed.extend(
            headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.trim().to_owned())),
        );
        signed.sort();
        let names = signed
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let canonical =
            format!("{method}\n{path}\n{query}\n{canonical_headers}\n{names}\n{payload_sha256}");
        let (signature, scope) = sign(&self.secret, &self.region, &amz_date, &date, &canonical);
        (format!("AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={names},Signature={signature}", self.key_id), amz_date)
    }

    /// A URL that lets its holder do `method` on `key` alone until it expires (at most 7 days by
    /// S3's rule; the host gives far less).
    pub fn presign(
        &self,
        method: &str,
        key: &str,
        expires_seconds: u32,
        at: OffsetDateTime,
    ) -> String {
        let (amz_date, date) = stamps(at);
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let path = self.path(key);
        let mut query = [
            ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
            (
                "X-Amz-Credential".to_owned(),
                format!("{}/{scope}", self.key_id),
            ),
            ("X-Amz-Date".to_owned(), amz_date.clone()),
            ("X-Amz-Expires".to_owned(), expires_seconds.to_string()),
            ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
        ];
        query.sort();
        let canonical_query = query
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k, false), encode(v, false)))
            .collect::<Vec<_>>()
            .join("&");
        // Signed for the address the browser uses, which is the one it sends as `Host`.
        let endpoint = self.public_endpoint.as_deref().unwrap_or(&self.endpoint);
        let canonical = format!(
            "{method}\n{path}\n{canonical_query}\nhost:{}\n\nhost\nUNSIGNED-PAYLOAD",
            host_of(endpoint)
        );
        let (signature, _) = sign(&self.secret, &self.region, &amz_date, &date, &canonical);
        format!(
            "{}{path}?{canonical_query}&X-Amz-Signature={signature}",
            endpoint.trim_end_matches('/')
        )
    }

    /// One signed request on `key`, its answer status and body.
    pub async fn object(
        &self,
        method: reqwest::Method,
        key: &str,
        body: Option<(Vec<u8>, Option<String>)>,
    ) -> Result<(u16, Vec<u8>), String> {
        let path = self.path(key);
        let (bytes, content_type) = body.unwrap_or_default();
        let payload = if bytes.is_empty() {
            EMPTY_SHA256.to_owned()
        } else {
            sha256_hex(&bytes)
        };
        let mut extra = Vec::new();
        if let Some(ct) = &content_type {
            extra.push(("content-type", ct.clone()));
        }
        let (authorization, amz_date) = self.authorization(
            method.as_str(),
            &path,
            "",
            &extra,
            &payload,
            OffsetDateTime::now_utc(),
        );
        let mut request = self
            .http
            .request(
                method,
                format!("{}{path}", self.endpoint.trim_end_matches('/')),
            )
            .header("authorization", authorization)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload);
        if let Some(ct) = content_type {
            request = request.header("content-type", ct);
        }
        if !bytes.is_empty() {
            request = request.body(bytes);
        }
        let response = request
            .send()
            .await
            .map_err(|err| format!("the object store: {err}"))?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|err| format!("the object store: {err}"))?;
        Ok((status, body.to_vec()))
    }
}

/// The text of every `<tag>…</tag>` in `xml`, entities decoded. S3's list answers are this plain.
fn elements(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        rest = &rest[start + open.len()..];
        let Some(end) = rest.find(&close) else { break };
        out.push(unescape(&rest[..end]));
        rest = &rest[end + close.len()..];
    }
    out
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';') else { break };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

impl Bucket {
    /// Every key under `prefix` with its size, following S3's pages.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, String> {
        let path = if self.bucket.is_empty() {
            "/".to_owned()
        } else {
            format!("/{}", encode(&self.bucket, false))
        };
        let mut found = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_owned(), "2".to_owned()),
                ("prefix".to_owned(), prefix.to_owned()),
            ];
            if let Some(token) = &token {
                query.push(("continuation-token".to_owned(), token.clone()));
            }
            query.sort();
            let canonical = query
                .iter()
                .map(|(k, v)| format!("{}={}", encode(k, false), encode(v, false)))
                .collect::<Vec<_>>()
                .join("&");
            let (authorization, amz_date) = self.authorization(
                "GET",
                &path,
                &canonical,
                &[],
                EMPTY_SHA256,
                OffsetDateTime::now_utc(),
            );
            let response = self
                .http
                .get(format!(
                    "{}{path}?{canonical}",
                    self.endpoint.trim_end_matches('/')
                ))
                .header("authorization", authorization)
                .header("x-amz-date", amz_date)
                .header("x-amz-content-sha256", EMPTY_SHA256)
                .send()
                .await
                .map_err(|err| format!("the object store: {err}"))?;
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .map_err(|err| format!("the object store: {err}"))?;
            if status != 200 {
                return Err(format!("the object store answered {status} to a listing"));
            }
            for contents in elements(&body, "Contents") {
                let key = elements(&contents, "Key")
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let size = elements(&contents, "Size")
                    .into_iter()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                found.push((key, size));
            }
            let truncated = elements(&body, "IsTruncated")
                .first()
                .is_some_and(|t| t == "true");
            token = elements(&body, "NextContinuationToken").into_iter().next();
            if !truncated || token.is_none() {
                return Ok(found);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's own examples ("Signature Calculations for the Authorization Header", "Authenticating
    /// Requests: Using Query Parameters"), with their published signatures.
    fn example() -> Bucket {
        Bucket {
            endpoint: "https://examplebucket.s3.amazonaws.com".into(),
            bucket: String::new(),
            region: "us-east-1".into(),
            key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            public_endpoint: None,
            http: reqwest::Client::new(),
        }
    }

    fn may_24_2013() -> OffsetDateTime {
        time::macros::datetime!(2013-05-24 00:00:00 UTC)
    }

    #[test]
    fn a_get_is_signed_as_aws_documents_it() {
        let (authorization, amz_date) = example().authorization(
            "GET",
            "/test.txt",
            "",
            &[("range", "bytes=0-9".into())],
            EMPTY_SHA256,
            may_24_2013(),
        );
        assert_eq!(amz_date, "20130524T000000Z");
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn a_presigned_url_is_signed_as_aws_documents_it() {
        // The example presigns on the bucket's own host: no bucket segment in the path.
        let url = example().presign("GET", "test.txt", 86_400, may_24_2013());
        assert_eq!(
            url,
            "https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
        let path_style = Bucket {
            endpoint: "http://rustfs:9000".into(),
            bucket: "apps".into(),
            ..example()
        };
        assert!(path_style
            .presign("PUT", "s1/a1/x.png", 300, may_24_2013())
            .starts_with("http://rustfs:9000/apps/s1/a1/x.png?"));
    }

    #[test]
    fn a_presigned_url_names_the_address_a_browser_reaches() {
        let internal = Bucket {
            endpoint: "http://rustfs.store:9000".into(),
            bucket: "apps".into(),
            ..example()
        };
        let public = Bucket {
            public_endpoint: Some("https://files.dev.example".into()),
            ..internal.clone()
        };
        let at = may_24_2013();
        let (inside, outside) = (
            internal.presign("GET", "k", 60, at),
            public.presign("GET", "k", 60, at),
        );
        assert!(
            inside.starts_with("http://rustfs.store:9000/apps/k?"),
            "{inside}"
        );
        assert!(
            outside.starts_with("https://files.dev.example/apps/k?"),
            "{outside}"
        );
        let signature = |url: &str| {
            url.rsplit_once("X-Amz-Signature=")
                .map(|(_, s)| s.to_owned())
        };
        assert_ne!(
            signature(&inside),
            signature(&outside),
            "signed for the host the browser sends"
        );
    }

    #[test]
    fn a_key_is_encoded_as_sigv4_wants_it() {
        assert_eq!(encode("a b/c+d~é", true), "a%20b/c%2Bd~%C3%A9");
        assert_eq!(encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn a_listing_is_read_with_its_entities() {
        let xml = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>apps/s1/a/a&amp;b.txt</Key><Size>3</Size></Contents>\
                   <Contents><Key>apps/s1/a/&#x3C;x&#62;</Key><Size>10</Size></Contents></ListBucketResult>";
        let keys: Vec<String> = elements(xml, "Contents")
            .iter()
            .flat_map(|c| elements(c, "Key"))
            .collect();
        assert_eq!(keys, ["apps/s1/a/a&b.txt", "apps/s1/a/<x>"]);
        assert_eq!(unescape("a &unknown; b &"), "a &unknown; b &");
    }

    #[test]
    fn the_secret_never_reaches_a_log() {
        assert!(!format!("{:?}", example()).contains("EXAMPLEKEY"));
    }
}
