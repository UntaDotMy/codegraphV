//! Methods of Go types: finding the type a name was written as, following aliases and embedded
//! fields, and the calls whose receiver is a local, a package-level variable or a struct field.

use super::chain::{go_asserted_type, go_chain_links, go_valued_has_no_project_methods, GoChainKind};
use super::qualify::*;
use super::scope::{go_local_decl, go_scope_index, GoDecl, GoDeclKind};
use crate::ctx::Ctx;
use crate::name_matcher::exact::prefer_call_site_file;
use crate::name_matcher::infer::{infer_local_receiver_type, GoReceiverDecl};
use crate::name_matcher::method::resolve_method_on_type;
use crate::name_matcher::std_methods::is_std_method_name;
use crate::name_matcher::util::*;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_search::query_utils::is_test_path;
use codegraph_types::jscompat::{js_trim, utf16_len};
use codegraph_types::nodepath::posix;
use codegraph_types::{Language, NodeKind};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// A Go type declared in the project: its name and the directory (package) that declares it.
#[derive(Clone, Debug, PartialEq)]
pub struct GoProjectType {
    pub name: String,
    pub dir: String,
}

/// A type alias's target, or one of a struct's embedded types: a name and the package directory
/// that declares it.
#[derive(Clone, Debug)]
pub struct GoTypeRef {
    pub name: String,
    pub pkg_dir: String,
}

memo_map!(AliasTargetMemo, String, Tri<GoTypeRef>);
memo_map!(EmbedsMemo, String, Rc<Vec<GoTypeRef>>);

/// The lines a type declaration spans, comments dropped.
fn decl_lines(node: &NodeRef, ctx: &Ctx, join: &str, skip_start_column: bool) -> String {
    let lines = lines_of(ctx, &node.file_path);
    let from = (node.start_line as usize).saturating_sub(1);
    let to = (node.end_line.max(node.start_line)) as usize;
    let end = to.min(lines.len());
    let slice = if from >= end { &lines[0..0] } else { &lines[from..end] };
    slice
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let l = if skip_start_column && i == 0 { after_col(l, node.start_column) } else { l.as_str() };
            let l = re!(r"\/\/.*$").replace_first(l, |_| String::new());
            re!(r"\/\*.*?\*\/", "g").replace_all_str(&l, "")
        })
        .collect::<Vec<_>>()
        .join(join)
}

/// What `type A = pkg.B` aliases: undefined for a type that is not an alias declaration, null for
/// an alias of something the project does not declare.
pub fn go_alias_target(type_node: &NodeRef, ctx: &Ctx) -> Tri<GoTypeRef> {
    if type_node.kind != NodeKind::TypeAlias {
        return Tri::Undefined;
    }
    memoized::<AliasTargetMemo>(ctx, type_node.id.clone(), || {
        let decl = decl_lines(type_node, ctx, " ", true);
        let head = re!(r"^\s*[A-Za-z_]\w*\s*(?:\[[^\]]*\])?\s*=").find(&decl);
        let m = head
            .as_ref()
            .and_then(|h| re!(r"^\s*(?:\*\s*)?([A-Za-z_]\w*)(?:\s*\.\s*([A-Za-z_]\w*))?\s*(?:\[.*\])?\s*;?\s*$").captures(&decl[h.end()..]))
            .map(|c| (c.text(1).unwrap_or("").to_string(), c.text(2).filter(|s| !s.is_empty()).map(String::from)));
        match m {
            Some((first, second)) => {
                let pkg_dir = if second.is_some() {
                    go_import_package_dir(&first, &type_node.file_path, ctx).value()
                } else {
                    go_bare_type_dir(&first, &type_node.file_path, ctx)
                };
                let name = second.clone().unwrap_or(first);
                match pkg_dir {
                    Some(pkg_dir) if !(second.is_none() && GO_BUILTIN_FIELD_TYPES.contains(name.as_str())) => Tri::Value(GoTypeRef { name, pkg_dir }),
                    _ => Tri::Null,
                }
            }
            None if head.is_some() => Tri::Null,
            None => Tri::Undefined,
        }
    })
}

