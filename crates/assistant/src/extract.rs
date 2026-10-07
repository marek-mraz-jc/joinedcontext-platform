//! Text out of what the crawler fetched, cut into passages and stored as chunks (T-3052,
//! ADR-N-040 §3.2).
//!
//! `kreuzberg` turns HTML and PDF into Markdown with page markers, detects the language and
//! chunks with overlap. [`Indexer`] is the crawl's [`Sink`]: each included page or document that
//! changed replaces its own chunks, without an embedding yet (T-3053 adds those).

use kreuzberg::{
    ChunkerType, ChunkingConfig, ExtractionConfig, LanguageDetectionConfig, PageConfig,
};
use sqlx::PgPool;

use crate::crawl::Sink;
use crate::Error;

/// The longest passage, in characters: about 300 tokens, the size the embedding model reads
/// whole (`multilingual-e5-small` truncates at 512 tokens).
pub const CHUNK_CHARS: usize = 1_200;

/// How much consecutive passages share, so a sentence cut at a boundary is whole in one of them.
pub const CHUNK_OVERLAP: usize = 200;

/// The most passages one page or document may become: a cap on what one source can cost.
pub const MAX_PASSAGES: usize = 2_000;

/// One passage of a page or a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    /// Its text, Markdown.
    pub text: String,
    /// The PDF page it starts on, 1-based; `None` for HTML.
    pub first_page: Option<u32>,
}

/// What one page or document became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    /// ISO 639-1 code of its language: the page's own `lang` when it declares one, else the
    /// detected one when it is a language the store has a configuration for, else `None`.
    pub language: Option<String>,
    /// The passages, in reading order.
    pub passages: Vec<Passage>,
    /// Whether passages beyond `pdf.maxPages` or [`MAX_PASSAGES`] were dropped.
    pub truncated: bool,
}

fn config() -> ExtractionConfig {
    ExtractionConfig {
        use_cache: false,
        // A scanned PDF yields no text; OCR would bring ONNX Runtime and its memory.
        disable_ocr: true,
        chunking: Some(ChunkingConfig {
            max_characters: CHUNK_CHARS,
            overlap: CHUNK_OVERLAP,
            trim: true,
            chunker_type: ChunkerType::Markdown,
            // Each passage carries the headings above it ("# Cyklotrasy > ## Trasy"), so it
            // reads on its own when it is retrieved alone.
            prepend_heading_context: true,
            ..ChunkingConfig::default()
        }),
        language_detection: Some(LanguageDetectionConfig {
            enabled: true,
            min_confidence: 0.5,
            detect_multiple: false,
        }),
        pages: Some(PageConfig {
            extract_pages: false,
            insert_page_markers: false,
            ..PageConfig::default()
        }),
        ..ExtractionConfig::default()
    }
}

/// The ISO 639-1 code of an ISO 639-3 one, for the languages the store indexes by name.
fn iso639_1(code: &str) -> Option<&'static str> {
    match code {
        "slk" => Some("sk"),
        "ces" => Some("cs"),
        "fin" => Some("fi"),
        "eng" => Some("en"),
        "deu" => Some("de"),
        "swe" => Some("sv"),
        _ => None,
    }
}

/// Extracts `bytes` of `mime`, keeping passages that start on a page up to `max_pages`.
pub async fn extract(
    bytes: &[u8],
    mime: &str,
    declared_language: Option<&str>,
    max_pages: u32,
) -> Result<Extracted, Error> {
    let result = kreuzberg::extract_bytes(bytes, mime, &config())
        .await
        .map_err(|err| Error::Extract(format!("{mime}: {err}")))?;
    let detected = result
        .detected_languages
        .as_ref()
        .and_then(|found| found.first())
        .and_then(|code| iso639_1(code));
    let language = declared_language
        .filter(|code| code.len() == 2 && code.bytes().all(|b| b.is_ascii_lowercase()))
        .map(str::to_owned)
        .or_else(|| detected.map(str::to_owned));

    let mut truncated = false;
    let mut passages = Vec::new();
    for chunk in result.chunks.unwrap_or_default() {
        let first_page = chunk
            .metadata
            .first_page
            .and_then(|page| u32::try_from(page).ok());
        if first_page.is_some_and(|page| page > max_pages) || passages.len() == MAX_PASSAGES {
            truncated = true;
            break;
        }
        let text = chunk.content.trim();
        if !text.is_empty() && !only_headings(text) {
            passages.push(Passage {
                text: text.to_owned(),
                first_page,
            });
        }
    }
    Ok(Extracted {
        language,
        passages,
        truncated,
    })
}

/// Whether a passage is headings and nothing else: its body lines empty or repeating a heading.
/// Such a passage is no answer to anything; its words travel on in the heading context of the
/// passages below it.
fn only_headings(text: &str) -> bool {
    // A heading line is one heading or a path of them, `# Mesto > ## Doprava`.
    let headings: Vec<&str> = text
        .lines()
        .filter(|line| line.trim_start().starts_with('#'))
        .flat_map(|line| line.split(" > "))
        .map(|heading| heading.trim().trim_start_matches('#').trim())
        .collect();
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .all(|line| headings.contains(&line))
}

/// Which row the passages belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// A page of the site.
    Page(i64),
    /// A document a page links.
    Document(i64),
}

