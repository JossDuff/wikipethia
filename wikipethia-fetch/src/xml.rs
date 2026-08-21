//! Minimal tag extraction for the two XML dialects the adapters consume:
//! RSS `<item>` lists (feeds) and atom `<entry>` lists (GitHub per-path
//! commit feeds). Deliberately not an XML parser — and deliberately not
//! `scraper`: html5ever treats `<link>` as a void element, which silently
//! destroys RSS link values. Feeds that stray beyond this shape fail
//! loudly at sync time, not silently at parse time.

/// The inner text of each `<tag>…</tag>` block, in order.
pub(crate) fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

/// The text of the first `<tag …>…</tag>` inside `block`, CDATA unwrapped
/// and the five XML entities unescaped. Attributes on the opening tag are
/// tolerated (atom's `<link href=…/>` self-closing form yields None — use
/// the attribute helper for those).
pub(crate) fn tag_text(block: &str, tag: &str) -> Option<String> {
    let open_plain = format!("<{tag}>");
    let open_attr = format!("<{tag} ");
    let close = format!("</{tag}>");
    let content_start = if let Some(pos) = block.find(&open_plain) {
        pos + open_plain.len()
    } else {
        let pos = block.find(&open_attr)?;
        pos + block[pos..].find('>')? + 1
    };
    let after = &block[content_start..];
    let end = after.find(&close)?;
    let raw = after[..end].trim();
    let raw = raw
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(raw);
    Some(unescape(raw.trim()))
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        // Ampersand last, or it would re-expand the others.
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_items_and_reads_tags() {
        let xml = "<rss><item><title>A &amp; B</title></item>\
                   <item><title><![CDATA[Raw <b>html</b>]]></title></item></rss>";
        let items = blocks(xml, "item");
        assert_eq!(items.len(), 2);
        assert_eq!(tag_text(items[0], "title").as_deref(), Some("A & B"));
        assert_eq!(tag_text(items[1], "title").as_deref(), Some("Raw <b>html</b>"));
        assert_eq!(tag_text(items[0], "missing"), None);
    }

    #[test]
    fn tolerates_attributes_on_the_opening_tag() {
        let entry = "<entry><updated type=\"iso\">2026-01-05T10:00:00Z</updated></entry>";
        assert_eq!(
            tag_text(entry, "updated").as_deref(),
            Some("2026-01-05T10:00:00Z")
        );
    }

    #[test]
    fn unterminated_blocks_are_dropped_not_panicked() {
        let xml = "<item><title>ok</title></item><item><title>trunc";
        assert_eq!(blocks(xml, "item").len(), 1);
    }
}
