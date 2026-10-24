//! Where a Swift reference to a type may land — port of
//! `src/resolution/swift-type-visibility.ts`.
//!
//! Two shapes bound type references to the wrong node:
//!
//! 1. `extension View { … }` parses as a class_declaration, so the index holds
//!    a `class View` node for every file that extends SwiftUI's `View`. An
//!    extension is not the type: a reference lands on the type's own
//!    declaration, or nowhere when the SDK declares it.
//! 2. A nested type is named bare only inside its parent (or an extension or
//!    subclass of it), so the site's qualifier is read back from the source.
//!
//! Applied to every strategy's result at resolveOne's seam.

use crate::ctx::Ctx;
use crate::types::{ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

fn is_type_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Enum
            | NodeKind::Interface
            | NodeKind::Protocol
            | NodeKind::TypeAlias
    )
}

#[derive(Default)]
struct SwiftMemo {
    extension: RefCell<HashMap<String, bool>>,
    declaration: RefCell<HashMap<String, Option<Rc<Node>>>>,
    conformances: RefCell<HashMap<String, Vec<String>>>,
    clauses: RefCell<HashMap<String, Vec<String>>>,
    inherited: RefCell<HashMap<String, Rc<HashSet<String>>>>,
    owners: RefCell<HashMap<String, Option<String>>>,
}

