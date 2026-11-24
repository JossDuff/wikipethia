//! Page adapter: one source is one Markdown document at one URL.
//!
//! For a canonical document that is none of the other kinds — not a forum,
//! not a feed, and not worth a repository snapshot. The first one is the EF
//! Mandate: its Markdown lives in `ethereum/ethereum-org-website`, a 1.6 GB
//! repository that moves several times a day, so the repo adapter would
//! pull the whole tarball into memory on every update for one 58 KB file.
//! The site serves that file directly; this fetches it, and nothing else.
//!
//! One request per sync. There is no cheaper "has it changed?" to ask first:
//! the client treats a 304 as an error, and the body is the answer anyway.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Map;
use wikipethia_core::{CoreError, Document};

use crate::error::FetchError;
use crate::repo::{body_after_frontmatter, frontmatter};
use crate::sync::{Fetcher, SyncIntent, SyncState, SyncStats, write_atomic_bytes};

/// The one raw file, under `data/<source_id>/`.
const RAW_NAME: &str = "page.md";

pub struct PageAdapter {
    pub source_id: String,
    /// The Markdown file to fetch.
    pub url: String,
    /// The rendered page a citation should send a reader to.
    pub doc_url: String,
    /// The document's own publication date, `YYYY-MM-DD`, from the manifest.
    /// Declared rather than derived: a page states its date in prose if at
    /// all, and the file's last-commit date is when someone last fixed a
    /// typo — re-stamping a document with that corrupts supersession
    /// reasoning, the one thing every citation's date is trusted for.
    pub published: String,
    pub author: Option<String>,
    /// data/<source_id>.
    pub data_dir: PathBuf,
}

impl PageAdapter {
    fn raw_path(&self) -> PathBuf {
        self.data_dir.join(RAW_NAME)
    }

    /// `<source_id>/<last path segment of doc_url>` — the manifest validates
    /// that the segment exists.
    fn doc_id(&self) -> String {
        let slug = self
            .doc_url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        format!("{}/{slug}", self.source_id)
    }
}

impl crate::Adapter for PageAdapter {
    /// Errors when nothing has been synced, like the other kinds: an
    /// explicitly requested index of an unsynced source must not pass with
    /// "0 files".
    fn raw_files(&self) -> Result<Vec<PathBuf>, FetchError> {
        let path = self.raw_path();
        fs::metadata(&path).map_err(|source| FetchError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(vec![path])
    }

    fn sync(
        &self,
        fetcher: &mut dyn Fetcher,
        _opts: &SyncIntent,
        _state: &SyncState,
    ) -> Result<(SyncStats, Option<SyncState>), FetchError> {
        let text = fetcher.get_text(&self.url)?;
        // A moved endpoint answers 200 with the site's HTML shell, and an
        // empty body is no document either. Writing one over the page would
        // replace the source's only document with an error page at the next
        // index — refuse, and leave what is on disk alone.
        let head = text.trim_start().to_ascii_lowercase();
        if head.is_empty() || head.starts_with("<!doctype") || head.starts_with("<html") {
            return Err(FetchError::Shape(format!(
                "{} did not return a Markdown document (empty, or an HTML page) — \
                 the page has probably moved; check `url` for {:?} in sources.toml",
                self.url, self.source_id
            )));
        }

        let dest = self.raw_path();
        let mut stats = SyncStats::default();
        let known = dest.exists();
        if fs::read(&dest).is_ok_and(|existing| existing == text.as_bytes()) {
            stats.skipped += 1;
            return Ok((stats, None));
        }
        fs::create_dir_all(&self.data_dir).map_err(|source| FetchError::Io {
            path: self.data_dir.clone(),
            source,
        })?;
        write_atomic_bytes(&dest, text.as_bytes())?;
        if known {
            stats.updated += 1;
            eprintln!("update {RAW_NAME}");
        } else {
            stats.fetched += 1;
            eprintln!("fetch  {RAW_NAME}");
        }
        Ok((stats, None))
    }

    fn parse_file(&self, path: &Path) -> Result<Vec<Document>, CoreError> {
        let raw = fs::read_to_string(path)
            .map_err(|e| CoreError::Parse(format!("reading {}: {e}", path.display())))?;
        let id = self.doc_id();
        let title = frontmatter(&raw)
            .and_then(|fm| fm.get("title").cloned())
            .map(|title| title.trim_matches(['"', '\'']).trim().to_string())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| id.clone());
        Ok(vec![Document {
            id,
            source: self.source_id.clone(),
            url: self.doc_url.clone(),
            title,
            author: self.author.clone(),
            published: format!("{}T00:00:00Z", self.published),
            content: clean_markdown(body_after_frontmatter(&raw)),
            meta: Map::new(),
        }])
    }
}

