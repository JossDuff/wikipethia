//! Pure text-shaping helpers for tool output. Everything a model reads
//! comes through here, so the shapes are tested.

use serde_json::{Map, Value};

/// Full post shown for the requested/original post before truncating.
pub const OP_MAX_CHARS: usize = 8_000;
/// Neighbor posts in a context window are capped tighter.
pub const NEIGHBOR_MAX_CHARS: usize = 1_500;
/// One-line reply-index excerpts.
pub const INDEX_EXCERPT_CHARS: usize = 90;
/// Excerpts under similarity results.
pub const RESULT_EXCERPT_CHARS: usize = 150;
/// Replies per get_topic page.
pub const REPLY_PAGE: usize = 50;
/// Hard cap on any limit parameter.
pub const MAX_LIMIT: usize = 50;
/// Hard cap on get_post_context before/after.
pub const MAX_CONTEXT: usize = 10;

/// The one citation shape every citable unit carries:
/// `doc_id · author · date · tier · url`. The tier segment is omitted when
/// the document's source has no manifest row.
pub fn citation(
    doc_id: &str,
    author: Option<&str>,
    published: &str,
    tier: Option<&str>,
    url: &str,
) -> String {
    let tier = tier.map(|t| format!("{t} · ")).unwrap_or_default();
    format!(
        "{doc_id} · {} · {} · {tier}{url}",
        author.unwrap_or("unknown"),
        date(published)
    )
}

/// The date part of an ISO-8601 timestamp; whole string if it's shorter.
pub fn date(published: &str) -> &str {
    published.get(..10).unwrap_or(published)
}

/// One line, at most `max_chars` characters, `…` when shortened.
pub fn excerpt(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let cut: String = flat.chars().take(max_chars).collect();
    format!("{}…", cut.trim_end())
}

/// Whole block up to `max_chars` characters; when cut, tells the model how
/// to get the rest.
pub fn truncate_block(text: &str, max_chars: usize, doc_id: &str) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!(
        "{}\n… [truncated — call get_post_context with doc_id={doc_id} for the full text]",
        cut.trim_end()
    )
}

/// "original post" for post_number 1, "reply #N" otherwise; a document with
/// no thread position (non-Discourse sources, later) gets an empty label.
pub fn post_label(meta: &Map<String, Value>) -> String {
    match meta.get("post_number").and_then(Value::as_u64) {
        Some(1) => "original post".to_string(),
        Some(n) => format!("reply #{n}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn citation_shape_with_and_without_tier() {
        assert_eq!(
            citation(
                "ethresearch/post/1",
                Some("vb"),
                "2018-01-03T22:07:33Z",
                Some("research"),
                "https://x"
            ),
            "ethresearch/post/1 · vb · 2018-01-03 · research · https://x"
        );
        assert_eq!(
            citation("ethresearch/post/1", Some("vb"), "2018-01-03T22:07:33Z", None, "https://x"),
            "ethresearch/post/1 · vb · 2018-01-03 · https://x"
        );
        assert!(citation("id", None, "2018-01-03", None, "u").contains("unknown"));
    }

    #[test]
    fn date_falls_back_on_short_input() {
        assert_eq!(date("2021"), "2021");
        assert_eq!(date("2021-06-26T00:00:00Z"), "2021-06-26");
    }

    #[test]
    fn excerpt_flattens_and_respects_char_boundaries() {
        assert_eq!(excerpt("a\nb\n\nc", 90), "a b c");
        // Multibyte content must not split a char: math and box glyphs.
        let math = "∑∆∇ ".repeat(40);
        let cut = excerpt(&math, 10);
        assert!(cut.chars().count() <= 11); // 10 + ellipsis
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn truncate_block_appends_recovery_hint() {
        let long = "x".repeat(20);
        let cut = truncate_block(&long, 10, "some/id");
        assert!(cut.contains("doc_id=some/id"));
        assert_eq!(truncate_block("short", 10, "id"), "short");
    }

    #[test]
    fn post_labels() {
        let mut meta = serde_json::Map::new();
        meta.insert("post_number".into(), json!(1));
        assert_eq!(post_label(&meta), "original post");
        meta.insert("post_number".into(), json!(7));
        assert_eq!(post_label(&meta), "reply #7");
        assert_eq!(post_label(&serde_json::Map::new()), "");
    }
}
