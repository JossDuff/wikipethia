//! Spec parsing tests over a real committed consensus-specs file
//! (specs/electra/beacon-chain.md) — no network, per the hard rule.

use std::fs;
use std::path::Path;

use corpus_core::spec::{
    SpecConstant, constants, functions, functions_in_python, solidity_constants,
    solidity_declarations,
};

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
fn fence_language_variants_from_real_eips_are_recognized() {
    // eip-100 uses "``` python" (space), the eip-3368 family "```py",
    // eip-3076 "```python3" — all hold spec functions.
    for opener in ["``` python", "```py", "```python3", "```python"] {
        let md = format!("{opener}\ndef calc_thing(x):\n    return x\n```");
        let fns = functions(&md);
        assert_eq!(fns.len(), 1, "opener {opener:?} not recognized");
        assert_eq!(fns[0].name, "calc_thing");
    }
    // Non-python languages still don't count.
    assert!(functions("```rust\ndef fake(x):\n```").is_empty());
}

#[test]
fn four_backtick_fences_nest_three_backtick_content() {
    // The erc-5252 shape: a ````-fence whose body contains ```-fences.
    // Everything inside is content; parsing resumes after the ```` closer.
    let md = "````math\n```math\nx = 1\n```\n````\n\n\
              #### `after_the_nested_block`\n\n\
              ```python\ndef after_the_nested_block(s):\n    return s\n```\n\n\
              | Name | Value |\n| - | - |\n| `AFTER_CONST` | `1` |";
    let fns = functions(md);
    assert_eq!(fns.len(), 1, "defs after a nested fence must survive");
    assert_eq!(fns[0].name, "after_the_nested_block");
    let consts = constants(md);
    assert_eq!(consts.len(), 1, "constants after a nested fence must survive");
    assert_eq!(consts[0].name, "AFTER_CONST");
}

