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
/// Definitions rendered per lookup_spec call before asking to narrow.
/// Above the number of forks the executable spec carries (24 and
/// growing): a cap below that would fill every slot with copies of one
/// identifier and never reach another document.
pub const MAX_DEFINITIONS: usize = 40;
/// One spec function's code before truncation points at the full document.
pub const FUNCTION_MAX_CHARS: usize = 2_500;

/// The one citation shape every citable unit carries:
/// `doc_id · author · date · tier · url`. The tier segment is omitted when
/// the document's source has no manifest row; the date segment is omitted
/// when the source supplied none (a blank " ·  · " reads like corruption).
pub fn citation(
    doc_id: &str,
    author: Option<&str>,
    published: &str,
    tier: Option<&str>,
    url: &str,
) -> String {
    let tier = tier.map(|t| format!("{t} · ")).unwrap_or_default();
    let date = match date(published) {
        "" => String::new(),
        d => format!("{d} · "),
    };
    format!(
        "{doc_id} · {} · {date}{tier}{url}",
        author.unwrap_or("unknown"),
    )
}

/// The date part of an ISO-8601 timestamp; whole string if it's shorter.
pub fn date(published: &str) -> &str {
    published.get(..10).unwrap_or(published)
}

/// The fork a spec document belongs to, as a short display label — the
/// segment after `forks/` (execution-specs) or after `specs/`
/// (consensus-specs), with `_features/<name>` kept whole because the segment
/// alone would label every experimental spec `_features`.
///
/// A display convenience for grouping, never a lookup key: `None` for
/// documents with no fork in their path (the EIPs and ERCs are fork-agnostic)
/// and the caller falls back to the doc id, which is always unambiguous. It
/// reads the same "a fork is a path segment" shape the `fork` parameter
/// already matches on, so the two cannot disagree about what a fork is.
pub fn fork_label(doc_id: &str) -> Option<&str> {
    let segments: Vec<&str> = doc_id.split('/').collect();
    // `forks` before `specs`, not "whichever comes first". A path can carry
    // both (`.../specs/forks/london/...`), and reading the earlier segment
    // there labels every fork `forks`. The explicit marker wins.
    let at = segments
        .iter()
        .position(|s| *s == "forks")
        .or_else(|| segments.iter().position(|s| *s == "specs"))?;
    match segments.get(at + 1)? {
        // specs/_features/eip6914/... — the fork name is the one below.
        s if *s == "_features" => segments.get(at + 2).copied(),
        s => Some(s),
    }
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

/// Absent query terms a search notice names before counting the rest.
pub const ABSENT_SHOWN: usize = 5;
/// Characters of one absent term shown. Agents quote whole guessed
/// sentences; the notice needs only enough to recognise which one.
pub const ABSENT_TERM_CHARS: usize = 60;

/// The line a search response opens with when terms of its query occur in
/// no indexed document (`Store::absent_terms`); `None` when every term
/// matched somewhere.
///
/// A prompt, not a log line: a full page of results reads as "keep going",
/// and agents were measured rewording one search until their budget ran
/// out. The claim is deliberately about the *terms* — it is true on
/// searches that go on to succeed too, so it must not tell the reader to
/// abandon the corpus, only that these words will not be found in it.
pub fn absent_notice(absent: &[String], any_word_matched: bool) -> Option<String> {
    if absent.is_empty() {
        return None;
    }
    let mut named = absent
        .iter()
        .take(ABSENT_SHOWN)
        .map(|term| format!("\"{}\"", excerpt(term, ABSENT_TERM_CHARS)))
        .collect::<Vec<_>>()
        .join(", ");
    if absent.len() > ABSENT_SHOWN {
        named.push_str(&format!(" and {} more", absent.len() - ABSENT_SHOWN));
    }
    if !any_word_matched {
        // The lexical arm had nothing to match: whatever follows came from
        // the vector side alone.
        return Some(format!(
            "Not in the corpus: none of this query's terms ({named}) appear in any \
             indexed document. The results below are nearest neighbours by embedding \
             only and are probably unrelated — do not treat them as matches; use \
             another source."
        ));
    }
    // Search ranks a quoted phrase as independent words, so an absent
    // phrase's own words can be what the results matched.
    let apart = if absent.iter().any(|term| term.contains(' ')) {
        ", or a quoted phrase's words apart"
    } else {
        ""
    };
    let (this, it, wording) = if absent.len() == 1 {
        ("this", "it", "this wording")
    } else {
        ("these", "them", "these words")
    };
    Some(format!(
        "Not in the corpus: {named} — no indexed document contains {this} as written. \
         The results below match only the query's other words{apart}. Rewording will \
         not surface {it}: check the spelling or the identifier's real name; if the \
         answer depends on a document that uses {wording}, it is not indexed here — \
         use another source."
    ))
}

/// Whole block up to `max_chars` characters; when cut, tells the model how
/// to get the rest. Correct ONLY where get_post_context genuinely shows
/// more than the caller did (search snippets, neighbor posts) — for text
/// that IS a get_post_context result, use [`window`], whose hint doesn't
/// point back at the call that produced the truncation.
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

/// A `max_chars` window into `text` starting at character `offset`, with
/// an accurate continuation hint. A 78k-char spec against an 8k cap is
/// ~90% unreachable without this; the old hint told the model to repeat
/// the exact call that produced the truncation.
pub fn window(text: &str, offset: usize, max_chars: usize, doc_id: &str) -> String {
    let total = text.chars().count();
    if offset >= total && offset != 0 {
        return format!(
            "[offset {offset} is past the end — the document is {total} characters; \
             call get_post_context with doc_id={doc_id} and a smaller offset]"
        );
    }
    let body: String = text.chars().skip(offset).take(max_chars).collect();
    let end = offset + body.chars().count();
    let prefix = if offset > 0 {
        format!("[…continuing from character {offset} of {total}]\n")
    } else {
        String::new()
    };
    if end >= total {
        return format!("{prefix}{body}");
    }
    format!(
        "{prefix}{}\n… [showing characters {offset}–{end} of {total} — call \
         get_post_context with doc_id={doc_id}, offset={end} for the next part]",
        body.trim_end()
    )
}

/// The spec status a repo-adapter document carries from its frontmatter
/// (Draft, Final, …). The one place that owns the meta key — every rendering
/// of the status goes through here so the key can't drift between surfaces.
pub fn spec_status(meta: &Map<String, Value>) -> Option<&str> {
    meta.get("status").and_then(Value::as_str)
}

/// "original post" for post_number 1, "reply #N" otherwise. A document with
/// no thread position falls back to its spec status ("status: Draft") — for
/// EIP/ERC documents that field decides questions recency cannot (which
/// hardfork is actually next), so it must be visible where models read
/// results. Empty when neither is present.
pub fn post_label(meta: &Map<String, Value>) -> String {
    match meta.get("post_number").and_then(Value::as_u64) {
        Some(1) => "original post".to_string(),
        Some(n) => format!("reply #{n}"),
        None => match spec_status(meta) {
            Some(s) => format!("status: {s}"),
            None => String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn absent_notice_is_silent_when_every_term_matched() {
        assert_eq!(absent_notice(&[], true), None);
        assert_eq!(absent_notice(&[], false), None);
    }

    #[test]
    fn absent_notice_names_the_terms_and_keeps_the_rest_of_the_query_in_play() {
        let one = absent_notice(&terms(&["incrementalist"]), true).unwrap();
        assert!(one.starts_with("Not in the corpus: \"incrementalist\" — "), "{one}");
        assert!(one.contains("contains this as written"), "{one}");
        assert!(one.contains("match only the query's other words."), "{one}");

        let two = absent_notice(&terms(&["incrementalist", "513,000"]), true).unwrap();
        assert!(two.contains("\"incrementalist\", \"513,000\""), "{two}");
        assert!(two.contains("contains these as written"), "{two}");
    }

    /// Search ORs the words of a quoted phrase, so an absent phrase is no
    /// evidence its words went unmatched — the line must not imply it.
    #[test]
    fn absent_notice_admits_a_phrases_words_may_have_matched_apart() {
        let out = absent_notice(&terms(&["Technical Pillar"]), true).unwrap();
        assert!(out.contains("other words, or a quoted phrase's words apart."), "{out}");
    }

    /// With no word matched there are no "other words": the results are
    /// vector neighbours only, and the line has to say that instead.
    #[test]
    fn absent_notice_says_so_when_no_word_matched_at_all() {
        let out = absent_notice(&terms(&["Verhaeghe"]), false).unwrap();
        assert!(out.contains("none of this query's terms (\"Verhaeghe\")"), "{out}");
        assert!(out.contains("nearest neighbours by embedding only"), "{out}");
        assert!(!out.contains("other words"), "{out}");
    }

    #[test]
    fn absent_notice_caps_the_list_and_the_length_of_each_term() {
        let many = terms(&["a1", "b2", "c3", "d4", "e5", "f6", "g7"]);
        let out = absent_notice(&many, true).unwrap();
        assert!(out.contains("\"e5\" and 2 more"), "{out}");
        assert!(!out.contains("f6"), "{out}");

        let sentence = "only a small number of people can understand how the protocol \
                        works and why it is secure";
        let out = absent_notice(&terms(&[sentence]), true).unwrap();
        assert!(out.contains("…\""), "long term is cut:\n{out}");
        assert!(!out.contains("why it is secure"), "{out}");
    }

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
        // A dateless source omits the segment instead of rendering "·  ·".
        assert_eq!(
            citation("id", Some("a"), "", Some("blog"), "u"),
            "id · a · blog · u"
        );
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
    fn window_pages_with_accurate_continuation_hints() {
        let text = "abcdefghij"; // 10 chars
        // Fits: returned whole, no hint.
        assert_eq!(window(text, 0, 10, "d/1"), "abcdefghij");
        // First page: hint names the exact next offset.
        let first = window(text, 0, 4, "d/1");
        assert!(first.starts_with("abcd"), "{first}");
        assert!(first.contains("offset=4"), "{first}");
        assert!(first.contains("characters 0–4 of 10"), "{first}");
        // Middle page: continuation banner plus next hint.
        let mid = window(text, 4, 4, "d/1");
        assert!(mid.contains("continuing from character 4"), "{mid}");
        assert!(mid.contains("efgh"), "{mid}");
        assert!(mid.contains("offset=8"), "{mid}");
        // Final page: no further hint.
        let last = window(text, 8, 4, "d/1");
        assert!(last.contains("ij"), "{last}");
        assert!(!last.contains("offset="), "{last}");
        // Past the end: instructive, names the document length.
        let past = window(text, 99, 4, "d/1");
        assert!(past.contains("past the end"), "{past}");
        assert!(past.contains("10 characters"), "{past}");
        // Multibyte safety: offsets are chars, not bytes.
        let math = "∑∆∇√∂∫≈≠≤≥";
        let page = window(math, 4, 3, "d/2");
        assert!(page.contains("∂∫≈"), "{page}");
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
        // Threadless documents surface their spec status instead.
        let mut spec = serde_json::Map::new();
        spec.insert("status".into(), json!("Draft"));
        assert_eq!(post_label(&spec), "status: Draft");
        // A thread position wins over a stray status field.
        meta.insert("status".into(), json!("Draft"));
        assert_eq!(post_label(&meta), "reply #7");
    }

    #[test]
    fn fork_labels_for_the_real_doc_id_shapes() {
        // execution-specs: the fork sits under `forks/`.
        assert_eq!(
            fork_label("executionspecs/src/ethereum/forks/cancun/vm/gas.py"),
            Some("cancun")
        );
        // consensus-specs: directly under `specs/`.
        assert_eq!(
            fork_label("consensusspecs/specs/electra/beacon-chain"),
            Some("electra")
        );
        // `_features` alone would label every experimental spec the same.
        assert_eq!(
            fork_label("consensusspecs/specs/_features/eip6914/beacon-chain"),
            Some("eip6914")
        );
        // Both markers present: `forks` wins, or every fork reads "forks".
        assert_eq!(fork_label("specs/forks/london/fork"), Some("london"));
        // Fork-agnostic documents have none, and the caller falls back to
        // the doc id rather than inventing a label.
        assert_eq!(fork_label("eips/eip-1271"), None);
        assert_eq!(fork_label("ethresearch/post/1"), None);
    }
}