fn memo(ctx: &Ctx) -> Rc<SwiftMemo> {
    ctx.memo::<SwiftMemo>()
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn skip_chars(s: &str, n: usize) -> String {
    s.chars().skip(n).collect()
}

fn starts_at(s: &str, at: usize, needle: &str) -> bool {
    s.chars().skip(at).take(needle.chars().count()).eq(needle.chars())
}

fn lines_of(ctx: &Ctx, file: &str) -> Option<Vec<String>> {
    if let Some(lines) = ctx.get_file_lines(file) {
        return Some((*lines).clone());
    }
    ctx.read_file(file)
        .map(|s| s.split('\n').map(str::to_owned).collect())
}

fn strip_string_literals(text: &str) -> String {
    re!(r#""(?:[^"\\\n]|\\.)*""#).replace_all_str(text, "\"\"")
}

/// Does the class_declaration at `node`'s position open with `extension`?
pub fn declares_extension(lines: &[String], node: &Node) -> bool {
    let start = node.start_line.saturating_sub(1) as usize;
    let end = node.end_line as usize;
    let mut parts = Vec::new();
    if let Some(line) = lines.get(start) {
        parts.push(skip_chars(line, node.start_column as usize));
    }
    for line in lines
        .iter()
        .take((node.start_line as usize + 4).min(end))
        .skip(node.start_line as usize)
    {
        parts.push(line.clone());
    }
    let head = strip_string_literals(&parts.join(" "));
    re!(r"\b(extension|class|struct|enum|actor|protocol)\b")
        .captures(&head)
        .and_then(|c| c.text(1))
        == Some("extension")
}

/// Is this Swift node an `extension` of a type rather than its declaration?
pub fn is_swift_extension(node: &Node, ctx: &Ctx) -> bool {
    if node.language != Language::Swift || node.kind != NodeKind::Class {
        return false;
    }
    let memo = memo(ctx);
    if let Some(hit) = memo.extension.borrow().get(&node.id) {
        return *hit;
    }
    let extension = lines_of(ctx, &node.file_path)
        .map(|lines| declares_extension(&lines, node))
        .unwrap_or(false);
    memo.extension.borrow_mut().insert(node.id.clone(), extension);
    extension
}

/// A declaration's inheritance clause: `struct HomeView: View, Sendable {`.
fn declared_supertypes(node: &Node, ctx: &Ctx) -> Vec<String> {
    let memo = memo(ctx);
    if let Some(hit) = memo.clauses.borrow().get(&node.id) {
        return hit.clone();
    }
    let mut names = Vec::new();
    if let Some(lines) = lines_of(ctx, &node.file_path) {
        let mut parts = Vec::new();
        if let Some(line) = lines.get(node.start_line.saturating_sub(1) as usize) {
            parts.push(skip_chars(line, node.start_column as usize));
        }
        for line in lines
            .iter()
            .take((node.start_line as usize + 6).min(node.end_line as usize))
            .skip(node.start_line as usize)
        {
            parts.push(line.clone());
        }
        let head = strip_string_literals(&parts.join(" "));
        if let Some(clause) = re!(
            r"\b(?:class|struct|enum|actor|protocol|extension)\s+[A-Za-z_][\w.]*\s*(?:<[^{}]*?>)?\s*:\s*([^{]*?)\s*(?:\bwhere\b[^{]*)?\{"
        )
        .captures(&head)
        .and_then(|c| c.text(1))
        {
            names = clause
                .split(',')
                .map(|part| {
                    let stripped = re!(r"<[^<>]*(?:<[^<>]*>[^<>]*)*>").replace_all_str(part, "");
                    stripped
                        .trim()
                        .split('.')
                        .next_back()
                        .unwrap_or("")
                        .trim()
                        .to_owned()
                })
                .filter(|name| re!(r"^[A-Za-z_]\w*$").is_match(name))
                .collect();
        }
    }
    memo.clauses.borrow_mut().insert(node.id.clone(), names.clone());
    names
}

/// The types a Swift type's own declaration conforms to that the project only
/// extends. Those conformances resolve to nothing, yet the members the
/// extensions add are the conformer's: the supertype walk reads them here.
pub fn swift_extended_conformances(node: &Node, ctx: &Ctx) -> Vec<String> {
    if node.language != Language::Swift {
        return Vec::new();
    }
    let memo = memo(ctx);
    if let Some(hit) = memo.conformances.borrow().get(&node.id) {
        return hit.clone();
    }
    let names: Vec<String> = declared_supertypes(node, ctx)
        .into_iter()
        .filter(|name| {
            let typed: Vec<Rc<Node>> = ctx
                .get_nodes_by_name(name)
                .iter()
                .filter(|n| n.language == Language::Swift && is_type_kind(n.kind))
                .cloned()
                .collect();
            !typed.is_empty() && typed.iter().all(|n| is_swift_extension(n, ctx))
        })
        .collect();
    memo.conformances.borrow_mut().insert(node.id.clone(), names.clone());
    names
}

/// Every type a Swift type inherits from or conforms to, transitively.
fn inherited_type_names(type_name: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    let memo = memo(ctx);
    if let Some(hit) = memo.inherited.borrow().get(type_name) {
        return Rc::clone(hit);
    }
    let mut found: HashSet<String> = HashSet::new();
    let mut frontier = vec![type_name.to_owned()];
    for _ in 0..4 {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for name in &frontier {
            for node in ctx.get_nodes_by_name(name).iter() {
                if node.language != Language::Swift || !is_type_kind(node.kind) {
                    continue;
                }
                for sup in declared_supertypes(node, ctx) {
                    if sup == type_name || found.contains(&sup) {
                        continue;
                    }
                    found.insert(sup.clone());
                    next.push(sup);
                }
            }
        }
        frontier = next;
    }
    let found = Rc::new(found);
    memo.inherited
        .borrow_mut()
        .insert(type_name.to_owned(), Rc::clone(&found));
    found
}

/// The type path a nested type is declared in, or `None` at the top level.
fn parent_path(node: &Node) -> Option<String> {
    let sep = node.qualified_name.rfind("::")?;
    if sep == 0 {
        return None;
    }
    let parent = &node.qualified_name[..sep];
    if parent.contains('/') || parent.ends_with(".swift") {
        return None;
    }
    Some(parent.to_owned())
}

/// How deep the code at `from` sits in `target`'s scope, when it may name
/// `target` bare: 0 for a top-level type, one more inside the type itself;
/// `None` when out of scope.
fn bare_scope_depth(target: &Node, from: Option<&Node>, ctx: &Ctx) -> Option<usize> {
    let Some(parent) = parent_path(target) else { return Some(0) };
    let from = from?;
    let scope = &from.qualified_name;
    let inside = |path: &str| -> bool {
        let segs: Vec<&str> = path.split("::").collect();
        for i in 0..segs.len() {
            let suffix = segs[i..].join("::");
            if scope == &suffix || scope.starts_with(&format!("{suffix}::")) {
                return true;
            }
        }
        false
    };
    let depth = parent.split("::").count();
    if inside(&target.qualified_name) {
        return Some(depth + 1);
    }
    if inside(&parent) {
        return Some(depth);
    }
    let parent_name = parent.rsplit("::").next().unwrap_or("");
    for name in scope.split("::") {
        if inherited_type_names(name, ctx).contains(parent_name) {
            return Some(depth);
        }
    }
    None
}

fn qualifier_segment() -> &'static str {
    r"[A-Za-z_]\w*(?:<[^<>]*(?:<[^<>]*>[^<>]*)*>)?\s*\.\s*"
}

/// The reference as its site writes it, read back from the source line.
fn written_name(reference: &UnresolvedRef, ctx: &Ctx) -> String {
    let name = &reference.reference_name;
    if name.contains('.') {
        return name.clone();
    }
    let Some(text) = lines_of(ctx, &reference.file_path)
        .and_then(|lines| lines.get(reference.line.saturating_sub(1) as usize).cloned())
    else {
        return name.clone();
    };
    let clean = |qualifier: &str| -> String {
        format!(
            "{}{}",
            re!(r"<[^<>]*(?:<[^<>]*>[^<>]*)*>").replace_all_str(qualifier, ""),
            name
        )
        .replace(char::is_whitespace, "")
    };
    let column = reference.column as usize;
    let composition = !starts_at(&text, column, name)
        && re!(r"^[ \t]*&[ \t]").is_match(&text)
        && starts_at(&text, column + 1, name);
    let column = if composition { column + 1 } else { column };
    if starts_at(&text, column, name) {
        let before = skip_chars(&text, column.saturating_sub(240));
        let before = take_chars(&before, column.saturating_sub(column.saturating_sub(240)));
        let qualifier_re = Re::cached(&format!(r"((?:{})+)$", qualifier_segment()), "");
        if let Some(caps) = qualifier_re.captures(&before) {
            if let Some(q) = caps.text(1) {
                return clean(q);
            }
        }
        return name.clone();
    }
    let from = skip_chars(&text, column);
    let window = take_chars(&from, 240);
    let chain_re = Re::cached(
        &format!(r"^((?:{})+){}\b", qualifier_segment(), escape_regex(name)),
        "",
    );
    match chain_re.captures(&window).and_then(|c| c.text(1).map(str::to_owned)) {
        Some(chain) => clean(&chain),
        None => name.clone(),
    }
}

/// `API.PackageController.Model` as `API::PackageController::Model`, up to `name`.
fn written_path(written: &str, name: &str) -> Option<String> {
    let segs: Vec<&str> = written.split('.').collect();
    let at = segs.iter().rposition(|s| *s == name)?;
    Some(segs[..=at].join("::"))
}

/// Does a type's qualified name fit the path a site wrote?
fn fits_path(node: &Node, path: &str, ctx: &Ctx) -> bool {
    let qn = &node.qualified_name;
    if qn == path || qn.ends_with(&format!("::{path}")) {
        return true;
    }
    if !path.ends_with(&format!("::{qn}")) {
        return false;
    }
    if qn.contains("::") {
        return true;
    }
    let head = &path[..path.len() - qn.len() - 2];
    let parent_name = head.rsplit("::").next().unwrap_or("");
    if ctx
        .get_nodes_in_file(&node.file_path)
        .iter()
        .any(|p| p.name == parent_name && is_type_kind(p.kind) && p.id != node.id)
    {
        return true;
    }
    // `App.Build`: a top-level type under a qualifier that names no type.
    !head.contains("::")
        && !ctx
            .get_nodes_by_name(head)
            .iter()
            .any(|n| n.language == Language::Swift && is_type_kind(n.kind))
}

/// Segments two paths' directories share.
fn shared_dirs(a: &str, b: &str) -> usize {
    let da: Vec<&str> = a.split('/').take(a.split('/').count().saturating_sub(1)).collect();
    let db: Vec<&str> = b.split('/').take(b.split('/').count().saturating_sub(1)).collect();
    let mut n = 0;
    while n < da.len() && n < db.len() && da[n] == db[n] {
        n += 1;
    }
    n
}

/// The project's own declaration of the type a site names, when exactly one fits.
fn declaration_for(
    name: &str,
    written: &str,
    reference: &UnresolvedRef,
    from: Option<&Node>,
    ctx: &Ctx,
) -> Option<Rc<Node>> {
    let memo = memo(ctx);
    let key = format!(
        "{}\0{}\0{}\0{}",
        reference.file_path,
        from.map(|n| n.qualified_name.as_str()).unwrap_or(""),
        written,
        name
    );
    if let Some(hit) = memo.declaration.borrow().get(&key) {
        return hit.clone();
    }

    let bare = !written.contains('.');
    let path = if bare { None } else { written_path(written, name) };
    let mut scored: Vec<(Rc<Node>, usize)> = Vec::new();
    for node in ctx.get_nodes_by_name(name).iter() {
        if node.language != Language::Swift || !is_type_kind(node.kind) || is_swift_extension(node, ctx) {
            continue;
        }
        if let Some(path) = &path {
            if fits_path(node, path, ctx) {
                scored.push((Rc::clone(node), 0));
            }
            continue;
        }
        let depth = if bare {
            bare_scope_depth(node, from, ctx)
        } else {
            Some(0)
        };
        if let Some(depth) = depth {
            scored.push((Rc::clone(node), depth));
        }
    }
    let deepest = scored.iter().map(|(_, d)| *d).max().unwrap_or(0);
    // One declaration per file: a framework resolver's one-line twin of a
    // declaration is the same type.
    let mut by_file: HashMap<String, Rc<Node>> = HashMap::new();
    for (node, depth) in &scored {
        if *depth != deepest {
            continue;
        }
        match by_file.get(&node.file_path) {
            Some(had)
                if node.end_line.saturating_sub(node.start_line)
                    <= had.end_line.saturating_sub(had.start_line) => {}
            _ => {
                by_file.insert(node.file_path.clone(), Rc::clone(node));
            }
        }
    }
    let declarations: Vec<Rc<Node>> = by_file.into_values().collect();
    let found = match declarations.len() {
        0 => None,
        1 => Some(Rc::clone(&declarations[0])),
        _ => {
            let same_file = declarations
                .iter()
                .find(|n| n.file_path == reference.file_path)
                .cloned();
            match same_file {
                Some(node) => Some(node),
                None => {
                    let near = declarations
                        .iter()
                        .map(|n| shared_dirs(&n.file_path, &reference.file_path))
                        .max()
                        .unwrap_or(0);
                    let nearest: Vec<&Rc<Node>> = declarations
                        .iter()
                        .filter(|n| shared_dirs(&n.file_path, &reference.file_path) == near)
                        .collect();
                    (nearest.len() == 1).then(|| Rc::clone(nearest[0]))
                }
            }
        }
    };
    memo.declaration
        .borrow_mut()
        .insert(key, found.clone());
    found
}

/// A Swift reference's resolved type target, checked against what the site wrote.
pub fn gate_swift_type_target(
    result: Option<ResolvedRef>,
    reference: &UnresolvedRef,
    ctx: &Ctx,
) -> Option<ResolvedRef> {
    let result = result?;
    if reference.language != Language::Swift || reference.is_kind(EdgeKind::Imports) {
        return Some(result);
    }
    let Some(target) = ctx.node_by_id(&result.target_node_id) else {
        return Some(result);
    };
    if target.language != Language::Swift || !is_type_kind(target.kind) {
        return Some(result);
    }
    let from = ctx.node_by_id(&reference.from_node_id);
    let written = written_name(reference, ctx);
    if !is_swift_extension(&target, ctx) {
        if !written.contains('.') {
            if bare_scope_depth(&target, from.as_deref(), ctx).is_some() {
                return Some(result);
            }
        } else {
            let path = written_path(&written, &target.name);
            if path.is_none() || path.is_some_and(|p| fits_path(&target, &p, ctx)) {
                return Some(result);
            }
        }
    }
    let Some(declared) = declaration_for(&target.name, &written, reference, from.as_deref(), ctx) else {
        return None;
    };
    if declared.id == result.target_node_id {
        return Some(result);
    }
    let mut fixed = result;
    fixed.target_node_id = declared.id.clone();
    Some(fixed)
}

/// A call through a type path, `API.PackageController.GetRoute.query`.
pub fn is_swift_type_path_call(name: &str) -> bool {
    re!(r"^(?!Self\.)[A-Z]\w*(?:\.[A-Z]\w*)+\.[A-Za-z_]\w*$").is_match(name)
}

fn path_member_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Method | NodeKind::Function | NodeKind::EnumMember
    )
}

