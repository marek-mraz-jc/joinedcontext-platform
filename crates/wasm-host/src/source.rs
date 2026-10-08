//! Where a shard fetches a component (ADR-N-044 §2.1): the bucket's `components/` by content
//! digest, or a directory for a single machine and the tests. The host checks the bytes against
//! the digest before it compiles them; this module only fetches.

use std::path::PathBuf;

use crate::placement::is_digest;
use crate::s3::Bucket;

/// The largest component a shard fetches.
pub const MAX_COMPONENT_BYTES: usize = 64 << 20;

pub enum Source {
    /// `<dir>/<64 hex>.wasm`.
    Dir(PathBuf),
    /// `components/<64 hex>.wasm` in the bucket, read with a key that may read and never write.
    Store(Bucket),
}

impl Source {
    /// The bytes recorded under `digest`, or why there are none.
    pub async fn fetch(&self, digest: &str) -> Result<Vec<u8>, String> {
        if !is_digest(digest) {
            return Err("not a sha256 digest".into());
        }
        let hex = &digest["sha256:".len()..];
        let bytes = match self {
            Self::Dir(dir) => tokio::fs::read(dir.join(format!("{hex}.wasm")))
                .await
                .map_err(|err| format!("component {hex}: {err}"))?,
            Self::Store(bucket) => match bucket
                .object(
                    reqwest::Method::GET,
                    &format!("components/{hex}.wasm"),
                    None,
                )
                .await?
            {
                (200, bytes) => bytes,
                (status, _) => {
                    return Err(format!(
                        "component {hex}: the object store answered {status}"
                    ))
                }
            },
        };
        if bytes.len() > MAX_COMPONENT_BYTES {
            return Err(format!(
                "component {hex} is larger than {MAX_COMPONENT_BYTES} bytes"
            ));
        }
        Ok(bytes)
    }
}
