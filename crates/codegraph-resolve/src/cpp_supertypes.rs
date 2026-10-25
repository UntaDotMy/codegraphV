//! C and C++ base classes — port of `src/resolution/cpp-supertypes.ts`.
//!
//! A base class is the one C++ name lookup finds from where the derived class is declared:
//! lexical scopes outward, `using` declarations and directives, namespace aliases. Never a
//! namesake of another namespace or language.

use crate::cpp_namespaces::{cpp_macro_namespace_frames, cpp_namespace_aliases};
use crate::cpp_type_aliases::{
    cpp_class_named, cpp_parent_scope, cpp_scopes_within, cpp_template_parameters, cpp_type_segments,
    strip_cpp_template_arguments, CppNamedClass, Visible,
};
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{js_trim, utf16_from};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

fn is_class_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union)
}

fn is_c_family(n: &Node) -> bool {
    matches!(n.language, Language::Cpp | Language::C)
}

pub fn is_cpp_supertype_ref(r: &UnresolvedRef) -> bool {
    matches!(r.language, Language::Cpp | Language::C) && (r.is_kind(EdgeKind::Extends) || r.is_kind(EdgeKind::Implements))
}

#[derive(Default)]
struct LocalTo(RefCell<HashMap<String, Option<NodeRef>>>);

#[derive(Default)]
struct Usings(RefCell<HashMap<String, Rc<Vec<CppUsing>>>>);

fn source_file(path: &str) -> bool {
    re!(r"\.(?:c|cc|cpp|cxx|c\+\+|cu|metal)$", "i").is_match(path)
}

/// The function that locally scopes class `cls` (a class declared inside a function body).
fn cpp_function_around(cls: &Node, ctx: &Ctx) -> Option<NodeRef> {
    let memo = ctx.memo::<LocalTo>();
    if let Some(hit) = memo.0.borrow().get(&cls.id) {
        return hit.clone();
    }
    let scope = cpp_parent_scope(&cls.qualified_name);
    let fn_ = if scope.is_empty() {
        None
    } else {
        ctx.get_nodes_by_qualified_name(&scope)
            .iter()
            .find(|f| {
                matches!(f.kind, NodeKind::Function | NodeKind::Method)
                    && f.file_path == cls.file_path
                    && f.start_line <= cls.start_line
                    && f.end_line >= cls.start_line
            })
            .cloned()
    };
    memo.0.borrow_mut().insert(cls.id.clone(), fn_.clone());
    fn_
}

fn visible_from(file: &str, unit: Option<&str>, n: &Node) -> bool {
    n.file_path == file || Some(n.file_path.as_str()) == unit || !source_file(&n.file_path)
}

fn seen_from(site: &CppNameSite, ctx: &Ctx, unit: Option<&str>, n: &Node) -> bool {
    if !visible_from(&site.file, unit, n) {
        return false;
    }
    match cpp_function_around(n, ctx) {
        None => true,
        Some(f) => f.file_path == site.file && site.line >= f.start_line as usize && site.line <= f.end_line as usize,
    }
}

/// Resolve the base class a C/C++ `extends`/`implements` reference names.
pub fn match_cpp_supertype(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let derived = ctx.node_by_id(&r.from_node_id)?;
    if !is_c_family(&derived) || !is_class_kind(derived.kind) {
        return None;
    }
    let site = CppNameSite {
        scope: cpp_parent_scope(&derived.qualified_name),
        file: derived.file_path.clone(),
        line: r.line as usize,
        parameters: cpp_template_parameters(&derived, ctx),
        self_id: Some(derived.id.clone()),
        class_scope: None,
    };
    let found = match cpp_class_written(&written_base(r, ctx), &site, r, ctx) {
        Tri::Value(f) => f,
        _ => return None,
    };
    Some(if found.exact {
        ResolvedRef::new(r, found.node.id.clone(), 0.9, ResolvedBy::QualifiedName)
    } else {
        ResolvedRef::new(r, found.node.id.clone(), 0.7, ResolvedBy::ExactMatch)
    })
}

