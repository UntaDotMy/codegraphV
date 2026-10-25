//! Java scope rules: which type a bare name can mean from where it is written (the package,
//! the imports, the enclosing and inherited nested types), and which method a bare call reaches.

use super::util::*;
use crate::ctx::Ctx;
use super::lang_shape::RefShape;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{Language, Node, NodeKind, ReferenceKind};
use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

/// Whether a resolution lands on a Java record's implicit accessor from outside the record. One is
/// reached through what types its receiver (a typed receiver, `Record::x`, `Record.x`) or from
/// inside the record; a name-only guess bound `result.content()` on a library type to any record
/// with a `content` component. Applied by the resolver to name-only results.
pub fn is_guessed_record_accessor(resolved: &crate::types::ResolvedRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(target) = ctx.node_by_id(&resolved.target_node_id) else { return false };
    if target.kind != NodeKind::Method || target.docstring.as_deref() != Some(codegraph_types::IMPLICIT_RECORD_ACCESSOR_DOC) {
        return false;
    }
    let record = target.qualified_name.rfind("::").map_or("", |i| &target.qualified_name[..i]);
    r.file_path != target.file_path
        || !ctx.get_nodes_in_file(&target.file_path).iter().any(|n| {
            n.kind == NodeKind::Class && n.qualified_name == record && n.start_line <= r.line && n.end_line.max(n.start_line) >= r.line
        })
}

fn is_visible_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Interface | NodeKind::Enum)
}

fn is_scope_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Struct | NodeKind::Trait)
}

/// A file's package and the imports that bring other types into scope.
pub struct JavaFileScope {
    pub pkg: String,
    pub single: HashSet<String>,
    pub demand: HashSet<String>,
}

memo_map!(JavaFileScopeMemo, String, Rc<JavaFileScope>);
memo_map!(JavaAncestorsMemo, String, Rc<RefCell<HashSet<String>>>);
memo_map!(JavaSupersMemo, String, Rc<Vec<String>>);
memo_map!(JavaStaticImportsMemo, String, Rc<JavaStaticImports>);

pub fn java_file_scope(file: &str, ctx: &Ctx) -> Rc<JavaFileScope> {
    memoized::<JavaFileScopeMemo>(ctx, file.to_string(), || {
        let source = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
        let text = strip_comments_for_regex(&source, "java");
        let pkg = re!(r"^\s*package\s+([\w.]+)\s*;", "m").captures(&text).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        let (mut single, mut demand) = (HashSet::new(), HashSet::new());
        for m in re!(r"^\s*import\s+(?:static\s+)?([\w.]+?)(\.\*)?\s*;", "gm").captures_iter(&text) {
            let name = m.text(1).unwrap_or("").to_string();
            if m.text(2).is_some_and(|s| !s.is_empty()) {
                demand.insert(name);
            } else {
                single.insert(name);
            }
        }
        Rc::new(JavaFileScope { pkg, single, demand })
    })
}

/// The simple names of everything the type `qn` extends or implements, transitively (to a depth).
fn java_ancestor_names(qn: &str, ctx: &Ctx, depth: u32) -> Rc<RefCell<HashSet<String>>> {
    let memo = ctx.memo::<JavaAncestorsMemo>();
    if let Some(hit) = memo.lookup(&qn.to_string()) {
        return hit;
    }
    let names: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
    memo.store(qn.to_string(), Rc::clone(&names));
    for decl in ctx.get_nodes_by_qualified_name(qn).iter() {
        if decl.language != Language::Java || !is_visible_type_kind(decl.kind) {
            continue;
        }
        let lines = lines_of(ctx, &decl.file_path);
        let mut header = String::new();
        let mut i = (decl.start_line as usize).saturating_sub(1);
        while i < lines.len().min(decl.start_line as usize + 6) && !header.contains('{') {
            header.push_str(&lines[i]);
            header.push(' ');
            i += 1;
        }
        let before_brace = header.split('{').next().unwrap_or("");
        let list = re!(r"\b(?:extends|implements)\b([^{]*)").captures(before_brace).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        let flat = re!(r"<[^<>]*(?:<[^<>]*>[^<>]*)*>", "g").replace_all_str(&list, "");
        for m in re!(r"([A-Za-z_]\w*)\s*(?=,|$|\bimplements\b|\s*$)", "g").captures_iter(&flat) {
            let name = m.text(1).unwrap_or("");
            if name != "implements" && name != "extends" {
                names.borrow_mut().insert(name.to_string());
            }
        }
    }
    if depth < 4 {
        let bases: Vec<String> = names.borrow().iter().cloned().collect();
        for base in bases {
            for t in ctx.get_nodes_by_name(&base).iter() {
                if t.language != Language::Java || !is_visible_type_kind(t.kind) || t.qualified_name == qn {
                    continue;
                }
                let ups: Vec<String> = java_ancestor_names(&t.qualified_name, ctx, depth + 1).borrow().iter().cloned().collect();
                names.borrow_mut().extend(ups);
            }
        }
    }
    names
}

