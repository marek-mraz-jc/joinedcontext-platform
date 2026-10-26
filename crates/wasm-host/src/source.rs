//! Where a shard fetches a component (ADR-N-044 §2.1): the bucket's `components/` by content
//! digest, or a directory for a single machine and the tests. The host checks the bytes against
//! the digest before it compiles them; this module only fetches.

use std::path::PathBuf;

use crate::placement::is_digest;
use crate::s3::Bucket;

/// Where the Portal stores a component and a shard fetches it: `components/sha256-<64 hex>.wasm`
/// (AP-151).
pub fn component_key(hex: &str) -> String {
    format!("components/sha256-{hex}.wasm")
}

/// The largest component a shard fetches.
pub const MAX_COMPONENT_BYTES: usize = 64 << 20;

pub enum Source {
    /// `<dir>/sha256-<64 hex>.wasm`.
    Dir(PathBuf),
    /// [`component_key`] in the bucket, read with a key that may read and never write.
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
            Self::Dir(dir) => tokio::fs::read(dir.join(format!("sha256-{hex}.wasm")))
                .await
                .map_err(|err| format!("component {hex}: {err}"))?,
            Self::Store(bucket) => match bucket
                .object(reqwest::Method::GET, &component_key(hex), None)
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

#[cfg(test)]
mod tests {
    #[test]
    fn a_component_is_kept_where_ap_151_says() {
        assert_eq!(super::component_key("ab12"), "components/sha256-ab12.wasm");
    }

    #[tokio::test]
    async fn a_directory_source_fetches_only_a_digest_it_holds() {
        let dir = std::env::temp_dir().join(format!("wasm-host-source-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.expect("dir");
        let hex = "a".repeat(64);
        tokio::fs::write(dir.join(format!("sha256-{hex}.wasm")), b"\0asm")
            .await
            .expect("write");
        let source = super::Source::Dir(dir.clone());
        assert_eq!(
            source.fetch(&format!("sha256:{hex}")).await,
            Ok(b"\0asm".to_vec())
        );
        assert_eq!(
            source.fetch("sha256:ab12").await,
            Err("not a sha256 digest".to_owned())
        );
        let missing = source.fetch(&format!("sha256:{}", "b".repeat(64))).await;
        assert!(missing.is_err_and(|why| why.starts_with(&format!("component {}", "b".repeat(64)))));
        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }
}
