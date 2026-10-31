//! An App's file links on the App's own host (T-6195, PF-32, AP-145). The store has no address in
//! the open: a presigned URL names `https://{name}.{apps host}/api/_jc/files/{key}`, which the edge
//! routes to the App's shard like the rest of `/api/*`, and the shard streams the object from or
//! into the App's prefix. A link names one App, one method, one key, its expiry and, for a sized
//! PUT, its length, signed with a key derived from the shard's store secret: every replica of the
//! shard checks it, and no other App's link and no altered one passes.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::placement::Placed;

/// The path below `/apps/{name}/api/` the shard keeps for file links; an App never sees it.
pub const PREFIX: &str = "_jc/files/";

/// The largest object a file link moves, as the Portal's files service holds it (AP-170). A PUT
/// is read whole before it is stored, so this bounds what one upload holds in the shard.
pub const MAX_FILE_BYTES: u64 = 25 << 20;

/// What a link was signed for, as the shard checked it.
#[derive(Debug, PartialEq, Eq)]
pub struct Signed {
    /// The key below the App's prefix.
    pub key: String,
    /// The exact body length of a sized PUT.
    pub length: Option<u64>,
}

/// The signer of file links.
#[derive(Clone)]
pub struct FileUrls {
    apps_host: String,
    key: Vec<u8>,
}

impl std::fmt::Debug for FileUrls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileUrls")
            .field("apps_host", &self.apps_host)
            .finish_non_exhaustive()
    }
}

fn mac(key: &[u8]) -> Hmac<Sha256> {
    <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length")
}

impl FileUrls {
    /// `apps_host` is the domain the Apps' hosts sit under (`apps.dev.example`, no scheme): a
    /// host name and nothing else. `secret` is the shard's store secret, which every replica of
    /// the shard mounts; the link key is derived from it and never leaves the process.
    pub fn new(apps_host: &str, secret: &str) -> Result<Self, String> {
        let host = apps_host.trim().trim_end_matches('.');
        let valid = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        if !valid {
            return Err(format!(
                "JC_WASM_APPS_HOST is a host name such as apps.example.org, not '{apps_host}'"
            ));
        }
        if secret.is_empty() {
            return Err("the store secret is empty".into());
        }
        let mut derive = mac(secret.as_bytes());
        derive.update(b"jc-wasm-host file links v1");
        Ok(Self {
            apps_host: host.to_ascii_lowercase(),
            key: derive.finalize().into_bytes().to_vec(),
        })
    }

    fn signature(
        &self,
        app: &Placed,
        method: &str,
        key: &str,
        expires: i64,
        length: Option<u64>,
    ) -> Hmac<Sha256> {
        let mut signature = mac(&self.key);
        let length = length.map(|n| n.to_string()).unwrap_or_default();
        signature.update(
            format!(
                "{}\n{}\n{method}\n{key}\n{expires}\n{length}",
                app.id, app.name
            )
            .as_bytes(),
        );
        signature
    }

    /// The link for `method` on `key` (below the App's prefix, already checked) until the Unix
    /// second `expires`.
    pub fn url(
        &self,
        app: &Placed,
        method: &str,
        key: &str,
        expires: i64,
        length: Option<u64>,
    ) -> String {
        let signature = hex::encode(
            self.signature(app, method, key, expires, length)
                .finalize()
                .into_bytes(),
        );
        let length = length.map(|n| format!("&length={n}")).unwrap_or_default();
        format!(
            "https://{}.{}/api/{PREFIX}{}?expires={expires}{length}&signature={signature}",
            app.name,
            self.apps_host,
            crate::s3::encode(key, true)
        )
    }

