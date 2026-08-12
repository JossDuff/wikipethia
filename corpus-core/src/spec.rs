//! Structured extraction from spec-repo markdown: constant tables and
//! python function fences. Pure text → structure, no I/O — callers decide
//! which documents to parse (spec-tier content is a few tens of MB, so
//! parsing on demand at query time is milliseconds and keeps ingest and
//! the search index untouched).
//!
//! The shapes parsed here are the consensus-specs/EIP conventions:
//!
//! ```markdown
//! | Name                    | Value                     | Description |
//! | ----------------------- | ------------------------- | ----------- |
//! | `MAX_EFFECTIVE_BALANCE` | `Gwei(2**5 * 10**9)` (= …) | Max balance |
//! ```
//!
//! ````markdown
//! ###### Modified `process_deposit`
//!
//! ```python
//! def process_deposit(state: BeaconState, deposit: Deposit) -> None:
//!     ...
//! ```
//! ````

/// One row of a constant/preset/config table.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecConstant {
    /// The backticked identifier from the first cell, e.g.
    /// `MAX_EFFECTIVE_BALANCE_ELECTRA`.
    pub name: String,
    /// The second cell verbatim, e.g. "`Gwei(2**11 * 10**9)` (= 2,048,000,000,000)".
    pub value: String,
    /// The third cell, when the table has one and it is non-empty.
    pub description: Option<String>,
}

/// One top-level `def` from a ```python fence.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecFunction {
    /// The identifier after `def`.
    pub name: String,
    /// The function's source: its `def` line through the line before the
    /// next top-level `def` in the same fence (or the fence's end).
    pub code: String,
    /// The nearest heading above the fence, hashes stripped — carries the
    /// spec's own "Modified"/"New" labeling, e.g. "Modified `process_deposit`".
    pub heading: Option<String>,
}

/// Every constant-table row in `content`. Rows are recognized by shape —
/// a pipe table whose first cell is a backticked ALL_CAPS identifier —
/// so headers, separator rows, and prose tables fall out naturally.
pub fn constants(content: &str) -> Vec<SpecConstant> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        // Tables inside code fences are content, not spec structure.
        if in_fence || !trimmed.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = trimmed
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if cells.len() < 2 {
            continue;
        }
        let Some(name) = constant_name(cells[0]) else {
            continue;
        };
        if cells[1].is_empty() {
            continue;
        }
        out.push(SpecConstant {
            name,
            value: cells[1].to_string(),
            description: cells.get(2).filter(|d| !d.is_empty()).map(|d| (*d).to_string()),
        });
    }
    out
}

/// The identifier inside `` `LIKE_THIS` `` when the cell is exactly a
/// backticked ALL_CAPS name; None for headers, separators, and prose.
fn constant_name(cell: &str) -> Option<String> {
    let inner = cell.strip_prefix('`')?.strip_suffix('`')?;
    let shaped = !inner.is_empty()
        && inner.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && inner.chars().any(|c| c.is_ascii_uppercase());
    shaped.then(|| inner.to_string())
}

/// Every top-level `def` in every ```python fence of `content`, each
/// carrying the nearest heading above its fence.
pub fn functions(content: &str) -> Vec<SpecFunction> {
    let mut out = Vec::new();
    let mut heading: Option<String> = None;
    // Some while inside any fence; the bool records whether it was ```python.
    let mut fence: Option<(bool, Vec<&str>)> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        match &mut fence {
            None => {
                if let Some(h) = trimmed.strip_prefix('#') {
                    heading = Some(h.trim_start_matches('#').trim().to_string());
                } else if trimmed.starts_with("```") {
                    fence = Some((trimmed == "```python", Vec::new()));
                }
            }
            Some((python, lines)) => {
                if trimmed == "```" {
                    if *python {
                        out.extend(fence_functions(lines, heading.as_deref()));
                    }
                    fence = None;
                } else {
                    lines.push(line);
                }
            }
        }
    }
    out
}

/// Split one fence's lines into per-`def` functions. Top-level means the
/// `def` starts at column 0 — methods of container classes are indented
/// and deliberately not extracted.
fn fence_functions(lines: &[&str], heading: Option<&str>) -> Vec<SpecFunction> {
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("def "))
        .map(|(i, _)| i)
        .collect();
    starts
        .iter()
        .enumerate()
        .filter_map(|(n, &start)| {
            let name: String = lines[start]
                .trim_start_matches("def ")
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                return None;
            }
            let end = starts.get(n + 1).copied().unwrap_or(lines.len());
            Some(SpecFunction {
                name,
                code: lines[start..end].join("\n").trim_end().to_string(),
                heading: heading.map(str::to_string),
            })
        })
        .collect()
}
