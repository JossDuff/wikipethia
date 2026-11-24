//! Page adapter: one source is one Markdown document at one URL.
//!
//! For a canonical document that is none of the other kinds — not a forum,
//! not a feed, and not worth a repository snapshot. The first one is the EF
//! Mandate: its Markdown lives in `ethereum/ethereum-org-website`, a 1.6 GB
//! repository that moves several times a day, so the repo adapter would
//! pull the whole tarball into memory on every update for one 58 KB file.
//! The site serves that file directly; this fetches it, and nothing else.
//!
//! **Fetched once.** A page is a published document, not a stream: the
//! checkpoint records the URL it came from, and while that still matches the
//! manifest a sync makes no request at all. The checkpoint lives in the
//! corpus database, so someone who downloads a published corpus never
//! fetches the page either. Editing `url` in sources.toml, or `--force`,
//! fetches it again. The price is stated, not hidden: a correction made
//! upstream stays invisible until one of those two.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Map;
use wikipethia_core::{CoreError, Document};

use crate::error::FetchError;
use crate::repo::{body_after_frontmatter, frontmatter};
use crate::sync::{Fetcher, SyncIntent, SyncState, SyncStats, write_atomic_bytes};

/// The one raw file, under `data/<source_id>/`.
const RAW_NAME: &str = "page.md";

/// A reply whose cleaned body is shorter than this is not the document.
/// The one fetch is kept, so it has to be right: "Not Found" or a JSON error
/// served with a 200 would otherwise be indexed under the page's title and
/// never fetched again.
const MIN_BODY_CHARS: usize = 200;

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

/// The name of a page's document: the last path segment of its `doc_url`
/// (`…/foundation/mandate/` → `mandate`), which must be fit for an id. The
/// document id is `<source_id>/<slug>`. `None` when the URL has no such
/// segment — the manifest refuses those, through this same function.
pub fn slug(doc_url: &str) -> Option<&str> {
    let rest = doc_url
        .strip_prefix("https://")
        .or_else(|| doc_url.strip_prefix("http://"))?;
    let (_host, path) = rest.trim_end_matches('/').split_once('/')?;
    let slug = path.rsplit('/').next()?;
    let fit = !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    fit.then_some(slug)
}

impl PageAdapter {
    fn raw_path(&self) -> PathBuf {
        self.data_dir.join(RAW_NAME)
    }

    fn doc_id(&self) -> String {
        format!("{}/{}", self.source_id, slug(&self.doc_url).unwrap_or_default())
    }

    /// Refuse a reply that is not the document. A moved endpoint answers 200
    /// with the site's HTML shell, a proxy with a one-line error; either
    /// would be written, indexed, and — fetched once — kept.
    fn refuse_unless_document(&self, text: &str) -> Result<(), FetchError> {
        let body = text.trim_start_matches('\u{feff}');
        let opening = body.chars().take(512).collect::<String>().to_ascii_lowercase();
        let saw = if body.trim().is_empty() {
            "an empty reply"
        } else if opening.contains("<!doctype html") || opening.contains("<html") {
            "an HTML page"
        } else if clean_markdown(body_after_frontmatter(body)).chars().count() < MIN_BODY_CHARS {
            "too little text to be a document"
        } else {
            return Ok(());
        };
        Err(FetchError::Shape(format!(
            "{} returned {saw}, not a Markdown document — refusing to keep it as \
             {:?}. The page has probably moved; check `url` in sources.toml",
            self.url, self.source_id
        )))
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
        opts: &SyncIntent,
        state: &SyncState,
    ) -> Result<(SyncStats, Option<SyncState>), FetchError> {
        let mut stats = SyncStats::default();
        // Already on record from this URL: no request. An empty `page_url`
        // (never fetched, or a checkpoint that could not be read) never
        // equals a manifest URL, so it always fetches.
        if !opts.force && state.page_url == self.url {
            stats.skipped += 1;
            return Ok((stats, None));
        }

        let text = fetcher.get_text(&self.url)?;
        self.refuse_unless_document(&text)?;

        let dest = self.raw_path();
        let known = dest.exists();
        if fs::read(&dest).is_ok_and(|existing| existing == text.as_bytes()) {
            stats.skipped += 1;
        } else {
            fs::create_dir_all(&self.data_dir).map_err(|source| FetchError::Io {
                path: self.data_dir.clone(),
                source,
            })?;
            write_atomic_bytes(&dest, text.as_bytes())?;
            if known {
                stats.updated += 1;
                eprintln!("update {}/{RAW_NAME}", self.source_id);
            } else {
                stats.fetched += 1;
                eprintln!("fetch  {}/{RAW_NAME}", self.source_id);
            }
        }
        // Recorded last, and only after the file is in place: this is the
        // claim that stops every later fetch.
        let fetched = SyncState {
            page_url: self.url.clone(),
            ..state.clone()
        };
        Ok((stats, Some(fetched)))
    }

