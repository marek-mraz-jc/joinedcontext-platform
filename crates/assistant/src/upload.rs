//! The files an administrator uploads to a `source: upload` (MF-51, API/05 §3, T-4184): each is
//! typed by its content, kept as a document of the source's site, and extracted, chunked and
//! searched like a crawled one. Its passages are data, never instructions (AG-105).

use sqlx::PgPool;

use crate::extract::{extract, store, Owner};
use crate::Error;

/// The longest file name.
pub const MAX_NAME_CHARS: usize = 200;

/// The file name as given, when it is one: 1 to [`MAX_NAME_CHARS`] characters, trimmed, with no
/// path separator and no control character. It is shown and cited, never used as a path.
pub fn file_name(name: &str) -> Option<&str> {
    let name = name.trim();
    let fits = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
        && name != "."
        && name != "..";
    fits.then_some(name)
}

/// The type `bytes` are read as, from their content; the name only tells CSV and Markdown from
/// plain text. Anything else is refused with the reason: a PDF and an Office file (a ZIP
/// archive) by name, because the assistant no longer reads them (T-4524).
pub fn sniff(name: &str, bytes: &[u8]) -> Result<&'static str, String> {
    if bytes.starts_with(b"%PDF-") {
        return Err(
            "a PDF is not read: the assistant reads plain text, CSV and Markdown only (T-4524)"
                .into(),
        );
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return Err("a DOCX, XLSX or other ZIP archive is not read: the assistant reads plain text, CSV and Markdown only (T-4524)".into());
    }
    let text = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(text) {
        Ok(text)
            if !text.is_empty()
                && !text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\x0c')) =>
        {
            let lower = name.to_ascii_lowercase();
            Ok(if lower.ends_with(".csv") {
                "text/csv"
            } else if lower.ends_with(".md") || lower.ends_with(".markdown") {
                "text/markdown"
            } else {
                "text/plain"
            })
        }
        _ => Err("the file is not UTF-8 text".into()),
    }
}

/// One uploaded file as the administration lists it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct File {
    pub id: i64,
    pub name: String,
    pub mime: Option<String>,
    pub bytes: Option<i64>,
    pub passages: i64,
    pub status: String,
}

/// The address an uploaded file's passages cite: not a link, a name under its source.
fn address(source: &str, name: &str) -> String {
    format!("upload:{source}/{name}")
}

/// A `documents` row as [`files`] reads it: id, url, mime, bytes, status, passages.
type FileRow = (i64, String, Option<String>, Option<i64>, String, i64);

/// Every file of the site, by name.
pub async fn files(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    source: &str,
) -> Result<Vec<File>, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let rows: Vec<FileRow> = sqlx::query_as(
        "SELECT d.id, d.url, d.mime, d.bytes, d.status, \
           (SELECT count(*) FROM chunks k WHERE k.document_id = d.id) \
         FROM documents d WHERE d.site_id = $1 ORDER BY d.url",
    )
    .bind(site_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    let prefix = address(source, "");
    Ok(rows
        .into_iter()
        .map(|(id, url, mime, bytes, status, passages)| File {
            id,
            name: url.strip_prefix(&prefix).unwrap_or(&url).to_owned(),
            mime,
            bytes,
            passages,
            status,
        })
        .collect())
}

/// Why a file was not kept.
#[derive(Debug)]
pub enum Refused {
    /// The site already holds its `maxFiles`.
    Full(u32),
    /// Its content is none of the types read.
    Type(String),
    Store(Error),
}

impl From<Error> for Refused {
    fn from(err: Error) -> Self {
        Self::Store(err)
    }
}

impl From<sqlx::Error> for Refused {
    fn from(err: sqlx::Error) -> Self {
        Self::Store(err.into())
    }
}

/// What one upload goes into.
pub struct Target<'a> {
    pub project: &'a str,
    pub source: &'a str,
    pub site_id: i64,
    pub visibility: &'a str,
    pub max_files: u32,
    pub language: Option<&'a str>,
}

