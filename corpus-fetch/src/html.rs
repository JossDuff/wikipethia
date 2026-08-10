//! HTML → indexable text. This is index food, not rendering: headings,
//! paragraphs, list items, and code fences survive; chrome (nav, scripts,
//! footers) is dropped. `pre` blocks become ``` fences so the chunker's
//! fence-awareness and BM25 both see real code.

use scraper::{ElementRef, Html, Selector};

pub fn html_to_text(html: &str) -> String {
    let doc = Html::parse_document(html);
    let root = ["article", "main", "body"]
        .iter()
        .find_map(|sel| doc.select(&Selector::parse(sel).unwrap()).next());
    let mut out = String::new();
    if let Some(root) = root {
        walk(root, &mut out);
    }
    collapse_blank_runs(&out)
}

const SKIP: &[&str] = &["script", "style", "nav", "header", "footer", "aside", "noscript"];

fn walk(element: ElementRef, out: &mut String) {
    let name = element.value().name();
    if SKIP.contains(&name) {
        return;
    }
    match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = name[1..].parse::<usize>().unwrap_or(1);
            out.push_str("\n\n");
            out.push_str(&"#".repeat(level));
            out.push(' ');
            push_inline_text(element, out);
            out.push_str("\n\n");
        }
        "pre" => {
            out.push_str("\n\n```\n");
            for piece in element.text() {
                out.push_str(piece);
            }
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
        }
        "li" => {
            out.push_str("\n- ");
            push_children(element, out);
        }
        "p" | "div" | "section" | "blockquote" | "ul" | "ol" | "table" | "tr" => {
            out.push_str("\n\n");
            push_children(element, out);
            out.push_str("\n\n");
        }
        "br" => out.push('\n'),
        _ => push_children(element, out),
    }
}

/// Children in order: text nodes verbatim, elements recursively.
fn push_children(element: ElementRef, out: &mut String) {
    for child in element.children() {
        if let Some(text) = child.value().as_text() {
            out.push_str(text);
        } else if let Some(child_el) = ElementRef::wrap(child) {
            walk(child_el, out);
        }
    }
}

/// Flattened text only — used for headings, where nested markup is noise.
fn push_inline_text(element: ElementRef, out: &mut String) {
    for piece in element.text() {
        out.push_str(piece);
    }
}

fn collapse_blank_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
            out.push('\n');
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_beats_body_and_chrome_is_dropped() {
        let html = r#"<html><body>
            <nav>Home | About</nav>
            <article><h1>Title</h1><p>Real content.</p>
              <script>alert(1)</script></article>
            <footer>© nobody</footer>
        </body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("# Title"));
        assert!(text.contains("Real content."));
        assert!(!text.contains("Home | About"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("nobody"));
    }

    #[test]
    fn pre_blocks_become_fences_and_lists_become_dashes() {
        let html = "<body><article><p>Before</p><pre>let x = 1;\nlet y = 2;</pre>\
                    <ul><li>one</li><li>two</li></ul></article></body>";
        let text = html_to_text(html);
        assert!(text.contains("```\nlet x = 1;\nlet y = 2;\n```"), "{text}");
        assert!(text.contains("- one"));
        assert!(text.contains("- two"));
    }

    #[test]
    fn falls_back_to_body_when_no_article() {
        let text = html_to_text("<html><body><p>Plain page.</p></body></html>");
        assert_eq!(text, "Plain page.");
    }
}
