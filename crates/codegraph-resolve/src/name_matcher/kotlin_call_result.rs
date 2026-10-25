//! A Kotlin call on what an unqualified call returns — `repository().pending(1)`, and a local set
//! from one, `val repo = repository()` — goes to the type that function declares (#2805): port of
//! the `kotlinCallResultCallee` … `kotlinLocalCallResultType` region of `name-matcher.ts`.

use super::js_store::enclosing_scope_start_line;
use super::kotlin::{is_kotlin_top_level_visible, kotlin_file_scope, kotlin_hierarchy_at};
use super::members::is_member_class_kind;
use super::method::resolve_method_on_type;
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{utf16_from, utf16_prefix};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::rc::Rc;

/// How many lines past its first a Kotlin call is read for its arguments.
const KOTLIN_CALL_LINES: usize = 8;

fn file_lines(file_path: &str, ctx: &Ctx) -> Option<Rc<Vec<String>>> {
    ctx.get_file_lines(file_path).or_else(|| ctx.read_file(file_path).map(|s| Rc::new(re!(r"\r?\n").split(&s).into_iter().map(String::from).collect())))
}

/// The index of the `)` closing the `(` at `open`, string literals skipped.
fn kotlin_closing_paren(t: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i64;
    let mut i = open;
    while i < t.len() {
        let c = t[i];
        if c == '"' || c == '\'' {
            i += 1;
            while i < t.len() && t[i] != c {
                if t[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
        } else if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn rest_after(t: &[char], close: usize) -> String {
    t.get(close + 1..).map_or_else(String::new, |s| s.iter().collect())
}

fn owner_of(qualified_name: &str) -> &str {
    qualified_name.rfind("::").map_or("", |i| &qualified_name[..i])
}

/// `owner_of` as TS slices it with a `-1` index: the name without its last character when it has
/// no `::`. Only ever asked whether it contains `::`, which it then never does.
fn has_scoped_owner(qualified_name: &str) -> bool {
    owner_of(qualified_name).contains("::")
}

fn is_type_kind(n: &Node) -> bool {
    is_member_class_kind(n.kind) || n.kind == NodeKind::Module
}

/// The project's Kotlin and Java types — classes, interfaces, enums, objects — named `name`.
fn kotlin_project_types(name: &str, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_nodes_by_name(name).iter().filter(|n| matches!(n.language, Language::Kotlin | Language::Java) && is_type_kind(n)).cloned().collect()
}

fn is_kotlin_project_type(name: &str, ctx: &Ctx) -> bool {
    !kotlin_project_types(name, ctx).is_empty()
}

/// A Kotlin call written on what an unqualified call returns (`repository().pending(1)`,
/// `this.repository()…`, `nullable()?.pending()`): the inner function's name.
fn kotlin_call_result_callee(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if r.language != Language::Kotlin || !r.is_kind(EdgeKind::Calls) || !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        return None;
    }
    let lines = file_lines(&r.file_path, ctx)?;
    let first = lines.get((r.line as usize).wrapping_sub(1))?;
    let mut parts: Vec<&str> = vec![utf16_from(first, r.column as usize)];
    let from = (r.line as usize).min(lines.len());
    parts.extend(lines[from..(from + KOTLIN_CALL_LINES).min(lines.len())].iter().map(String::as_str));
    let text = parts.join("\n");
    let head = re!(r"^(?:this\s*\.\s*)?([a-z_]\w*)\s*(?:<[^<>()]*>)?\s*\(").captures(&text)?;
    let t: Vec<char> = text.chars().collect();
    let close = kotlin_closing_paren(&t, head.text(0)?.chars().count() - 1)?;
    let outer = Re::cached(&format!(r"^\s*(?:!!\s*)?\??\.\s*{}\s*[({{<]", escape_regex(&r.reference_name)), "");
    outer.is_match(&rest_after(&t, close)).then(|| head.text(1).unwrap_or("").to_string())
}

/// The type an unqualified Kotlin call to `name` returns, as the function it calls declares it: a
/// method of the calling class, else of a type around the call or one those inherit, else a
/// top-level function the calling file can name.
fn kotlin_call_result_type(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let fns: Vec<NodeRef> =
        ctx.get_nodes_by_name(name).iter().filter(|n| n.language == Language::Kotlin && matches!(n.kind, NodeKind::Method | NodeKind::Function)).cloned().collect();
    if fns.is_empty() {
        return None;
    }
    let caller = ctx.node_by_id(&r.from_node_id);
    // Inside an extension function on a type from outside the project, that type's own members
    // are the first a bare call reaches, and they aren't known here.
    if let Some(caller) = caller.as_ref().filter(|c| c.kind == NodeKind::Method) {
        let owner = owner_of(&caller.qualified_name);
        let member = ctx.get_nodes_by_qualified_name(owner).iter().any(|n| is_type_kind(n));
        if !member && !is_kotlin_project_type(re!(r"::|\.").split(owner).last().copied().unwrap_or(""), ctx) {
            return None;
        }
    }
    let mut pool: Vec<NodeRef> = match &caller {
        Some(c) => fns.iter().filter(|n| n.kind == NodeKind::Method && owner_of(&n.qualified_name) == owner_of(&c.qualified_name)).cloned().collect(),
        None => Vec::new(),
    };
    if pool.is_empty() {
        let hierarchy = kotlin_hierarchy_at(r, ctx);
        pool = fns
            .iter()
            .filter(|n| {
                if n.kind != NodeKind::Method {
                    return false;
                }
                let mut owner: Vec<&str> = re!(r"::|\.").split(owner_of(&n.qualified_name));
                let ty = owner.pop();
                // A companion object's members are called bare inside its class.
                let key = if ty == Some("Companion") { owner.pop().unwrap_or("") } else { ty.unwrap_or("") };
                hierarchy.contains(key)
            })
            .cloned()
            .collect();
    }
    if pool.is_empty() {
        pool = fns
            .iter()
            .filter(|n| {
                if n.kind != NodeKind::Function {
                    return false;
                }
                let pkg = &kotlin_file_scope(&n.file_path, ctx).pkg;
                let want = if pkg.is_empty() { n.name.clone() } else { format!("{pkg}::{}", n.name) };
                n.qualified_name == want && is_kotlin_top_level_visible(n, r, ctx)
            })
            .cloned()
            .collect();
    }
    if pool.is_empty() {
        return None;
    }
    let mut types: Vec<String> = pool.iter().map(|n| kotlin_scoped_return_type(n, ctx).unwrap_or_default()).collect();
    types.sort();
    types.dedup();
    (types.len() == 1 && !types[0].is_empty()).then(|| types.remove(0))
}

/// The type a Kotlin function declares it returns, a name written alone taken as the nested type
/// of the class it is declared in, or of a class around that, that has the name.
fn kotlin_scoped_return_type(f: &Node, ctx: &Ctx) -> Option<String> {
    let written = kotlin_declared_return_type(f)?;
    if written.contains('.') {
        return Some(written);
    }
    let is_type = |qn: &str| ctx.get_nodes_by_qualified_name(qn).iter().any(|n| is_type_kind(n));
    let scopes: Vec<&str> = f.qualified_name.split("::").collect();
    let scopes = &scopes[..scopes.len() - 1];
    for i in (1..=scopes.len()).rev() {
        let scope = scopes[..i].join("::");
        if is_type(&scope) && is_type(&format!("{scope}::{written}")) {
            return Some(format!("{}.{written}", scopes[i - 1]));
        }
    }
    Some(written)
}

/// The type a Kotlin function's signature declares it returns, as its type names from the
/// outermost type on, without nullability, type arguments or a package.
fn kotlin_declared_return_type(f: &Node) -> Option<String> {
    let sig = f.signature.as_deref()?.trim();
    if !sig.starts_with('(') {
        return None;
    }
    let t: Vec<char> = sig.chars().collect();
    let close = kotlin_closing_paren(&t, 0)?;
    let after = rest_after(&t, close);
    let m = re!(r"^\s*:\s*([\s\S]+)$").captures(&after)?;
    let written = m.text(1)?.trim();
    let written = re!(r"\?+$").replace_all_str(written, "");
    let written = re!(r"<[\s\S]*>$").replace_all_str(&written, "");
    let path: Vec<&str> = written.trim().split('.').map(str::trim).collect();
    if !path.iter().all(|p| re!(r"^[A-Za-z_]\w*$").is_match(p)) {
        return None;
    }
    let upper = |p: &&str| p.starts_with(|c: char| c.is_ascii_uppercase());
    let from = path.iter().position(upper).unwrap_or(path.len() - 1);
    let types = &path[from..];
    (!types.is_empty() && types.iter().all(upper)).then(|| types.join("."))
}

/// A Kotlin call on what an unqualified call returns: the method of the type that function
/// declares, or nothing — never a namesake found by the method's name alone. Undefined when the
/// shape or the declared type can't be read.
pub fn match_kotlin_call_result_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some(callee) = kotlin_call_result_callee(r, ctx) else { return Tri::Undefined };
    let written = kotlin_call_result_type(&callee, r, ctx);
    let path: Vec<&str> = written.as_deref().map_or_else(Vec::new, |w| w.split('.').collect());
    // A type from outside the project is no help.
    let Some(&ty) = path.last() else { return Tri::Undefined };
    if !is_kotlin_project_type(ty, ctx) {
        return Tri::Undefined;
    }
    let name = &r.reference_name;
    let fqn = ctx.get_import_mappings(&r.file_path, r.language).iter().find(|i| i.local_name == ty).map(|i| i.source.clone());
    let found = resolve_method_on_type(ty, name, r, ctx, 0.85, ResolvedBy::InstanceMethod, fqn.as_deref(), 0);
    // Whole names: `MockResponse::Builder::body` is no `Response::Builder::body`.
    let member = format!("{}::{name}", path.join("::"));
    let is_path_member = |n: &Node| n.qualified_name == member || n.qualified_name.ends_with(&format!("::{member}"));
    let named = ctx.get_nodes_by_name(name);
    if let Some(found) = found {
        let target = ctx.node_by_id(&found.target_node_id);
        // An extension function, kept under its receiver type's bare name, is the one the calling
        // file can name.
        if let Some(target) = target.as_ref().filter(|t| t.kind == NodeKind::Method && !has_scoped_owner(&t.qualified_name)) {
            let visible: Vec<&NodeRef> =
                named.iter().filter(|n| n.kind == NodeKind::Method && n.qualified_name == target.qualified_name && is_kotlin_top_level_visible(n, r, ctx)).collect();
            if visible.iter().any(|n| n.id == target.id) {
                return Tri::Value(found);
            }
            if visible.len() == 1 {
                let mut retargeted = found;
                retargeted.target_node_id = visible[0].id.clone();
                return Tri::Value(retargeted);
            }
            return Tri::Undefined;
        }
        if path.len() == 1 || target.as_ref().is_some_and(|t| is_path_member(t)) {
            return Tri::Value(found);
        }
        // Another type's `Builder`: the method of the one the path names, or the later strategies.
        let own: Vec<&NodeRef> = named.iter().filter(|n| n.kind == NodeKind::Method && is_path_member(n)).collect();
        return if own.len() == 1 { Tri::Value(ResolvedRef::new(r, own[0].id.clone(), 0.85, ResolvedBy::InstanceMethod)) } else { Tri::Undefined };
    }
    // An extension the index doesn't keep under the type may be it.
    let extension = named
        .iter()
        .any(|n| n.language == Language::Kotlin && (n.kind == NodeKind::Function || (n.kind == NodeKind::Method && !has_scoped_owner(&n.qualified_name))));
    if extension {
        Tri::Undefined
    } else {
        Tri::Null
    }
}

/// The type a Kotlin local holds when its nearest declaration in the calling function sets it from
/// an unqualified call alone — `val repo = repository()`, `var repo = this.repository()!!`.
pub fn kotlin_local_call_result_type(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let lines = file_lines(&r.file_path, ctx)?;
    let rx = escape_regex(receiver);
    let declares = Re::cached(&format!(r"\b(?:val|var)\s+{rx}\b"), "");
    let initializer = Re::cached(&format!(r"^(?:val|var)\s+{rx}\s*=\s*(?:this\s*\.\s*)?([a-z_]\w*)\s*(?:<[^<>()]*>)?\s*\("), "");
    let start = (enclosing_scope_start_line(r, ctx) as i64 - 1).max(0);
    let mut i = (lines.len() as i64).min(r.line as i64) - 1;
    while i >= start {
        let line = &lines[i as usize];
        let scanned = if i == r.line as i64 - 1 { utf16_prefix(line, r.column as usize) } else { line.as_str() };
        let Some(at) = declares.find(scanned) else {
            i -= 1;
            continue;
        };
        let iu = i as usize;
        let mut parts: Vec<&str> = vec![&line[at.start()..]];
        parts.extend(lines[iu + 1..(iu + 1 + KOTLIN_CALL_LINES).min(lines.len())].iter().map(String::as_str));
        let text = parts.join("\n");
        let call = initializer.captures(&text)?;
        let t: Vec<char> = text.chars().collect();
        let close = kotlin_closing_paren(&t, call.text(0)?.chars().count() - 1)?;
        if !re!(r"^[ \t]*(?:!![ \t]*)?(?:;|//|\r?\n|$)").is_match(&rest_after(&t, close)) {
            return None;
        }
        // Only a type of the project's, and a nested one only when no other type shares its name.
        let written = kotlin_call_result_type(call.text(1)?, r, ctx)?;
        let path: Vec<&str> = written.split('.').collect();
        let last = *path.last()?;
        let types = kotlin_project_types(last, ctx);
        return (types.len() == 1 || (types.len() > 1 && path.len() == 1)).then(|| last.to_string());
    }
    None
}
