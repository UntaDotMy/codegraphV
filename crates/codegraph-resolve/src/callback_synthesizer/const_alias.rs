//! A module-level value that only hands a function on.
//!
//! `export const extname = isWin ? win32.extname : posix.extname` names two methods and calls
//! neither. An import of `extname` lands on the const, so a flow through it used to stop there.
//! This pass links the value to every function its initializer can hand on.

use super::common::{calls_edge, each_node_in};
use crate::frameworks::object_literal::{match_bracket, skip_string};
use crate::frameworks::react::{declaration_text, initializer_at};
use crate::import_resolver::exports::is_stated_default_export;
use crate::import_resolver::resolve_import;
use crate::meta;
use crate::name_matcher::object_literal::{resolve_object_literal_binding, resolve_object_literal_member};
use crate::synth_utils::is_fn_kind;
use crate::types::{NodeRef, ResolvedBy, UnresolvedRef};
use crate::ctx::Ctx;
use codegraph_jsre::re;
use codegraph_search::is_generated_file;
use codegraph_text::strip_comments_for_regex;
use codegraph_types::jscompat::{js_trim, utf16_byte_index, utf16_len};
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::collections::HashSet;

const ALIAS_KINDS: [NodeKind; 2] = [NodeKind::Constant, NodeKind::Variable];
const ALIAS_LANGUAGES: [Language; 4] = [Language::TypeScript, Language::JavaScript, Language::Tsx, Language::Jsx];
const NOT_A_BINDING: &[&str] = &["true", "false", "null", "undefined", "this", "super", "NaN", "Infinity"];
const MAX_NESTING: usize = 8;
const SIGNATURE_CHARS: usize = 100;
const MAX_NAMES_LOOKED_UP: usize = 64;

struct Top {
    operands: Option<Vec<String>>,
    cast: Option<usize>,
}

struct Link {
    target: NodeRef,
    via: String,
}

/// What `code` does at its own top level, past its strings and brackets.
/// `None` when it can't be read that way, or is an arrow function.
fn read_top_level(code: &str) -> Option<Top> {
    let bytes = code.as_bytes();
    let mut operands: Vec<String> = Vec::new();
    let mut question: Option<usize> = None;
    let mut nested = 0i32;
    let mut start = 0usize;
    let mut cast: Option<usize> = None;
    let mut i = 0usize;
    while i < bytes.len() && cast.is_none() {
        let ch = bytes[i];
        if ch >= 128 {
            i += code[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            continue;
        }
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            i = skip_string(code, i)? + 1;
            continue;
        }
        if ch == b'(' || ch == b'[' || ch == b'{' {
            i = match_bracket(code, i)? + 1;
            continue;
        }
        if ch == b')' || ch == b']' || ch == b'}' {
            return None;
        }
        if ch == b'?' {
            if bytes.get(i + 1) == Some(&b'?') {
                if question.is_none() {
                    operands.push(code[start..i].to_string());
                    start = i + 2;
                }
                i += 2;
                continue;
            }
            // `?.` is optional chaining, except `?.` followed by a digit (`?.5`).
            let optional = bytes.get(i + 1) == Some(&b'.') && !bytes.get(i + 2).is_some_and(|c| c.is_ascii_digit());
            if !optional {
                if question.is_none() {
                    question = Some(i);
                } else {
                    nested += 1;
                }
            }
            i += 1;
            continue;
        }
        if ch == b':' && question.is_some() {
            let at_zero = nested == 0;
            nested -= 1;
            if at_zero {
                let q = question.unwrap();
                return Some(Top {
                    operands: Some(vec![code[q + 1..i].to_string(), code[i + 1..].to_string()]),
                    cast: None,
                });
            }
            i += 1;
            continue;
        }
        if ch == b'|' && bytes.get(i + 1) == Some(&b'|') {
            if question.is_none() {
                operands.push(code[start..i].to_string());
                start = i + 2;
            }
            i += 2;
            continue;
        }
        if question.is_none() {
            if ch == b'=' && bytes.get(i + 1) == Some(&b'>') && operands.is_empty() {
                return None;
            }
            if re!(r"^\s+(?:as|satisfies)\s").is_match(&code[i..]) {
                cast = Some(i);
                break;
            }
        }
        i += 1;
    }
    if question.is_some() {
        return None;
    }
    if operands.is_empty() {
        return Some(Top { operands: None, cast });
    }
    operands.push(code[start..].to_string());
    Some(Top { operands: Some(operands), cast: None })
}

