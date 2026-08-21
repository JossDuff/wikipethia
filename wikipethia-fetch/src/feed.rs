//! Any blog with an RSS feed. The feed supplies metadata (title, canonical
//! link, date, author); the article body comes from the feed's own
//! `description` when it carries full content, else from fetching the post
//! page. Raw storage is one JSON wrapper per post so the feed metadata
//! travels with the HTML into `parse`:
//!   feed.xml                 the last-fetched feed, for debugging
//!   posts/<slug>.json        { title, url, published, author, html }

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use wikipethia_core::{CoreError, Document};
use serde_json::{Map, Value, json};

use crate::error::FetchError;
use crate::html::html_to_text;
use crate::sync::{
    Fetcher, SyncIntent, SyncStats, progress_note, write_atomic, write_atomic_bytes,
};

/// How far down a feed a routine sync looks for edits. Feeds are newest
/// first, so this is "the newest N articles".
///
/// Corrections land within days of publication; an article untouched for a
/// year is not about to change. `--full` covers the rest when it matters.
const RECHECK_RECENT: usize = 30;
use crate::xml;

pub struct FeedAdapter {
    pub source_id: String,
    pub feed_url: String,
    /// data/<source_id>.
    pub data_dir: PathBuf,
}

struct FeedItem {
    title: String,
    link: String,
    published: Option<String>,
    author: Option<String>,
    description: Option<String>,
}

fn items(xml_text: &str) -> Vec<FeedItem> {
    xml::blocks(xml_text, "item")
        .into_iter()
        .filter_map(|block| {
            Some(FeedItem {
                title: xml::tag_text(block, "title")?,
                link: xml::tag_text(block, "link")?,
                published: xml::tag_text(block, "pubDate").and_then(|d| rfc2822_to_iso(&d)),
                author: xml::tag_text(block, "author")
                    .or_else(|| xml::tag_text(block, "dc:creator")),
                description: xml::tag_text(block, "description").filter(|d| !d.is_empty()),
            })
        })
        .collect()
}

/// "Mon, 29 Jun 2026 14:03:00 GMT" → "2026-06-29T14:03:00Z". Hand-rolled:
/// both target feeds emit GMT, and within one feed the format is constant,
/// so lexicographic date ordering holds. Nonzero offsets are preserved
/// verbatim rather than normalized.
fn rfc2822_to_iso(s: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let s = s.trim();
    // Optional weekday prefix "Mon, ".
    let rest = s.split_once(", ").map_or(s, |(_, r)| r);
    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = MONTHS.iter().position(|m| *m == month_name)? + 1;
    let year: u32 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let (h, m, sec) = {
        let mut t = time.split(':');
        (
            t.next()?.parse::<u32>().ok()?,
            t.next()?.parse::<u32>().ok()?,
            t.next().and_then(|x| x.parse::<u32>().ok()).unwrap_or(0),
        )
    };
    let zone = match parts.next() {
        None | Some("GMT") | Some("UT") | Some("UTC") | Some("Z") | Some("+0000") => {
            "Z".to_string()
        }
        Some("EST") => "-05:00".into(),
        Some("EDT") => "-04:00".into(),
        Some("CST") => "-06:00".into(),
        Some("CDT") => "-05:00".into(),
        Some("MST") => "-07:00".into(),
        Some("MDT") => "-06:00".into(),
        Some("PST") => "-08:00".into(),
        Some("PDT") => "-07:00".into(),
        // Numeric offsets keep ISO shape; anything else would produce a
        // malformed timestamp — better no date than a corrupt one.
        Some(offset) if offset.starts_with('+') || offset.starts_with('-') => {
            offset.to_string()
        }
        Some(_) => return None,
    };
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{sec:02}{zone}"
    ))
}

/// The path component of a URL, without scheme/host, ".html" stripped.
fn url_path(link: &str) -> &str {
    let path = link
        .split_once("://")
        .map_or(link, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    path.strip_suffix(".html").unwrap_or(path)
}

/// File-name-safe slug from the post's URL path.
fn slug(link: &str) -> String {
    let path = url_path(link);
    let slug: String = path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug.trim_matches('-').to_string()
}

/// Doc id: source id + the URL path, ".html" stripped.
fn doc_id(source_id: &str, link: &str) -> String {
    format!("{source_id}/{}", url_path(link).trim_matches('/'))
}

/// A description that is the article itself, not a teaser: block-level
/// markup and real length.
fn is_full_content(description: &str) -> bool {
    description.len() > 1000 && (description.contains("<p") || description.contains("<div"))
}

/// Whether this run will look at an item at all, as opposed to leaving a
/// stored copy alone because comparing it would cost a request nobody asked
/// for. The single home of the skip rule: the sync loop and the progress
/// denominator both read it, so they cannot drift into disagreeing about how
/// much work a run contains.
///
/// Never skips an item with no local copy — discovery is unbounded at any
/// depth in the feed, and only edit detection is windowed.
fn will_examine(
    known: bool,
    has_full_content: bool,
    position: usize,
    opts: &SyncIntent,
) -> bool {
    // A full-content description carries any correction in the XML already
    // parsed, so comparing it costs nothing and there is no reason to go on
    // serving stale text. Neither real feed does this today (see `sync`).
    let needs_a_request = !has_full_content;
    !(known && needs_a_request && !opts.full && !opts.force && position >= RECHECK_RECENT)
}

/// Rebase an item link onto the feed's own origin. Feeds that moved domains
/// keep emitting legacy hostnames (vitalik.ca is dead DNS; the live site is
/// vitalik.eth.limo) — the feed's origin is the one address we know is
/// alive, because we just fetched the feed from it.
fn rebase(feed_url: &str, link: &str) -> String {
    let origin_end = feed_url
        .find("://")
        .and_then(|scheme| feed_url[scheme + 3..].find('/').map(|p| scheme + 3 + p));
    let Some(origin_end) = origin_end else {
        return link.to_string();
    };
    let origin = &feed_url[..origin_end];
    let path = link
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, p)| p)
        .unwrap_or("");
    format!("{origin}/{path}")
}

