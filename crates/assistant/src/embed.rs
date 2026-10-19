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

use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokenizers::{Tokenizer, TruncationParams};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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
/// threads, off the async runtime, and a visitor's question before any passage (`Turns`).
#[derive(Clone)]
pub struct Embedder {
    model: Arc<Mutex<Model>>,
    turns: Turns,
}

/// How long a question's embedding waited for the model and how long the model took (T-4751).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Took {
    pub waited: Duration,
    pub ran: Duration,
}

/// Whose turn the model is: one text at a time, and a question before every passage (T-4751).
/// The passages of the minute's indexing take the model one text each and give it back at once
/// when a question waits, so a question waits at most for the one passage already running, never
/// for a batch or a backlog.
#[derive(Clone)]
struct Turns {
    model: Arc<Semaphore>,
    questions: Arc<AtomicUsize>,
}

/// One waiting question, counted until it has the model or is given up.
struct Waiting(Arc<AtomicUsize>);

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Turns {
    fn new() -> Self {
        Self {
            model: Arc::new(Semaphore::new(1)),
            questions: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn take(&self) -> Result<OwnedSemaphorePermit, Error> {
        Arc::clone(&self.model)
            .acquire_owned()
            .await
            .map_err(|_| Error::Model("the embedder was shut down".into()))
    }

    /// The model for a question: queued in order with the other questions, ahead of passages.
    async fn question(&self) -> Result<OwnedSemaphorePermit, Error> {
        self.questions.fetch_add(1, Ordering::SeqCst);
        let _waiting = Waiting(Arc::clone(&self.questions));
        self.take().await
    }

    /// The model for one passage, once no question waits for it.
    async fn passage(&self) -> Result<OwnedSemaphorePermit, Error> {
        loop {
            let turn = self.take().await?;
            if self.questions.load(Ordering::SeqCst) == 0 {
                return Ok(turn);
            }
            // The waiting question is queued on the semaphore already, or about to be: handing
            // the turn back puts this passage behind it.
            drop(turn);
            tokio::task::yield_now().await;
        }
    }
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

/// The file `file` of `dir`, open, once its SHA-256 is `sha`: hashed as a stream, so the
/// model is never held whole for the check, and handed back open, so what loads is the file that
/// was hashed and not one put at the path since (T-4357).
fn open_checked(dir: &Path, file: &str, sha: &str) -> Result<std::fs::File, Error> {
    let path = dir.join(file);
    let unreadable =
        |err: std::io::Error| Error::Model(format!("{} could not be read: {err}", path.display()));
    let mut opened = std::fs::File::open(&path).map_err(unreadable)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut opened, &mut hasher).map_err(unreadable)?;
    let found = hex::encode(hasher.finalize());
    if found != sha {
        return Err(Error::Model(format!(
            "{} is not the pinned file (SHA-256 {found}, expected {sha}); fetch it again with scripts/ci/e5-small.sh",
            path.display()
        )));
    }
    Ok(opened)
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
        let onnx = open_checked(dir, onnx_file, onnx_sha)?;
        // ONNX Runtime opens the descriptor's own path: the inode that was hashed, read from
        // the file rather than from a copy of it in memory.
        let session = Session::builder()
            .map_err(|err| loading("model")(err.to_string()))?
            .with_intra_threads(threads.max(1))
            .map_err(|err| loading("model")(err.to_string()))?
            .commit_from_file(format!("/proc/self/fd/{}", onnx.as_raw_fd()))
            .map_err(|err| loading("model")(err.to_string()))?;
        drop(onnx);
        Ok(Self {
            model: Arc::new(Mutex::new(Model { tokenizer, session })),
            turns: Turns::new(),
        })
    }

    /// One text through the model, holding `turn` until it is done.
    async fn run(&self, _turn: OwnedSemaphorePermit, text: String) -> Result<Vec<f32>, Error> {
        let model = Arc::clone(&self.model);
        tokio::task::spawn_blocking(move || {
            model
                .lock()
                .map_err(|_| Error::Model("an earlier embedding panicked".into()))?
                .embed(&text)
        })
        .await
        .map_err(|err| Error::Model(format!("embedding failed: {err}")))?
    }

    /// The embeddings of passages to index, in order, one text per turn of the model.
    pub async fn passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        let mut vectors = Vec::with_capacity(texts.len());
        for text in texts {
            let turn = self.turns.passage().await?;
            vectors.push(self.run(turn, format!("passage: {text}")).await?);
        }
        Ok(vectors)
    }

    /// The embedding of a question.
    pub async fn query(&self, text: &str) -> Result<Vec<f32>, Error> {
        Ok(self.query_timed(text).await?.0)
    }

    /// The embedding of a question, and how long it waited for the model and ran on it.
    pub async fn query_timed(&self, text: &str) -> Result<(Vec<f32>, Took), Error> {
        let began = Instant::now();
        let turn = self.turns.question().await?;
        let waited = began.elapsed();
        let vector = self.run(turn, format!("query: {text}")).await?;
        Ok((
            vector,
            Took {
                waited,
                ran: began.elapsed().saturating_sub(waited),
            },
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One turn of "the model": holds it for `millis`, then records `who`.
    async fn work(
        turn: OwnedSemaphorePermit,
        millis: u64,
        done: Arc<Mutex<Vec<String>>>,
        who: String,
    ) {
        tokio::time::sleep(Duration::from_millis(millis)).await;
        done.lock().expect("lock").push(who);
        drop(turn);
    }

    #[tokio::test(start_paused = true)]
    async fn a_question_goes_before_500_queued_passages() {
        let turns = Turns::new();
        let done = Arc::new(Mutex::new(Vec::new()));
        let indexing = {
            let (turns, done) = (turns.clone(), Arc::clone(&done));
            tokio::spawn(async move {
                for i in 0..500 {
                    let turn = turns.passage().await.expect("turn");
                    work(turn, 10, Arc::clone(&done), format!("passage {i}")).await;
                }
            })
        };
        // The question comes while the third passage runs.
        tokio::time::sleep(Duration::from_millis(25)).await;
        let began = tokio::time::Instant::now();
        let turn = turns.question().await.expect("turn");
        let waited = began.elapsed();
        work(turn, 10, Arc::clone(&done), "question".into()).await;
        indexing.await.expect("indexing");
        let done = done.lock().expect("lock");
        let at = done
            .iter()
            .position(|who| who == "question")
            .expect("answered");
        assert_eq!(
            at,
            3,
            "only the passage running when it came went before it: {:?}",
            &done[..5]
        );
        assert!(waited <= Duration::from_millis(10), "waited {waited:?}");
        assert_eq!(done.len(), 501);
    }

    #[tokio::test(start_paused = true)]
    async fn questions_keep_their_order_and_passages_wait_for_all_of_them() {
        let turns = Turns::new();
        let done = Arc::new(Mutex::new(Vec::new()));
        let first = turns.question().await.expect("turn");
        let mut waiting = Vec::new();
        for who in ["passage", "question 2", "question 3"] {
            let (turns, done) = (turns.clone(), Arc::clone(&done));
            waiting.push(tokio::spawn(async move {
                let turn = if who == "passage" {
                    turns.passage().await
                } else {
                    turns.question().await
                };
                work(turn.expect("turn"), 5, done, who.into()).await;
            }));
            tokio::task::yield_now().await;
        }
        work(first, 5, Arc::clone(&done), "question 1".into()).await;
        for task in waiting {
            task.await.expect("task");
        }
        assert_eq!(
            *done.lock().expect("lock"),
            ["question 1", "question 2", "question 3", "passage"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_question_given_up_no_longer_holds_the_passages_back() {
        let turns = Turns::new();
        let busy = turns.passage().await.expect("turn");
        let gone = tokio::time::timeout(Duration::from_millis(5), turns.question()).await;
        assert!(gone.is_err(), "the model was busy");
        drop(busy);
        let next = tokio::time::timeout(Duration::from_millis(5), turns.passage()).await;
        assert!(
            next.is_ok(),
            "the passage takes the model once the question is gone"
        );
    }
}
