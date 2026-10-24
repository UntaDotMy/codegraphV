//! Whether a Rust name resolves to a candidate where it is written: paths, `use` imports, glob
//! imports, associated items and enum variants.

use super::impls::*;
use super::uses::*;
use crate::ctx::Ctx;
use crate::name_matcher::util::{after_col, before_col, line_at, starts_with_at};
use crate::types::UnresolvedRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_byte_index, utf16_len};
use codegraph_types::{EdgeKind, Node, NodeKind};

fn declares_rust_variant(type_: &Node, variant: &str, ctx: &Ctx) -> bool {
    if type_.kind != NodeKind::Enum {
        return false;
    }
    let qualified = format!("{}::{variant}", type_.qualified_name);
    ctx.get_nodes_in_file_named(&type_.file_path, variant).iter().any(|n| n.kind == NodeKind::EnumMember && n.qualified_name == qualified)
}

fn is_in_enclosing_rust_impl(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let header = rust_enclosing_impl_line(r, ctx);
    header > 0 && candidate.file_path == r.file_path && candidate.start_line >= header && candidate.start_line <= r.line
}

/// `Assoc = Type` / `Assoc: Bound` written inside `<…>` (an associated type binding, not a type read).
fn is_rust_assoc_binding(line: &str, column: u32, name: &str) -> bool {
    if !starts_with_at(line, name, column)
        || !re!(r"^\s*(?:=(?![=>])|:(?!:))").is_match(after_col(after_col(line, column), utf16_len(name) as u32))
    {
        return false;
    }
    let before: Vec<char> = before_col(line, column).chars().collect();
    let mut angles = 0i32;
    let mut parens = 0i32;
    for i in (0..before.len()).rev() {
        let c = before[i];
        if c == '>' && (i == 0 || before[i - 1] != '-') {
            angles += 1;
        } else if c == '<' {
            if angles == 0 {
                return parens == 0;
            }
            angles -= 1;
        } else if c == ')' {
            parens += 1;
        } else if c == '(' {
            if parens == 0 {
                return false;
            }
            parens -= 1;
        } else if c == '{' || c == '}' || c == ';' {
            return false;
        }
    }
    false
}

struct QualifiedSelf {
    self_: String,
    trait_: Option<String>,
}

fn qualified_self_head(part: &str) -> String {
    let stripped = re!(r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?|dyn\s+)+").replace_first(part, |_| String::new());
    re!(r"((?:[A-Za-z_]\w*\s*::\s*)*)([A-Za-z_]\w*)\s*(?:<[\s\S]*)?$")
        .captures(js_trim(&stripped))
        .and_then(|c| c.text(2).map(String::from))
        .unwrap_or_default()
}

/// `<Self as Trait>::` / `<T>::` just before the reference.
fn rust_qualified_self(before: &str) -> Option<QualifiedSelf> {
    let text = re!(r"\s*::\s*$").replace_first(before, |_| String::new());
    if !text.ends_with('>') {
        return None;
    }
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut open: Option<usize> = None;
    let mut i = bytes.len();
    while i > 0 && open.is_none() {
        i -= 1;
        if bytes[i] == b'>' && (i == 0 || bytes[i - 1] != b'-') {
            depth += 1;
        } else if bytes[i] == b'<' {
            depth -= 1;
            if depth == 0 {
                open = Some(i);
            }
        }
    }
    let open = open?;
    if re!(r"::\s*$").is_match(&text[..open]) {
        return None;
    }
    let inner = js_trim(&text[open + 1..text.len() - 1]).to_string();
    let as_ = re!(r"\s+as\s+").find(&inner);
    let self_ = qualified_self_head(match &as_ {
        Some(a) => &inner[..a.start()],
        None => &inner,
    });
    if self_.is_empty() {
        return None;
    }
    let trait_ = as_.map(|a| qualified_self_head(&inner[a.end()..])).filter(|t| !t.is_empty());
    Some(QualifiedSelf { self_, trait_ })
}

fn is_type_member_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Property | NodeKind::Field | NodeKind::EnumMember)
}