/// Where a type name is written: the scope, file, line, template parameters in reach.
pub struct CppNameSite {
    pub scope: String,
    pub file: String,
    pub line: usize,
    pub parameters: Rc<HashSet<String>>,
    pub self_id: Option<String>,
    pub class_scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CppWrittenClass {
    pub node: NodeRef,
    pub exact: bool,
}

/// The class `written` names at `site`: `Undefined` when nothing of that name is reached, `Null`
/// when something is but it is not a class (or is a template parameter).
pub fn cpp_class_written(written: &str, site: &CppNameSite, r: &UnresolvedRef, ctx: &Ctx) -> Tri<CppWrittenClass> {
    let Some(segments) = cpp_type_segments(written) else { return Tri::Null };
    let global = written.starts_with("::");
    if !global && site.parameters.contains(&segments[0]) {
        return Tri::Null;
    }
    let unit = Some(r.file_path.as_str());
    let visible_fn = |n: &Node| seen_from(site, ctx, unit, n);
    let visible: Visible = Some(&visible_fn);
    let mut scopes = cpp_scopes_within(&site.scope);
    if let Some(cs) = &site.class_scope {
        scopes.insert(1.min(scopes.len()), cs.clone());
    }
    let resolved = |node: NodeRef| CppWrittenClass { node, exact: true };

    let lexical_scopes: Vec<String> = if global { vec![String::new()] } else { scopes[..scopes.len().saturating_sub(1)].to_vec() };
    let lexical = class_node(cpp_class_named(written, &lexical_scopes, r, ctx, visible), site, ctx, visible);
    match lexical {
        Tri::Value(n) => return Tri::Value(resolved(n)),
        Tri::Null => return Tri::Null,
        Tri::Undefined => {}
    }
    let path = segments.join("::");
    let first = re!(r"^\s*(?:::\s*)?[A-Za-z_]\w*").find(written).map(|m| m.as_str()).unwrap_or("");
    let after = &written[first.len()..];
    let mut usings: Vec<CppUsing> = usings_in(&site.file, ctx).iter().filter(|u| u.line < site.line).cloned().collect();
    usings.reverse();
    let look_up = |spelled: &str| -> Option<NodeRef> {
        let absolute = spelled.starts_with("::");
        let sc: Vec<String> = if absolute { vec![String::new()] } else { scopes.clone() };
        class_node(cpp_class_named(spelled, &sc, r, ctx, visible), site, ctx, visible).value()
    };
    if segments.len() > 1 {
        let local = usings.iter().find(|u| u.kind == UsingKind::Alias && u.name == segments[0]);
        let target: Option<String> = match local {
            Some(l) => Some(l.path.clone()),
            None => cpp_namespace_aliases(ctx).get(&segments[0]).cloned(),
        };
        if let Some(target) = target {
            if let Some(hit) = look_up(&format!("{target}{after}")) {
                return Tri::Value(resolved(hit));
            }
        }
    }
    if !global {
        for u in &usings {
            if u.kind != UsingKind::Declaration || u.name != segments[0] {
                continue;
            }
            if let Some(hit) = look_up(&format!("{}{after}", u.path)) {
                return Tri::Value(resolved(hit));
            }
        }
        for u in &usings {
            if u.kind != UsingKind::Directive {
                continue;
            }
            if let Some(hit) = look_up(&format!("{}::{written}", u.path)) {
                return Tri::Value(resolved(hit));
            }
        }
        let outermost = class_node(cpp_class_named(written, &[String::new()], r, ctx, visible), site, ctx, visible);
        match outermost {
            Tri::Value(n) => return Tri::Value(resolved(n)),
            Tri::Null => return Tri::Null,
            Tri::Undefined => {}
        }
    }
    if segments.len() > 1 {
        let named: Vec<NodeRef> =
            named_classes(&segments, site, ctx, visible).into_iter().filter(|n| macro_qualified_name(n, ctx) == path).collect();
        if let Some(hit) = pick(&named, &site.file) {
            return Tri::Value(resolved(hit));
        }
    }
    let tail: Vec<NodeRef> = named_classes(&segments, site, ctx, visible)
        .into_iter()
        .filter(|n| n.qualified_name == path || n.qualified_name.ends_with(&format!("::{path}")))
        .collect();
    let own: Vec<&NodeRef> = tail.iter().filter(|n| n.file_path == site.file).collect();
    let sole: Vec<&NodeRef> = if !own.is_empty() { own } else { tail.iter().collect() };
    if sole.len() == 1 {
        Tri::Value(CppWrittenClass { node: Rc::clone(sole[0]), exact: false })
    } else {
        Tri::Undefined
    }
}

/// The base-class text as written at the reference (with its template arguments), or the
/// reference's own name when the source does not match.
fn written_base(r: &UnresolvedRef, ctx: &Ctx) -> String {
    let lines: Option<Rc<Vec<String>>> = ctx.get_file_lines(&r.file_path);
    let text = lines
        .map(|l| {
            let from = (r.line as usize).saturating_sub(1);
            let joined = l.iter().skip(from).take(12).cloned().collect::<Vec<_>>().join("\n");
            utf16_from(&joined, r.column as usize).to_string()
        })
        .unwrap_or_default();
    let chars: Vec<char> = text.chars().collect();
    let mut depth = 0i32;
    let mut end = chars.len();
    for i in 0..chars.len() {
        let c = chars[i];
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        let next = chars.get(i + 1).copied();
        let opens = c == '('
            || (c == '<' && prev.is_some_and(|p| p.is_ascii_alphanumeric() || p == '_') && next != Some('=') && next != Some('<'));
        let closes = c == ')' || (c == '>' && prev != Some('-') && next != Some('='));
        if opens {
            depth += 1;
        } else if closes {
            if depth == 0 {
                end = i;
                break;
            }
            depth -= 1;
        } else if depth == 0 && (c == ',' || c == '{' || c == ';') {
            end = i;
            break;
        }
    }
    let written: String = chars[..end].iter().collect();
    let written = js_trim(&written).to_string();
    let squash = |s: &str| re!(r"\s+", "g").replace_all_str(s, "");
    if squash(&strip_cpp_template_arguments(&written)) == squash(&r.reference_name) {
        written
    } else {
        js_trim(&r.reference_name).to_string()
    }
}

fn class_node(named: Tri<CppNamedClass>, site: &CppNameSite, ctx: &Ctx, visible: Visible) -> Tri<NodeRef> {
    let named = match named {
        Tri::Undefined => return Tri::Undefined,
        Tri::Null => return Tri::Null,
        Tri::Value(v) => v,
    };
    let cls = match named {
        CppNamedClass::Alias(a) => return Tri::Value(a),
        CppNamedClass::Cls(c) => c,
    };
    let classes: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&cls)
        .iter()
        .filter(|n| is_c_family(n) && is_class_kind(n.kind) && Some(&n.id) != site.self_id.as_ref())
        .cloned()
        .collect();
    if classes.is_empty() {
        return Tri::Undefined;
    }
    let shown: Vec<NodeRef> = classes.iter().filter(|n| visible.is_none_or(|v| v(n))).cloned().collect();
    // `pick` of an empty list is `undefined`.
    match pick(if shown.is_empty() { &classes } else { &shown }, &site.file) {
        Some(n) => Tri::Value(n),
        None => Tri::Undefined,
    }
}