/// Whether Java lets the reference's file name the candidate type: its own package's, one an
/// import names, a nested type in reach of the enclosing types, or a static enum member in scope.
pub fn is_java_type_visible(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if r.language != Language::Java || candidate.language != Language::Java {
        return true;
    }
    if !re!(r"^[A-Za-z_$][\w$]*$").is_match(&r.reference_name) {
        return true;
    }
    if candidate.kind == NodeKind::Method {
        let segs: Vec<&str> = candidate.qualified_name.split("::").collect();
        if segs.len() < 2 || segs[segs.len() - 2] != candidate.name || r.reference_kind == ReferenceKind::Edge(codegraph_types::EdgeKind::Calls) {
            return true;
        }
        let owner_qn = segs[..segs.len() - 1].join("::");
        let owner = ctx.get_nodes_in_file(&candidate.file_path).iter().find(|n| n.qualified_name == owner_qn && is_visible_type_kind(n.kind)).cloned();
        return owner.is_none_or(|o| is_java_type_visible(&o, r, ctx));
    }
    if candidate.kind == NodeKind::EnumMember {
        if candidate.file_path == r.file_path {
            return true;
        }
        let segs: Vec<&str> = candidate.qualified_name.split("::").collect();
        let enum_name = if segs.len() >= 2 { segs[segs.len() - 2] } else { "" };
        let line = line_at(ctx, &r.file_path, r.line).unwrap_or_default();
        let name = escape_regex(&r.reference_name);
        if Re::cached(&format!(r"\bcase\b[^:;]*\b{name}\b"), "").is_match(&line)
            || Re::cached(&format!(r"\b{}\s*\.\s*{name}\b", escape_regex(enum_name)), "").is_match(&line)
        {
            return true;
        }
        let here = java_file_scope(&r.file_path, ctx);
        let candidate_pkg = java_file_scope(&candidate.file_path, ctx).pkg.clone();
        let mut parts: Vec<&str> = vec![candidate_pkg.as_str()];
        parts.extend(js_slice(&segs, 1, -1));
        let enum_fqn = parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(".");
        return here.single.contains(&format!("{enum_fqn}.{}", r.reference_name)) || here.demand.contains(&enum_fqn);
    }
    if !is_visible_type_kind(candidate.kind) {
        return true;
    }
    let segs: Vec<&str> = candidate.qualified_name.split("::").collect();
    let candidate_scope = java_file_scope(&candidate.file_path, ctx);
    let type_path: Vec<&str> = if !candidate_scope.pkg.is_empty() && segs[0] == candidate_scope.pkg { segs[1..].to_vec() } else { segs.clone() };
    let mut fqn_parts: Vec<&str> = vec![candidate_scope.pkg.as_str()];
    fqn_parts.extend(&type_path);
    let fqn = fqn_parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(".");
    let here = java_file_scope(&r.file_path, ctx);
    let line = line_at(ctx, &r.file_path, r.line).unwrap_or_default();
    let name = escape_regex(&r.reference_name);
    if Re::cached(&format!(r"\.\s*new\s+{name}\b"), "").is_match(&line) {
        return true;
    }
    let qualifiers: Vec<String> = Re::cached(&format!(r"([A-Za-z_$][\w$.]*)\s*\.\s*(?:@[\w.]+(?:\([^)]*\))?\s+)*{name}\b"), "g")
        .captures_iter(&line)
        .filter_map(|m| m.text(1).map(String::from))
        .collect();
    let owner_qn = segs[..segs.len() - 1].join("::");
    let owner_node = if segs.len() > 1 {
        ctx.get_nodes_in_file(&candidate.file_path).iter().find(|n| n.qualified_name == owner_qn && n.kind != NodeKind::Namespace && n.kind != NodeKind::File).cloned()
    } else {
        None
    };
    if owner_node.as_ref().is_some_and(|o| !is_visible_type_kind(o.kind)) {
        return true;
    }
    let nested = owner_node.is_some();
    let owner_name = owner_node.as_ref().map_or("", |o| o.name.as_str());
    if qualifiers.iter().any(|q| (nested && (q == owner_name || q.ends_with(&format!(".{owner_name}")))) || (!nested && *q == candidate_scope.pkg)) {
        return true;
    }
    if !nested {
        if candidate.file_path == r.file_path || candidate_scope.pkg == here.pkg {
            return true;
        }
        return here.single.contains(&fqn) || here.demand.contains(&candidate_scope.pkg);
    }
    let enclosing: Vec<_> = ctx
        .get_nodes_in_file(&r.file_path)
        .iter()
        .filter(|p| is_visible_type_kind(p.kind) && p.start_line <= r.line && p.end_line >= r.line)
        .cloned()
        .collect();
    let owner_prefix = format!("{owner_qn}::");
    if enclosing.iter().any(|p| p.qualified_name == owner_qn || p.qualified_name.starts_with(&owner_prefix)) {
        return true;
    }
    let supertypes_around = |p: &Node| -> Vec<String> {
        let anon = re!(r"<([A-Za-z_$][\w$]*)\$anon@\d+>$")
            .captures(&p.name)
            .and_then(|c| c.text(1).map(String::from))
            .or_else(|| re!(r"<([A-Za-z_$][\w$]*)\$anon@\d+>").captures(p.qualified_name.split("::").last().unwrap_or("")).and_then(|c| c.text(1).map(String::from)));
        let Some(anon) = anon else {
            return java_ancestor_names(&p.qualified_name, ctx, 0).borrow().iter().cloned().collect();
        };
        let mut ups = vec![anon.clone()];
        for t in ctx.get_nodes_by_name(&anon).iter() {
            if t.language == Language::Java && is_visible_type_kind(t.kind) {
                ups.extend(java_ancestor_names(&t.qualified_name, ctx, 0).borrow().iter().cloned());
            }
        }
        ups
    };
    if enclosing.iter().any(|p| supertypes_around(p).iter().any(|s| s == owner_name)) {
        return true;
    }
    let owner_fqn = &fqn[..fqn.rfind('.').unwrap_or(0)];
    here.single.contains(&fqn) || here.demand.contains(owner_fqn)
}