/// The page's Markdown with its site scaffolding removed: heading anchors
/// (`## Our Role {#ii-our-role}`) and lines that are only HTML/JSX tags
/// (`<details>`, `<div className="…">`). Text inside such a line survives —
/// `<summary>SOURCE SEPPUKU LICENSE</summary>` keeps its words — and inline
/// HTML in a line of prose is left alone. Both would otherwise be indexed
/// as content: `{#ii-our-role}` and `className` are words to a tokenizer.
fn clean_markdown(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut blank_run = 0;
    let mut in_fence = false;
    for line in body.lines() {
        // A fenced block is quoted source: an HTML sample in one is content.
        let fence = line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~");
        if fence {
            in_fence = !in_fence;
        }
        let cleaned = if fence || in_fence {
            line.to_string()
        } else if line.trim_start().starts_with('<') {
            let text = strip_tags(line);
            if text.trim().is_empty() {
                // A dropped tag line must not leave the blank lines around
                // it stacked into a gap.
                continue;
            }
            text.trim().to_string()
        } else if line.starts_with('#') {
            strip_heading_anchor(line).to_string()
        } else {
            line.to_string()
        };
        if cleaned.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(&cleaned);
        out.push('\n');
    }
    out.trim().to_string()
}

/// `## Our Role {#ii-our-role}` → `## Our Role`.
fn strip_heading_anchor(line: &str) -> &str {
    let trimmed = line.trim_end();
    match trimmed.rfind(" {#") {
        Some(at) if trimmed.ends_with('}') => trimmed[..at].trim_end(),
        _ => line,
    }
}

/// `line` with its HTML/JSX tags removed. A tag is `<`, an optional `/`, a
/// name, then whitespace, `/` or `>` — which is what keeps a Markdown
/// autolink (`<https://example.org>`) and a bare `<` in prose out of it.
fn strip_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let name = after.strip_prefix('/').unwrap_or(after);
        let name_len = name
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(name.len());
        let is_tag = name.starts_with(|c: char| c.is_ascii_alphabetic())
            && name[name_len..].starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>');
        match after.find('>') {
            Some(close) if is_tag => rest = &after[close + 1..],
            _ => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_anchors_are_removed_and_other_braces_kept() {
        assert_eq!(strip_heading_anchor("## II. OUR ROLE {#ii-our-role}"), "## II. OUR ROLE");
        assert_eq!(strip_heading_anchor("### Limits {#v-limits}  "), "### Limits");
        assert_eq!(strip_heading_anchor("## A set {1, 2}"), "## A set {1, 2}");
        assert_eq!(strip_heading_anchor("## Plain"), "## Plain");
    }

    #[test]
    fn only_real_tags_are_stripped() {
        assert_eq!(strip_tags("<details>"), "");
        assert_eq!(strip_tags("<div className=\"text-sm [&_p]:mb-2\">"), "");
        assert_eq!(strip_tags("<summary>SOURCE SEPPUKU LICENSE</summary>"), "SOURCE SEPPUKU LICENSE");
        assert_eq!(strip_tags("<br/>"), "");
        // Not tags: an autolink, a comparison, an unclosed bracket.
        assert_eq!(strip_tags("<https://example.org>"), "<https://example.org>");
        assert_eq!(strip_tags("<1 ETH and > 0"), "<1 ETH and > 0");
        assert_eq!(strip_tags("<details"), "<details");
    }

    #[test]
    fn cleaning_leaves_fenced_code_alone() {
        let body = "```html\n<details>\n## not a heading {#x}\n```\n\n<details>\n";
        assert_eq!(clean_markdown(body), "```html\n<details>\n## not a heading {#x}\n```");
    }

    #[test]
    fn cleaning_drops_tag_lines_without_leaving_a_gap() {
        let body = "Before.\n\n<details>\n<summary>TITLE</summary>\n<div className=\"x\">\n\n\
                    > quoted\n\n</div>\n</details>\n\n## Next {#next}\n\nProse with <b>inline</b> html.\n";
        assert_eq!(
            clean_markdown(body),
            "Before.\n\nTITLE\n\n> quoted\n\n## Next\n\nProse with <b>inline</b> html."
        );
    }
}
