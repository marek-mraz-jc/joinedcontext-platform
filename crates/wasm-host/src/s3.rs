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
        self.endpoint
            .split_once("://")
            .map_or(self.endpoint.as_str(), |(_, rest)| rest)
            .trim_end_matches('/')
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
        let canonical = format!(
            "{method}\n{path}\n{canonical_query}\nhost:{}\n\nhost\nUNSIGNED-PAYLOAD",
            self.host()
        );
        let (signature, _) = sign(&self.secret, &self.region, &amz_date, &date, &canonical);
        format!(
            "{}{path}?{canonical_query}&X-Amz-Signature={signature}",
            self.endpoint.trim_end_matches('/')
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
    fn a_key_is_encoded_as_sigv4_wants_it() {
        assert_eq!(encode("a b/c+d~é", true), "a%20b/c%2Bd~%C3%A9");
        assert_eq!(encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn the_secret_never_reaches_a_log() {
        assert!(!format!("{:?}", example()).contains("EXAMPLEKEY"));
    }
}