// -- Bare calls -------------------------------------------------------------------------------------

/// The owners a file statically imports from, and the individual members it imports.
pub struct JavaStaticImports {
    pub owners: HashSet<String>,
    pub members: HashSet<String>,
}

fn java_static_imports_of(file: &str, ctx: &Ctx) -> Rc<JavaStaticImports> {
    memoized::<JavaStaticImportsMemo>(ctx, file.to_string(), || {
        let mut found = JavaStaticImports { owners: HashSet::new(), members: HashSet::new() };
        let text = ctx.read_file(file).map(|s| s.to_string()).unwrap_or_default();
        for m in re!(r"^\s*import\s+static\s+([\w.$]+)\s*\.\s*(\*|[\w$]+)\s*;", "gm").captures_iter(&text) {
            let owner = m.text(1).unwrap_or("").rsplit('.').next().unwrap_or("").to_string();
            let member = m.text(2).unwrap_or("");
            if member == "*" {
                found.owners.insert(owner);
            } else {
                found.members.insert(format!("{owner}.{member}"));
            }
        }
        Rc::new(found)
    })
}

fn java_supertypes_of(type_name: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<JavaSupersMemo>(ctx, type_name.to_string(), || {
        let mut names = Vec::new();
        for decl in ctx.get_nodes_by_name(type_name).iter() {
            if decl.language != Language::Java || !is_scope_type_kind(decl.kind) {
                continue;
            }
            let lines = lines_of(ctx, &decl.file_path);
            let head = js_slice(&lines, decl.start_line as i64 - 1, decl.start_line as i64 + 5).join(" ");
            let clause = re!(r"\b(?:extends|implements)\b([^{]*)\{").captures(&head).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
            let flat = re!(r"<[^<>]*(?:<[^<>]*>[^<>]*)*>", "g").replace_all_str(&clause, "");
            for m in re!(r"([A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*)", "g").captures_iter(&flat) {
                let simple = m.text(1).unwrap_or("").rsplit('.').next().unwrap_or("");
                if simple != "extends" && simple != "implements" {
                    names.push(simple.to_string());
                }
            }
        }
        Rc::new(names)
    })
}

/// Whether the method a bare call named is one the enclosing types, their supertypes or a static
/// import put in scope.
pub fn is_java_method_in_scope(method: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(cut) = method.qualified_name.rfind("::") else { return true };
    let owner = method.qualified_name[..cut].rsplit("::").next().unwrap_or("").to_string();
    let imports = java_static_imports_of(&r.file_path, ctx);
    if imports.owners.contains(&owner) || imports.members.contains(&format!("{owner}.{}", r.reference_name)) {
        return true;
    }
    let mut queue: VecDeque<String> = ctx
        .get_nodes_in_file(&r.file_path)
        .iter()
        .filter(|n| is_scope_type_kind(n.kind) && n.start_line <= r.line && n.end_line >= r.line)
        .map(|n| n.name.clone())
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(name) = queue.pop_front() {
        if seen.len() >= 40 {
            break;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        if name == owner {
            return true;
        }
        queue.extend(java_supertypes_of(&name, ctx).iter().cloned());
    }
    false
}

/// The Java contribution to [`super::lang_shape::LangShape`]: a bare call.
pub struct JavaShape;

impl JavaShape {
    pub fn of(r: &UnresolvedRef) -> Option<JavaShape> {
        (r.language == Language::Java && is_calls(r) && re!(r"^[A-Za-z_$][\w$]*$").is_match(&r.reference_name)).then_some(JavaShape)
    }
}

impl RefShape for JavaShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        n.kind == NodeKind::Method && !is_java_method_in_scope(n, r, ctx)
    }
}