fn paths_in(code: &str, nesting: usize) -> Vec<String> {
    let text = js_trim(code);
    if text.is_empty() || nesting > MAX_NESTING {
        return Vec::new();
    }
    let Some(top) = read_top_level(text) else { return Vec::new() };
    if let Some(operands) = top.operands {
        let mut out = Vec::new();
        for operand in operands {
            out.extend(paths_in(&operand, nesting + 1));
        }
        return out;
    }
    if let Some(cast) = top.cast {
        if re!(r"\bMock").is_match(&text[cast..]) {
            return Vec::new();
        }
        return paths_in(&text[..cast], nesting + 1);
    }
    if text.ends_with('!') {
        return paths_in(&text[..text.len() - 1], nesting + 1);
    }
    if text.starts_with('(') {
        return match match_bracket(text, 0) {
            Some(end) if end + 1 == text.len() => paths_in(&text[1..text.len() - 1], nesting + 1),
            _ => Vec::new(),
        };
    }
    if !re!(r"^[A-Za-z_$][\w$]*(?:\s*\??\.\s*[A-Za-z_$][\w$]*)*$").is_match(text) {
        return Vec::new();
    }
    let path = re!(r"[\s?]").replace_all_str(text, "");
    let root = path.split('.').next().unwrap_or("");
    if NOT_A_BINDING.contains(&root) {
        return Vec::new();
    }
    vec![path]
}

/// The names an initializer hands on, dotted, in the order written.
fn aliased_paths(initializer: &str) -> Vec<String> {
    let code = if re!(r"/[/*]").is_match(initializer) {
        strip_comments_for_regex(initializer, "typescript")
    } else {
        initializer.to_string()
    };
    let mut out = Vec::new();
    for path in paths_in(&code, 0) {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// A module-level value's initializer: what extraction kept, or the declaration itself when that
/// kept only the start. `None` when the initializer can't hand a name on.
fn initializer_of(value: &Node, ctx: &Ctx) -> Option<String> {
    if value.qualified_name != value.name {
        return None;
    }
    let signature = value.signature.as_deref()?;
    let kept = signature.strip_prefix("= ")?;
    if re!(r#"^(?:[{['"`\d]|new\s)"#).is_match(kept) {
        return None;
    }
    if utf16_len(kept) < SIGNATURE_CHARS || !kept.ends_with("...") {
        return Some(kept.to_string());
    }
    // Most long initializers are one call, and its arguments are no reason to open the file.
    if let Some(call) = re!(r"^[\w$.]+(?:<[^(]*>)?\s*\(").captures(kept) {
        let open = call.get(0)?.end() - 1;
        if match_bracket(kept, open).is_none() {
            return None;
        }
    }
    let body = declaration_text(value, ctx);
    let code = strip_comments_for_regex(&body, "typescript");
    let at = initializer_at(&code, utf16_byte_index(&code, utf16_len(&value.name)))?;
    Some(code[at..].to_string())
}

fn is_binding_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Component | NodeKind::Class | NodeKind::Constant | NodeKind::Variable)
}

fn is_value(node: &Node) -> bool {
    matches!(node.kind, NodeKind::Constant | NodeKind::Variable)
}

fn own_binding(name: &str, file: &str, ctx: &Ctx) -> Option<NodeRef> {
    ctx.get_nodes_in_file_named(file, name).into_iter().find(|n| n.qualified_name == name && is_binding_kind(n.kind))
}

