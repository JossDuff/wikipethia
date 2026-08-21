//! Split [`Document::content`] into overlapping chunks for indexing.
//!
//! [`Document::content`]: crate::document::Document::content

/// Soft ceiling per chunk, in `char`s — roughly 400–500 BM25 or embedding
/// tokens. Most posts (median 687 chars) stay a single chunk.
const TARGET_CHARS: usize = 2000;

/// Trailing content repeated at the start of the next chunk so a sentence
/// straddling a boundary stays findable in one piece.
const OVERLAP_CHARS: usize = 200;

/// A single atomic block longer than this is split mid-block. Below it we
/// tolerate an oversized chunk rather than cut inside a fence or `$$…$$`.
const HARD_MAX_CHARS: usize = 3000;

/// Split cleaned markdown into overlapping chunks at paragraph boundaries,
/// never inside a ``` fence or a `$$…$$` display-math block — except for a
/// pathological block over [`HARD_MAX_CHARS`] with no blank line to cut at,
/// which is split on plain `char` boundaries.
///
/// Deterministic: the same content always yields the same chunks, so chunk
/// ids derived from position are stable across re-indexing. Empty or
/// whitespace-only content yields no chunks.
pub fn chunk(content: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    for block in split_blocks(content) {
        if block.chars().count() > HARD_MAX_CHARS {
            blocks.extend(hard_split(&block));
        } else {
            blocks.push(block);
        }
    }
    pack(&blocks)
}

/// Segment into atomic blocks separated by blank lines. Blank lines inside a
/// code fence or display-math region do not split — those regions ride along
/// inside whatever block they started in. Unclosed fences and math run to end
/// of input; forum markdown is often malformed and must never panic.
fn split_blocks(content: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut cur = String::new();
    let mut in_fence = false;
    let mut in_math = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if !in_fence && !in_math && trimmed.is_empty() {
            if !cur.is_empty() {
                blocks.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
        if !in_math && trimmed.starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence && line.matches("$$").count() % 2 == 1 {
            // An odd number of $$ on a line opens or closes display math;
            // `$$x$$` inline is even and leaves the state alone.
            in_math = !in_math;
        }
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }
    blocks
}

/// Cut an oversized block every [`TARGET_CHARS`] with an
/// [`OVERLAP_CHARS`]-char tail carried into the next piece.
fn hard_split(block: &str) -> Vec<String> {
    let chars: Vec<char> = block.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    loop {
        let end = (start + TARGET_CHARS).min(chars.len());
        out.push(chars[start..end].iter().collect());
        if end == chars.len() {
            return out;
        }
        start = end - OVERLAP_CHARS;
    }
}

/// Greedily pack blocks into chunks of about [`TARGET_CHARS`], joining with
/// blank lines. Each new chunk is seeded with trailing whole blocks of the
/// previous one totaling at most [`OVERLAP_CHARS`] — whole blocks only, so a
/// chunk never begins with half a fence.
fn pack(blocks: &[String]) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut cur_len = 0usize;
    for block in blocks {
        let block_len = block.chars().count();
        if !cur.is_empty() && cur_len + 2 + block_len > TARGET_CHARS {
            chunks.push(cur.join("\n\n"));
            (cur, cur_len) = overlap_seed(&cur);
        }
        cur_len += block_len + if cur.is_empty() { 0 } else { 2 };
        cur.push(block);
    }
    if !cur.is_empty() {
        chunks.push(cur.join("\n\n"));
    }
    chunks
}

/// Trailing whole blocks of `prev` totaling at most [`OVERLAP_CHARS`]
/// (joined length), and that length. Empty when even the last block alone
/// is over budget.
fn overlap_seed<'a>(prev: &[&'a str]) -> (Vec<&'a str>, usize) {
    let mut seed: Vec<&'a str> = Vec::new();
    let mut len = 0usize;
    for block in prev.iter().rev() {
        let extra = block.chars().count() + if seed.is_empty() { 0 } else { 2 };
        if len + extra > OVERLAP_CHARS {
            break;
        }
        len += extra;
        seed.push(block);
    }
    seed.reverse();
    (seed, len)
}
