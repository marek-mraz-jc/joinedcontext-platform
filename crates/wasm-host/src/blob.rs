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
    read_limit: usize,
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
            read_limit: crate::limits::Limits::default().response_bytes,
            sizes: Mutex::new(HashMap::new()),
        }
    }

    /// The most one `get` reads from the store; the default is the largest answer a guest gives.
    pub fn with_read_limit(mut self, bytes: usize) -> Self {
        self.read_limit = bytes;
        self
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

    /// One object, read only up to the read limit: a presigned PUT can leave an object of any
    /// size under the prefix, and the shard never holds more than one answer's worth of it.
    pub async fn get(&self, app: &Placed, key: &str) -> Result<Vec<u8>, BlobError> {
        let key = self.key(app, key)?;
        let Some((status, bytes)) = self
            .bucket
            .get_capped(&key, self.read_limit)
            .await
            .map_err(BlobError::Unavailable)?
        else {
            return Err(refused(
                app,
                format!(
                    "the object passes {} bytes, the read limit",
                    self.read_limit
                ),
            ));
        };
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
        let size = data.len() as u64;
        self.room_for(app, size).await?;
        let (status, _) = self
            .bucket
            .object(reqwest::Method::PUT, &key, Some((data, content_type)))
            .await
            .map_err(BlobError::Unavailable)?;
        Self::answer(status)?;
        self.count(app, size);
        Ok(())
    }

    /// `size` more bytes in the cached use until the next listing.
    fn count(&self, app: &Placed, size: u64) {
        if let Ok(mut sizes) = self.sizes.lock() {
            if let Some((_, used)) = sizes.get_mut(&app.id) {
                *used = used.saturating_add(size);
            }
        }
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

    /// A URL for one key and one method, for at most [`MAX_PRESIGN_SECONDS`]. A PUT is refused
    /// once the application's files are at their quota, as [`Self::put`] refuses a write.
    /// ponytail: this unsized PUT (`jc:app@0.1.0` `presign`) signs no length, so one upload below
    /// the quota can still pass it; [`Self::presign_put`] signs one, kept for published components.
    pub async fn presign(
        &self,
        app: &Placed,
        key: &str,
        method: Method,
        expires: u32,
    ) -> Result<String, BlobError> {
        let key = self.key(app, key)?;
        let method = match method {
            Method::Get => "GET",
            Method::Put => {
                // At least one byte fits: refused once the files are at their quota.
                self.room_for(app, 1).await?;
                "PUT"
            }
        };
        Ok(self.signed(method, &key, expires, None))
    }

    /// A PUT URL for one key whose body is exactly `size` bytes: `content-length` is signed, so
    /// the store refuses any other length, and the URL is refused when the application's files
    /// plus `size` would pass its quota (`jc:app@0.1.2` `presign-put`, AP-145).
    pub async fn presign_put(
        &self,
        app: &Placed,
        key: &str,
        size: u64,
        expires: u32,
    ) -> Result<String, BlobError> {
        let key = self.key(app, key)?;
        self.room_for(app, size).await?;
        // Counted when signed, so several URLs inside one cached listing cannot add up past the
        // quota; an unused one holds its room until the listing refreshes.
        self.count(app, size);
        Ok(self.signed("PUT", &key, expires, Some(size)))
    }

    /// `Quota` unless `size` more bytes fit beside the application's files.
    async fn room_for(&self, app: &Placed, size: u64) -> Result<(), BlobError> {
        if self.used(app).await?.saturating_add(size) > self.quota_bytes {
            tracing::warn!(app = %app.id, kind = "blob", "refused: quota");
            return Err(BlobError::Quota(format!(
                "the application's files would pass {} bytes, its quota",
                self.quota_bytes
            )));
        }
        Ok(())
    }

    fn signed(&self, method: &str, key: &str, expires: u32, length: Option<u64>) -> String {
        self.bucket.presign(
            method,
            key,
            expires.clamp(1, MAX_PRESIGN_SECONDS),
            length,
            OffsetDateTime::now_utc(),
        )
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
