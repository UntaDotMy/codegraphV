//! Whether a symbol is test code — one answer for the surfaces that mark implementors as tests
//! (upstream 08b187aa).
//!
//! Three readings, cheapest first: the file path, the qualified name (a nested `Tests` class, a
//! `tests` module the extractor names), and, for Rust, the source itself: `#[cfg(test)] mod tests`
//! lives inside `src/`, and the extractor neither mints a node for the module nor prefixes the
//! names declared in it, so a mock there is invisible to the first two readings.
use crate::dead_code::is_test_scope;
use codegraph_resolve::rust_scopes::blank_rust_code;
use codegraph_search::query_utils::is_test_file;
use codegraph_types::Node;
use regex::Regex;
use std::sync::LazyLock;

/// `#[cfg(test)]`, or `test` as one condition of `#[cfg(all(…))]`. `not(test)` is neither.
static CFG_TEST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^#\[cfg\((?:test\)|all\((?:.*,\s*)?test\s*[,)])").unwrap());
static MOD_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{").unwrap());

/// `node` is test code. `source` is the node's file, read when the caller has it.
pub fn is_test_code(node: &Node, source: Option<&str>) -> bool {
    if is_test_file(&node.file_path) {
        return true;
    }
    if !node.qualified_name.is_empty() && is_test_scope(&node.qualified_name) {
        return true;
    }
    node.language.as_str() == "rust" && source.is_some_and(|s| in_cfg_test_module(s, node.start_line as usize))
}

/// `line` (1-based) is inside a module declared under a test `cfg`. The module's extent is its
/// brace block, counted with comments and the contents of strings and characters blanked.
pub fn in_cfg_test_module(source: &str, line: usize) -> bool {
    if line == 0 {
        return false;
    }
    let blanked = blank_rust_code(source);
    let lines: Vec<&str> = blanked.split('\n').collect();
    let mut armed = false;
    let mut i = 0;
    while i < lines.len() && i < line {
        let text = lines[i].trim();
        if CFG_TEST.is_match(text) {
            armed = true;
            i += 1;
            continue;
        }
        if !armed || text.starts_with("#[") || text.is_empty() {
            i += 1;
            continue;
        }
        armed = false;
        if !MOD_OPEN.is_match(text) {
            i += 1;
            continue;
        }
        // Walk the block: it ends on the line where its braces close.
        let mut depth = 0i64;
        let mut j = i;
        while j < lines.len() {
            for ch in lines[j].chars() {
                match ch {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if depth <= 0 {
                if line - 1 > i && line - 1 <= j {
                    return true;
                }
                i = j;
                break;
            }
            j += 1;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::in_cfg_test_module;

    #[test]
    fn inside_the_module_and_not_before_after_or_in_other_modules() {
        let source = [
            "pub struct A;",
            "#[cfg(test)]",
            "#[allow(dead_code)]",
            "mod tests {",
            "    struct Mock { x: u8 }",
            "    fn braces() { let _ = \"{\"; }",
            "}",
            "pub struct B;",
            "#[cfg(not(test))]",
            "mod real { pub struct C; }",
        ]
        .join("\n");
        assert!(!in_cfg_test_module(&source, 1));
        assert!(in_cfg_test_module(&source, 5));
        assert!(in_cfg_test_module(&source, 6));
        assert!(!in_cfg_test_module(&source, 8));
        assert!(!in_cfg_test_module(&source, 10));
    }

    #[test]
    fn finds_the_end_past_a_brace_in_a_char_a_raw_string_or_a_block_comment() {
        let tricky = [
            "#[cfg(test)]",
            "mod tests {",
            "    const OPEN: char = '{';",
            "    const RAW: &str = r#\"{\"#;",
            "    /* { */",
            "    struct Mock;",
            "}",
            "pub struct After;",
        ]
        .join("\n");
        assert!(in_cfg_test_module(&tricky, 6));
        assert!(!in_cfg_test_module(&tricky, 8));
    }

    #[test]
    fn reads_test_inside_cfg_all_and_not_a_negated_test() {
        let gated = [
            "#[cfg(all(test, not(loom)))]",
            "mod tests {",
            "    struct Mock;",
            "}",
            "#[cfg(all(feature = \"sync\", test))]",
            "mod sync_tests {",
            "    struct SyncMock;",
            "}",
            "#[cfg(all(unix, not(test)))]",
            "mod real {",
            "    struct Real;",
            "}",
        ]
        .join("\n");
        assert!(in_cfg_test_module(&gated, 3));
        assert!(in_cfg_test_module(&gated, 7));
        assert!(!in_cfg_test_module(&gated, 11));
    }
}
