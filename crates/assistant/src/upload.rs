//! The files an administrator uploads to a `source: upload` (MF-51, API/05 §3, T-4184): each is
//! typed by its content, kept as a document of the source's site, and extracted, chunked and
//! searched like a crawled one. Its passages are data, never instructions (AG-105).

use sqlx::PgPool;

use crate::extract::{extract, store, Owner};
use crate::Error;

/// The most an Office file's archive may unpack to, and the most entries it may hold: past
/// either it is refused before the extractor opens it (a zip bomb).
pub const MAX_UNPACKED: u64 = 200 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 5_000;
/// The longest file name.
pub const MAX_NAME_CHARS: usize = 200;

pub const PDF: &str = "application/pdf";
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// The most folders a file's name may sit in.
pub const MAX_FOLDERS: usize = 8;

/// The file name as given, when it is one: 1 to [`MAX_NAME_CHARS`] characters, trimmed, with no
/// `\\` and no control character. It may sit in up to [`MAX_FOLDERS`] folders, `/` between them
/// (`reports/2026/budget.pdf`), which become the path its passages are searched and cited by
/// (T-6183); no part is empty, `.` or `..`. It is shown and cited, never used as a file path.
pub fn file_name(name: &str) -> Option<&str> {
    let name = name.trim();
    let parts: Vec<&str> = name.split('/').collect();
    let fits = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(|c| c == '\\' || c.is_control())
        && parts.len() <= MAX_FOLDERS + 1
        && parts.iter().all(|part| {
            !part.trim().is_empty() && part.trim() == *part && *part != "." && *part != ".."
        });
    fits.then_some(name)
}

/// The type `bytes` are read as, from their content; the name only tells CSV and Markdown from
/// plain text. Anything else is refused with the reason.
pub fn sniff(name: &str, bytes: &[u8]) -> Result<&'static str, String> {
    if bytes.starts_with(b"%PDF-") {
        return Ok(PDF);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return office(bytes);
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
        _ => Err("the file is not a PDF, a DOCX, an XLSX or UTF-8 text".into()),
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// A DOCX or an XLSX, read from the archive's central directory alone: what it holds, how much
/// it unpacks to and whether any entry is encrypted, before the extractor inflates anything.
fn office(bytes: &[u8]) -> Result<&'static str, String> {
    const BROKEN: &str = "the archive is not a readable DOCX or XLSX";
    // The end-of-central-directory record: 22 bytes and a comment of at most 64 KiB, at the end.
    let earliest = bytes.len().saturating_sub(22 + 0xFFFF);
    let end = (earliest..bytes.len().saturating_sub(21))
        .rev()
        .find(|&at| bytes[at..].starts_with(b"PK\x05\x06"))
        .ok_or(BROKEN)?;
    let entries = usize::from(u16_at(bytes, end + 10).ok_or(BROKEN)?);
    let start = u32_at(bytes, end + 16).ok_or(BROKEN)?;
    if entries == 0xFFFF || start == u32::MAX {
        return Err("a ZIP64 archive is not read".into());
    }
    if entries > MAX_ENTRIES {
        return Err(format!("the archive holds more than {MAX_ENTRIES} entries"));
    }
    let (mut at, mut unpacked, mut kind) = (start as usize, 0u64, None);
    for _ in 0..entries {
        if !bytes
            .get(at..)
            .is_some_and(|b| b.starts_with(b"PK\x01\x02"))
        {
            return Err(BROKEN.into());
        }
        let flags = u16_at(bytes, at + 8).ok_or(BROKEN)?;
        let size = u32_at(bytes, at + 24).ok_or(BROKEN)?;
        let name_len = usize::from(u16_at(bytes, at + 28).ok_or(BROKEN)?);
        let extra = usize::from(u16_at(bytes, at + 30).ok_or(BROKEN)?);
        let comment = usize::from(u16_at(bytes, at + 32).ok_or(BROKEN)?);
        let name = bytes.get(at + 46..at + 46 + name_len).ok_or(BROKEN)?;
        if flags & 1 == 1 {
            return Err("an encrypted archive is not read".into());
        }
        unpacked += u64::from(size);
        if unpacked > MAX_UNPACKED {
            return Err(format!(
                "the archive unpacks past {} MiB",
                MAX_UNPACKED / 1024 / 1024
            ));
        }
        match name {
            b"word/document.xml" => kind = Some(DOCX),
            b"xl/workbook.xml" => kind = Some(XLSX),
            _ => {}
        }
        at += 46 + name_len + extra + comment;
    }
    let kind = kind.ok_or("the archive is neither a DOCX nor an XLSX")?;
    inflated_within(bytes)?;
    Ok(kind)
}

