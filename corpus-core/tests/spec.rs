//! Spec parsing tests over a real committed consensus-specs file
//! (specs/electra/beacon-chain.md) — no network, per the hard rule.

use std::fs;
use std::path::Path;

use corpus_core::spec::{SpecConstant, constants, functions};

fn fixture() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/spec_electra_beacon_chain.md");
    fs::read_to_string(path).expect("fixture exists")
}

#[test]
fn finds_the_electra_max_effective_balance_row() {
    let all = constants(&fixture());
    let hit = all
        .iter()
        .find(|c| c.name == "MAX_EFFECTIVE_BALANCE_ELECTRA")
        .expect("the M10 gate constant");
    assert!(hit.value.contains("Gwei(2**11 * 10**9)"), "{}", hit.value);
    assert!(hit.value.contains("2,048,000,000,000"), "human-readable form kept");
    assert_eq!(
        hit.description.as_deref(),
        Some("Maximum effective balance for a compounding validator")
    );
}

#[test]
fn table_headers_and_separators_are_not_constants() {
    let all = constants(&fixture());
    assert!(!all.is_empty());
    for c in &all {
        assert!(!c.name.contains('-'), "separator row leaked: {:?}", c.name);
        assert_ne!(c.name, "NAME", "header row leaked");
        assert!(!c.value.is_empty());
    }
}

#[test]
fn extracts_functions_with_their_modified_headings() {
    let all = functions(&fixture());
    let deposit = all
        .iter()
        .find(|f| f.name == "process_deposit")
        .expect("process_deposit");
    assert!(deposit.code.starts_with("def process_deposit(state: BeaconState"));
    assert!(deposit.code.contains("apply_deposit"), "body captured");
    // The fixture labels it via the heading above the fence.
    assert!(
        deposit.heading.as_deref().is_some_and(|h| h.contains("process_deposit")),
        "heading: {:?}",
        deposit.heading
    );

    let new_fn = all
        .iter()
        .find(|f| f.name == "process_deposit_request")
        .expect("process_deposit_request");
    assert!(
        new_fn.heading.as_deref().is_some_and(|h| h.starts_with("New")),
        "spec's New label survives: {:?}",
        new_fn.heading
    );
}

#[test]
fn class_definitions_are_not_functions() {
    // The fixture's container classes (`class BeaconState(Container):`)
    // live in python fences too; only top-level defs come out.
    for f in functions(&fixture()) {
        assert!(!f.name.is_empty());
        assert!(f.code.starts_with("def "), "{}", f.name);
    }
}

#[test]
fn edge_cases_from_synthetic_markdown() {
    // Two defs in one fence split at the def boundary.
    let two = "```python\ndef first(x):\n    return x\n\ndef second(y):\n    return y\n```";
    let fns = functions(two);
    assert_eq!(fns.len(), 2);
    assert_eq!(fns[0].name, "first");
    assert!(!fns[0].code.contains("def second"));
    assert_eq!(fns[1].name, "second");

    // A rust fence is ignored even though it contains "def" text.
    let rust = "```rust\n// def looks_like_python(\n```";
    assert!(functions(rust).is_empty());

    // A two-column table (no description) parses with description = None.
    let table = "| Name | Value |\n| - | - |\n| `FOO_BAR` | `7` |";
    assert_eq!(
        constants(table),
        vec![SpecConstant {
            name: "FOO_BAR".into(),
            value: "`7`".into(),
            description: None,
        }]
    );

    // Tables inside fences are content, not constants.
    let fenced = "```python\n# | `NOT_A_CONSTANT` | `1` |\n```";
    assert!(constants(fenced).is_empty());

    // Lowercase or non-identifier first cells are prose, not constants.
    let prose = "| `state.validators` | something |\n| plain text | more |";
    assert!(constants(prose).is_empty());
}