fn pick(classes: &[NodeRef], file: &str) -> Option<NodeRef> {
    if classes.len() <= 1 {
        return classes.first().cloned();
    }
    if let Some(own) = classes.iter().find(|n| n.file_path == file) {
        return Some(Rc::clone(own));
    }
    let stem = |p: &str| re!(r"\.[^./\\]+$").replace_first(p, |_| String::new());
    if let Some(header) = classes.iter().find(|n| stem(&n.file_path) == stem(file)) {
        return Some(Rc::clone(header));
    }
    let shared = |p: &str| {
        let a: Vec<&str> = p.split('/').collect();
        let b: Vec<&str> = file.split('/').collect();
        let mut i = 0;
        while i + 1 < a.len() && i + 1 < b.len() && a[i] == b[i] {
            i += 1;
        }
        i
    };
    let mut best = &classes[0];
    for n in &classes[1..] {
        if shared(&n.file_path) > shared(&best.file_path) {
            best = n;
        }
    }
    Some(Rc::clone(best))
}

fn named_classes(segments: &[String], site: &CppNameSite, ctx: &Ctx, visible: Visible) -> Vec<NodeRef> {
    ctx.get_nodes_by_name(&segments[segments.len() - 1])
        .iter()
        .filter(|n| is_c_family(n) && is_class_kind(n.kind) && Some(&n.id) != site.self_id.as_ref() && visible.is_none_or(|v| v(n)))
        .cloned()
        .collect()
}