impl crate::Adapter for FeedAdapter {
    fn raw_files(&self) -> Result<Vec<PathBuf>, FetchError> {
        let posts_dir = self.data_dir.join("posts");
        let mut paths: Vec<PathBuf> = fs::read_dir(&posts_dir)
            .map_err(|source| FetchError::Io {
                path: posts_dir.clone(),
                source,
            })?
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        Ok(paths)
    }

    /// Re-derive recent items and write only what changed.
    ///
    /// The old rule — a file on disk means done — meant an article corrected
    /// after publication kept its first version forever. There is no cheaper
    /// signal to use instead: `items` parses RSS 2.0, and neither real feed
    /// carries a per-item `updated`, so the content itself is the only thing
    /// that can be compared.
    ///
    /// For a feed whose descriptions are teasers that means one request per
    /// item compared, and **both real feeds turn out to be full archives
    /// rather than truncated windows** — 634 items for the EF blog, 174 for
    /// vitalik.eth.limo. Comparing every one of them would be minutes of
    /// rate-limited fetching on every run, for ever.
    ///
    /// **Both** feeds, measured 2026-08-19: an earlier version of this comment
    /// claimed the EF blog's descriptions carried whole articles and that
    /// comparing it was therefore free. They do not — all 634 are ~330-char
    /// teasers, so [`is_full_content`] fires for neither real feed today and
    /// the EF blog costs the same 30 requests per routine sync that vitalik
    /// does. The branch is kept because it is a property of the feed, not of
    /// the adapter, and a feed that starts serving full content should stop
    /// costing requests the moment it does.
    ///
    /// So a routine sync compares the newest [`RECHECK_RECENT`] items, where
    /// corrections realistically land, and `--full` compares the lot. An item
    /// with no local copy is always fetched wherever it sits in the feed, so
    /// this bounds edit detection only — never discovery.
    fn sync(&self, fetcher: &mut dyn Fetcher, opts: &SyncIntent) -> Result<SyncStats, FetchError> {
        let limit = opts.limit;
        let feed_xml = fetcher.get_text(&self.feed_url)?;
        fs::create_dir_all(self.data_dir.join("posts")).map_err(|source| FetchError::Io {
            path: self.data_dir.join("posts"),
            source,
        })?;
        write_atomic_bytes(&self.data_dir.join("feed.xml"), feed_xml.as_bytes())?;

        let items = items(&feed_xml);
        let posts_dir = self.data_dir.join("posts");
        // How many items this run will actually look at — the feed's length
        // is the wrong denominator for a routine sync, which examines the
        // newest RECHECK_RECENT plus whatever it has never seen and leaves
        // the rest untouched. Feeding `progress_note` the full 634 made it
        // project the observed pace across ~600 items that were about to be
        // skipped in microseconds: "~1h36m left" for ~11s of real work.
        // One `stat` per item, no requests.
        let planned = items
            .iter()
            .enumerate()
            .filter(|(position, item)| {
                let slug = slug(&rebase(&self.feed_url, &item.link));
                !slug.is_empty()
                    && will_examine(
                        posts_dir.join(format!("{slug}.json")).exists(),
                        item.description.as_deref().is_some_and(is_full_content),
                        *position,
                        opts,
                    )
            })
            .count() as u64;
        let started = Instant::now();
        let mut stats = SyncStats::default();
        // Skips of items this run actually compared, as opposed to the
        // out-of-window ones it declined to look at. Only the former belong
        // in the progress numerator, or it would climb past `planned`.
        let mut examined_skips = 0usize;
        let mut failed = 0usize;
        for (position, item) in items.into_iter().enumerate() {
            if limit.is_some_and(|l| stats.processed() >= l) {
                break;
            }
            let link = rebase(&self.feed_url, &item.link);
            let slug = slug(&link);
            if slug.is_empty() {
                eprintln!("skip: unusable link {link:?}");
                continue;
            }
            let dest = posts_dir.join(format!("{slug}.json"));
            let known = dest.exists();
            if !will_examine(
                known,
                item.description.as_deref().is_some_and(is_full_content),
                position,
                opts,
            ) {
                stats.skipped += 1;
                continue;
            }
            // The stored wrapper is the comparison basis — no sidecar state,
            // and an unparseable one simply reads as absent and gets rewritten.
            let stored: Option<Value> = if opts.force {
                None
            } else {
                fs::read_to_string(&dest)
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
            };
            // Full-content descriptions (EF-blog style) save a fetch; teaser
            // or empty descriptions (vitalik style) need the page itself.
            // The rebased URL is tried first (feeds outlive their domains);
            // the feed's original link is the fallback for items that
            // genuinely live elsewhere. A page that is dead under both is
            // warned and skipped — one 404 must not starve the rest of the
            // feed, and the missing file retries next sync.
            let fetched = match &item.description {
                Some(d) if is_full_content(d) => Some((d.clone(), link.clone())),
                _ => match fetcher.get_text(&link) {
                    Ok(html) => Some((html, link.clone())),
                    Err(rebased_err) if item.link != link => {
                        match fetcher.get_text(&item.link) {
                            Ok(html) => Some((html, item.link.clone())),
                            Err(_) => {
                                eprintln!("warn: skipping {slug}: {rebased_err}");
                                failed += 1;
                                None
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("warn: skipping {slug}: {err}");
                        failed += 1;
                        None
                    }
                },
            };
            let Some((html, final_url)) = fetched else {
                continue;
            };
            let wrapper = json!({
                "title": item.title,
                "url": final_url,
                "published": item.published.clone().unwrap_or_default(),
                "author": item.author,
                "html": html,
            });
            // Compare the whole wrapper, not just the article body: a
            // corrected title, a fixed byline, and a re-dated post are all
            // edits the corpus should pick up, and they cost nothing extra
            // to notice here.
            if stored.as_ref() == Some(&wrapper) {
                stats.skipped += 1;
                examined_skips += 1;
                continue;
            }
            write_atomic(&dest, &wrapper)?;
            if item.published.is_none() {
                // Empty published silently breaks the "weigh the dates"
                // retrieval invariant — make it visible at sync time.
                eprintln!("warn: {slug} has no usable pubDate — published will be empty");
            }
            if known {
                stats.updated += 1;
            } else {
                stats.fetched += 1;
            }
            // Progress counts only what this run examined, against what it
            // planned to examine — `stats.skipped` also carries the
            // out-of-window items, which would push the numerator past
            // `planned` and print percentages over 100.
            let progress = SyncStats {
                fetched: stats.fetched,
                updated: stats.updated,
                skipped: examined_skips,
                ..SyncStats::default()
            };
            eprintln!(
                "{} {slug}{}",
                if known { "update" } else { "fetch " },
                progress_note(Some(planned), &progress, started.elapsed())
            );
        }
        if failed > 0 {
            eprintln!("warn: {failed} item(s) skipped on dead links — they retry next sync");
        }
        Ok(stats)
    }

    fn parse(&self, raw: &Value) -> Result<Vec<Document>, CoreError> {
        let field = |key: &str| raw[key].as_str().map(str::to_string);
        let (Some(title), Some(url), Some(html)) = (field("title"), field("url"), field("html"))
        else {
            return Err(CoreError::Parse(
                "feed post wrapper missing title/url/html".into(),
            ));
        };
        let content = html_to_text(&html);
        if content.trim().is_empty() {
            // A page with no extractable article (video-only posts on the
            // early EF blog) has nothing to index — skip, don't fail.
            eprintln!("warn: no article text in {url} — skipping");
            return Ok(Vec::new());
        }
        Ok(vec![Document {
            id: doc_id(&self.source_id, &url),
            source: self.source_id.clone(),
            url,
            title,
            author: field("author"),
            published: field("published").unwrap_or_default(),
            content,
            meta: Map::new(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2822_conversions() {
        assert_eq!(
            rfc2822_to_iso("Mon, 29 Jun 2026 14:03:00 GMT").as_deref(),
            Some("2026-06-29T14:03:00Z")
        );
        assert_eq!(
            rfc2822_to_iso("Tue, 5 Aug 2025 00:00:00 +0000").as_deref(),
            Some("2025-08-05T00:00:00Z")
        );
        // No weekday, no seconds field tolerated.
        assert_eq!(
            rfc2822_to_iso("29 Jun 2026 14:03 GMT").as_deref(),
            Some("2026-06-29T14:03:00Z")
        );
        assert_eq!(rfc2822_to_iso("not a date"), None);
    }

    #[test]
    fn slugs_and_doc_ids_come_from_the_url_path() {
        let link = "https://vitalik.eth.limo/general/2026/06/29/obfuscation1.html";
        assert_eq!(slug(link), "general-2026-06-29-obfuscation1");
        assert_eq!(
            doc_id("vitalik", link),
            "vitalik/general/2026/06/29/obfuscation1"
        );
    }

    #[test]
    fn full_content_detection() {
        assert!(!is_full_content("short teaser"));
        assert!(!is_full_content(&"plain text without markup ".repeat(100)));
        assert!(is_full_content(&format!(
            "<p>{}</p>",
            "actual article body ".repeat(100)
        )));
    }
}
