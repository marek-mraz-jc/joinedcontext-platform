//! Passage and question embeddings (T-3053, ADR-N-040 §3.1): `multilingual-e5-small`, the int8
//! ONNX export, run with ONNX Runtime and its own tokenizer, mean-pooled and normalized.
//!
//! The files come from a directory (`scripts/ci/e5-small.sh` fills it, the image carries one)
//! and each is checked against its SHA-256 here before it is loaded, so the service embeds with
//! the model the eval measured or refuses to start. e5 reads `passage: ` before a text it indexes
//! and `query: ` before a question; without the prefixes its recall drops.
//!
//! fastembed wraps the same two libraries, but it keeps a second copy of the 250,000-piece
//! tokenizer while it configures it, and the loaded service sat at 713 MB against 433 MB driven
//! directly (T-3053), most of the 1 GB the whole service has (ADR-N-040 §3.8).

use std::path::Path;
use std::sync::{Arc, Mutex};

use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokenizers::{Tokenizer, TruncationParams};
use tokio::sync::Semaphore;

use crate::{vector_literal, Error, DIMENSIONS};

/// The model's files and their SHA-256, as `scripts/ci/e5-small.sh` pins them
/// (`intfloat/multilingual-e5-small` at 614241f6): the int8 export and its tokenizer.
pub const MODEL_FILES: [(&str, &str); 2] = [
    (
        "model_qint8_avx512_vnni.onnx",
        "dd476dd0c2514e9b9be83aeb3853fac0763e0bdf4a71645407587d77c48a2d88",
    ),
    (
        "tokenizer.json",
        "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    ),
];

/// The longest input the model reads, in tokens; a passage of [`crate::extract::CHUNK_CHARS`]
/// fits.
const MAX_TOKENS: usize = 512;

/// How many passages one round of [`embed_missing`] takes from the store and writes back. The
/// model reads them one at a time: the int8 export quantizes its activations with a scale taken
/// from the whole input, so a passage embedded beside others would get a vector that depends on
/// its neighbours, and a question (always alone) would be measured against it.
pub const BATCH: usize = 16;

struct Model {
    tokenizer: Tokenizer,
    session: Session,
}

/// The loaded model. Cloning shares it; one embedding runs at a time, on `threads` ONNX Runtime
/// threads, off the async runtime.
#[derive(Clone)]
pub struct Embedder {
    model: Arc<Mutex<Model>>,
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

fn loading(what: &str) -> impl Fn(String) -> Error + '_ {
    move |why| Error::Model(format!("the {what} did not load: {why}"))
}

impl Model {
    /// The mean of the last hidden state over the text's tokens, normalized to length one.
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, Error> {
        let failed = |why: String| Error::Model(format!("embedding failed: {why}"));
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|err| failed(err.to_string()))?;
        let tokens = encoding.get_ids().len();
        if tokens == 0 {
            return Err(failed("the text has no tokens".into()));
        }
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| i64::from(id)).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&bit| i64::from(bit))
            .collect();
        let tensor = |values: Vec<i64>| {
            Tensor::from_array(([1usize, tokens], values)).map_err(|err| failed(err.to_string()))
        };
        let outputs = self
            .session
            .run(ort::inputs![
                "input_ids" => tensor(ids)?,
                "attention_mask" => tensor(mask)?,
                "token_type_ids" => tensor(vec![0; tokens])?,
            ])
            .map_err(|err| failed(err.to_string()))?;
        let hidden = outputs
            .get("last_hidden_state")
            .ok_or_else(|| failed("the model has no last_hidden_state output".into()))?;
        let (shape, values) = hidden
            .try_extract_tensor::<f32>()
            .map_err(|err| failed(err.to_string()))?;
        if shape.len() != 3 || values.len() != tokens * DIMENSIONS {
            return Err(failed(format!(
                "the model answered a {shape:?} tensor for {tokens} tokens"
            )));
        }
        // No padding (one text at a time), so every token counts.
        let mut pooled = vec![0f32; DIMENSIONS];
        for token in values.as_chunks::<DIMENSIONS>().0 {
            for (sum, value) in pooled.iter_mut().zip(token) {
                *sum += value;
            }
        }
        let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
        Ok(pooled.into_iter().map(|v| v / norm).collect())
    }
}

impl Embedder {
    /// Loads the model from `dir` after checking every file's hash.
    pub fn load(dir: &Path, threads: usize) -> Result<Self, Error> {
        let [(onnx_file, onnx_sha), (tokenizer_file, tokenizer_sha)] = MODEL_FILES;
        let mut tokenizer =
            Tokenizer::from_bytes(read_checked(dir, tokenizer_file, tokenizer_sha)?)
                .map_err(|err| loading("tokenizer")(err.to_string()))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..TruncationParams::default()
            }))
            .map_err(|err| loading("tokenizer")(err.to_string()))?;
        tokenizer.with_padding(None);
        let onnx = read_checked(dir, onnx_file, onnx_sha)?;
        let session = Session::builder()
            .map_err(|err| loading("model")(err.to_string()))?
            .with_intra_threads(threads.max(1))
            .map_err(|err| loading("model")(err.to_string()))?
            .commit_from_memory(&onnx)
            .map_err(|err| loading("model")(err.to_string()))?;
        Ok(Self {
            model: Arc::new(Mutex::new(Model { tokenizer, session })),
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
        tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|_| Error::Model("an earlier embedding panicked".into()))?;
            texts.iter().map(|text| model.embed(text)).collect()
        })
        .await
        .map_err(|err| Error::Model(format!("embedding failed: {err}")))?
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

/// Embeds up to `limit` chunks of `project` that have none yet, [`BATCH`] per round, and
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