/// The types a struct or interface declaration embeds (anonymous fields).
pub fn go_embedded_types(type_node: &NodeRef, ctx: &Ctx) -> Rc<Vec<GoTypeRef>> {
    memoized::<EmbedsMemo>(ctx, type_node.id.clone(), || {
        let mut embedded = Vec::new();
        let decl = decl_lines(type_node, ctx, "\n", false);
        if let (Some(open), Some(close)) = (decl.find('{'), decl.rfind('}')) {
            if close > open {
                let mut body = decl[open + 1..close].to_string();
                // Nested literal bodies are not this type's own members.
                while re!(r"\{[^{}]*\}").is_match(&body) {
                    body = re!(r"\{[^{}]*\}", "g").replace_all_str(&body, "");
                }
                for member in re!(r"[\n;]").split(&body) {
                    let Some(m) = re!(r#"^\s*\*?\s*([A-Za-z_]\w*)(?:\.([A-Za-z_]\w*))?\s*(?:\[[^\]]*\])?\s*(?:`[^`]*`|"[^"]*")?\s*$"#).captures(member) else { continue };
                    let first = m.text(1).unwrap_or("");
                    let second = m.text(2).filter(|s| !s.is_empty());
                    let pkg_dir = if second.is_some() {
                        go_import_package_dir(first, &type_node.file_path, ctx).value()
                    } else {
                        go_bare_type_dir(first, &type_node.file_path, ctx)
                    };
                    if let Some(pkg_dir) = pkg_dir {
                        embedded.push(GoTypeRef { name: second.unwrap_or(first).to_string(), pkg_dir });
                    }
                }
            }
        }
        Rc::new(embedded)
    })
}

/// `type_name`'s method in `pkg_dir`, the way Go finds it: the package's own methods, then an
/// alias's target, then the embedded types'. Undefined when the package declares no such type.
#[allow(clippy::too_many_arguments)]
pub fn resolve_go_method_in_package(
    type_name: &str,
    method_name: &str,
    pkg_dir: &str,
    matches: &[NodeRef],
    r: &UnresolvedRef,
    ctx: &Ctx,
    confidence: f64,
    resolved_by: ResolvedBy,
    depth: u32,
) -> Tri<ResolvedRef> {
    let own: Vec<NodeRef> = matches.iter().filter(|m| go_package_dir(&m.file_path) == pkg_dir).cloned().collect();
    if !own.is_empty() {
        if r.reference_kind == codegraph_types::ReferenceKind::FunctionRef && own.len() != 1 {
            return Tri::Null;
        }
        let chosen = prefer_call_site_file(own, &r.file_path);
        return Tri::Value(ResolvedRef::new(r, chosen[0].id.clone(), confidence, resolved_by));
    }
    let types = go_package_types(type_name, pkg_dir, ctx);
    if types.is_empty() {
        return Tri::Undefined;
    }
    // A method an unread file of the package declares is the package's.
    if go_unread_file_declares(type_name, method_name, pkg_dir, ctx) {
        return Tri::Null;
    }
    if depth >= 4 {
        return Tri::Null;
    }
    let mut unplaced = 0;
    for t in &types {
        match go_alias_target(t, ctx) {
            Tri::Undefined => {}
            aliased => {
                match aliased {
                    Tri::Value(a) => {
                        if let Some(via) = resolve_method_on_type(&a.name, method_name, r, ctx, confidence, resolved_by, Some(&a.pkg_dir), depth + 1) {
                            return Tri::Value(via);
                        }
                    }
                    _ => unplaced += 1,
                }
                continue;
            }
        }
        for embedded in go_embedded_types(t, ctx).iter() {
            if let Some(via) = resolve_method_on_type(&embedded.name, method_name, r, ctx, confidence, resolved_by, Some(&embedded.pkg_dir), depth + 1) {
                return Tri::Value(via);
            }
        }
    }
    if unplaced == types.len() {
        Tri::Undefined
    } else {
        Tri::Null
    }
}

// -- Unread files ---------------------------------------------------------------------------------

memo_map!(PackageFilesMemo, (), Rc<HashMap<String, Vec<String>>>);
memo_map!(UnreadFilesMemo, String, Rc<Vec<String>>);
memo_map!(UnreadMethodsMemo, String, Option<Rc<HashSet<String>>>);

/// The largest file read off disk to find the methods it declares.
const GO_UNREAD_SCAN_BYTES: u64 = 32 * 1024 * 1024;

fn go_unread_package_files(dir: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<UnreadFilesMemo>(ctx, dir.to_string(), || {
        let files = memoized::<PackageFilesMemo>(ctx, (), || {
            let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
            for f in ctx.get_all_files() {
                if !f.ends_with(".go") || f.ends_with("_test.go") {
                    continue;
                }
                by_dir.entry(go_package_dir(&f)).or_default().push(f);
            }
            Rc::new(by_dir)
        });
        // Files the index holds no node for: too large to index, or skipped.
        Rc::new(files.get(dir).map(|v| v.as_slice()).unwrap_or(&[]).iter().filter(|f| ctx.get_nodes_in_file_named(f, &posix::basename(f)).is_empty()).cloned().collect())
    })
}

fn go_unread_file_methods(file_path: &str, ctx: &Ctx) -> Option<Rc<HashSet<String>>> {
    memoized::<UnreadMethodsMemo>(ctx, file_path.to_string(), || {
        let abs = posix::join(&[ctx.project_root(), file_path]);
        let meta = std::fs::metadata(&abs).ok()?;
        if meta.len() > GO_UNREAD_SCAN_BYTES {
            return None;
        }
        let text = String::from_utf8_lossy(&std::fs::read(&abs).ok()?).into_owned();
        let mut methods = HashSet::new();
        for m in re!(r"^func\s*\(\s*(?:[A-Za-z_]\w*\s+)?\*?\s*([A-Za-z_]\w*)\s*(?:\[[^\]\n]*\])?\s*\)\s*([A-Za-z_]\w*)\s*[[(]", "gm").captures_iter(&text) {
            methods.insert(format!("{}::{}", m.text(1).unwrap_or(""), m.text(2).unwrap_or("")));
        }
        Some(Rc::new(methods))
    })
}

/// Does a file of the package the index holds no nodes for declare `type_name`'s `method_name`
/// (or may it, being unreadable)?
pub fn go_unread_file_declares(type_name: &str, method_name: &str, dir: &str, ctx: &Ctx) -> bool {
    go_unread_package_files(dir, ctx).iter().any(|f| match go_unread_file_methods(f, ctx) {
        None => true,
        Some(methods) => methods.contains(&format!("{type_name}::{method_name}")),
    })
}

// -- Written types --------------------------------------------------------------------------------

/// The project type a written type names: undefined when it cannot be placed, null when it is
/// known not to be the project's (or is an alias of something that is not).
pub fn go_written_type(written: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<GoProjectType> {
    let Some(m) = re!(r"^\*?\s*(?:([A-Za-z_]\w*)\s*\.\s*)?([A-Za-z_]\w*)\s*(?:\[[\s\S]*\])?$").captures(written) else { return Tri::Undefined };
    let qualifier = m.text(1).filter(|s| !s.is_empty());
    let name = m.text(2).unwrap_or("");
    let declares = |dir: &str| !dir.is_empty() && !go_package_types(name, dir, ctx).is_empty();
    let dir = if let Some(q) = qualifier {
        let dir = match go_import_package_dir(q, &r.file_path, ctx) {
            Tri::Value(d) if !d.is_empty() => d,
            _ => return Tri::Null,
        };
        if !declares(&dir) {
            return Tri::Undefined;
        }
        dir
    } else {
        let Some(dir) = go_bare_type_dir(name, &r.file_path, ctx) else { return Tri::Null };
        if !declares(&dir) {
            return if GO_BUILTIN_FIELD_TYPES.contains(name) { Tri::Null } else { Tri::Undefined };
        }
        dir
    };
    // A type that is an alias of something outside the project is not the project's.
    if go_package_types(name, &dir, ctx).iter().all(|t| matches!(go_alias_target(t, ctx), Tri::Null)) {
        return Tri::Null;
    }
    Tri::Value(GoProjectType { name: name.to_string(), dir })
}

memo_map!(AssertionBindingsMemo, String, Rc<HashSet<String>>);

/// The names a file assigns from an expression containing a type assertion.
fn go_assertion_bindings(file_path: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<AssertionBindingsMemo>(ctx, file_path.to_string(), || {
        let mut names = HashSet::new();
        for line in lines_of(ctx, file_path).iter() {
            if !line.contains(".(") || utf16_len(line) > 10_000 {
                continue;
            }
            for m in re!(r"(?:^|[^\w.])([A-Za-z_]\w*)\s*(?:,\s*[A-Za-z_]\w*\s*)?:?=(?!=)", "g").captures_iter(line) {
                if let Some(n) = m.text(1) {
                    names.insert(n.to_string());
                }
            }
        }
        Rc::new(names)
    })
}

/// The type a local was asserted to: `x := v.(pkg.T)` (not `v.(type)`, not a `x, ok :=` form).
pub fn go_asserted_local_type(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if !go_assertion_bindings(&r.file_path, ctx).contains(name) {
        return None;
    }
    let decl = go_local_decl(name, r, ctx)?;
    if decl.kind == GoDeclKind::Param {
        return None;
    }
    let index = go_scope_index(&r.file_path, ctx)?;
    let code = index.code.as_str();
    let line_start = code[..decl.at].rfind('\n').map_or(0, |x| x + 1);
    if re!(r",[ \t]*$").is_match(&code[line_start..decl.at]) {
        return None;
    }
    let head = re!(r"^[A-Za-z_]\w*[ \t]*(?:,[ \t]*[A-Za-z_]\w*[ \t]*)?:?=(?!=)[ \t]*").find(&code[decl.at..])?;
    let mut end = 0usize;
    let links = go_chain_links(code, decl.at + head.end(), Some(&mut end), false);
    let last = links.last()?;
    if links.len() < 2 || last.kind != GoChainKind::Assert || last.text == "type" || !re!(r"^(?:[;\r\n]|$)").is_match(slice_bytes(code, end, end + 1)) {
        return None;
    }
    Some(last.text.clone())
}

/// A local that is a type-asserted value of a type the project declares (or can be placed at).
pub fn is_go_asserted_local(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    go_asserted_local_type(name, r, ctx).is_some_and(|w| !go_written_type(&w, r, ctx).is_undefined())
}

/// What a local's value is: a project type, or a package it was made in (`origin`).
pub enum GoLocalValue {
    Type(GoProjectType),
    Origin(Option<String>),
}

/// None when the local's value is known to be from outside the project.
pub fn go_local_value(name: &str, local: &GoDecl, r: &UnresolvedRef, ctx: &Ctx) -> Option<GoLocalValue> {
    let written = go_asserted_local_type(name, r, ctx).or_else(|| local.ty.clone());
    let ty = match &written {
        Some(w) => go_written_type(w, r, ctx),
        None => Tri::Undefined,
    };
    match ty {
        Tri::Value(t) => return Some(GoLocalValue::Type(t)),
        Tri::Null => return None,
        Tri::Undefined => {}
    }
    if local.callee.is_some() {
        return Some(GoLocalValue::Origin(Some(go_package_dir(&r.file_path))));
    }
    match &local.from {
        None => Some(GoLocalValue::Origin(None)),
        Some(from) => match go_import_package_dir(from, &r.file_path, ctx) {
            Tri::Null => None,
            Tri::Value(dir) => Some(GoLocalValue::Origin(Some(dir))),
            Tri::Undefined => Some(GoLocalValue::Origin(None)),
        },
    }
}

/// `x.Method()` where `x` is a local of a project type, or made in the project.
pub fn is_go_project_local_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(dot) = r.reference_name.find('.').filter(|d| *d > 0) else { return false };
    if !is_calls(r) {
        return false;
    }
    let name = &r.reference_name[..dot];
    go_local_decl(name, r, ctx).is_some_and(|local| go_local_value(name, &local, r, ctx).is_some())
}

/// `pkg.Method()` where `pkg` is a local that takes a standard-library package's name.
pub fn match_go_stdlib_local_call(receiver: &str, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some(local) = go_local_decl(receiver, r, ctx) else { return Tri::Undefined };
    match go_local_value(receiver, &local, r, ctx) {
        None => Tri::Null,
        Some(value) => Tri::from(match_go_stdlib_named_local_call(receiver, method, &value, r, ctx)),
    }
}

fn match_go_stdlib_named_local_call(receiver: &str, method: &str, value: &GoLocalValue, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let origin = match value {
        GoLocalValue::Type(t) => return resolve_method_on_type(&t.name, method, r, ctx, 0.9, ResolvedBy::InstanceMethod, Some(&t.dir), 0),
        GoLocalValue::Origin(o) => o,
    };
    let methods: Vec<NodeRef> = ctx
        .get_nodes_by_name(method)
        .iter()
        .filter(|n| n.kind == NodeKind::Method && n.language == Language::Go && n.id != r.from_node_id && (is_test_path(&r.file_path) || !is_test_path(&n.file_path)))
        .cloned()
        .collect();
    let own: Vec<&NodeRef> = match origin {
        Some(o) => methods.iter().filter(|n| &go_package_dir(&n.file_path) == o).collect(),
        None => Vec::new(),
    };
    if !own.is_empty() {
        let want = format!("{receiver}::{method}");
        let named: Vec<&NodeRef> = own.iter().copied().filter(|n| n.qualified_name == want).collect();
        let pool = if named.is_empty() { own } else { named };
        return if pool.len() == 1 { Some(ResolvedRef::new(r, pool[0].id.clone(), 0.8, ResolvedBy::InstanceMethod)) } else { None };
    }
    if methods.len() == 1 && !is_std_method_name("go", method) {
        Some(ResolvedRef::new(r, methods[0].id.clone(), 0.7, ResolvedBy::InstanceMethod))
    } else {
        None
    }
}

// -- Package-level variables ----------------------------------------------------------------------

type PackageValueType = (String, String);

/// The type a package-level `var` / `const` is declared with or initialised to: undefined when
/// it cannot be read, null when it is a literal of a type the project does not declare.
pub fn go_package_value_type(n: &NodeRef, ctx: &Ctx) -> Tri<PackageValueType> {
    let Some(lines) = ctx.get_file_lines(&n.file_path) else { return Tri::Undefined };
    let mut i = n.start_line as i64 - 1;
    while i >= 0 && i > n.start_line as i64 - 200 {
        let raw = lines.get(i as usize).map(String::as_str).unwrap_or("");
        let line = re!(r"\/\/.*$").replace_first(raw, |_| String::new());
        let own = i == n.start_line as i64 - 1;
        if !own && re!(r"^\s*(?:$|\/\*|\*)").is_match(&line) {
            i -= 1;
            continue;
        }
        let Some(spec) = re!(r"^\s*(?:(?:var|const)\s*(?:\(\s*)?)?([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)(.*)$").captures(&line) else { return Tri::Undefined };
        if own && !re!(r"\s*,\s*").split(spec.text(1).unwrap_or("")).into_iter().any(|x| x == n.name) {
            return Tri::Undefined;
        }
        let rest = js_trim(spec.text(2).unwrap_or("")).to_string();
        if rest.is_empty() {
            // A const group's bare name repeats the previous spec.
            if n.kind != NodeKind::Constant || re!(r"^\s*(?:var|const)\b").is_match(&line) {
                return Tri::Undefined;
            }
            i -= 1;
            continue;
        }
        let eq = re!(r"=(?!=)").find(&rest).map(|m| m.start());
        let written = js_trim(match eq {
            Some(e) => &rest[..e],
            None => &rest,
        })
        .to_string();
        if re!(r#"[:"'`]"#).is_match(&written) {
            return Tri::Undefined;
        }
        if !written.is_empty() {
            return Tri::Value((written, n.file_path.clone()));
        }
        return go_value_type(&rest[eq.map_or(0, |e| e + 1)..], &n.file_path, ctx);
    }
    Tri::Undefined
}

/// The type a `&T{…}` / `T(…)` / `pkg.T{…}` initialiser builds.
fn go_value_type(value: &str, file_path: &str, ctx: &Ctx) -> Tri<PackageValueType> {
    let Some(m) = re!(r"^\s*&?\s*(?:([A-Za-z_]\w*)\s*\.\s*)?([A-Za-z_]\w*)\s*(?:\[[^\]\n]*\]\s*)?([({])").captures(value) else { return Tri::Undefined };
    let qualifier = m.text(1).filter(|s| !s.is_empty());
    let name = m.text(2).unwrap_or("");
    let open = m.text(3).unwrap_or("");
    if qualifier.is_some_and(|q| is_go_outside_package(q, file_path, ctx)) {
        return Tri::Null;
    }
    let written = match qualifier {
        Some(q) => format!("{q}.{name}"),
        None => name.to_string(),
    };
    if open == "{" {
        return if super::scope::GO_TYPE_KEYWORDS.contains(name) { Tri::Null } else { Tri::Value((written, file_path.to_string())) };
    }
    let dir = match qualifier {
        Some(q) => go_import_package_dir(q, file_path, ctx).value(),
        None => go_bare_type_dir(name, file_path, ctx),
    };
    match dir {
        Some(d) if !d.is_empty() && !go_package_types(name, &d, ctx).is_empty() => Tri::Value((written, file_path.to_string())),
        _ => Tri::Undefined,
    }
}

/// `value.Method()` where `value` is a package-level variable of the reference's own package.
pub fn match_go_package_value_call(receiver: &str, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let dir = go_package_dir(&r.file_path);
    let decls: Vec<NodeRef> = ctx
        .get_nodes_by_name(receiver)
        .iter()
        .filter(|n| n.language == Language::Go && matches!(n.kind, NodeKind::Variable | NodeKind::Constant) && go_package_dir(&n.file_path) == dir)
        .cloned()
        .collect();
    if decls.is_empty() {
        return Tri::Undefined;
    }
    if ctx.get_import_mappings(&r.file_path, Language::Go).iter().any(|m| m.local_name == receiver) {
        return Tri::Undefined;
    }
    if go_local_decl(receiver, r, ctx).is_some() {
        return Tri::Undefined;
    }
    // Every declaration of the name in this package (build twins) must agree on its type.
    let pkg = go_package_clause(&r.file_path, ctx);
    let squash = |s: &str| re!(r"\s", "g").replace_all_str(s, "");
    let mut ty: Option<Option<PackageValueType>> = None;
    for d in &decls {
        if go_package_clause(&d.file_path, ctx) != pkg {
            continue;
        }
        let t = go_package_value_type(d, ctx);
        let t = match t {
            Tri::Undefined => return Tri::Undefined,
            Tri::Null => None,
            Tri::Value(v) => Some(v),
        };
        if let Some(prev) = &ty {
            if prev.as_ref().map(|p| squash(&p.0)) != t.as_ref().map(|p| squash(&p.0)) {
                return Tri::Undefined;
            }
        }
        ty = Some(t);
    }
    let Some(ty) = ty else { return Tri::Undefined };
    let Some((written, file)) = ty else { return Tri::Null };
    let Some(named) = go_named_type(&written) else { return Tri::Null };
    if is_go_outside_type_name(&named, &file, ctx) {
        return Tri::Null;
    }
    let dot = named.find('.');
    let name = &named[dot.map_or(0, |d| d + 1)..];
    if dot.is_none() && GO_BUILTIN_FIELD_TYPES.contains(name) {
        return Tri::Null;
    }
    let pkg_dir = match dot {
        None => match go_bare_type_dir(name, &file, ctx) {
            Some(d) => Some(d),
            None => return Tri::Null,
        },
        Some(d) => go_import_package_dir(&named[..d], &file, ctx).value(),
    };
    let Some(pkg_dir) = pkg_dir.filter(|d| !d.is_empty() && !go_package_types(name, d, ctx).is_empty()) else { return Tri::Undefined };
    // Methods of the type may live in a file the index never read.
    match resolve_method_on_type(name, method, r, ctx, 0.9, ResolvedBy::InstanceMethod, Some(&pkg_dir), 0) {
        Some(found) => Tri::Value(found),
        None if go_unread_file_declares(name, method, &pkg_dir, ctx) => Tri::Null,
        None => Tri::Undefined,
    }
}

/// A written type qualified by a package from outside the project.
fn is_go_outside_type_name(written: &str, file_path: &str, ctx: &Ctx) -> bool {
    re!(r"^[*&\s]*([A-Za-z_]\w*)\s*\.").captures(written).and_then(|c| c.text(1).map(String::from)).is_some_and(|q| is_go_outside_package(&q, file_path, ctx))
}

// -- Assertions and field chains ------------------------------------------------------------------

/// `x.(pkg.T).Method()`: the method on the asserted type.
pub fn match_go_asserted_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if r.language != Language::Go || !is_calls(r) || !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        return Tri::Undefined;
    }
    let Some(asserted) = go_asserted_type(r, ctx) else { return Tri::Undefined };
    match go_written_type(&asserted, r, ctx) {
        Tri::Value(t) => Tri::from(resolve_method_on_type(&t.name, &r.reference_name, r, ctx, 0.9, ResolvedBy::InstanceMethod, Some(&t.dir), 0)),
        _ => Tri::Null,
    }
}