/// Whether `candidate` is what the Rust reference `r` names, given its scope (paths, `use`s, globs).
pub fn is_rust_name_in_scope(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !is_rust_item_visible_from(candidate, r) {
        return false;
    }
    let name = r.reference_name.as_str();
    let line = line_at(ctx, &r.file_path, r.line);
    let references = r.is_kind(EdgeKind::References);
    if references {
        if let Some(line) = &line {
            if is_rust_assoc_binding(line, r.column, name) {
                return false;
            }
        }
    }
    let at: String = line.as_deref().map(|l| after_col(l, r.column).to_string()).unwrap_or_default();
    let type_read = references && re!(r"^\w+$").is_match(name);
    let via_self = type_read && name != "Self" && re!(r"^Self\s*::").is_match(&at);
    let variant = if type_read && (via_self || (at.starts_with(name) && !re!(r"^\w").is_match(&at[name.len()..]))) {
        re!(r"^\w+\s*::\s*([A-Z]\w*)").captures(&at).and_then(|c| c.text(1).map(String::from))
    } else {
        None
    };
    if let Some(variant) = &variant {
        if !declares_rust_variant(candidate, variant, ctx) {
            return false;
        }
    }
    let pathed: Option<(String, String)> = match (&line, via_self) {
        (None, _) | (_, true) => None,
        (Some(line), false) => {
            let before = before_col(line, r.column);
            if starts_with_at(line, name, r.column) && re!(r"::\s*$").is_match(before) {
                re!(r"((?:[A-Za-z_]\w*\s*::\s*)*)([A-Za-z_]\w*)?\s*::\s*$")
                    .captures(before)
                    .map(|c| (c.text(1).unwrap_or("").to_string(), c.text(2).unwrap_or("").to_string()))
            } else if !Re::cached(&format!(r"(?<![\w$:]){}\b", escape_regex(name)), "").is_match(line) {
                Re::cached(&format!(r"((?:[A-Za-z_]\w*\s*::\s*)*)([A-Za-z_]\w*)\s*::\s*{}\b", escape_regex(name)), "")
                    .captures(line)
                    .map(|c| (c.text(1).unwrap_or("").to_string(), c.text(2).unwrap_or("").to_string()))
            } else {
                None
            }
        }
    };
    if let Some((prefix, seg)) = pathed {
        let root = re!(r"^([A-Za-z_]\w*)").captures(&prefix).and_then(|c| c.text(1).map(String::from)).unwrap_or_else(|| seg.clone());
        if references && prefix.is_empty() && line.is_some() {
            if seg == "Self" {
                return candidate.kind == NodeKind::TypeAlias && is_in_enclosing_rust_impl(candidate, r, ctx);
            }
            if is_rust_generic_param(&seg, r, ctx) {
                return false;
            }
        }
        if is_rust_associated_item(candidate) {
            if seg == "Self" {
                return is_in_enclosing_rust_impl(candidate, r, ctx);
            }
            let Some(line) = &line else { return false };
            if !seg.is_empty() {
                return false;
            }
            let Some(qself) = rust_qualified_self(before_col(line, r.column)) else { return true };
            if qself.self_ == "Self" {
                return is_in_enclosing_rust_impl(candidate, r, ctx);
            }
            if is_rust_generic_param(&qself.self_, r, ctx) || candidate.qualified_name != format!("{}::{}", qself.self_, candidate.name) {
                return false;
            }
            return qself.trait_.is_none() || rust_trait_impl_header(candidate, ctx).is_some_and(|h| Some(&h.trait_) == qself.trait_.as_ref());
        }
        let project_crates = rust_project_crates(ctx);
        let path_uses = rust_uses_of(&r.file_path, ctx);
        if (root == "std" || root == "core" || root == "alloc" || rust_dependency_crates(ctx).contains(&root))
            && !project_crates.contains(&root)
            && !path_uses.bound.contains(&root)
        {
            return false;
        }
        if seg.is_empty() || seg == "crate" || seg == "self" || seg == "super" || seg == "Self" || project_crates.contains(&seg) {
            return true;
        }
        if path_uses.external.contains(&seg) && !path_uses.bound.contains(&seg) {
            return false;
        }
        if let Some(from) = path_uses.bound_from.get(&root) {
            if !rust_use_can_reach(from, candidate, r, ctx) {
                return false;
            }
        }
        return rust_module_name(&candidate.file_path) == seg
            || candidate.file_path.contains(&format!("/{seg}/"))
            || candidate.qualified_name.split("::").any(|s| s == seg)
            || (candidate.file_path == r.file_path && is_in_rust_inline_module(candidate, &seg, ctx));
    }
    if is_rust_associated_item(candidate) {
        return false;
    }
    if candidate.kind == NodeKind::EnumMember {
        if references {
            return false;
        }
        let uses = rust_uses_of(&r.file_path, ctx);
        let owner = match candidate.qualified_name.rfind("::") {
            Some(cut) => candidate.qualified_name[..cut].rsplit("::").next().unwrap_or("").to_string(),
            None => String::new(),
        };
        return (!owner.is_empty() && uses.globs.contains(&owner)) || (uses.names.contains(name) && uses.names.contains(&owner));
    }
    if candidate.file_path == r.file_path {
        return true;
    }
    let uses = rust_uses_of(&r.file_path, ctx);
    let from = if is_rust_module_item_kind(candidate.kind) { uses.bound_from.get(name) } else { None };
    if let Some(from) = from {
        if !rust_use_can_reach(from, candidate, r, ctx) {
            return false;
        }
        if let Some(items) = rust_use_items(name, &r.file_path, ctx) {
            if !items.contains(&candidate.id) {
                return false;
            }
        }
    }
    if uses.external.contains(name) && !uses.bound.contains(name) {
        if let Some(line) = &line {
            let from_col = after_col(line, r.column);
            if let Some(m) = Re::cached(&format!(r"(?<![\w$]){}\b", escape_regex(name)), "").find(from_col) {
                let upto = &line[..(utf16_byte_index(line, r.column as usize) + m.start()).min(line.len())];
                if !re!(r"\.\s*$").is_match(upto) {
                    return false;
                }
            }
        }
    }
    if !in_rust_prelude(name) {
        if is_type_member_kind(candidate.kind)
            || r.is_kind(EdgeKind::Imports)
            || matches!(candidate.kind, NodeKind::File | NodeKind::Module | NodeKind::Namespace)
        {
            return true;
        }
        return uses.bound.contains(name) || rust_glob_scope(name, candidate, &uses, r, ctx);
    }
    uses.names.contains(name) || rust_glob_scope(name, candidate, &uses, r, ctx)
}