/// Keeps `bytes` as the file `name` of the site, replacing a file of that name, and indexes it.
/// A file that cannot be extracted is kept as `failed` with no passage.
pub async fn add(
    pool: &PgPool,
    target: &Target<'_>,
    name: &str,
    bytes: &[u8],
) -> Result<File, Refused> {
    let mime = sniff(name, bytes).map_err(Refused::Type)?;
    let url = address(target.source, name);
    let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let mut tx = crate::project_scope(pool, target.project).await?;
    // ponytail: count then insert in two statements; two uploads racing past maxFiles at once
    // may keep one file over it, which the next upload's count refuses.
    let (held, known): (i64, bool) = sqlx::query_as(
        "SELECT count(*), bool_or(url = $2) IS TRUE FROM documents WHERE site_id = $1",
    )
    .bind(target.site_id)
    .bind(&url)
    .fetch_one(&mut *tx)
    .await?;
    if !known && held >= i64::from(target.max_files) {
        return Err(Refused::Full(target.max_files));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO documents (site_id, project, url, mime, bytes, status, fetched_at) \
         VALUES ($1, $2, $3, $4, $5, 'fetched', now()) \
         ON CONFLICT (site_id, url) DO UPDATE \
           SET mime = EXCLUDED.mime, bytes = EXCLUDED.bytes, status = 'fetched', fetched_at = now() \
         RETURNING id",
    )
    .bind(target.site_id)
    .bind(target.project)
    .bind(&url)
    .bind(mime)
    .bind(size)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    let indexed = match extract(bytes, mime, target.language).await {
        Ok(extracted) => {
            store(
                pool,
                target.project,
                target.site_id,
                Owner::Document(id),
                &url,
                target.visibility,
                &extracted,
            )
            .await
        }
        Err(err) => Err(err),
    };
    let (status, passages) = match indexed {
        Ok(stored) => ("fetched", i64::try_from(stored).unwrap_or(i64::MAX)),
        Err(err) => {
            tracing::warn!(project = %target.project, source = %target.source, %err, "an uploaded file could not be extracted");
            ("failed", 0)
        }
    };
    let mut tx = crate::project_scope(pool, target.project).await?;
    sqlx::query("UPDATE documents SET status = $2 WHERE id = $1")
        .bind(id)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(File {
        id,
        name: name.to_owned(),
        mime: Some(mime.to_owned()),
        bytes: Some(size),
        passages,
        status: status.to_owned(),
    })
}

/// Removes file `id` of the site and its passages; `false` when the site holds no such file.
pub async fn remove(pool: &PgPool, project: &str, site_id: i64, id: i64) -> Result<bool, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    // The chunks go with the document (ON DELETE CASCADE).
    let removed = sqlx::query("DELETE FROM documents WHERE id = $1 AND site_id = $2")
        .bind(id)
        .bind(site_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(removed == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_typed_by_its_content_never_its_name() {
        assert_eq!(
            sniff("Rozpočet.CSV", "obec;suma\nBB;1\n".as_bytes()),
            Ok("text/csv")
        );
        assert_eq!(
            sniff("navod.md", b"\xEF\xBB\xBF# Navod\n"),
            Ok("text/markdown")
        );
        assert_eq!(
            sniff("report.pdf", b"<html>not a pdf</html>"),
            Ok("text/plain")
        );
        for (bytes, why) in [
            (&b"\x89PNG\r\n\x1a\n"[..], "UTF-8 text"),
            (&b""[..], "UTF-8 text"),
            (&b"text\x00with a NUL"[..], "UTF-8 text"),
        ] {
            assert!(
                sniff("a.txt", bytes).is_err_and(|e| e.contains(why)),
                "{bytes:?}"
            );
        }
    }

    /// T-4524: a PDF, DOCX or XLSX is refused with the reason, whatever its name.
    #[test]
    fn a_pdf_or_an_office_file_is_refused_by_name_of_its_type() {
        let pdf = sniff("a.txt", b"%PDF-1.7\n...").expect_err("a PDF");
        assert!(
            pdf.contains("a PDF is not read") && pdf.contains("T-4524"),
            "{pdf}"
        );
        let docx =
            sniff("x.md", b"PK\x03\x04\x14\x00\x06\x00word/document.xml").expect_err("a DOCX");
        assert!(
            docx.contains("DOCX, XLSX") && docx.contains("T-4524"),
            "{docx}"
        );
    }

    #[test]
    fn a_name_is_a_file_name_and_never_a_path() {
        assert_eq!(
            file_name("  Výročná správa 2025.pdf "),
            Some("Výročná správa 2025.pdf")
        );
        for bad in [
            "",
            "   ",
            "../etc/passwd",
            "a/b.pdf",
            "a\\b.pdf",
            "..",
            "a\nb",
            &"x".repeat(201),
        ] {
            assert_eq!(file_name(bad), None, "{bad:?}");
        }
    }
}