/// `base.field.Method()` where `base` is a local of a struct type declaring `field`.
pub fn match_go_field_chain_call(receiver_chain: &str, method_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let segs: Vec<&str> = receiver_chain.split('.').collect();
    if segs.len() != 2 || segs[0].is_empty() || segs[1].is_empty() {
        return None;
    }
    let (base, field) = (segs[0], segs[1]);
    // A base bound from a type assertion has the type Go finds for it, or one whose fields the
    // project doesn't declare, as does one declared as a type from outside the project:
    // `req.URL.Query()` on an `*http.Request` is no project `Request`'s.
    let mut decl = GoReceiverDecl::default();
    let base_type = infer_local_receiver_type(base, r, ctx, Some(&mut decl))?;
    if matches!(decl.asserted, Tri::Null) {
        return None;
    }
    if let Tri::Value(declared) = &decl.declared {
        if go_has_no_project_methods(declared, r, ctx) {
            return None;
        }
    }
    // Nor one a call gives a value of a type from outside the project.
    if let Tri::Value(valued) = &decl.valued {
        if go_valued_has_no_project_methods(valued, ctx) {
            return None;
        }
    }
    let field_type = Re::cached(&format!(r"\b{}\s+\*?\[?\]?([A-Za-z_][\w.]*)", escape_regex(field)), "");
    let asserted = decl.asserted.as_value();
    let mut structs: Vec<NodeRef> = ctx
        .get_nodes_by_name(asserted.map_or(base_type.as_str(), |a| a.name.as_str()))
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Struct | NodeKind::Class) && n.language == Language::Go)
        .cloned()
        .collect();
    // The struct the declaring package has is the one the base's type means (a method receiver's
    // type is always the call site's own package's): Go type names repeat across packages, and
    // another package's same-named struct with a same-named field is not this one (#2323).
    let base_pkg = asserted.map(|a| a.dir.clone()).or_else(|| go_declared_type_package(decl.raw.as_deref(), decl.file.as_deref().unwrap_or(&r.file_path), ctx));
    let declared: Vec<NodeRef> = structs.iter().filter(|n| Some(go_package_dir(&n.file_path)) == base_pkg).cloned().collect();
    if !declared.is_empty() || asserted.is_some() {
        structs = declared;
    }
    structs = prefer_call_site_file(structs, &r.file_path);
    for s in &structs {
        let Some(source) = ctx.read_file(&s.file_path).filter(|s| !s.is_empty()) else { continue };
        // Only the struct's own declaration lines — a same-named identifier elsewhere in the
        // file can't donate a type. Matched line by line with comments stripped: chi's `Mux` has
        // a doc comment reading "the tree router" right above `tree *node`.
        let from = (s.start_line as usize).saturating_sub(1);
        for raw_line in source.split('\n').skip(from).take((s.end_line as usize).saturating_sub(from)) {
            let line = re!(r"\/\/.*$").replace_first(raw_line, |_| String::new());
            let line = re!(r"\/\*.*?\*\/", "g").replace_all_str(&line, "");
            let Some(raw_type) = field_type.captures(&line).and_then(|c| c.text(1).filter(|t| !t.is_empty()).map(String::from)) else { continue };
            // The package that declares the field's type: for a bare type name the struct's own,
            // or one its file dot-imports; the imported one for a package-qualified name — only
            // when that package is one of the project's.
            let mut pkg_dir: Option<String> = if raw_type.contains('.') {
                None
            } else {
                match go_bare_type_dir(&raw_type, &s.file_path, ctx) {
                    Some(d) => Some(d),
                    None => continue,
                }
            };
            if raw_type.contains('.') {
                match go_import_package_dir(raw_type.split('.').next().unwrap_or(""), &s.file_path, ctx).value() {
                    Some(d) => pkg_dir = Some(d),
                    None => continue,
                }
            }
            let field_type_name = raw_type.rsplit('.').next().unwrap_or("");
            if field_type_name.is_empty() || !re!(r"^[A-Za-z_]").is_match(field_type_name) || GO_BUILTIN_FIELD_TYPES.contains(field_type_name) {
                continue;
            }
            let mut probe = r.clone();
            probe.file_path = s.file_path.clone();
            if go_has_no_project_methods(&raw_type, &probe, ctx) {
                continue;
            }
            if let Some(resolved) = resolve_method_on_type(field_type_name, method_name, r, ctx, 0.85, ResolvedBy::InstanceMethod, pkg_dir.as_deref(), 0) {
                return Some(resolved);
            }
        }
    }
    None
}