/// `API.PackageController.GetRoute.query(on:)` lands on the `query` of the type
/// the path names, never on the member's name alone.
pub fn resolve_swift_type_path_call(reference: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let name = &reference.reference_name;
    let dot = name.rfind('.')?;
    let member = &name[dot + 1..];
    let path = name[..dot].split('.').collect::<Vec<_>>().join("::");
    let on_path = |kinds: &dyn Fn(NodeKind) -> bool| -> Vec<Rc<Node>> {
        let mut exact = Vec::new();
        let mut shortened = Vec::new();
        for node in ctx.get_nodes_by_name(member).iter() {
            if node.language != Language::Swift || !kinds(node.kind) {
                continue;
            }
            if is_type_kind(node.kind) && is_swift_extension(node, ctx) {
                continue;
            }
            let Some(owner) = owner_path(node, ctx) else {
                continue;
            };
            if owner == path {
                exact.push(Rc::clone(node));
            } else if owner.ends_with(&format!("::{path}")) || module_qualified(&owner, &path, ctx) {
                shortened.push(Rc::clone(node));
            }
        }
        if exact.is_empty() {
            shortened
        } else {
            exact
        }
    };
    let mut fit = if re!(r"^[A-Z]").is_match(member) {
        on_path(&is_type_kind)
    } else {
        Vec::new()
    };
    if fit.is_empty() {
        fit = on_path(&path_member_kind);
    }
    if fit.is_empty() {
        if let Some(reducer) = re!(r"^(.+)::(?:State|Action)$")
            .captures(&path)
            .and_then(|c| c.text(1).map(str::to_owned))
        {
            fit = ctx
                .get_nodes_by_name(member)
                .iter()
                .filter(|n| n.language == Language::Swift && n.kind == NodeKind::EnumMember)
                .filter(|n| {
                    owner_path(n, ctx).is_some_and(|owner| {
                        owner == reducer || owner.ends_with(&format!("::{reducer}"))
                    }) && is_reducer_enum(n, ctx)
                })
                .cloned()
                .collect();
        }
    }
    if fit.is_empty() {
        return None;
    }
    let owners: HashSet<Option<String>> = fit.iter().map(|n| owner_path(n, ctx)).collect();
    if owners.len() > 1 {
        return None;
    }
    let target = fit.into_iter().reduce(|a, b| {
        if a.file_path < b.file_path
            || (a.file_path == b.file_path && a.start_line <= b.start_line)
        {
            a
        } else {
            b
        }
    })?;
    Some(ResolvedRef::new(
        reference,
        target.id.clone(),
        0.9,
        ResolvedBy::QualifiedName,
    ))
}

