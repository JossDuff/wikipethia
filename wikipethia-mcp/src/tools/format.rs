//! Pure text-shaping helpers for tool output. Everything a model reads
//! comes through here, so the shapes are tested.

use serde_json::{Map, Value};
use wikipethia_core::UnmatchedTerm;

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

/// Unmatched query terms a search notice names, per kind, before counting
/// the rest.
pub const ABSENT_SHOWN: usize = 5;
/// Characters of one unmatched term shown. Agents quote whole guessed
/// sentences; the notice needs only enough to recognise which one.
pub const ABSENT_TERM_CHARS: usize = 60;

/// The lines a search response carries when terms of its query matched
/// nothing as written (`Store::unmatched_terms`); `None` when every term
/// matched somewhere.
///
/// A prompt, not a log line: a full page of results reads as "keep going",
/// and agents were measured rewording one search until their budget ran
/// out. Two claims of different strength, kept apart on purpose:
///
/// - "Not in the corpus" — a word that is no token of any indexed chunk.
///   Exact as stated, which is why it is stated as "a whole word".
/// - "No exact match" — every word occurs, never as this sequence. Weaker:
///   it is also what two indexed identifiers joined by a slash look like,
///   and a real phrase split by a link. So it says to search the words
///   separately, and names the limit of the check.
///
/// Neither line describes the results below it. Whether those matched other
/// words or are embedding neighbours depends on scope and on the vector
/// arm, and a wrong guess ("probably unrelated") was measured sending an
/// agent away from the right documents after a one-letter typo.
pub fn absent_notice(unmatched: &[UnmatchedTerm]) -> Option<String> {
    // The query is echoed into text a model reads, so it is escaped the way
    // the header's `{:?}` escapes it: no raw control characters, and a quote
    // inside a term cannot close the quotes around it.
    let quoted = |term: &str| {
        let mut out = String::from('"');
        for c in excerpt(term, ABSENT_TERM_CHARS).chars() {
            if c.is_control() || c == '"' || c == '\\' {
                out.extend(c.escape_debug());
            } else {
                out.push(c);
            }
        }
        out.push('"');
        out
    };
    let mut words: Vec<String> = Vec::new();
    let mut sequences: Vec<String> = Vec::new();
    for item in unmatched {
        let term = quoted(&item.term);
        if item.absent_words.is_empty() {
            sequences.push(term.clone());
        }
        for word in &item.absent_words {
            // Name the word, and where it came from when that is not obvious:
            // "EFECTIVE" alone would not tell the reader which identifier.
            let shown = if word.eq_ignore_ascii_case(&item.term) {
                quoted(word)
            } else {
                format!("{} (in {term})", quoted(word))
            };
            if !words.contains(&shown) {
                words.push(shown);
            }
        }
    }
    let list = |items: &[String]| {
        let mut named = items.iter().take(ABSENT_SHOWN).cloned().collect::<Vec<_>>().join(", ");
        if items.len() > ABSENT_SHOWN {
            named.push_str(&format!(" and {} more", items.len() - ABSENT_SHOWN));
        }
        named
    };
    let mut lines = Vec::new();
    if !words.is_empty() {
        // "As a whole word" is the whole claim: the index knows tokens, and
        // `1626ba7e` is not the token `0x1626ba7e`. Saying a document cannot
        // contain it, or that no rewording will find it, would be false there.
        let (this, one, it_is, it) = if words.len() == 1 {
            ("this as a whole word", "it", "it is", "it")
        } else {
            ("these as whole words", "one", "they are", "them")
        };
        lines.push(format!(
            "Not in the corpus: {} — no indexed document contains {this} ({one} can \
             still sit inside a longer token, such as a 0x-prefixed literal). Check \
             the spelling or the identifier's real name; if {it_is} right, the corpus \
             does not hold {it} as written.",
            list(&words)
        ));
    }
    if !sequences.is_empty() {
        lines.push(format!(
            "No exact match: {} — every word occurs in the corpus, but not together \
             in this order. Search the words separately. Matching is per passage of \
             raw Markdown, so a phrase split by a link or a section break can be \
             missed; otherwise no indexed document uses this exact wording.",
            list(&sequences)
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
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

    /// A term none of whose words is missing: only the sequence is unmatched.
    fn sequence(term: &str) -> UnmatchedTerm {
        UnmatchedTerm { term: term.into(), absent_words: Vec::new() }
    }

    fn missing(term: &str, words: &[&str]) -> UnmatchedTerm {
        UnmatchedTerm {
            term: term.into(),
            absent_words: words.iter().map(|w| w.to_string()).collect(),
        }
    }

    #[test]
    fn absent_notice_is_silent_when_every_term_matched() {
        assert_eq!(absent_notice(&[]), None);
    }

    #[test]
    fn absent_notice_names_words_that_are_nowhere_and_asks_for_a_spelling_check() {
        let one = absent_notice(&[missing("wexlurb", &["wexlurb"])]).unwrap();
        assert!(one.starts_with("Not in the corpus: \"wexlurb\" — "), "{one}");
        assert!(one.contains("contains this as a whole word"), "{one}");
        assert!(one.contains("Check the spelling or the identifier's real name"), "{one}");
        assert!(!one.contains("No exact match"), "{one}");

        let two = absent_notice(&[
            missing("wexlurb", &["wexlurb"]),
            missing("Verhaeghe", &["Verhaeghe"]),
        ])
        .unwrap();
        assert!(two.contains("\"wexlurb\", \"Verhaeghe\""), "{two}");
        assert!(two.contains("contains these as whole words"), "{two}");
    }

    /// A typo inside an identifier: naming the bare fragment would not tell
    /// the reader which identifier to fix.
    #[test]
    fn absent_notice_says_which_term_a_missing_word_came_from() {
        let out = absent_notice(&[missing("MAX_EFECTIVE_BALANCE", &["EFECTIVE"])]).unwrap();
        assert!(out.contains("\"EFECTIVE\" (in \"MAX_EFECTIVE_BALANCE\")"), "{out}");
    }

    /// Every word indexed, the sequence not: a quoted phrase nobody wrote,
    /// or two real identifiers joined by a slash. It must not be called
    /// absent, and must say how to find the parts.
    #[test]
    fn absent_notice_keeps_an_unmatched_sequence_apart_from_a_missing_word() {
        let out =
            absent_notice(&[sequence("Technical Pillar"), sequence("EIP-7702/EIP-3074")]).unwrap();
        assert!(
            out.starts_with("No exact match: \"Technical Pillar\", \"EIP-7702/EIP-3074\" — "),
            "{out}"
        );
        assert!(out.contains("Search the words separately"), "{out}");
        assert!(!out.contains("Not in the corpus"), "{out}");

        let both = absent_notice(&[missing("wexlurb", &["wexlurb"]), sequence("a b")]).unwrap();
        let lines: Vec<&str> = both.lines().collect();
        assert_eq!(lines.len(), 2, "{both}");
        assert!(lines[0].starts_with("Not in the corpus: \"wexlurb\""), "{both}");
        assert!(lines[1].starts_with("No exact match: \"a b\""), "{both}");
    }

    /// The line is about the query's terms. It was measured misdescribing
    /// the results (calling the right documents "probably unrelated"), so
    /// it no longer says anything about them, or tells the reader to leave.
    #[test]
    fn absent_notice_does_not_describe_the_results() {
        let out = absent_notice(&[missing("wexlurb", &["wexlurb"]), sequence("a b")]).unwrap();
        for claim in ["results below", "unrelated", "use another source", "neighbours"] {
            assert!(!out.contains(claim), "{claim:?} in:\n{out}");
        }
    }

    /// The index knows whole tokens: `1626ba7e` is absent from a corpus full
    /// of `0x1626ba7e`. The line may not claim more than that, or promise
    /// that no other search will find it.
    #[test]
    fn absent_notice_claims_only_a_whole_word_miss() {
        let out = absent_notice(&[missing("1626ba7e", &["1626ba7e"])]).unwrap();
        assert!(out.contains("as a whole word"), "{out}");
        assert!(out.contains("inside a longer token"), "{out}");
        assert!(out.ends_with("does not hold it as written."), "{out}");
        assert!(!out.contains("will not find"), "{out}");
    }

    /// The query is caller-supplied text echoed to a model.
    #[test]
    fn absent_notice_escapes_what_it_echoes() {
        let out = absent_notice(&[
            missing("x\u{1b}[31mwexlurb", &["wexlurb"]),
            sequence("say \"hi\" \\ there\u{0}"),
        ])
        .unwrap();
        assert!(!out.contains('\u{1b}') && !out.contains('\u{0}'), "{out:?}");
        assert!(out.contains("(in \"x\\u{1b}[31mwexlurb\")"), "{out}");
        assert!(out.contains("\"say \\\"hi\\\" \\\\ there\\0\""), "{out}");
    }

    #[test]
    fn absent_notice_caps_the_list_and_the_length_of_each_term() {
        let many: Vec<UnmatchedTerm> =
            ["a1", "b2", "c3", "d4", "e5", "f6", "g7"].iter().map(|t| sequence(t)).collect();
        let out = absent_notice(&many).unwrap();
        assert!(out.contains("\"e5\" and 2 more"), "{out}");
        assert!(!out.contains("f6"), "{out}");

        let sentence = "only a small number of people can understand how the protocol \
                        works and why it is secure";
        let out = absent_notice(&[sequence(sentence)]).unwrap();
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
