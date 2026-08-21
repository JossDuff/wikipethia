//! Text cleanup between a source's raw form and [`Document::content`].
//!
//! [`Document::content`]: crate::document::Document::content

/// Remove `[quote="user, post:3, topic:99"]…[/quote]` blocks, including
/// nested ones, from raw Discourse markdown. Quoted text duplicates the
/// parent post across replies and pollutes both BM25 and embeddings.
///
/// Everything outside quote blocks passes through byte-for-byte — `$$…$$`,
/// inline `$…$`, and code fences survive intact. Malformed input (an open
/// tag that never closes) is emitted as-is rather than truncating the post.
pub fn strip_quote_blocks(raw: &str) -> String {
    const CLOSE: &str = "[/quote]";
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0usize;
    // Start of the outermost open tag currently being stripped, so an
    // unclosed quote can be restored verbatim at end of input.
    let mut strip_start = 0usize;
    let mut i = 0usize;
    while i < raw.len() {
        let Some(rel) = raw[i..].find('[') else {
            if depth == 0 {
                out.push_str(&raw[i..]);
            }
            break;
        };
        if depth == 0 {
            out.push_str(&raw[i..i + rel]);
        }
        let tag = i + rel;
        if let Some(len) = open_tag_len(&raw[tag..]) {
            if depth == 0 {
                strip_start = tag;
            }
            depth += 1;
            i = tag + len;
        } else if raw[tag..].starts_with(CLOSE) {
            if depth > 0 {
                depth -= 1;
            } else {
                // Stray close with no open: not a quote block, keep it.
                out.push_str(CLOSE);
            }
            i = tag + CLOSE.len();
        } else {
            if depth == 0 {
                out.push('[');
            }
            i = tag + 1;
        }
    }
    if depth > 0 {
        out.push_str(&raw[strip_start..]);
    }
    collapse_blank_runs(&out)
}

/// Byte length of an opening quote tag at the start of `s`: `[quote]`, or
/// `[quote=…]` with the attribute running to the next `]`. `None` when `s`
/// starts with something else (including `[quote` used as plain text).
fn open_tag_len(s: &str) -> Option<usize> {
    let rest = s.strip_prefix("[quote")?;
    if rest.starts_with(']') {
        return Some("[quote]".len());
    }
    let attr = rest.strip_prefix('=')?;
    attr.find(']').map(|end| "[quote=".len() + end + 1)
}

/// Stripping a block out of `text\n\n[quote]…[/quote]\n\ntext` leaves a run
/// of blank lines behind; collapse anything deeper than one blank line and
/// trim the ends.
fn collapse_blank_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut newlines = 0usize;
    for ch in s.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(ch);
            }
        } else {
            newlines = 0;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_a_simple_block() {
        let raw = "before\n\n[quote=\"vbuterin, post:1, topic:426\"]\nquoted text\n[/quote]\n\nafter";
        assert_eq!(strip_quote_blocks(raw), "before\n\nafter");
    }

    #[test]
    fn strips_nested_blocks_as_one() {
        let raw = "a\n\n[quote=\"x, post:1, topic:2\"]outer[quote=\"y, post:2, topic:2\"]inner[/quote]more[/quote]\n\nb";
        assert_eq!(strip_quote_blocks(raw), "a\n\nb");
    }

    #[test]
    fn strips_multiple_blocks_keeping_text_between() {
        let raw = "[quote=\"a, post:1, topic:9\"]q1[/quote]\nreply one\n[quote=\"b, post:2, topic:9\"]q2[/quote]\nreply two";
        assert_eq!(strip_quote_blocks(raw), "reply one\n\nreply two");
    }

    #[test]
    fn strips_bare_quote_tags() {
        assert_eq!(strip_quote_blocks("x [quote]y[/quote] z"), "x  z");
    }

    #[test]
    fn unclosed_quote_is_kept_verbatim() {
        let raw = "text [quote=\"a, post:1, topic:2\"] rest of post";
        assert_eq!(strip_quote_blocks(raw), raw);
    }

    #[test]
    fn stray_close_is_kept() {
        assert_eq!(strip_quote_blocks("a [/quote] b"), "a [/quote] b");
    }

    #[test]
    fn plain_brackets_pass_through() {
        let raw = "arrays[0] and [links](https://x.y) and [quoted text] stay";
        assert_eq!(strip_quote_blocks(raw), raw);
    }

    #[test]
    fn mathjax_is_untouched() {
        let raw = "To prove $f_1(x_1) = y_1$ let\n\n$$F = \\prod_{i \\ne 1} (X - x_i)$$\n\nhold.";
        assert_eq!(strip_quote_blocks(raw), raw);
    }
}