fn is_reducer_enum(member: &Node, ctx: &Ctx) -> bool {
    let in_file = ctx.get_nodes_in_file(&member.file_path);
    let owner = in_file
        .iter()
        .filter(|t| {
            t.kind == NodeKind::Enum
                && t.start_line <= member.start_line
                && t.end_line >= member.end_line
        })
        .reduce(|inner, t| {
            if t.start_line >= inner.start_line {
                t
            } else {
                inner
            }
        });
    let Some(owner) = owner else {
        return false;
    };
    let lines = lines_of(ctx, &owner.file_path).unwrap_or_default();
    let from = owner.start_line.saturating_sub(2) as usize;
    let to = (owner.start_line as usize + 1).min(lines.len());
    re!(r"@Reducer\b").is_match(&lines[from.min(to)..to].join(" "))
}

/// `Vapor::HTTPStatus` for a type `HTTPStatus`: a qualifier that names no
/// project type is a module.
fn module_qualified(owner: &str, path: &str, ctx: &Ctx) -> bool {
    if !path.ends_with(&format!("::{owner}")) {
        return false;
    }
    let head = &path[..path.len() - owner.len() - 2];
    !head.contains("::")
        && !ctx
            .get_nodes_by_name(head)
            .iter()
            .any(|n| n.language == Language::Swift && is_type_kind(n.kind))
}

