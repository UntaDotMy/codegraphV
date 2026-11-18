//! Rust items written inside a declarative macro's braces.
//!
//! `pin_project! { pub struct Timeout<T> { .. } }` and tokio's `cfg_rt! { .. }` reach the
//! grammar as a `macro_invocation` whose body is an opaque `token_tree`, so the walker never
//! sees the `struct_item` inside it and the declaration never reaches the index. Measured on
//! tokio 1.52.3: 464 type names indexed, 97 missing, and all 97 sit inside a macro body.
//!
//! The grammar decides what a body holds; no macro-name list is involved. An invocation is
//! unwrapped when its body — comments, attributes and whitespace skipped — starts with an
//! item keyword and parses, standalone and error-free, as a run of items that declares at
//! least one type or function. `macro_rules! make { ($n:ident) => { pub struct $n; } }` is
//! refused (a pattern and a template are not items), and so is a macro inside a function
//! body: only item position is considered.
//!
//! Unwrapping blanks the invocation's name and its two braces to spaces, which keeps every
//! byte offset, line and column, and makes each body item a file-level item of the tree the
//! walker reads. The walker keeps slicing the source as written, so a recovered item's
//! signature and docstring are the text the file actually holds.
//!
//! Recovery is a heuristic: a `static` inside `lazy_static!`/`thread_local!` is not a type or
//! function declaration and is left alone, and a file whose blanked form does not parse
//! cleanly is walked as written.

use crate::ts::{self, Node};

/// A blankable invocation: `(name_start, name_end, open_brace, close_brace)`, absolute offsets.
pub(crate) type Span = (usize, usize, usize, usize);

/// Words that begin an item. A body starting with any other word is an expression
/// (`vec![]`, `format!`, `x! { .. }`), so it is refused without a parse.
const ITEM_WORDS: &[&str] = &[
    "pub", "struct", "enum", "union", "trait", "type", "fn", "impl", "const", "static", "mod", "use", "extern",
    "unsafe", "async", "macro_rules", "macro",
];

/// Item kinds a body may contain once unwrapped. A comment and an attribute are allowed
/// because they are siblings of the item they document or decorate.
fn is_item(kind: &str) -> bool {
    matches!(
        kind,
        "struct_item"
            | "enum_item"
            | "union_item"
            | "trait_item"
            | "type_item"
            | "function_item"
            | "function_signature_item"
            | "impl_item"
            | "const_item"
            | "static_item"
            | "mod_item"
            | "use_declaration"
            | "extern_crate_declaration"
            | "foreign_mod_item"
            | "attribute_item"
            // `feature! { #![any(..)] .. }` — an inner attribute is a crate-level decorator, not
            // an expression, so it does not stop the body being a run of items.
            | "inner_attribute_item"
            | "line_comment"
            | "block_comment"
            // A `macro_rules!` written beside the items a body also holds is part of the run;
            // tokio's `cfg_io_util!` bodies open with one.
            | "macro_definition"
            // A nested `cfg_*!` is part of the run too: it is blanked in the same pass, so the
            // outer body must not be refused for holding it.
            | "macro_invocation"
    )
}

/// Item kinds that declare a name code is looked up by. A body holding none of them — a
/// `lazy_static!` body of `static ref`s, say — is not unwrapped: those items are written in
/// a macro's own dialect and are not Rust declarations as they stand.
fn declares_a_name(kind: &str) -> bool {
    matches!(
        kind,
        "struct_item" | "enum_item" | "union_item" | "trait_item" | "type_item" | "function_item" | "impl_item"
    )
}

/// Every item-position macro invocation in `tree` that the grammar reads as holding items,
/// including the ones a nested body only shows once its parent is unwrapped.
pub(crate) fn item_macro_spans(tree: &ts::Tree, src: &str, grammar: &ts::Language) -> Vec<Span> {
    let mut out = Vec::new();
    scan_items(&tree.root_node(), src, 0, grammar, &mut out);
    out
}