/// The `member` of what `owner` is: a class's static, or a member of an object literal.
fn member_of(owner: &Node, member: &str, at: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    if owner.kind == NodeKind::Class {
        let qualified = format!("{}::{member}", owner.qualified_name);
        return ctx.get_nodes_by_qualified_name(&qualified).iter().find(|n| {
            n.file_path == owner.file_path && n.kind == NodeKind::Method && n.is_static == Some(true)
        }).cloned();
    }
    let mut member_ref = at.clone();
    member_ref.reference_name = member.to_string();
    let hit = resolve_object_literal_member(owner, member, &member_ref, ctx, 0.9, ResolvedBy::Import)
        .or_else(|| resolve_object_literal_binding(owner, member, &member_ref, ctx))?;
    ctx.node_by_id(&hit.target_node_id)
}

/// The node a dotted name reaches from `alias`, through the file's imports and then its own top
/// level. Never by the name alone.
fn alias_target(path: &str, alias: &Node, ctx: &Ctx) -> Option<NodeRef> {
    let at = UnresolvedRef::new(
        alias.id.clone(),
        path,
        EdgeKind::Calls,
        alias.start_line,
        alias.start_column,
        alias.file_path.clone(),
        alias.language,
    );
    let mut parts = path.split('.');
    let root = parts.next().unwrap_or("");
    let mut members: Vec<&str> = parts.collect();
    let imported = ctx.get_import_mappings(&alias.file_path, alias.language).iter().find(|m| m.local_name == root).cloned();
    let mut target = if let Some(binding) = imported {
        if binding.is_namespace && members.is_empty() {
            return None;
        }
        let reference_name = if binding.is_namespace {
            let member = members.remove(0);
            format!("{root}.{member}")
        } else {
            root.to_string()
        };
        let mut at_import = at.clone();
        at_import.reference_name = reference_name;
        let resolved = resolve_import(&at_import, ctx).and_then(|hit| ctx.node_by_id(&hit.target_node_id));
        if let Some(node) = &resolved {
            if binding.is_default && !is_stated_default_export(node, ctx) {
                return None;
            }
        }
        resolved
    } else {
        own_binding(root, &alias.file_path, ctx)
    };
    for member in members {
        let owner = target?;
        target = member_of(&owner, member, &at, ctx);
    }
    target
}

/// A declaration at the start of a line whose value opens like an alias.
pub(crate) fn has_const_alias_shape(file_path: &str, content: &str) -> bool {
    re!(r"\.[cm]?[jt]sx?$").is_match(file_path)
        && re!(
            r"^[ \t]*(?:export[ \t]+)?(?:const|let|var)[ \t]+[\w$]+[^=\n;]*=[ \t\r\n]*[(A-Za-z_$][^;\n(]*(?:[;\n]|$)",
            "m"
        )
        .is_match(content)
}

/// Whether a value in the project names one of the definitions a sync just changed.
pub fn alias_may_name(store: &codegraph_db::Store, defined: &[Node]) -> bool {
    let mut names = Vec::new();
    for node in defined {
        if ALIAS_LANGUAGES.contains(&node.language) && (is_fn_kind(node.kind) || is_value(node)) && !names.contains(&node.name) {
            names.push(node.name.clone());
        }
    }
    if names.is_empty() {
        return false;
    }
    if names.len() > MAX_NAMES_LOOKED_UP {
        return true;
    }
    let signatures = match store.top_level_signatures_mentioning(&ALIAS_KINDS, &ALIAS_LANGUAGES, &names) {
        Ok(rows) => rows,
        Err(_) => return true,
    };
    let name_set: HashSet<&str> = names.iter().map(String::as_str).collect();
    signatures.iter().any(|signature| {
        let Some(kept) = signature.strip_prefix("= ") else { return false };
        aliased_paths(kept).iter().any(|path| name_set.contains(path.rsplit('.').next().unwrap_or(path.as_str())))
    })
}

