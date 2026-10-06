//! Passage and question embeddings (T-3053, ADR-N-040 §3.1): `multilingual-e5-small`, the int8
//! ONNX export, through fastembed as a user-defined model.
//!
//! The files come from a directory (`scripts/ci/e5-small.sh` fills it, the image carries one)
//! and each is checked against its SHA-256 here before it is loaded, so the service embeds with
//! the model the eval measured or refuses to start. e5 reads `passage: ` before a text it indexes
//! and `query: ` before a question; without the prefixes its recall drops.

use std::path::Path;
use std::sync::{Arc, Mutex};

use fastembed::{
    InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::{vector_literal, Error, DIMENSIONS};

/// The model's files and their SHA-256, as `scripts/ci/e5-small.sh` pins them
/// (`intfloat/multilingual-e5-small` at 614241f6).
pub const MODEL_FILES: [(&str, &str); 5] = [
    (
        "model_qint8_avx512_vnni.onnx",
        "dd476dd0c2514e9b9be83aeb3853fac0763e0bdf4a71645407587d77c48a2d88",
    ),
    (
        "tokenizer.json",
        "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    ),
    (
        "config.json",
        "bbb7c1333fc4b3e27fbc9cd5d2070aabcc1d4dfb99917c3633e772f97545a6b6",
    ),
    (
        "special_tokens_map.json",
        "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7",
    ),
    (
        "tokenizer_config.json",
        "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
    ),
];

/// The longest input the model reads, in tokens; a passage of [`crate::extract::CHUNK_CHARS`]
/// fits.
const MAX_TOKENS: usize = 512;

/// How many passages go through the model in one call: memory grows with the batch, and a crawl
/// is not in a hurry.
pub const BATCH: usize = 16;

/// The loaded model. Cloning shares it; one embedding runs at a time, on `threads` ONNX Runtime
/// threads, off the async runtime.
#[derive(Clone)]
pub struct Embedder {
    model: Arc<Mutex<TextEmbedding>>,
    turn: Arc<Semaphore>,
}

fn read_checked(dir: &Path, file: &str, sha: &str) -> Result<Vec<u8>, Error> {
    let path = dir.join(file);
    let bytes = std::fs::read(&path)
        .map_err(|err| Error::Model(format!("{} could not be read: {err}", path.display())))?;
    let found = hex::encode(Sha256::digest(&bytes));
    if found != sha {
        return Err(Error::Model(format!(
            "{} is not the pinned file (SHA-256 {found}, expected {sha}); fetch it again with scripts/ci/e5-small.sh",
            path.display()
        )));
    }
    Ok(bytes)
}

impl Embedder {
    /// Loads the model from `dir` after checking every file's hash.
    pub fn load(dir: &Path, threads: usize) -> Result<Self, Error> {
        let [onnx, tokenizer, config, special, tokenizer_config] =
            MODEL_FILES.map(|(file, sha)| read_checked(dir, file, sha));
        let model = UserDefinedEmbeddingModel::new(
            onnx?,
            TokenizerFiles {
                tokenizer_file: tokenizer?,
                config_file: config?,
                special_tokens_map_file: special?,
                tokenizer_config_file: tokenizer_config?,
            },
        )
        .with_pooling(Pooling::Mean)
        .with_quantization(QuantizationMode::Dynamic);
        let options = InitOptionsUserDefined::new()
            .with_max_length(MAX_TOKENS)
            .with_intra_threads(threads.max(1));
        let model = TextEmbedding::try_new_from_user_defined(model, options)
            .map_err(|err| Error::Model(format!("the model did not load: {err}")))?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            turn: Arc::new(Semaphore::new(1)),
        })
    }

    async fn run(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, Error> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let _turn = self
            .turn
            .acquire()
            .await
            .map_err(|_| Error::Model("the embedder was shut down".into()))?;
        let model = Arc::clone(&self.model);
        let count = texts.len();
        let vectors = tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|_| Error::Model("an earlier embedding panicked".into()))?;
            model
                .embed(&texts, Some(BATCH))
                .map_err(|err| Error::Model(format!("embedding failed: {err}")))
        })
        .await
        .map_err(|err| Error::Model(format!("embedding failed: {err}")))??;
        if vectors.len() != count || vectors.iter().any(|v| v.len() != DIMENSIONS) {
            return Err(Error::Model(format!(
                "the model answered {} vectors for {count} texts, not {DIMENSIONS} numbers each",
                vectors.len()
            )));
        }
        Ok(vectors)
    }

    /// The embeddings of passages to index, in order.
    pub async fn passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        self.run(texts.iter().map(|t| format!("passage: {t}")).collect())
            .await
    }

    /// The embedding of a question.
    pub async fn query(&self, text: &str) -> Result<Vec<f32>, Error> {
        let mut vectors = self.run(vec![format!("query: {text}")]).await?;
        vectors
            .pop()
            .ok_or_else(|| Error::Model("the model answered no vector".into()))
    }
}

/// Embeds up to `limit` chunks of `project` that have none yet, [`BATCH`] at a time, and
/// returns how many it embedded. A chunk gets one when it is stored or replaced (its page or
/// document changed), so this is the whole of "re-embed on change"; a batch the model fails on
/// stays without and is tried again on the next call.
pub async fn embed_missing(
    pool: &PgPool,
    embedder: &Embedder,
    project: &str,
    limit: i64,
) -> Result<usize, Error> {
    let mut done = 0;
    while (done as i64) < limit {
        let mut tx = crate::project_scope(pool, project).await?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, text FROM chunks WHERE embedding IS NULL ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED",
        )
        .bind((limit - done as i64).min(BATCH as i64))
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            tx.rollback().await?;
            break;
        }
        let (ids, texts): (Vec<i64>, Vec<String>) = rows.into_iter().unzip();
        let vectors = embedder.passages(&texts).await?;
        for (id, vector) in ids.iter().zip(&vectors) {
            sqlx::query("UPDATE chunks SET embedding = $2::vector WHERE id = $1")
                .bind(id)
                .bind(vector_literal(vector)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        done += ids.len();
    }
    Ok(done)
}