    fn parse_file(&self, path: &Path) -> Result<Vec<Document>, CoreError> {
        let raw = fs::read_to_string(path)
            .map_err(|e| CoreError::Parse(format!("reading {}: {e}", path.display())))?;
        let raw = raw.trim_start_matches('\u{feff}');
        let id = self.doc_id();
        let title = frontmatter(raw)
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
            content: clean_markdown(body_after_frontmatter(raw)),
            meta: Map::new(),
        }])
    }
}

/// The page's Markdown with its site scaffolding removed: heading anchors
/// (`## Our Role {#ii-our-role}`) and lines that are nothing but HTML/JSX
/// tags (`<details>`, `<div className="…">`). One element wrapping plain
/// text keeps the text — `<summary>SOURCE SEPPUKU LICENSE</summary>`. Any
/// other line is left exactly as written, inline HTML included: a link's
/// target and an image's description are content. Fenced code is quoted
/// source and is never touched.
///
/// The scaffolding would otherwise be indexed: `{#ii-our-role}` and
/// `className` are words to a tokenizer.
fn clean_markdown(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut blank_run = 0;
    // The open fenced block: its marker and how many of it opened the block.
    // Only a line of that marker, at least that long, closes it — so a `~~~`
    // inside a ``` block, or a three-backtick sample inside a four-backtick
    // one, cannot end it early and leave the rest of the page uncleaned.
    let mut fence: Option<(char, usize)> = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'));
        let run = marker.map_or(0, |m| trimmed.chars().take_while(|c| *c == m).count());
        let in_code = match (fence, marker) {
            (Some((open, len)), Some(m)) if m == open && run >= len && trimmed[run..].trim().is_empty() => {
                fence = None;
                true
            }
            (Some(_), _) => true,
            // An opening backtick fence cannot have a backtick after it:
            // ```inline``` at the start of a line of prose is a code span.
            (None, Some(m)) if run >= 3 && !(m == '`' && trimmed[run..].contains('`')) => {
                fence = Some((m, run));
                true
            }
            (None, _) => false,
        };
        let cleaned = if in_code {
            line.to_string()
        } else if trimmed.starts_with('<') {
            if strip_tags(line).trim().is_empty() {
                // A dropped tag line must not leave the blank lines around
                // it stacked into a gap.
                continue;
            }
            match wrapped_text(line) {
                Some(text) => text.to_string(),
                None => line.to_string(),
            }
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

/// Where the tag whose body is `body` (the text after its `<`) ends: the
/// first `>` outside a quoted attribute and outside a `{…}` expression. A
/// class name like `[&>p]:mb-2` and an arrow function both carry a `>` that
/// closes nothing.
fn tag_end(body: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    for (at, c) in body.char_indices() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') => depth = depth.saturating_sub(1),
            (None, '>') if depth == 0 => return Some(at),
            (None, _) => {}
        }
    }
    None
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
        match tag_end(after) {
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

/// The text of a line that is one element around plain text:
/// `<summary>Title</summary>` → `Title`. `None` for anything else — prose
/// that merely starts or ends with a tag is not this.
fn wrapped_text(line: &str) -> Option<&str> {
    let line = line.trim();
    let open_len = tag_end(line.strip_prefix('<')?)? + 2;
    let (inner, close) = line.get(open_len..)?.rsplit_once("</")?;
    let closes = close.strip_suffix('>')?;
    let plain = !inner.contains(['<', '>']) && !closes.contains(['<', '>']);
    plain.then_some(inner.trim()).filter(|text| !text.is_empty())
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

    /// The first `>` is not always the end of the tag. The site's wrapper
    /// uses `[&_p]` today; its child-selector form `[&>p]` would have
    /// leaked `p]:mb-2">` into the document.
    #[test]
    fn a_tag_ends_outside_its_quotes_and_braces() {
        assert_eq!(strip_tags("<div className=\"text-sm [&>p]:mb-2\">"), "");
        assert_eq!(strip_tags("<Button onClick={() => go()}>Go</Button>"), "Go");
        assert_eq!(strip_tags("<a title='a > b'>x</a>"), "x");
        assert_eq!(
            clean_markdown("Before.\n\n<div className=\"text-sm [&>p]:mb-2\">\n\nInside.\n\n</div>\n"),
            "Before.\n\nInside."
        );
    }

    /// Only a line that is nothing but tags is scaffolding. Prose that
    /// starts with a tag keeps its link target and its image description.
    #[test]
    fn prose_that_starts_with_a_tag_is_left_as_written() {
        for line in [
            "<a href=\"https://x.org/spec\">The spec</a> says <b>no</b>.",
            "<b>Note</b> that this holds, see <a href=\"/x\">here</a>",
            "<https://example.org> is the canonical copy.",
        ] {
            assert_eq!(clean_markdown(line), line);
        }
        // One element around plain text gives up its text, nothing else does.
        assert_eq!(wrapped_text("<summary>SOURCE SEPPUKU LICENSE</summary>"), Some("SOURCE SEPPUKU LICENSE"));
        assert_eq!(wrapped_text("  <b>Bold</b>  "), Some("Bold"));
        assert_eq!(wrapped_text("<a href=\"/x\">The spec</a> says <b>no</b>"), None);
        assert_eq!(wrapped_text("<details>"), None);
    }

    /// A block closes only on its own marker, at least as long as the one
    /// that opened it. One stray marker used to flip cleaning off for the
    /// rest of the document.
    #[test]
    fn a_fence_closes_only_on_its_own_marker() {
        // `~~~` inside a backtick block is content, not a closer.
        let body = "```\n~~~\n<details>\n```\n\n<details>\nAfter {#x}\n\n## H {#h}\n";
        assert_eq!(clean_markdown(body), "```\n~~~\n<details>\n```\n\nAfter {#x}\n\n## H");
        // Four backticks wrapping a three-backtick sample: the sample's
        // lines survive, and cleaning resumes after the outer close.
        let body = "````md\n```html\n<div>\n```\n````\n<div>\n\n## H {#h}\n";
        assert_eq!(clean_markdown(body), "````md\n```html\n<div>\n```\n````\n\n## H");
        // A line of prose that opens with a code span is not a fence.
        let body = "```inline``` starts this line.\n\n<details>\n## H {#h}\n";
        assert_eq!(clean_markdown(body), "```inline``` starts this line.\n\n## H");
    }

    #[test]
    fn the_slug_is_the_last_path_segment_and_must_fit_an_id() {
        assert_eq!(slug("https://ethereum.org/foundation/mandate/"), Some("mandate"));
        assert_eq!(slug("https://ethereum.org/foundation/mandate"), Some("mandate"));
        assert_eq!(slug("https://ethereum.org/"), None);
        assert_eq!(slug("https://ethereum.org"), None);
        assert_eq!(slug("https://x.org/a/Mandate.html"), None);
        assert_eq!(slug("ethereum.org/foundation/mandate"), None);
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