#[test]
fn an_unclosed_python_fence_still_yields_its_functions() {
    let md = "```python\ndef trailing(x):\n    return x";
    let fns = functions(md);
    assert_eq!(fns.len(), 1);
    assert_eq!(fns[0].name, "trailing");
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

#[test]
fn python_function_bodies_stop_at_the_next_module_level_statement() {
    // The real shape from EELS stack.py: a def followed by module-level
    // aliases. Taking def-to-next-def would return all of it as the
    // function's source.
    let src = "\"\"\"Stack ops.\"\"\"\n\n\
               def swap_n(evm: Evm, n: int) -> None:\n\
               \x20   \"\"\"Swap stack items.\"\"\"\n\
               \x20   evm.stack[0], evm.stack[n] = evm.stack[n], evm.stack[0]\n\n\
               swap1 = partial(swap_n, n=1)\n\
               swap2 = partial(swap_n, n=2)\n\n\
               def dup_n(evm: Evm, n: int) -> None:\n\
               \x20   push(evm, evm.stack[n])\n";
    let fns = functions_in_python(src);
    assert_eq!(fns.len(), 2);
    assert_eq!(fns[0].name, "swap_n");
    assert!(fns[0].code.contains("evm.stack[0], evm.stack[n]"), "body kept");
    assert!(!fns[0].code.contains("swap1 = partial"), "aliases are not the body:\n{}", fns[0].code);
    assert_eq!(fns[1].name, "dup_n");
    assert!(fns[1].code.contains("push(evm"));
    // Continuation lines of a multi-line signature stay attached.
    let wrapped = "def f(\n    a: int,\n) -> None:\n    return a\n\nX = 1\n";
    let fns = functions_in_python(wrapped);
    assert_eq!(fns.len(), 1);
    assert!(fns[0].code.contains("return a"));
    assert!(!fns[0].code.contains("X = 1"));
    // A bare .py file has no fences, so the markdown parser sees nothing.
    assert!(functions(src).is_empty());
}

// ---------------------------------------------------------------------------
// Solidity fences. The shapes below are copied from real ERC documents, not
// invented — erc-1271's fence in particular is verbatim, mislabelled
// `javascript` and all.
// ---------------------------------------------------------------------------

/// erc-1271, verbatim. Note the info string: `javascript`. 19 ingested
/// EIP/ERC documents mislabel their Solidity this way and never tag a single
/// fence `solidity`, so an info-string-only rule misses the exact document
/// that motivated Solidity support.
const ERC1271: &str = r#"
## Specification

```javascript
pragma solidity ^0.5.0;

contract ERC1271 {

  // bytes4(keccak256("isValidSignature(bytes32,bytes)")
  bytes4 constant internal MAGICVALUE = 0x1626ba7e;

  /**
   * @dev Should return whether the signature provided is valid
   *
   * MUST return the bytes4 magic value 0x1626ba7e when function passes.
   */
  function isValidSignature(
    bytes32 _hash,
    bytes memory _signature)
    public
    view
    returns (bytes4 magicValue);
}
```

Prose after the fence, with `function notReal(` inside it.
"#;

#[test]
fn a_mislabelled_solidity_fence_is_still_read() {
    let fns = solidity_declarations(ERC1271);
    assert_eq!(fns.len(), 1, "one function declaration, got {fns:?}");
    assert_eq!(fns[0].name, "isValidSignature");
    assert_eq!(fns[0].language, "solidity", "must not be fenced as python");
}

#[test]
fn a_declaration_carries_the_doc_comment_that_states_the_answer() {
    let fns = solidity_declarations(ERC1271);
    // The whole point: the magic value is in the comment, not the signature.
    assert!(
        fns[0].code.contains("MUST return the bytes4 magic value 0x1626ba7e"),
        "{}",
        fns[0].code
    );
    // And the declaration itself, which wraps across seven lines.
    assert!(fns[0].code.contains("returns (bytes4 magicValue);"), "{}", fns[0].code);
    // But not the closing brace of the enclosing contract.
    assert!(!fns[0].code.trim_end().ends_with('}'), "{}", fns[0].code);
}

#[test]
fn solidity_constants_carry_the_magic_value_and_its_visibility() {
    let consts = solidity_constants(ERC1271);
    assert_eq!(consts.len(), 1, "{consts:?}");
    assert_eq!(consts[0].name, "MAGICVALUE");
    assert_eq!(consts[0].value, "0x1626ba7e");
    // Visibility matters to a reader deciding whether they can rely on it.
    assert_eq!(consts[0].description.as_deref(), Some("bytes4 constant internal"));
}

#[test]
fn prose_outside_a_fence_is_not_mistaken_for_a_declaration() {
    // The trailing paragraph of ERC1271 contains "function notReal(".
    let fns = solidity_declarations(ERC1271);
    let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["isValidSignature"]);
}

#[test]
fn a_javascript_fence_without_solidity_is_left_alone() {
    // Sniffing for `function` shapes rather than `pragma solidity` would
    // claim this, and every other JS example in the ERCs.
    let js = r#"
```javascript
function transferHelper(token, to, amount) {
  return token.methods.transfer(to, amount).send();
}
```
"#;
    assert!(solidity_declarations(js).is_empty());
    assert!(solidity_constants(js).is_empty());
}

#[test]
fn a_definition_with_a_body_ends_at_its_closing_brace() {
    let src = r#"
```solidity
contract C {
    function first() public pure returns (uint256) {
        if (true) {
            return 1;
        }
        return 2;
    }

    function second() public {}
}
```
"#;
    let fns = solidity_declarations(src);
    let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["first", "second"]);
    // Nested braces must not end the block early, and `second` must not be
    // swallowed into `first`.
    assert!(fns[0].code.contains("return 2;"), "{}", fns[0].code);
    assert!(!fns[0].code.contains("second"), "{}", fns[0].code);
}

#[test]
fn constant_is_matched_as_a_word_not_a_prefix() {
    let src = r#"
```solidity
uint256 constantProduct = 1;
uint256 public constant MAX_SUPPLY = 10_000;
```
"#;
    let consts = solidity_constants(src);
    let names: Vec<&str> = consts.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["MAX_SUPPLY"], "constantProduct is not a constant");
}

#[test]
fn python_extraction_still_labels_itself_python() {
    // The language tag exists so the renderer stops calling everything
    // python; the python paths must actually say so.
    let py = "```python\ndef f():\n    return 1\n```\n";
    assert_eq!(functions(py)[0].language, "python");
    assert_eq!(functions_in_python("def g():\n    return 2\n")[0].language, "python");
}
