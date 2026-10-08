//! `jc:app/blob` on RustFS (ADR-N-044 §2.4, §2.5, AP-145, AP-146). Every key is the App's own,
//! under `apps/<shard>/<id>/`; a key that would leave that prefix is refused here, and the shard's
//! key may not reach another shard's prefix there. A presigned URL names one key and one method
//! and lives at most five minutes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use crate::placement::Placed;
use crate::s3::Bucket;
use crate::storage::{BlobError, Method};

/// The longest a presigned URL lives (AP-145).
pub const MAX_PRESIGN_SECONDS: u32 = 300;

/// The longest key an App may name, below its prefix.
pub const MAX_KEY_BYTES: usize = 512;

/// An App's key, checked: relative, no `.` or `..` segment, no empty segment, no backslash or
/// control character, at most 512 bytes. Nothing is rewritten: a key either stays inside the
/// prefix as written or is refused.
pub fn checked(key: &str) -> Result<&str, String> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(format!("a key is 1 to {MAX_KEY_BYTES} bytes"));
    }
    if key.starts_with('/') {
        return Err("a key is relative to the application's own prefix".into());
    }
    if key.chars().any(|c| c.is_control() || c == '\\') {
        return Err("a key holds no control character or backslash".into());
    }
    if key
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err("a key has no empty, `.` or `..` segment".into());
    }
    Ok(key)
}

/// A listing prefix: empty (everything), or a checked key, or one ending in `/`.
fn checked_prefix(prefix: &str) -> Result<&str, String> {
    if prefix.is_empty() {
        return Ok(prefix);
    }
    checked(prefix.strip_suffix('/').unwrap_or(prefix)).map(|_| prefix)
}

pub struct S3Blob {
    bucket: Bucket,
    shard: String,
    quota_bytes: u64,
    sizes: Mutex<HashMap<String, (Instant, u64)>>,
}

fn refused(app: &Placed, why: String) -> BlobError {
    tracing::warn!(app = %app.id, kind = "blob", %why, "refused");
    BlobError::Refused(why)
}

impl S3Blob {
    pub fn new(bucket: Bucket, shard: &str, quota_bytes: u64) -> Self {
        Self {
            bucket,
            shard: shard.to_owned(),
            quota_bytes,
            sizes: Mutex::new(HashMap::new()),
        }
    }

    /// `apps/<shard>/<id>/`.
    pub fn prefix(&self, app: &Placed) -> String {
        format!("apps/{}/{}/", self.shard, app.id)
    }

    fn key(&self, app: &Placed, key: &str) -> Result<String, BlobError> {
        let key = checked(key).map_err(|why| refused(app, why))?;
        Ok(format!("{}{key}", self.prefix(app)))
    }

    /// The bytes the App's prefix holds, listed at most every 30 seconds.
    async fn used(&self, app: &Placed) -> Result<u64, BlobError> {
        if let Some((at, bytes)) = self.sizes.lock().ok().and_then(|s| s.get(&app.id).copied()) {
            if at.elapsed() < Duration::from_secs(30) {
                return Ok(bytes);
            }
        }
        let bytes = self
            .bucket
            .list(&self.prefix(app))
            .await
            .map_err(BlobError::Unavailable)?
            .iter()
            .map(|(_, size)| size)
            .sum();
        if let Ok(mut sizes) = self.sizes.lock() {
            sizes.insert(app.id.clone(), (Instant::now(), bytes));
        }
        Ok(bytes)
    }

    fn answer(status: u16) -> Result<(), BlobError> {
        match status {
            200..=299 => Ok(()),
            404 => Err(BlobError::NotFound),
            403 => Err(BlobError::Refused(
                "the object store refused the key".into(),
            )),
            other => Err(BlobError::Unavailable(format!(
                "the object store answered {other}"
            ))),
        }
    }

    pub async fn get(&self, app: &Placed, key: &str) -> Result<Vec<u8>, BlobError> {
        let key = self.key(app, key)?;
        let (status, bytes) = self
            .bucket
            .object(reqwest::Method::GET, &key, None)
            .await
            .map_err(BlobError::Unavailable)?;
        Self::answer(status).map(|()| bytes)
    }

    pub async fn put(
        &self,
        app: &Placed,
        key: &str,
        data: Vec<u8>,
        content_type: Option<String>,
    ) -> Result<(), BlobError> {
        let key = self.key(app, key)?;
        if self.used(app).await? + data.len() as u64 > self.quota_bytes {
            tracing::warn!(app = %app.id, kind = "blob", "refused: quota");
            return Err(BlobError::Quota(format!(
                "the application's files would pass {} bytes, its quota",
                self.quota_bytes
            )));
        }
        let size = data.len() as u64;
        let (status, _) = self
            .bucket
            .object(reqwest::Method::PUT, &key, Some((data, content_type)))
            .await
            .map_err(BlobError::Unavailable)?;
        Self::answer(status)?;
        if let Ok(mut sizes) = self.sizes.lock() {
            if let Some((_, used)) = sizes.get_mut(&app.id) {
                *used += size;
            }
        }
        Ok(())
    }

    pub async fn list(&self, app: &Placed, prefix: &str) -> Result<Vec<String>, BlobError> {
        let prefix = checked_prefix(prefix).map_err(|why| refused(app, why))?;
        let own = self.prefix(app);
        let keys = self
            .bucket
            .list(&format!("{own}{prefix}"))
            .await
            .map_err(BlobError::Unavailable)?;
        Ok(keys
            .into_iter()
            .filter_map(|(key, _)| key.strip_prefix(&own).map(str::to_owned))
            .collect())
    }

    pub async fn delete(&self, app: &Placed, key: &str) -> Result<(), BlobError> {
        let key = self.key(app, key)?;
        let (status, _) = self
            .bucket
            .object(reqwest::Method::DELETE, &key, None)
            .await
            .map_err(BlobError::Unavailable)?;
        if let Ok(mut sizes) = self.sizes.lock() {
            sizes.remove(&app.id);
        }
        Self::answer(status)
    }

    /// A URL for one key and one method, for at most [`MAX_PRESIGN_SECONDS`].
    pub fn presign(
        &self,
        app: &Placed,
        key: &str,
        method: Method,
        expires: u32,
    ) -> Result<String, BlobError> {
        let key = self.key(app, key)?;
        let method = match method {
            Method::Get => "GET",
            Method::Put => "PUT",
        };
        Ok(self.bucket.presign(
            method,
            &key,
            expires.clamp(1, MAX_PRESIGN_SECONDS),
            OffsetDateTime::now_utc(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_stays_under_the_prefix_or_is_refused() {
        for key in [
            "notes/1.txt",
            "a",
            "photos/2026/x.png",
            "a b/ü.txt",
            "...dots",
            "x..",
        ] {
            assert_eq!(checked(key), Ok(key), "{key}");
        }
        for key in [
            "",
            "/etc/passwd",
            "../b/x",
            "a/../../s2/b/x",
            "a/./b",
            "a//b",
            "a/",
            "a\\..\\b",
            "a\u{0}b",
            "a\nb",
            ".",
            "..",
        ] {
            assert!(checked(key).is_err(), "{key:?}");
        }
        assert!(checked(&"k".repeat(MAX_KEY_BYTES + 1)).is_err());
        assert_eq!(checked_prefix(""), Ok(""));
        assert_eq!(checked_prefix("notes/"), Ok("notes/"));
        assert!(checked_prefix("../").is_err());
    }
}
