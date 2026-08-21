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

/// One extracted routine: a top-level `def` from a ```python fence, or a
/// `function` declaration from a Solidity one.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecFunction {
    /// The identifier after `def` or `function`.
    pub name: String,
    /// The function's source: its `def` line through the line before the
    /// next top-level `def` in the same fence (or the fence's end). For
    /// Solidity, the declaration and any doc comment immediately above it.
    pub code: String,
    /// The nearest heading above the fence, hashes stripped — carries the
    /// spec's own "Modified"/"New" labeling, e.g. "Modified `process_deposit`".
    pub heading: Option<String>,
    /// Which language `code` is, for the fence the renderer wraps it in.
    /// Labelling Solidity as python would be a small lie that costs a
    /// reader real time.
    pub language: &'static str,
}

/// Every constant-table row in `content`. Rows are recognized by shape —
/// a pipe table whose first cell is a backticked ALL_CAPS identifier —
/// so headers, separator rows, and prose tables fall out naturally.
pub fn constants(content: &str) -> Vec<SpecConstant> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut fence_ticks = 0usize; // 0 = outside any fence
    for line in content.lines() {
        let trimmed = line.trim();
        if in_fence {
            if fence_close(trimmed, fence_ticks) {
                in_fence = false;
            }
            continue;
        }
        if let Some((ticks, _)) = fence_open(trimmed) {
            in_fence = true;
            fence_ticks = ticks;
            continue;
        }
        // Tables inside code fences are content, not spec structure.
        if !trimmed.starts_with('|') {
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

/// A fence opener: three or more backticks, then an info string. Returns
/// (tick count, info string). CommonMark semantics matter on real spec
/// files: erc-5252 uses four-backtick fences with three-backtick fences
/// nested inside as content.
fn fence_open(trimmed: &str) -> Option<(usize, &str)> {
    let ticks = trimmed.chars().take_while(|c| *c == '`').count();
    (ticks >= 3).then(|| (ticks, trimmed[ticks..].trim()))
}

/// A fence closer for an opener of `ticks` backticks: a line of at least
/// that many backticks and nothing else.
fn fence_close(trimmed: &str, ticks: usize) -> bool {
    trimmed.len() >= ticks && trimmed.chars().all(|c| c == '`')
}

/// Whether a fence info string marks Python. Real spec-tier documents use
/// "python", "py", "python3", and "``` python" (leading space) — eip-100,
/// eip-3076, and the eip-3368 family are live examples of each.
fn python_info(info: &str) -> bool {
    matches!(
        info.split_whitespace().next(),
        Some("python") | Some("py") | Some("python3")
    )
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
    // Some while inside any fence: (opener's tick count, was python, lines).
    let mut fence: Option<(usize, bool, Vec<&str>)> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        match &mut fence {
            None => {
                if let Some(h) = trimmed.strip_prefix('#') {
                    heading = Some(h.trim_start_matches('#').trim().to_string());
                } else if let Some((ticks, info)) = fence_open(trimmed) {
                    fence = Some((ticks, python_info(info), Vec::new()));
                }
            }
            Some((ticks, python, lines)) => {
                if fence_close(trimmed, *ticks) {
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
    // An unclosed fence runs to end of input (CommonMark); its functions
    // are still real — dropping them cost every def below erc-5252's
    // unterminated block before this flush existed.
    if let Some((_, true, lines)) = fence {
        out.extend(fence_functions(&lines, heading.as_deref()));
    }
    out
}

/// Every top-level `def` in a file that IS Python, rather than markdown
/// containing Python. [`functions`] finds nothing here — it looks for
/// fences, and a `.py` file has none.
///
/// Headings are deliberately `None`: [`functions`] treats a leading `#` as
/// a markdown heading, which in Python is a comment, so inferring one would
/// label every function with whatever remark happened to precede it.
pub fn functions_in_python(content: &str) -> Vec<SpecFunction> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    for (start, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("def ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        // A def's body ends at the next column-0 statement, not at the
        // next `def`. Inside a markdown fence the terminator bounds the
        // last function, but a whole .py file has no such bound: taking
        // def-to-next-def hands back every module-level constant and
        // alias in between as if it were the function's source.
        let end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, l)| !l.trim().is_empty() && !l.starts_with([' ', '\t', ')', ']', '}']))
            .map_or(lines.len(), |(i, _)| i);
        out.push(SpecFunction {
            name,
            code: lines[start..end].join("\n").trim_end().to_string(),
            heading: None,
            language: "python",
        });
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
                language: "python",
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Solidity fences.
//
// The ERCs put their normative surface in Solidity, not Python or a constant
// table, so none of the above reaches it. The case that forced this: asking
// what `isValidSignature` returns scored 0.00 in the retrieval eval while the
// answer — the magic value `0x1626ba7e` — sat in erc-1271's fence the whole
// time. The answer IS a 4-byte literal, so neither retrieval arm can help:
// FTS stems it apart and the vector side has nothing to grip.
//
// Scope is deliberately narrow: `function` declarations and `constant` state
// variables. Events, errors, structs, and modifiers are not extracted — they
// can be, on the same walk, when a question needs them.
// ---------------------------------------------------------------------------

/// Whether a fence holds Solidity: its info string says so, or its contents
/// declare a `pragma solidity`.
///
/// The sniff is not belt-and-braces. Measured across the ingested EIPs and
/// ERCs, **19 documents carry `pragma solidity` in a fence tagged
/// `javascript` or `js` and never tag a single fence `solidity`** — among them
/// erc-1271 (the magic-value case this exists for), erc-3156 (flash loans),
/// and erc-1822 (proxies). An info-string-only rule would miss precisely the
/// documents that motivated the feature.
///
/// A Solidity fence with neither marker is not detected. That is accepted:
/// `pragma solidity` is a strong, self-describing signal, where sniffing for
/// `contract`/`function` shapes would start claiming JavaScript fences.
fn solidity_fence(info: &str, lines: &[&str]) -> bool {
    matches!(info.split_whitespace().next(), Some("solidity" | "sol"))
        || lines
            .iter()
            .any(|l| l.trim_start().starts_with("pragma solidity"))
}

/// Every Solidity fence in `content`, as (nearest heading above, lines).
///
/// Shared by [`solidity_declarations`] and [`solidity_constants`] because the
/// fence walk — CommonMark tick counting, unclosed-fence flush, heading
/// tracking — is fiddly enough that two copies would drift.
fn solidity_fences(content: &str) -> Vec<(Option<String>, Vec<&str>)> {
    let mut out = Vec::new();
    let mut heading: Option<String> = None;
    let mut fence: Option<(usize, String, Vec<&str>)> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        match &mut fence {
            None => {
                if let Some(h) = trimmed.strip_prefix('#') {
                    heading = Some(h.trim_start_matches('#').trim().to_string());
                } else if let Some((ticks, info)) = fence_open(trimmed) {
                    fence = Some((ticks, info.to_string(), Vec::new()));
                }
            }
            Some((ticks, info, lines)) => {
                if fence_close(trimmed, *ticks) {
                    if solidity_fence(info, lines) {
                        out.push((heading.clone(), std::mem::take(lines)));
                    }
                    fence = None;
                } else {
                    lines.push(line);
                }
            }
        }
    }
    // An unclosed fence runs to end of input, same as in `functions`.
    if let Some((_, info, lines)) = fence
        && solidity_fence(&info, &lines)
    {
        out.push((heading, lines));
    }
    out
}

/// Every `function` declaration in every Solidity fence of `content`.
///
/// The extracted block runs from any doc comment immediately above the
/// declaration through its terminating `;` or closing brace. The comment is
/// not decoration — in erc-1271 the sentence "MUST return the bytes4 magic
/// value 0x1626ba7e when function passes" lives there, and it is the answer.
pub fn solidity_declarations(content: &str) -> Vec<SpecFunction> {
    let mut out = Vec::new();
    for (heading, lines) in solidity_fences(content) {
        for (start, line) in lines.iter().enumerate() {
            let Some(rest) = line.trim_start().strip_prefix("function ") else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                continue;
            }
            let from = doc_comment_start(&lines, start);
            let end = declaration_end(&lines, start);
            out.push(SpecFunction {
                name,
                code: lines[from..end].join("\n").trim_end().to_string(),
                heading: heading.clone(),
                language: "solidity",
            });
        }
    }
    out
}

/// Every `constant` state variable in every Solidity fence of `content`,
/// e.g. `bytes4 constant internal MAGICVALUE = 0x1626ba7e;` — the shape that
/// actually carries an ERC's magic values and selectors.
///
/// `value` is the right-hand side; `description` is the type and modifiers,
/// which say whether a reader can rely on it (`public` vs `internal`).
pub fn solidity_constants(content: &str) -> Vec<SpecConstant> {
    let mut out = Vec::new();
    for (_, lines) in solidity_fences(content) {
        for line in lines {
            let trimmed = line.trim();
            let Some(body) = trimmed.strip_suffix(';') else {
                continue;
            };
            let Some((decl, value)) = body.split_once('=') else {
                continue;
            };
            // `constant` as its own word, so `constantProduct` never matches.
            let mut words = decl.split_whitespace();
            if !decl.split_whitespace().any(|w| w == "constant") {
                continue;
            }
            let Some(name) = words.next_back().map(str::to_string) else {
                continue;
            };
            let value = value.trim();
            if name.is_empty() || value.is_empty() {
                continue;
            }
            let modifiers: Vec<&str> = decl.split_whitespace().collect();
            let description = modifiers
                .split_last()
                .map(|(_, head)| head.join(" "))
                .filter(|d| !d.is_empty());
            out.push(SpecConstant {
                name,
                value: value.to_string(),
                description,
            });
        }
    }
    out
}

/// Walk back from `at` over a contiguous doc comment, returning the line to
/// start the extracted block at. Handles `///`, `//`, and `/** … */` blocks;
/// stops at the first line that is neither, so an unrelated statement above
/// is never dragged in.
fn doc_comment_start(lines: &[&str], at: usize) -> usize {
    let mut from = at;
    while from > 0 {
        let above = lines[from - 1].trim();
        let is_comment = above.starts_with("///")
            || above.starts_with("//")
            || above.starts_with("/*")
            || above.starts_with('*')
            || above.ends_with("*/");
        if !is_comment {
            break;
        }
        from -= 1;
    }
    from
}

/// Where a declaration beginning at `start` ends: the line carrying its
/// terminating `;` for an interface declaration, or the line closing its body
/// for a definition.
///
/// Brace-counted rather than assuming one line, because ERC fences wrap long
/// parameter lists across many lines — erc-1271's `isValidSignature` spans
/// seven. The scan is bounded by the fence, so a malformed block costs the
/// rest of that fence and nothing else.
fn declaration_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    for (i, line) in lines.iter().enumerate().skip(start) {
        for c in line.chars() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth <= 0 {
                        return i + 1;
                    }
                }
                ';' if depth == 0 => return i + 1,
                _ => {}
            }
        }
    }
    lines.len()
}