/// Replaces the chunks of `owner` with `extracted`'s passages, in one project-scoped
/// transaction, so a reader never sees half of an old and half of a new version.
pub async fn store(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    owner: Owner,
    url: &str,
    visibility: &str,
    extracted: &Extracted,
) -> Result<usize, Error> {
    let (page_id, document_id) = match owner {
        Owner::Page(id) => (Some(id), None),
        Owner::Document(id) => (None, Some(id)),
    };
    let mut tx = crate::project_scope(pool, project).await?;
    sqlx::query("DELETE FROM chunks WHERE page_id IS NOT DISTINCT FROM $1 AND document_id IS NOT DISTINCT FROM $2")
        .bind(page_id)
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    for (ordinal, passage) in extracted.passages.iter().enumerate() {
        // A PDF passage cites its page, so a reader lands where the text is.
        let cited = match passage.first_page {
            Some(page) => format!("{url}#page={page}"),
            None => url.to_owned(),
        };
        sqlx::query(
            "INSERT INTO chunks (site_id, project, page_id, document_id, ordinal, url, text, lang, visibility) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(site_id)
        .bind(project)
        .bind(page_id)
        .bind(document_id)
        .bind(i32::try_from(ordinal).unwrap_or(i32::MAX))
        .bind(cited)
        .bind(&passage.text)
        .bind(extracted.language.as_deref())
        .bind(visibility)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(extracted.passages.len())
}

/// The crawl's [`Sink`]: extracts each changed, included page or document and replaces its
/// chunks. A failure is kept in [`Indexer::failures`] with the URL, and the crawl goes on.
pub struct Indexer {
    pool: PgPool,
    project: String,
    site_id: i64,
    visibility: String,
    max_pdf_pages: u32,
    /// Passages stored in this crawl.
    pub passages: usize,
    /// Pages and documents whose passages were cut at a cap.
    pub truncated: usize,
    /// What could not be extracted or stored, `url: reason`.
    pub failures: Vec<String>,
}

impl Indexer {
    /// An indexer for one site of one project; `visibility` is the source's (`public` or
    /// `internal`) and `max_pdf_pages` its `pdf.maxPages`.
    pub fn new(
        pool: PgPool,
        project: &str,
        site_id: i64,
        visibility: &str,
        max_pdf_pages: u32,
    ) -> Self {
        Self {
            pool,
            project: project.to_owned(),
            site_id,
            visibility: visibility.to_owned(),
            max_pdf_pages,
            passages: 0,
            truncated: 0,
            failures: Vec::new(),
        }
    }

    /// Whether an administrator excluded the page or document (T-3057): it is fetched and kept
    /// in the tree, and never indexed.
    async fn excluded(&self, owner: Owner) -> Result<bool, Error> {
        let (sql, id) = match owner {
            Owner::Page(id) => ("SELECT excluded_by_admin FROM pages WHERE id = $1", id),
            Owner::Document(id) => ("SELECT excluded_by_admin FROM documents WHERE id = $1", id),
        };
        let mut tx = crate::project_scope(&self.pool, &self.project).await?;
        let excluded: Option<bool> = sqlx::query_scalar(sql)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        tx.rollback().await?;
        Ok(excluded.unwrap_or(false))
    }

    async fn index(
        &mut self,
        owner: Owner,
        url: &str,
        mime: &str,
        language: Option<&str>,
        bytes: &[u8],
    ) {
        match self.excluded(owner).await {
            Ok(false) => {}
            Ok(true) => return,
            Err(err) => {
                self.failures.push(format!("{url}: {err}"));
                return;
            }
        }
        let outcome = match extract(bytes, mime, language, self.max_pdf_pages).await {
            Ok(extracted) => {
                if extracted.truncated {
                    self.truncated += 1;
                }
                store(
                    &self.pool,
                    &self.project,
                    self.site_id,
                    owner,
                    url,
                    &self.visibility,
                    &extracted,
                )
                .await
            }
            Err(err) => Err(err),
        };
        match outcome {
            Ok(stored) => self.passages += stored,
            Err(err) => self.failures.push(format!("{url}: {err}")),
        }
    }
}

impl Sink for Indexer {
    async fn page(&mut self, page_id: i64, url: &str, language: Option<&str>, html: &str) {
        self.index(
            Owner::Page(page_id),
            url,
            "text/html",
            language,
            html.as_bytes(),
        )
        .await;
    }

    async fn document(&mut self, document_id: i64, url: &str, mime: Option<&str>, bytes: &[u8]) {
        // The server's type without parameters; a document with none is read as a PDF only when
        // its address says so, and refused otherwise rather than guessed.
        let mime = mime
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                url.to_ascii_lowercase()
                    .ends_with(".pdf")
                    .then(|| "application/pdf".to_owned())
            });
        match mime {
            Some(mime) => {
                self.index(Owner::Document(document_id), url, &mime, None, bytes)
                    .await
            }
            None => self
                .failures
                .push(format!("{url}: the server named no content type")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::only_headings;

    #[test]
    fn a_passage_of_headings_alone_is_recognised() {
        assert!(only_headings("# Cyklotrasy"));
        assert!(only_headings("# Cyklotrasy\n\nCyklotrasy"));
        assert!(only_headings("# Mesto > ## Doprava\n\nDoprava"));
        assert!(!only_headings("# Cyklotrasy\n\nTrasa vedie popri Hrone."));
        assert!(!only_headings("Telefón: 048 433 0111"));
    }
}
