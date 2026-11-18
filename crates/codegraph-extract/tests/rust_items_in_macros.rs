//! Rust items declared inside a declarative or function-like macro.
//!
//! `pin_project! { pub struct Timeout<T> { .. } }` and tokio's `cfg_rt! { .. }` wrappers are
//! `macro_invocation` nodes whose body tree-sitter reports as an opaque `token_tree`
//! (verified with `tree_probe`: `(macro_invocation macro: (identifier) (token_tree ..))`).
//! The walker only recognised `struct_item`, so every item a macro wraps was invisible: 69 of the
//! 76 type declarations missing from tokio's index sat inside one. An attribute macro and a
//! `macro_rules!` definition are not items to recover, so they stay untouched.
use codegraph_extract::extract_source;
use codegraph_types::Language;

fn names_in(source: &str, kind: &str) -> Vec<String> {
    let result = extract_source("src/pin.rs", source, Language::Rust).expect("extract");
    result
        .nodes
        .into_iter()
        .filter(|n| n.kind.as_str() == kind)
        .map(|n| n.name)
        .collect()
}

#[test]
fn a_struct_inside_pin_project_is_indexed() {
    let source = "pin_project! {\n\
                  \x20   /// A future.\n\
                  \x20   pub struct Timeout<T> {\n\
                  \x20       #[pin] value: T,\n\
                  \x20   }\n\
                  }\n";
    let structs = names_in(source, "struct");
    assert_eq!(structs, vec!["Timeout".to_string()], "structs: {structs:?}");
}

#[test]
fn items_inside_a_cfg_macro_are_indexed() {
    let source = "cfg_rt! {\n\
                  \x20   pub struct Runtime {\n\
                  \x20       handle: Handle,\n\
                  \x20   }\n\
                  \x20   pub fn runtime() -> Runtime { Runtime { handle: Handle } }\n\
                  }\n";
    assert_eq!(names_in(source, "struct"), vec!["Runtime".to_string()]);
    let fns = names_in(source, "function");
    assert!(fns.contains(&"runtime".to_string()), "functions: {fns:?}");
}

#[test]
fn an_enum_inside_a_macro_is_indexed() {
    let source = "cfg_io_util! {\n\
                  \x20   pub enum Interest {\n\
                  \x20       Readable,\n\
                  \x20       Writable,\n\
                  \x20   }\n\
                  }\n";
    assert_eq!(names_in(source, "enum"), vec!["Interest".to_string()]);
}

#[test]
fn an_attribute_macro_on_an_item_does_not_hide_it() {
    let source = "#[derive(Debug)]\npub struct Visible {\n    a: u8,\n}\n";
    assert_eq!(names_in(source, "struct"), vec!["Visible".to_string()]);
}

#[test]
fn a_macro_rules_definition_declares_no_item() {
    // The body is a definition, not a use: nothing inside it is a real symbol.
    let source = "macro_rules! make {\n    ($name:ident) => { pub struct $name; };\n}\n";
    let structs = names_in(source, "struct");
    assert!(structs.is_empty(), "invented structs: {structs:?}");
}