/// Inflates every entry once, counting the bytes it really unpacks to: the sizes the central
/// directory declares are the file's own word, so a crafted entry that declares 1 KiB is refused
/// as soon as it passes [`MAX_UNPACKED`], before the extractor inflates it unbounded.
fn inflated_within(bytes: &[u8]) -> Result<(), String> {
    const BROKEN: &str = "the archive is not a readable DOCX or XLSX";
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|_| BROKEN.to_owned())?;
    let mut left = MAX_UNPACKED;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|_| BROKEN.to_owned())?;
        let read = std::io::copy(
            &mut std::io::Read::take(entry, left + 1),
            &mut std::io::sink(),
        )
        .map_err(|_| BROKEN.to_owned())?;
        left = left.checked_sub(read).ok_or_else(|| {
            format!(
                "the archive unpacks past {} MiB",
                MAX_UNPACKED / 1024 / 1024
            )
        })?;
    }
    Ok(())
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
    pub max_pdf_pages: u32,
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

    let indexed = match extract(bytes, mime, target.language, target.max_pdf_pages).await {
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

    /// A stored (uncompressed) ZIP of `entries` (name, declared size), as an Office file is laid out.
    pub(crate) fn zip(entries: &[(&str, u32)], encrypted: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (name, size) in entries {
            let offset = u32::try_from(out.len()).unwrap_or(0);
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[0; 26]);
            out.extend_from_slice(name.as_bytes());
            directory.extend_from_slice(b"PK\x01\x02");
            directory.extend_from_slice(&[0; 4]);
            directory.extend_from_slice(&u16::from(encrypted).to_le_bytes());
            directory.extend_from_slice(&[0; 14]);
            directory.extend_from_slice(&size.to_le_bytes());
            directory.extend_from_slice(&u16::try_from(name.len()).unwrap_or(0).to_le_bytes());
            directory.extend_from_slice(&[0; 12]);
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(name.as_bytes());
        }
        let start = u32::try_from(out.len()).unwrap_or(0);
        let count = u16::try_from(entries.len()).unwrap_or(0);
        out.extend_from_slice(&directory);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&u32::try_from(directory.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&[0; 2]);
        out
    }

    /// A real Office-shaped ZIP whose entries hold what they declare, deflated.
    fn office_zip(names: &[&str]) -> Vec<u8> {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for name in names {
            assert!(writer.start_file(*name, options).is_ok());
            assert!(writer.write_all(b"<x/>").is_ok());
        }
        writer
            .finish()
            .map(std::io::Cursor::into_inner)
            .unwrap_or_default()
    }

    /// T-5699, F-PLT1-010: an entry whose central directory declares 1 KiB but that inflates
    /// past the cap is refused; the declared sizes alone let it through before. Since zip 9
    /// (T-6053) the reader stops at the declared size and calls the entry broken, before the
    /// count reaches the cap.
    #[test]
    fn an_entry_that_inflates_past_what_it_declares_is_refused() {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(false);
        assert!(writer.start_file("word/document.xml", options).is_ok());
        let block = vec![0u8; 1024 * 1024];
        for _ in 0..=MAX_UNPACKED / 1024 / 1024 {
            assert!(writer.write_all(&block).is_ok());
        }
        let mut bomb = writer
            .finish()
            .map(std::io::Cursor::into_inner)
            .unwrap_or_default();
        // Rewrite the declared uncompressed size, in the local header and the directory, to 1 KiB.
        let local = 22;
        bomb[local..local + 4].copy_from_slice(&1024u32.to_le_bytes());
        let directory = bomb
            .windows(4)
            .rposition(|window| window == b"PK\x01\x02")
            .unwrap_or_default();
        bomb[directory + 24..directory + 28].copy_from_slice(&1024u32.to_le_bytes());
        let refused = sniff("x.docx", &bomb);
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e == "the archive is not a readable DOCX or XLSX"),
            "{refused:?}"
        );
    }

    #[test]
    fn a_file_is_typed_by_its_content_never_its_name() {
        assert_eq!(sniff("a.txt", b"%PDF-1.7\n..."), Ok(PDF));
        assert_eq!(
            sniff(
                "x",
                &office_zip(&["[Content_Types].xml", "word/document.xml"])
            ),
            Ok(DOCX)
        );
        assert_eq!(sniff("x.pdf", &office_zip(&["xl/workbook.xml"])), Ok(XLSX));
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

    #[test]
    fn an_archive_is_refused_past_its_limits_or_when_it_is_no_office_file() {
        let bomb = zip(
            &[("word/document.xml", u32::MAX), ("word/x.xml", u32::MAX)],
            false,
        );
        assert!(sniff("x.docx", &bomb).is_err_and(|e| e.contains("unpacks past 200 MiB")));
        let locked = zip(&[("word/document.xml", 10)], true);
        assert!(sniff("x.docx", &locked).is_err_and(|e| e.contains("encrypted")));
        let jar = zip(&[("META-INF/MANIFEST.MF", 10)], false);
        assert!(sniff("x.docx", &jar).is_err_and(|e| e.contains("neither")));
        let mut cut = zip(&[("word/document.xml", 10)], false);
        cut.truncate(cut.len() - 30);
        assert!(sniff("x.docx", &cut).is_err());
        let many: Vec<(String, u32)> = (0..=MAX_ENTRIES).map(|n| (format!("f{n}"), 1)).collect();
        let many: Vec<(&str, u32)> = many.iter().map(|(n, s)| (n.as_str(), *s)).collect();
        assert!(sniff("x.xlsx", &zip(&many, false)).is_err_and(|e| e.contains("entries")));
    }

    #[test]
    fn a_name_is_a_file_name_in_folders_and_never_a_way_out() {
        assert_eq!(
            file_name("  Výročná správa 2025.pdf "),
            Some("Výročná správa 2025.pdf")
        );
        assert_eq!(
            file_name("správy/2025/Výročná správa.pdf"),
            Some("správy/2025/Výročná správa.pdf")
        );
        assert!(file_name(&format!("{}b.pdf", "a/".repeat(MAX_FOLDERS))).is_some());
        for bad in [
            "",
            "   ",
            "../etc/passwd",
            "a/../b.pdf",
            "./b.pdf",
            "/b.pdf",
            "a//b.pdf",
            "a/b.pdf/",
            "a /b.pdf",
            &format!("{}b.pdf", "a/".repeat(MAX_FOLDERS + 1)),
            "a\\b.pdf",
            "..",
            "a\nb",
            &"x".repeat(201),
        ] {
            assert_eq!(file_name(bad), None, "{bad:?}");
        }
    }
}