    /// The key and length a request's link was signed for, or why it is refused: `path` is the
    /// encoded key after [`PREFIX`], `query` the request's query, `now` the Unix second.
    pub fn verify(
        &self,
        app: &Placed,
        method: &str,
        path: &str,
        query: &str,
        now: i64,
    ) -> Result<Signed, &'static str> {
        const INVALID: &str = "this file link is not valid; ask the application for a new one";
        let key = percent_encoding::percent_decode_str(path)
            .decode_utf8()
            .map_err(|_| INVALID)?;
        let key = crate::blob::checked(&key).map_err(|_| INVALID)?.to_owned();
        let (mut expires, mut length, mut signature) = (None, None, None);
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (name, value) = pair.split_once('=').ok_or(INVALID)?;
            let slot = match name {
                "expires" => &mut expires,
                "length" => &mut length,
                "signature" => &mut signature,
                _ => return Err(INVALID),
            };
            if slot.replace(value).is_some() {
                return Err(INVALID);
            }
        }
        let expires: i64 = expires.ok_or(INVALID)?.parse().map_err(|_| INVALID)?;
        let length = length
            .map(|n| n.parse::<u64>().map_err(|_| INVALID))
            .transpose()?;
        let signature = hex::decode(signature.ok_or(INVALID)?).map_err(|_| INVALID)?;
        self.signature(app, method, &key, expires, length)
            .verify_slice(&signature)
            .map_err(|_| INVALID)?;
        if now > expires {
            return Err("this file link has expired; ask the application for a new one");
        }
        Ok(Signed { key, length })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, id: &str) -> Placed {
        serde_json::from_value(serde_json::json!({
            "name": name, "id": id, "tenant": "p", "digest": format!("sha256:{}", "0".repeat(64)),
        }))
        .expect("placed")
    }

    fn split(url: &str) -> (String, String) {
        let rest = url
            .strip_prefix("https://reach.apps.dev.example/api/_jc/files/")
            .expect("on the App's host");
        let (path, query) = rest.split_once('?').expect("query");
        (path.to_owned(), query.to_owned())
    }

    #[test]
    fn a_link_names_the_apps_own_host_and_opens_until_it_expires() {
        let urls = FileUrls::new("Apps.Dev.Example", "secret").expect("urls");
        let reach = app("reach", "a1");
        let url = urls.url(&reach, "GET", "tiles/a b.geojson", 1_000, None);
        assert!(!url.contains("svc.cluster.local"), "{url}");
        let (path, query) = split(&url);
        assert_eq!(path, "tiles/a%20b.geojson");
        assert_eq!(
            urls.verify(&reach, "GET", &path, &query, 1_000),
            Ok(Signed {
                key: "tiles/a b.geojson".into(),
                length: None
            })
        );
        assert!(urls
            .verify(&reach, "GET", &path, &query, 1_001)
            .unwrap_err()
            .contains("expired"));
    }

    #[test]
    fn a_link_opens_for_its_own_app_method_key_and_length_alone() {
        let urls = FileUrls::new("apps.dev.example", "secret").expect("urls");
        let reach = app("reach", "a1");
        let (path, query) = split(&urls.url(&reach, "PUT", "up.png", 1_000, Some(4)));
        assert_eq!(
            urls.verify(&reach, "PUT", &path, &query, 0),
            Ok(Signed {
                key: "up.png".into(),
                length: Some(4)
            })
        );
        let refused = |app: &Placed, method: &str, path: &str, query: &str| {
            urls.verify(app, method, path, query, 0).is_err()
        };
        assert!(refused(&reach, "GET", &path, &query), "another method");
        assert!(
            refused(&app("reach", "a2"), "PUT", &path, &query),
            "another App id"
        );
        assert!(
            refused(&app("other", "a1"), "PUT", &path, &query),
            "another name"
        );
        assert!(refused(&reach, "PUT", "up2.png", &query), "another key");
        let longer = query.replace("length=4", "length=5");
        assert!(refused(&reach, "PUT", &path, &longer), "another length");
        let unsized_ = query.replace("&length=4", "");
        assert!(
            refused(&reach, "PUT", &path, &unsized_),
            "the length dropped"
        );
        let later = query.replace("expires=1000", "expires=9000");
        assert!(refused(&reach, "PUT", &path, &later), "a later expiry");
        let twice = format!("{query}&length=4");
        assert!(refused(&reach, "PUT", &path, &twice), "a parameter twice");
        let extra = format!("{query}&x=1");
        assert!(
            refused(&reach, "PUT", &path, &extra),
            "an unknown parameter"
        );
        assert!(
            refused(&reach, "PUT", "..%2Fx", &query),
            "a key leaving the prefix"
        );
        let other = FileUrls::new("apps.dev.example", "other secret").expect("urls");
        assert!(
            other.verify(&reach, "PUT", &path, &query, 0).is_err(),
            "another shard key"
        );
    }

    #[test]
    fn the_apps_host_is_a_host_name_and_nothing_more() {
        for bad in [
            "",
            "https://apps.example",
            "apps.example/x",
            "apps.example:443",
            "a..b",
            "-a.b",
        ] {
            assert!(FileUrls::new(bad, "s").is_err(), "{bad}");
        }
        assert!(FileUrls::new("apps.example", "").is_err());
        assert!(!format!(
            "{:?}",
            FileUrls::new("apps.example", "s3cr3t").expect("urls")
        )
        .contains("key"));
    }
}