fn macro_qualified_name(n: &Node, ctx: &Ctx) -> String {
    let mut frames: Vec<_> = cpp_macro_namespace_frames(&n.file_path, ctx)
        .iter()
        .filter(|f| f.start <= n.start_line as usize && f.end >= n.start_line as usize)
        .cloned()
        .collect();
    frames.sort_by_key(|f| f.start);
    let prefix: Vec<String> = frames.into_iter().flat_map(|f| f.path).collect();
    if prefix.is_empty() {
        n.qualified_name.clone()
    } else {
        format!("{}::{}", prefix.join("::"), n.qualified_name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsingKind {
    Declaration,
    Directive,
    Alias,
}

#[derive(Debug, Clone)]
struct CppUsing {
    line: usize,
    kind: UsingKind,
    name: String,
    path: String,
}

fn using_pattern() -> String {
    let path = r"(?:::\s*)?[A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*";
    format!(
        r"\busing\s+namespace\s+({path})\s*;|\busing\s+({path}\s*::\s*[A-Za-z_]\w*)\s*;|\bnamespace\s+([A-Za-z_]\w*)\s*=\s*({path})\s*;"
    )
}

fn usings_in(file: &str, ctx: &Ctx) -> Rc<Vec<CppUsing>> {
    let memo = ctx.memo::<Usings>();
    if let Some(hit) = memo.0.borrow().get(file) {
        return Rc::clone(hit);
    }
    let mut found: Vec<CppUsing> = Vec::new();
    if let Some(source) = ctx.read_file(file) {
        if source.contains("using") || source.contains("namespace") {
            let text = strip_comments_for_regex(&source, "cpp");
            let mut line = 1usize;
            let mut at = 0usize;
            let bytes = text.as_bytes();
            let squash = |s: &str| re!(r"\s+", "g").replace_all_str(s, "");
            for m in Re::cached(&using_pattern(), "g").captures_iter(&text) {
                let start = m.get(0).unwrap().start();
                while at < start {
                    if bytes[at] == b'\n' {
                        line += 1;
                    }
                    at += 1;
                }
                if let Some(d) = m.text(1).filter(|s| !s.is_empty()) {
                    found.push(CppUsing { line, kind: UsingKind::Directive, name: String::new(), path: squash(d) });
                } else if let Some(d) = m.text(2).filter(|s| !s.is_empty()) {
                    let path = squash(d);
                    let name = path[path.rfind("::").map_or(0, |i| i + 2)..].to_string();
                    found.push(CppUsing { line, kind: UsingKind::Declaration, name, path });
                } else if let (Some(n), Some(p)) = (m.text(3).filter(|s| !s.is_empty()), m.text(4).filter(|s| !s.is_empty())) {
                    found.push(CppUsing { line, kind: UsingKind::Alias, name: n.to_string(), path: squash(p) });
                }
            }
        }
    }
    let found = Rc::new(found);
    memo.0.borrow_mut().insert(file.to_string(), Rc::clone(&found));
    found
}
