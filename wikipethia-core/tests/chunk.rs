//! Chunking tests: sizing, overlap, and never splitting a fence or a
//! display-math block (except the documented hard-split degenerate case).

use std::fs;
use std::path::Path;

use wikipethia_core::{chunk, parse_topic};
use serde_json::Value;

/// One ~100-char paragraph, distinguishable by index.
fn para(i: usize) -> String {
    format!("paragraph {i:03} {}", "lorem ipsum dolor sit amet ".repeat(3))
}

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    serde_json::from_str(&fs::read_to_string(&path).expect("fixture exists"))
        .expect("fixture parses")
}

#[test]
fn short_document_is_one_unchanged_chunk() {
    let content = "A short post.\n\nWith two paragraphs.";
    assert_eq!(chunk(content), vec![content.to_string()]);
}

#[test]
fn empty_and_whitespace_content_yield_no_chunks() {
    assert!(chunk("").is_empty());
    assert!(chunk("  \n\n \n").is_empty());
}

#[test]
fn long_document_chunks_are_bounded_and_complete() {
    let paras: Vec<String> = (0..40).map(para).collect();
    let content = paras.join("\n\n");
    let chunks = chunk(&content);

    assert!(chunks.len() > 1, "4k chars must split");
    for c in &chunks {
        // Worst case: overlap seed + joiner + one block just under the
        // hard-split threshold.
        assert!(c.chars().count() <= 3202, "chunk too big: {} chars", c.len());
    }
    for p in &paras {
        assert!(
            chunks.iter().any(|c| c.contains(p.trim_end())),
            "paragraph lost: {}",
            &p[..13]
        );
    }
}

#[test]
fn consecutive_chunks_overlap_by_whole_blocks() {
    let content = (0..40).map(para).collect::<Vec<_>>().join("\n\n");
    let chunks = chunk(&content);
    assert!(chunks.len() > 1);
    for pair in chunks.windows(2) {
        // The overlap seed is one or more whole blocks: simultaneously a
        // block-aligned prefix of the next chunk and a suffix of the previous.
        let overlaps = pair[1]
            .match_indices("\n\n")
            .map(|(i, _)| &pair[1][..i])
            .any(|prefix| pair[0].ends_with(prefix));
        assert!(overlaps, "chunks must share whole-block overlap");
    }
}

#[test]
fn chunking_is_deterministic() {
    let content = (0..40).map(para).collect::<Vec<_>>().join("\n\n");
    assert_eq!(chunk(&content), chunk(&content));
}

#[test]
fn code_fences_are_never_split() {
    let fence = format!("```rust\nfn f() {{}}\n\nlet x = 1; // {}\n```", "x".repeat(500));
    let content = format!("{}\n\n{fence}\n\n{}", para(0).repeat(19), para(1));
    let chunks = chunk(&content);
    assert!(chunks.len() > 1, "must split around the fence");
    assert!(
        chunks.iter().any(|c| c.contains(&fence)),
        "fence must survive intact in one chunk"
    );
    for c in &chunks {
        let delimiters = c.lines().filter(|l| l.trim().starts_with("```")).count();
        assert_eq!(delimiters % 2, 0, "unbalanced fence in chunk: {c:.60}");
    }
}

#[test]
fn display_math_is_never_split() {
    let math = format!("$$\nF = \\prod_i (X - x_i)\n\n+ {}\n$$", "y ".repeat(300));
    let content = format!("{}\n\n{math}\n\n{}", para(0).repeat(19), para(1));
    let chunks = chunk(&content);
    assert!(chunks.len() > 1, "must split around the math block");
    assert!(
        chunks.iter().any(|c| c.contains(&math)),
        "math block must survive intact in one chunk"
    );
    for c in &chunks {
        assert_eq!(c.matches("$$").count() % 2, 0, "unbalanced $$ in chunk");
    }
}

#[test]
fn pathological_block_hard_splits_with_overlap() {
    // 10k chars, no blank line anywhere: must terminate and stay bounded.
    let content = "word ".repeat(2000);
    let content = content.trim_end();
    let chunks = chunk(content);
    assert!(chunks.len() > 1);
    for c in &chunks {
        assert!(c.chars().count() <= 2000);
    }
    for pair in chunks.windows(2) {
        let tail: String = pair[0].chars().rev().take(200).collect();
        let tail: String = tail.chars().rev().collect();
        assert!(pair[1].starts_with(&tail), "hard-split pieces must overlap");
    }
}

#[test]
fn real_thread_chunks_cleanly() {
    let docs = parse_topic(&fixture("topic_426.json"), "ethresearch", "https://ethresear.ch").unwrap();
    let mut multi = 0usize;
    for doc in &docs {
        let chunks = chunk(&doc.content);
        assert!(!chunks.is_empty(), "{}: no chunks", doc.id);
        if chunks.len() > 1 {
            multi += 1;
        }
    }
    assert!(multi > 0, "the long posts in topic 426 must multi-chunk");
}

#[test]
fn a_fence_beyond_hard_max_splits_without_panicking() {
    // consensus-specs files carry python fences far past HARD_MAX_CHARS;
    // they hard-split mid-fence (acceptable) but must stay bounded.
    let fence_body = "def f(x):\n    return x + 1\n".repeat(300); // ~7800 chars
    let content = format!("Intro paragraph.\n\n```python\n{fence_body}```\n\nOutro.");
    let chunks = wikipethia_core::chunk(&content);
    assert!(chunks.len() > 2, "must split: got {}", chunks.len());
    for c in &chunks {
        assert!(c.chars().count() <= 3202, "chunk too big: {}", c.chars().count());
    }
    let total: String = chunks.concat();
    assert!(total.contains("Intro paragraph."));
    assert!(total.contains("Outro."));
}
