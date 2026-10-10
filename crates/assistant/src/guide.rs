//! A `guide` knowledge source (T-3226, AG-118): the platform's User Guide, which this crate ships
//! in `guide/` (`scripts/sync-user-guide.sh` copies `User-Guide/NN-*.md` of the docs repository
//! and nothing else, with the docs commit in `guide/STAMP`).
//!
//! Each numbered section is one page. The Portal paths the guide writes for its example projects
//! (`/projects/helsinki/endpoints`) are rewritten to the same page of the source's own project on
//! the Portal's address, so an answer that gives the steps of a workflow links into the form of
//! each step; the section cites the first Portal page it names, else the project's Portal home.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::crawl::Sink;
use crate::Error;

/// Every page of the shipped guide, by file name, in file order.
pub const PAGES: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/guide_pages.rs"));

/// The docs commit the shipped pages were copied from.
pub fn stamp() -> &'static str {
    include_str!("../guide/STAMP").trim()
}

/// One section of a guide page, as it is indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The citation: a Portal page of the project, with a fragment naming the section.
    pub url: String,
    pub html: String,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn is_slug_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '{' | '}')
}

/// `text` with every Portal path of an example project (`/projects/{slug}...` standing on its
/// own, not inside an API path or another URL) on `portal` and `project`.
pub fn rewrite(text: &str, portal: &str, project: &str) -> String {
    const PREFIX: &str = "/projects/";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let standalone = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '`' | '(' | '"' | '\'' | '['));
        let after = &rest[at + PREFIX.len()..];
        let slug = after.find(|c| !is_slug_char(c)).unwrap_or(after.len());
        let ends = after[slug..].chars().next().is_none_or(|c| {
            c.is_whitespace() || matches!(c, '/' | '?' | '#' | '`' | ')' | '"' | '\'' | '.' | ',')
        });
        out.push_str(&rest[..at]);
        if standalone && slug > 0 && ends {
            out.push_str(portal);
            out.push_str(PREFIX);
            out.push_str(project);
            rest = &after[slug..];
        } else {
            out.push_str(PREFIX);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The first Portal page `text` names on `portal`, without its fragment.
fn first_portal_page(text: &str, portal: &str) -> Option<String> {
    let start = text.find(&format!("{portal}/projects/"))?;
    let tail = &text[start..];
    let end = tail
        .find(|c: char| c.is_whitespace() || matches!(c, '`' | ')' | '"' | '\'' | '<' | '>' | '#'))
        .unwrap_or(tail.len());
    Some(tail[..end].trim_end_matches(['.', ',']).to_owned())
}

/// A line of Markdown as plain text: links as their text (an absolute one with its address),
/// emphasis and code marks dropped.
fn inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let link = rest[open + 1..].find("](").and_then(|mid| {
            let target_at = open + 1 + mid + 2;
            rest[target_at..]
                .find(')')
                .map(|close| (open + 1 + mid, target_at, target_at + close))
        });
        out.push_str(&rest[..open]);
        match link {
            Some((text_end, target_at, close)) => {
                out.push_str(&rest[open + 1..text_end]);
                let target = &rest[target_at..close];
                if target.starts_with("https://") || target.starts_with("http://") {
                    out.push_str(" (");
                    out.push_str(target);
                    out.push(')');
                }
                rest = &rest[close + 1..];
            }
            None => {
                out.push('[');
                rest = &rest[open + 1..];
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").replace('`', "")
}

fn is_list_item(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("- ")
        || line.starts_with("* ")
        || line
            .split_once(". ")
            .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// A section's Markdown body as HTML: headings, paragraphs, list items and table rows each
/// their own element, code blocks preformatted, every text escaped.
fn body_html(lines: &[&str]) -> String {
    let mut html = String::new();
    let mut paragraph: Vec<String> = Vec::new();
    let flush = |paragraph: &mut Vec<String>, html: &mut String| {
        if !paragraph.is_empty() {
            html.push_str(&format!("<p>{}</p>", escape(&paragraph.join(" "))));
            paragraph.clear();
        }
    };
    let mut fence: Option<Vec<&str>> = None;
    for line in lines {
        if line.trim_start().starts_with("```") {
            match fence.take() {
                Some(code) => html.push_str(&format!("<pre>{}</pre>", escape(&code.join("\n")))),
                None => {
                    flush(&mut paragraph, &mut html);
                    fence = Some(Vec::new());
                }
            }
            continue;
        }
        if let Some(code) = fence.as_mut() {
            code.push(line);
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut paragraph, &mut html);
        } else if let Some(heading) = trimmed.strip_prefix('#') {
            flush(&mut paragraph, &mut html);
            let heading = heading.trim_start_matches('#').trim();
            html.push_str(&format!("<h3>{}</h3>", escape(&inline(heading))));
        } else if trimmed.starts_with('|') {
            flush(&mut paragraph, &mut html);
            let cells: Vec<String> = trimmed
                .trim_matches('|')
                .split('|')
                .map(|cell| inline(cell.trim()))
                .collect();
            if !cells
                .iter()
                .all(|cell| cell.chars().all(|c| matches!(c, '-' | ':' | ' ')))
            {
                html.push_str(&format!("<p>{}</p>", escape(&cells.join(" | "))));
            }
        } else {
            if is_list_item(line) {
                flush(&mut paragraph, &mut html);
            }
            paragraph.push(inline(trimmed));
        }
    }
    if let Some(code) = fence {
        html.push_str(&format!("<pre>{}</pre>", escape(&code.join("\n"))));
    }
    flush(&mut paragraph, &mut html);
    html
}

/// The sections of one guide page (`file` its name, `markdown` its text), for `project` on the
/// Portal at `portal` (scheme and authority, no trailing slash). The text before the first
/// numbered section is a section of its own when it holds any.
pub fn sections(file: &str, markdown: &str, portal: &str, project: &str) -> Vec<Section> {
    let text = rewrite(markdown, portal, project);
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.first().is_some_and(|line| line.trim() == "---") {
        if let Some(end) = lines.iter().skip(1).position(|line| line.trim() == "---") {
            lines.drain(..end + 2);
        }
    }
    let stem = file.trim_end_matches(".md");
    let mut title = stem.to_owned();
    let mut parts: Vec<(Option<&str>, Vec<&str>)> = vec![(None, Vec::new())];
    let mut in_fence = false;
    for line in lines {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence {
            if let Some(h1) = line.strip_prefix("# ") {
                title = inline(h1.trim());
                continue;
            }
            if let Some(h2) = line.strip_prefix("## ") {
                parts.push((Some(h2.trim()), Vec::new()));
                continue;
            }
        }
        if let Some((_, body)) = parts.last_mut() {
            body.push(line);
        }
    }
    let home = format!("{portal}/projects/{project}");
    parts
        .into_iter()
        .enumerate()
        .filter(|(_, (heading, body))| {
            heading.is_some() || body.iter().any(|line| !line.trim().is_empty())
        })
        .map(|(n, (heading, body))| {
            let page = first_portal_page(&body.join("\n"), portal).unwrap_or_else(|| home.clone());
            let mut html = format!("<html><body><h1>{}</h1>", escape(&title));
            if let Some(heading) = heading {
                html.push_str(&format!("<h2>{}</h2>", escape(&inline(heading))));
            }
            html.push_str(&body_html(&body));
            html.push_str("</body></html>");
            Section {
                url: format!("{page}#guide-{stem}-{n}"),
                html,
            }
        })
        .collect()
}

/// What one read of a guide source did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuideReport {
    pub sections: usize,
    pub changed: usize,
    pub removed: usize,
}

/// Indexes `pages` (file name, Markdown) into the source's site as sections of `project` on the
/// Portal at `portal`, hands only new or changed sections to `sink`, and removes the sections no
/// longer shipped.
pub async fn sync_guide(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    portal: Option<&str>,
    pages: &[(&str, &str)],
    sink: &mut impl Sink,
) -> Result<GuideReport, Error> {
    let Some(portal) = portal else {
        return Err(Error::Crawl(
            "a guide source needs JC_ASSISTANT_PORTAL_URL, the Portal address its sections cite"
                .into(),
        ));
    };
    let language = Some("en");
    let mut listed: BTreeSet<String> = BTreeSet::new();
    let mut changed = 0;
    for (file, markdown) in pages {
        for section in sections(file, markdown, portal, project) {
            if !listed.insert(section.url.clone()) {
                continue;
            }
            let hash = hex::encode(Sha256::digest(section.html.as_bytes()));
            let mut tx = crate::project_scope(pool, project).await?;
            let known: Option<String> = sqlx::query_scalar(
                "SELECT content_hash FROM pages WHERE site_id = $1 AND url = $2",
            )
            .bind(site_id)
            .bind(&section.url)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
            if known.as_deref() == Some(hash.as_str()) {
                continue;
            }
            let page_id: i64 = sqlx::query_scalar(
                "INSERT INTO pages (site_id, project, url, depth, status, content_hash, language, included, fetched_at) \
                 VALUES ($1, $2, $3, 0, 'fetched', $4, $5, true, now()) \
                 ON CONFLICT (site_id, url) DO UPDATE SET status = 'fetched', content_hash = EXCLUDED.content_hash, \
                     language = EXCLUDED.language, fetched_at = now() \
                 RETURNING id",
            )
            .bind(site_id)
            .bind(project)
            .bind(&section.url)
            .bind(&hash)
            .bind(language)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            changed += 1;
            sink.page(page_id, &section.url, language, &section.html)
                .await;
        }
    }
    let listed: Vec<String> = listed.into_iter().collect();
    let mut tx = crate::project_scope(pool, project).await?;
    let removed = sqlx::query("DELETE FROM pages WHERE site_id = $1 AND NOT (url = ANY($2))")
        .bind(site_id)
        .bind(&listed)
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;
    sqlx::query("UPDATE sites SET last_crawl = now() WHERE id = $1")
        .bind(site_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(GuideReport {
        sections: listed.len(),
        changed,
        removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTAL: &str = "https://portal.example.org";

    #[test]
    fn a_portal_path_of_an_example_project_points_into_the_sources_project() {
        let text = "Open `/projects/helsinki/endpoints`, then (/projects/{p}/workspaces). \
                    Call /api/v1/projects/helsinki/endpoints or https://x.org/projects/a/b; \
                    /projects/Helsinki stays, /projectsfoo stays.";
        assert_eq!(
            rewrite(text, PORTAL, "zilina"),
            "Open `https://portal.example.org/projects/zilina/endpoints`, then \
             (https://portal.example.org/projects/zilina/workspaces). \
             Call /api/v1/projects/helsinki/endpoints or https://x.org/projects/a/b; \
             /projects/Helsinki stays, /projectsfoo stays."
        );
        assert_eq!(rewrite("", PORTAL, "zilina"), "");
        assert_eq!(
            rewrite("/projects/helsinki", PORTAL, "zilina"),
            "https://portal.example.org/projects/zilina"
        );
    }

    #[test]
    fn a_page_reads_as_its_sections_each_citing_the_first_portal_page_it_names() {
        let page = "---\ntitle: Endpoints\n---\n\n# Endpoints & Sharing\n\nIntro text.\n\n\
                    ## 1. Audiences\n\nNo Portal page here.\n\n\
                    ## 2. Publishing an Endpoint\n\n1. Open **Endpoints** at `/projects/helsinki/endpoints`.\n\
                    2. See [Policies](./05-x.md) and [ETSI](https://etsi.org/ngsi).\n\
                    ```yaml\n## not a section <b>\n```\n\
                    | Field | Meaning |\n|---|---|\n| `title` | The <name> |\n";
        let found = sections("05-endpoints.md", page, PORTAL, "zilina");
        let urls: Vec<&str> = found.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://portal.example.org/projects/zilina#guide-05-endpoints-0",
                "https://portal.example.org/projects/zilina#guide-05-endpoints-1",
                "https://portal.example.org/projects/zilina/endpoints#guide-05-endpoints-2",
            ]
        );
        let publishing = &found[2].html;
        assert!(publishing.starts_with(
            "<html><body><h1>Endpoints &amp; Sharing</h1><h2>2. Publishing an Endpoint</h2>"
        ));
        assert!(publishing.contains(
            "<p>1. Open Endpoints at https://portal.example.org/projects/zilina/endpoints.</p>"
        ));
        assert!(publishing.contains("<p>2. See Policies and ETSI (https://etsi.org/ngsi).</p>"));
        assert!(publishing.contains("<pre>## not a section &lt;b&gt;</pre>"));
        assert!(publishing.contains("<p>title | The &lt;name&gt;</p>"));
        assert!(!publishing.contains("---"), "{publishing}");
        assert!(
            !found[0].html.contains("title: Endpoints"),
            "front matter is not text"
        );
    }

    #[test]
    fn an_empty_page_and_an_unclosed_fence_still_read() {
        assert!(sections("00-x.md", "", PORTAL, "p").is_empty());
        let open = sections("00-x.md", "## A\n```\ncode", PORTAL, "p");
        assert_eq!(open.len(), 1);
        assert!(open[0].html.contains("<pre>code</pre>"));
    }

    #[test]
    fn the_shipped_guide_is_the_user_guide_alone_with_its_commit() {
        assert!(
            PAGES.len() >= 10,
            "the User Guide ships ({} pages)",
            PAGES.len()
        );
        for (file, text) in PAGES {
            let (number, rest) = file.split_at(2);
            assert!(
                number.chars().all(|c| c.is_ascii_digit())
                    && rest.starts_with('-')
                    && rest.ends_with(".md"),
                "{file} is no User Guide page"
            );
            assert!(!text.trim().is_empty(), "{file} is empty");
        }
        assert!(
            stamp().len() == 40 && stamp().chars().all(|c| c.is_ascii_hexdigit()),
            "guide/STAMP names the docs commit: {:?}",
            stamp()
        );
    }

    #[test]
    fn every_workflow_of_t3226_has_a_section_linking_its_form() {
        let all: Vec<Section> = PAGES
            .iter()
            .flat_map(|(file, text)| sections(file, text, PORTAL, "zilina"))
            .collect();
        for form in [
            "/apps",
            "/endpoints",
            "/pipelines",
            "/datasources",
            "/models",
            "/policies",
            "/workspaces",
        ] {
            let page = format!("{PORTAL}/projects/zilina{form}");
            assert!(
                all.iter().any(|s| s.url.starts_with(&page)),
                "no section cites {page}"
            );
        }
        let urls: BTreeSet<&str> = all.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls.len(), all.len(), "every section has its own citation");
    }
}