/// `offset` is the absolute start of the slice the tree was parsed from; the tree's own byte
/// offsets are relative to it.
fn scan_items<'t>(root: &Node<'t>, slice: &str, offset: usize, grammar: &ts::Language, out: &mut Vec<Span>) {
    for i in 0..root.named_child_count() {
        let Some(child) = root.named_child(i) else { continue };
        match child.kind() {
            "macro_invocation" => scan_invocation(&child, slice, offset, grammar, out),
            // `pin_project! { .. };` lands inside a statement wrapper instead of the file.
            "expression_statement" => {
                if let Some(inner) = child.named_child(0) {
                    if inner.kind() == "macro_invocation" && child.named_child_count() == 1 {
                        scan_invocation(&inner, slice, offset, grammar, out);
                    }
                }
            }
            // A module's body holds items, so a macro there is in item position too.
            "mod_item" => {
                for j in 0..child.named_child_count() {
                    if let Some(list) = child.named_child(j) {
                        if list.kind() == "declaration_list" {
                            scan_items(&list, slice, offset, grammar, out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn scan_invocation<'t>(invocation: &Node<'t>, slice: &str, offset: usize, grammar: &ts::Language, out: &mut Vec<Span>) {
    match unwrappable(invocation, slice, grammar) {
        Some((name, open, close)) => {
            out.push((offset + name.start, offset + name.end, offset + open, offset + close));
            // The body's own macros are nodes only after this one is blanked; collect them now,
            // so one pass covers a nested `cfg_rt! { cfg_net! { .. } }`.
            if let Some(body) = token_tree(invocation) {
                let text = &slice[body.byte_range()];
                let inner = &text[1..text.len() - 1];
                if let Some(tree) = parse_body(grammar, inner) {
                    scan_items(&tree.root_node(), inner, offset + body.start_byte() + 1, grammar, out);
                }
            }
        }
        // Not items as written, but a body can still hold a run of items below its first
        // non-item (a `cfg_*!` body that also carries a statement): look inside it.
        None => {
            if let Some(body) = token_tree(invocation) {
                let text = &slice[body.byte_range()];
                if text.starts_with('{') && text.ends_with('}') {
                    let inner = &text[1..text.len() - 1];
                    if let Some(tree) = parse_body(grammar, inner) {
                        scan_items(&tree.root_node(), inner, offset + body.start_byte() + 1, grammar, out);
                    }
                }
            }
        }
    }
}

struct Named {
    start: usize,
    end: usize,
}

/// The invocation's blankable span when its body reads as items: the `name!` run and the two
/// braces.
fn unwrappable<'t>(invocation: &Node<'t>, slice: &str, grammar: &ts::Language) -> Option<(Named, usize, usize)> {
    let macro_node = invocation.child_by_field_name("macro")?;
    let name_end = macro_node.end_byte();
    let body = token_tree(invocation)?;
    let text = &slice[body.byte_range()];
    if !(text.starts_with('{') && text.ends_with('}')) || text.len() < 2 {
        return None;
    }
    let inner = &text[1..text.len() - 1];
    if !starts_with_item_word(inner) {
        return None;
    }
    let tree = parse_body(grammar, inner)?;
    let root = tree.root_node();
    if root.has_error() || !reads_as_items(&root) {
        return None;
    }
    Some((
        Named { start: macro_node.start_byte(), end: name_end },
        body.start_byte(),
        body.end_byte() - 1,
    ))
}

fn token_tree<'t>(invocation: &Node<'t>) -> Option<Node<'t>> {
    (0..invocation.named_child_count())
        .filter_map(|i| invocation.named_child(i))
        .find(|c| c.kind() == "token_tree")
}

fn parse_body(grammar: &ts::Language, body: &str) -> Option<ts::Tree> {
    ts::parse(grammar, "rust", body).ok()
}

/// Every named child is an item, and at least one of them declares a name.
fn reads_as_items<'t>(root: &Node<'t>) -> bool {
    let mut declares = false;
    for i in 0..root.named_child_count() {
        let Some(child) = root.named_child(i) else { continue };
        if !is_item(child.kind()) {
            return false;
        }
        declares |= declares_a_name(child.kind());
    }
    declares
}

/// The body's first word is an item keyword, after any comments and attributes.
fn starts_with_item_word(mut rest: &str) -> bool {
    loop {
        let trimmed = rest.trim_start();
        let skipped = if let Some(after) = trimmed.strip_prefix("//") {
            after.find('\n').map_or("", |i| &after[i + 1..])
        } else if let Some(after) = trimmed.strip_prefix("/*") {
            after.find("*/").map_or("", |i| &after[i + 2..])
        } else if trimmed.starts_with('#') && trimmed[1..].trim_start().starts_with('[') {
            after_attribute(trimmed).unwrap_or("")
        } else if let Some(after) = trimmed.strip_prefix("#!").filter(|a| a.trim_start().starts_with('[')) {
            // `#![any(..)]`: a crate-level inner attribute, as `feature!` bodies carry.
            after_attribute(after).unwrap_or("")
        } else {
            let word: &str = trimmed
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap_or("");
            return ITEM_WORDS.contains(&word);
        };
        if skipped.is_empty() {
            return false;
        }
        rest = skipped;
    }
}

/// The text after an attribute's closing `]`, counting nested `[` `]`.
fn after_attribute(text: &str) -> Option<&str> {
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&text[i + 1..]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `source` with every span's name, `!` and braces blanked. Blanks are ASCII spaces over ASCII
/// bytes the grammar placed, so the source's length and every other byte are untouched.
///
/// `name_end` stops before the `!`, so the `!` is blanked here too — a leftover `!` would make
/// the blanked file `{ .. }!`-shaped, which does not parse, and the safety check below would
/// then discard a correct unwrap.
pub(crate) fn blank_spans(source: &str, spans: &[Span]) -> String {
    let mut bytes = source.as_bytes().to_vec();
    for (name_start, name_end, open, close) in spans {
        for at in *name_start..*name_end {
            if let Some(b) = bytes.get_mut(at) {
                *b = b' ';
            }
        }
        // The `!` (and any whitespace before the body) between the name and the brace.
        for at in *name_end..*open {
            if let Some(b) = bytes.get_mut(at) {
                if !b.is_ascii_whitespace() {
                    *b = b' ';
                }
            }
        }
        for at in [*open, *close] {
            if let Some(b) = bytes.get_mut(at) {
                *b = b' ';
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
