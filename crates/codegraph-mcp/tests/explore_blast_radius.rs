//! explore's blast radius (upstream's always-on section): for the named symbols, who calls them
//! and which tests cover them, so the agent knows what to update before editing. Rust unit tests
//! count as tests in both standard layouts, a `tests.rs` beside the module and an inline
//! `#[cfg(test)] mod` (upstream #3265), by their `#[test]` attribute rather than their path.
use codegraph_mcp::render::explore;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;

fn index(root: &Path, files: &[(&str, &str)]) -> codegraph_core::CodeGraph {
    for (rel, text) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let mut cg = codegraph_core::CodeGraph::init(root.to_str().unwrap()).unwrap();
    cg.index_all(&Default::default(), None, None).unwrap();
    cg
}

fn text(cg: &codegraph_core::CodeGraph, query: &str) -> String {
    let result = explore(cg, &json!({ "query": query }), &mut HashMap::new()).unwrap();
    result.content[0]["text"].as_str().unwrap().to_string()
}

fn blast_line<'a>(out: &'a str, name: &str) -> &'a str {
    let section = out.split("**Blast radius").nth(1).unwrap_or_else(|| panic!("no blast radius section: {out}"));
    section.lines().find(|l| l.starts_with(&format!("- `{name}`"))).unwrap_or_else(|| panic!("no entry for {name}: {out}"))
}

#[test]
fn rust_unit_tests_in_both_layouts_count_as_tests() {
    let dir = tempfile::tempdir().unwrap();
    let cg = index(
        dir.path(),
        &[
            ("Cargo.toml", "[package]\nname = \"r\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            ("src/main.rs", "mod store;\nfn main() { store::create_note(); }\n"),
            (
                "src/store.rs",
                "pub fn create_note() -> i32 { 1 }\n#[cfg(test)]\nmod tests;\n#[cfg(test)]\nmod inline {\n    #[test]\n    fn t2() { super::create_note(); }\n}\n",
            ),
            ("src/store/tests.rs", "use super::*;\n#[test]\nfn t1() { create_note(); }\n"),
        ],
    );
    let out = text(&cg, "create_note");
    let line = blast_line(&out, "create_note");
    assert!(line.contains("3 callers"), "{line}");
    assert!(line.contains("in `src/main.rs`"), "the production caller: {line}");
    assert!(line.contains("tests: "), "{line}");
    assert!(line.contains("`src/store/tests.rs`"), "the out-of-line tests.rs: {line}");
    assert!(line.contains("`src/store.rs`"), "the inline #[cfg(test)] module: {line}");
    assert!(!line.contains("no tests found"), "{line}");
}

#[test]
fn a_typescript_caller_in_a_test_file_is_its_test_and_a_symbol_nobody_calls_has_no_entry() {
    let dir = tempfile::tempdir().unwrap();
    let cg = index(
        dir.path(),
        &[
            ("src/price.ts", "export function price(n: number) { return n * 2; }\nexport function unused() { return 0; }\n"),
            ("src/cart.ts", "import { price } from './price';\nexport function total() { return price(3); }\n"),
            ("src/price.test.ts", "import { price } from './price';\nexport function checksPrice() { return price(1) === 2; }\n"),
        ],
    );
    let out = text(&cg, "price");
    let line = blast_line(&out, "price");
    assert!(line.contains("in `src/cart.ts`"), "{line}");
    assert!(line.contains("tests: `src/price.test.ts`"), "{line}");
    let out = text(&cg, "unused");
    assert!(!out.contains("**Blast radius"), "{out}");
}
