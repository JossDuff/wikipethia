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

use corpus_core::{CoreError, Document};
use serde_json::{Map, Value, json};

use crate::error::FetchError;
use crate::html::html_to_text;
use crate::sync::{Fetcher, SyncStats, progress_note, write_atomic, write_atomic_bytes};
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
        None | Some("GMT") | Some("UT") | Some("Z") | Some("+0000") => "Z".to_string(),
        Some(offset) => offset.to_string(),
    };
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{sec:02}{zone}"
    ))
}

/// File-name-safe slug from the post's URL path.
fn slug(link: &str) -> String {
    let path = link
        .split_once("://")
        .map_or(link, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    let path = path.strip_suffix(".html").unwrap_or(path);
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
    let path = link
        .split_once("://")
        .map_or(link, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    let path = path.strip_suffix(".html").unwrap_or(path);
    format!("{source_id}/{}", path.trim_matches('/'))
}

/// A description that is the article itself, not a teaser: block-level
/// markup and real length.
fn is_full_content(description: &str) -> bool {
    description.len() > 1000 && (description.contains("<p") || description.contains("<div"))
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

    fn sync(
        &self,
        fetcher: &mut dyn Fetcher,
        limit: Option<usize>,
    ) -> Result<SyncStats, FetchError> {
        let feed_xml = fetcher.get_text(&self.feed_url)?;
        fs::create_dir_all(self.data_dir.join("posts")).map_err(|source| FetchError::Io {
            path: self.data_dir.join("posts"),
            source,
        })?;
        write_atomic_bytes(&self.data_dir.join("feed.xml"), feed_xml.as_bytes())?;

        let items = items(&feed_xml);
        let total = items.len() as u64;
        let started = Instant::now();
        let mut stats = SyncStats::default();
        for item in items {
            if limit.is_some_and(|l| stats.fetched + stats.skipped >= l) {
                break;
            }
            let link = rebase(&self.feed_url, &item.link);
            let slug = slug(&link);
            if slug.is_empty() {
                eprintln!("skip: unusable link {link:?}");
                continue;
            }
            let dest = self.data_dir.join("posts").join(format!("{slug}.json"));
            if dest.exists() {
                stats.skipped += 1;
                continue;
            }
            // Full-content descriptions (EF-blog style) save a fetch; teaser
            // or empty descriptions (vitalik style) need the page itself.
            let html = match &item.description {
                Some(d) if is_full_content(d) => d.clone(),
                _ => fetcher.get_text(&link)?,
            };
            write_atomic(
                &dest,
                &json!({
                    "title": item.title,
                    "url": link,
                    "published": item.published.clone().unwrap_or_default(),
                    "author": item.author,
                    "html": html,
                }),
            )?;
            stats.fetched += 1;
            eprintln!(
                "fetch {slug}{}",
                progress_note(Some(total), &stats, started.elapsed())
            );
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
            return Err(CoreError::Parse(format!("no article text in {url}")));
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