pub(crate) fn const_alias_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut named: IndexMap<String, (NodeRef, Vec<Link>)> = IndexMap::new();
    each_node_in(ctx, &ALIAS_KINDS, &ALIAS_LANGUAGES, |alias| {
        let Some(initializer) = initializer_of(&alias, ctx) else { return };
        let paths = aliased_paths(&initializer);
        if paths.is_empty() || is_generated_file(&alias.file_path) {
            return;
        }
        let mut links = Vec::new();
        for via in paths {
            let Some(target) = alias_target(&via, &alias, ctx) else { continue };
            if target.id != alias.id && (is_fn_kind(target.kind) || is_value(&target)) {
                links.push(Link { target, via });
            }
        }
        if !links.is_empty() {
            named.insert(alias.id.clone(), (alias, links));
        }
    });

    // A value another one names counts only once it reaches a function.
    let mut live: HashSet<String> = HashSet::new();
    loop {
        let mut grew = false;
        for (id, (_, links)) in &named {
            if live.contains(id) {
                continue;
            }
            if links.iter().any(|link| !is_value(&link.target) || live.contains(&link.target.id)) {
                live.insert(id.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let mut edges = Vec::new();
    for (alias, links) in named.values() {
        for link in links {
            if is_value(&link.target) && !live.contains(&link.target.id) {
                continue;
            }
            edges.push(calls_edge(
                &alias.id,
                &link.target.id,
                Some(alias.start_line),
                meta!(
                    "synthesizedBy" => "const-alias",
                    "via" => link.via.as_str(),
                    "registeredAt" => format!("{}:{}", alias.file_path, alias.start_line),
                ),
            ));
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::aliased_paths;

    #[test]
    fn initializers_that_hand_a_function_on() {
        let cases = [
            ("a.b", &["a.b"][..]),
            ("(a.b)", &["a.b"]),
            ("a?.b", &["a.b"]),
            ("a.b.c", &["a.b.c"]),
            ("fn", &["fn"]),
            ("cond ? a.b : c.d", &["a.b", "c.d"]),
            ("(cond ? a.b : c.d)", &["a.b", "c.d"]),
            ("one.two ? a.b : c.d", &["a.b", "c.d"]),
            ("cond ? a.b : other ? c.d : e.f", &["a.b", "c.d", "e.f"]),
            ("get(x, \":\") ? a.b : c.d", &["a.b", "c.d"]),
            ("a.b ?? c.d", &["a.b", "c.d"]),
            ("a.b || c.d", &["a.b", "c.d"]),
            ("a.b ?? c.d ?? e.f", &["a.b", "c.d", "e.f"]),
            ("a.b || noop", &["a.b", "noop"]),
            ("a.b ?? (() => 1)", &["a.b"]),
            ("cond ? a.b : null", &["a.b"]),
            ("a.b as Handler", &["a.b"]),
            ("a.b satisfies Handler", &["a.b"]),
            ("a.b!", &["a.b"]),
            ("(cond ? a.b : c.d) as (p: string) => string", &["a.b", "c.d"]),
            ("(x as boolean) ? a.b : c.d", &["a.b", "c.d"]),
            ("cond /* why */ ? a.b : c.d", &["a.b", "c.d"]),
            ("cond\r\n  ? a.b\r\n  : c.d", &["a.b", "c.d"]),
            // The scan stops at the first cast, so the `||` after it is not an arm.
            ("a.b as Handler || c.d", &["a.b"]),
        ];
        for (code, paths) in cases {
            assert_eq!(aliased_paths(code), paths, "{code}");
        }
    }

    #[test]
    fn initializers_that_hand_nothing_on() {
        for code in [
            "1",
            "\"a.b\"",
            "`a.b`",
            "true",
            "null",
            "undefined",
            "this.x",
            "super.x",
            "a.b()",
            "a.b.bind(a)",
            "new a.B()",
            "[a.b]",
            "{ a: b.c }",
            "a.b + c.d",
            "a.b && c.d",
            "!a.b",
            "typeof a.b",
            "() => a.b",
            "a[b]",
            "useAuth as jest.Mock",
            "useAuth as unknown as jest.MockedFunction<typeof useAuth>",
            "api.load as Mock",
        ] {
            assert_eq!(aliased_paths(code), Vec::<String>::new(), "{code}");
        }
    }
}