/// The full path of the type a member is declared on.
fn owner_path(member: &Node, ctx: &Ctx) -> Option<String> {
    let memo = memo(ctx);
    if let Some(hit) = memo.owners.borrow().get(&member.id) {
        return hit.clone();
    }
    let cut = member.qualified_name.rfind("::");
    let mut path = cut.map(|i| member.qualified_name[..i].to_owned());
    if let Some(current) = &path {
        let first = current.split("::").next().unwrap_or("");
        let in_file = ctx.get_nodes_in_file(&member.file_path);
        let outer = in_file
            .iter()
            .filter(|t| {
                t.name == first
                    && is_type_kind(t.kind)
                    && t.start_line <= member.start_line
                    && t.end_line >= member.end_line
            })
            .reduce(|out, t| {
                if t.start_line < out.start_line {
                    t
                } else {
                    out
                }
            });
        if let Some(extended) = outer.and_then(|outer| extended_path(&outer, ctx)) {
            path = Some(format!("{extended}{}", &current[first.len()..]));
        }
    }
    memo.owners.borrow_mut().insert(member.id.clone(), path.clone());
    path
}

/// `extension API.PackageController {` → `API::PackageController`.
fn extended_path(node: &Node, ctx: &Ctx) -> Option<String> {
    if !is_swift_extension(node, ctx) {
        return None;
    }
    let lines = lines_of(ctx, &node.file_path)?;
    let mut parts = Vec::new();
    if let Some(line) = lines.get(node.start_line.saturating_sub(1) as usize) {
        parts.push(skip_chars(line, node.start_column as usize));
    }
    for line in lines
        .iter()
        .take((node.start_line as usize + 3).min(lines.len()))
        .skip(node.start_line as usize)
    {
        parts.push(line.clone());
    }
    let head = strip_string_literals(&parts.join(" "));
    let written = re!(r"\bextension\s+((?:[A-Za-z_]\w*\s*\.\s*)+[A-Za-z_]\w*)")
        .captures(&head)
        .and_then(|c| c.text(1))?;
    Some(written.replace(char::is_whitespace, "").split('.').collect::<Vec<_>>().join("::"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, file: &str, qn: &str) -> Node {
        Node::new(
            format!("{file}::{qn}"),
            NodeKind::Class,
            name,
            qn,
            file,
            Language::Swift,
            1,
            1,
            0,
            0,
            0,
        )
    }

    #[test]
    fn an_extension_is_not_a_declaration() {
        let lines = vec!["extension View {".to_string()];
        assert!(declares_extension(&lines, &node("View", "a.swift", "View")));
        let lines = vec!["struct HomeView: View {".to_string()];
        assert!(!declares_extension(&lines, &node("HomeView", "a.swift", "HomeView")));
    }

    #[test]
    fn a_keyword_inside_an_attribute_string_is_not_the_declaration() {
        let lines = vec![
            "@available(*, message: \"use class Foo\")".to_string(),
            "struct Foo {".to_string(),
        ];
        assert!(!declares_extension(&lines, &node("Foo", "a.swift", "Foo")));
    }

    #[test]
    fn a_type_path_call_is_recognised() {
        assert!(is_swift_type_path_call("API.PackageController.GetRoute.query"));
        assert!(!is_swift_type_path_call("self.run"));
        assert!(!is_swift_type_path_call("Self.run"));
    }

    #[test]
    fn written_paths_stop_at_the_named_segment() {
        assert_eq!(
            written_path("API.PackageController.Model", "Model").as_deref(),
            Some("API::PackageController::Model")
        );
        assert_eq!(written_path("API.Model", "Other"), None);
    }
}
