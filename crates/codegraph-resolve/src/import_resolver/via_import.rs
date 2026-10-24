//! `resolveViaImport` and its per-language binders — the import resolver's reference
//! resolution: a name (or `Receiver.member`) the file imports is followed to the module it
//! comes from and the symbol that module exports.

use super::{
    collect_rust_use_bindings, find_exported_symbol, is_cobol_copybook_ref, is_dart_import_ref, is_esm_import_language, is_external_import,
    is_nix_path_import_ref, is_php_include_path_ref, is_rust_stdlib_root, lua_basename_index, names_own_config_module, resolve_import_path,
    resolve_php_include_path, resolve_static_member, ExternalOpts, Want, REEXPORT_MAX_DEPTH,
};
use crate::ambient_modules::{
    ambient_module_declares, find_ambient_module_export, find_ambient_module_file, in_block, is_in_module_block, namespace_blocks, AmbientWant, NamespaceBlock,
};
use crate::ctx::Ctx;
use crate::dart_libraries::dart_directive_file;
use crate::import_resolver::python::python_module_file_node;
use crate::name_matcher::infer::{array_element_type, infer_local_receiver_type, local_receiver_type_patterns, normalize_inferred_type_name, ts_imported_type_file, GoReceiverDecl};
use crate::name_matcher::js_imports::re_export_source;
use crate::name_matcher::match_by_file_path;
use crate::name_matcher::object_literal::{resolve_object_literal_binding, resolve_object_literal_member};
use crate::name_matcher::{go_ref_qualifier, is_rust_associated_item};
use crate::name_matcher::python::python_function_module_alias;
use crate::name_matcher::reach::{is_ts_js_file, ts_global_scope};
use crate::name_matcher::resolve_method_on_type;
use crate::types::{ImportMapping, NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_db::reference_tail::{module_reference_keys, module_tail};
use codegraph_jsre::re;
use codegraph_search::is_test_file;
use codegraph_types::{nodepath::posix, EdgeKind, Language, Node, NodeKind};
use std::collections::HashSet;

fn imported(r: &UnresolvedRef, target: &str, confidence: f64) -> ResolvedRef {
    ResolvedRef::new(r, target, confidence, ResolvedBy::Import)
}

fn want(is_default: bool, is_namespace: bool, exported_name: &str, member_name: Option<&str>) -> Want {
    Want { is_default, is_namespace, exported_name: exported_name.to_string(), member_name: member_name.map(|s| s.to_string()) }
}

fn exported(file: &str, w: &Want, language: Language, ctx: &Ctx) -> Option<NodeRef> {
    find_exported_symbol(file, w, language, ctx, &mut HashSet::new(), 0)
}

fn file_node_of(file: &str, ctx: &Ctx) -> Option<NodeRef> {
    ctx.get_nodes_in_file(file).iter().find(|n| n.kind == NodeKind::File).cloned()
}

fn file_node_by_path(resolved_path: &str, ctx: &Ctx) -> Option<NodeRef> {
    let basename = resolved_path.rsplit('/').next().unwrap_or("");
    ctx.get_nodes_by_name(basename).iter().find(|n| n.kind == NodeKind::File && n.file_path == resolved_path).cloned()
}

pub fn is_bare_module_specifier(source: &str, language: Language) -> bool {
    is_esm_import_language(language) && !source.starts_with('.') && !source.starts_with('/')
}

// -- JS path / bare module imports -------------------------------------------------------------

fn is_js_path_specifier(name: &str) -> bool {
    name.starts_with("./") || name.starts_with("../") || name == "." || name == ".." || (name.contains('/') && !re!(r"\s").is_match(name))
}

/// Is this a JS/TS reference naming a file path (`./x`, `../y`)?
pub fn is_js_path_import_ref(r: &UnresolvedRef) -> bool {
    r.is_kind(EdgeKind::Imports) && is_esm_import_language(r.language) && is_js_path_specifier(&r.reference_name)
}

fn is_js_bare_module_ref(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !r.is_kind(EdgeKind::Imports) || !is_esm_import_language(r.language) || is_js_path_specifier(&r.reference_name) {
        return false;
    }
    let imports = ctx.get_import_mappings(&r.file_path, r.language);
    imports.iter().any(|m| m.source == r.reference_name) && !imports.iter().any(|m| m.local_name == r.reference_name)
}

/// The file an `imports` reference names, when it is a JS/TS module import.
pub fn js_module_file(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if !is_js_path_import_ref(r) && !is_js_bare_module_ref(r, ctx) {
        return None;
    }
    if is_bare_module_specifier(&r.reference_name, r.language) {
        if let Some(declaring) = find_ambient_module_file(&r.reference_name, &r.file_path, ctx) {
            return Some(declaring.file_path.clone());
        }
    }
    let file = resolve_import_path(&r.reference_name, &r.file_path, r.language, ctx)?;
    if file != r.file_path {
        Some(file)
    } else {
        None
    }
}

// -- JVM -----------------------------------------------------------------------------------------

fn pick_closest_jvm_candidate(candidates: &[NodeRef], from_path: &str) -> NodeRef {
    let from_dirs: Vec<&str> = {
        let mut p: Vec<&str> = from_path.split('/').collect();
        p.pop();
        p
    };
    let shared_prefix = |p: &str| -> usize {
        let mut d: Vec<&str> = p.split('/').collect();
        d.pop();
        let mut shared = 0;
        for i in 0..from_dirs.len().min(d.len()) {
            if from_dirs[i] == d[i] {
                shared += 1;
            } else {
                break;
            }
        }
        shared
    };
    let is_expect = |n: &Node| n.decorators.as_ref().is_some_and(|d| d.iter().any(|x| x == "expect"));
    let mut best = candidates[0].clone();
    let mut best_prox = shared_prefix(&best.file_path);
    for c in &candidates[1..] {
        let prox = shared_prefix(&c.file_path);
        if prox > best_prox || (prox == best_prox && is_expect(c) && !is_expect(&best)) {
            best = c.clone();
            best_prox = prox;
        }
    }
    best
}

/// A Java/Kotlin `import a.b.C` resolved by qualified name.
pub fn resolve_jvm_import(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !r.is_kind(EdgeKind::Imports) || !matches!(r.language, Language::Java | Language::Kotlin) {
        return None;
    }
    let fqn = &r.reference_name;
    let last_dot = fqn.rfind('.').filter(|d| *d > 0)?;
    let pkg = &fqn[..last_dot];
    let sym = &fqn[last_dot + 1..];
    if sym == "*" {
        return None;
    }
    let candidates = ctx.get_nodes_by_qualified_name(&format!("{pkg}::{sym}"));
    if candidates.is_empty() {
        return None;
    }
    let best = if candidates.len() == 1 { candidates[0].clone() } else { pick_closest_jvm_candidate(&candidates, &r.file_path) };
    Some(imported(r, &best.id, 0.95))
}

fn resolve_jvm_alias(r: &UnresolvedRef, imports: &[ImportMapping], ctx: &Ctx) -> Option<ResolvedRef> {
    let dot = r.reference_name.find('.');
    let root = dot.map_or(r.reference_name.as_str(), |d| &r.reference_name[..d]);
    let imp = imports.iter().find(|m| m.local_name == root && m.local_name != m.exported_name)?;
    let parts: Vec<&str> = imp.source.split('.').collect();
    for i in (1..parts.len()).rev() {
        let qn = format!("{}::{}", parts[..i].join("."), parts[i..].join("::"));
        let Some(target) = ctx.get_nodes_by_qualified_name(&qn).first().cloned() else { continue };
        let member = dot.map(|d| &r.reference_name[d + 1..]);
        return match member {
            None => Some(imported(r, &target.id, 0.9)),
            Some(m) if m.contains('.') => None,
            Some(m) => ctx.get_nodes_by_qualified_name(&format!("{}::{m}", target.qualified_name)).first().map(|f| imported(r, &f.id, 0.9)),
        };
    }
    None
}

fn resolve_java_imported_reference(r: &UnresolvedRef, imports: &[ImportMapping], ctx: &Ctx) -> Option<ResolvedRef> {
    if imports.is_empty() {
        return None;
    }
    let ext = if r.language == Language::Kotlin { ".kt" } else { ".java" };
    for imp in imports {
        let matches_bare = imp.local_name == r.reference_name;
        let matches_qualified = r.reference_name.starts_with(&format!("{}.", imp.local_name));
        if !matches_bare && !matches_qualified {
            continue;
        }
        let fqn_path = format!("{}{ext}", imp.source.replace('.', "/"));
        let member_name = if matches_bare { imp.local_name.clone() } else { r.reference_name[imp.local_name.len() + 1..].to_string() };
        let candidates = ctx.get_nodes_by_name(&member_name);
        for node in candidates.iter() {
            if node.language != r.language {
                continue;
            }
            let fp = node.file_path.replace('\\', "/");
            if fp.ends_with(&fqn_path) || fp.ends_with(&format!("/{fqn_path}")) {
                return Some(imported(r, &node.id, 0.9));
            }
        }
        if matches_bare {
            if let Some(dot) = imp.source.rfind('.').filter(|d| *d > 0) {
                let owner_path = format!("{}{ext}", imp.source[..dot].replace('.', "/"));
                for node in candidates.iter() {
                    if node.language != r.language {
                        continue;
                    }
                    let fp = node.file_path.replace('\\', "/");
                    if fp.ends_with(&owner_path) || fp.ends_with(&format!("/{owner_path}")) {
                        return Some(imported(r, &node.id, 0.9));
                    }
                }
            }
        }
    }
    None
}

// -- PHP -----------------------------------------------------------------------------------------

fn is_static_member_container_kind(k: NodeKind) -> bool {
    super::is_static_member_container(k)
}

/// `Imported::member()` — a static call through a `use`d class.
pub fn resolve_php_imported_static_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if r.language != Language::Php || !r.is_kind(EdgeKind::Calls) {
        return Tri::Undefined;
    }
    let Some(call) = re!(r"^(\w+)\.(\w+)$").captures(&r.reference_name) else { return Tri::Undefined };
    let (receiver, member) = (call.text(1).unwrap_or(""), call.text(2).unwrap_or(""));
    let imports = ctx.get_import_mappings(&r.file_path, r.language);
    let Some(imp) = imports.iter().find(|i| i.local_name == receiver) else { return Tri::Undefined };
    let line = ctx.get_file_lines(&r.file_path).and_then(|l| l.get((r.line as usize).wrapping_sub(1)).cloned());
    if line.is_some_and(|l| crate::name_matcher::util::after_col(&l, r.column).starts_with('$')) {
        return Tri::Undefined;
    }
    let fqn = imp.source.strip_prefix('\\').unwrap_or(&imp.source);
    let type_name = match fqn.rfind('\\') {
        None => fqn.to_string(),
        Some(sep) => format!("{}::{}", &fqn[..sep], &fqn[sep + 1..]),
    };
    let owners: Vec<NodeRef> =
        ctx.get_nodes_by_qualified_name(&type_name).iter().filter(|n| n.language == Language::Php && is_static_member_container_kind(n.kind)).cloned().collect();
    if owners.len() != 1 {
        return Tri::Null;
    }
    let owner = &owners[0];
    let methods: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&format!("{}::{member}", owner.qualified_name))
        .iter()
        .filter(|n| n.language == Language::Php && n.kind == NodeKind::Method && n.file_path == owner.file_path)
        .cloned()
        .collect();
    if methods.len() != 1 {
        return Tri::Null;
    }
    Tri::Value(imported(r, &methods[0].id, 0.95))
}

/// A PHP fully-qualified class name (`\App\Foo`, `Models\User`) resolved to its declaration.
pub fn resolve_php_qualified_class_ref(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if r.language != Language::Php {
        return Tri::Undefined;
    }
    let mut name = r.reference_name.clone();
    let mut member: Option<String> = None;
    if r.is_kind(EdgeKind::Calls) {
        let parsed = re!(r"^(.*\\[^\\.:]+)(?:\.|::)(\w+)$")
            .captures(&name)
            .map(|call| (call.text(1).unwrap_or("").to_string(), call.text(2).unwrap_or("").to_string()));
        let Some((n, m)) = parsed else { return Tri::Undefined };
        name = n;
        member = Some(m);
    } else if !matches!(
        r.reference_kind,
        codegraph_types::ReferenceKind::Edge(EdgeKind::Instantiates | EdgeKind::Extends | EdgeKind::Implements | EdgeKind::References)
    ) {
        return Tri::Undefined;
    }
    let Some(separator) = name.find('\\') else { return Tri::Undefined };
    let fqn = if separator == 0 {
        name[1..].to_string()
    } else {
        let head = &name[..separator];
        let imports = ctx.get_import_mappings(&r.file_path, r.language);
        if let Some(imp) = imports.iter().find(|i| i.local_name == head) {
            format!("{}{}", imp.source.strip_prefix('\\').unwrap_or(&imp.source), &name[separator..])
        } else {
            let mut namespaces: Vec<NodeRef> =
                ctx.get_nodes_in_file(&r.file_path).iter().filter(|n| n.kind == NodeKind::Namespace && n.start_line <= r.line).cloned().collect();
            namespaces.sort_by(|a, b| b.start_line.cmp(&a.start_line));
            match namespaces.first() {
                Some(ns) => format!("{}\\{name}", ns.qualified_name),
                None => name.clone(),
            }
        }
    };
    let qualified_name = match fqn.rfind('\\') {
        None => fqn.clone(),
        Some(cut) => format!("{}::{}", &fqn[..cut], &fqn[cut + 1..]),
    };
    let classes: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&qualified_name)
        .iter()
        .filter(|n| n.language == Language::Php && is_static_member_container_kind(n.kind))
        .cloned()
        .collect();
    if classes.len() != 1 {
        return if r.is_kind(EdgeKind::References) { Tri::Undefined } else { Tri::Null };
    }
    let owner = &classes[0];
    let Some(member) = member.filter(|m| !m.is_empty()) else { return Tri::Value(imported(r, &owner.id, 0.95)) };
    let methods: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&format!("{}::{member}", owner.qualified_name))
        .iter()
        .filter(|n| n.language == Language::Php && n.kind == NodeKind::Method && n.file_path == owner.file_path)
        .cloned()
        .collect();
    if methods.len() != 1 {
        return Tri::Undefined;
    }
    Tri::Value(imported(r, &methods[0].id, 0.95))
}

// -- TypeScript qualified type references (`Ns.Inner.Type`) --------------------------------------

const MEMBER_DECLARATION_KINDS: [NodeKind; 9] = [
    NodeKind::Class,
    NodeKind::Interface,
    NodeKind::TypeAlias,
    NodeKind::Enum,
    NodeKind::Struct,
    NodeKind::Function,
    NodeKind::Component,
    NodeKind::Constant,
    NodeKind::Variable,
];

fn is_type_declaration_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Interface | NodeKind::TypeAlias | NodeKind::Enum | NodeKind::EnumMember | NodeKind::Struct)
}

fn types_first_key(n: &Node) -> u8 {
    (!is_type_declaration_kind(n.kind)) as u8
}

fn innermost_block<'a>(line: u32, column: u32, blocks: &'a [NamespaceBlock]) -> Option<&'a NamespaceBlock> {
    let mut inner: Option<&NamespaceBlock> = None;
    for block in blocks {
        if !in_block(line, column, block) {
            continue;
        }
        if inner.is_none_or(|i| block.start_line > i.start_line || (block.start_line == i.start_line && block.start_column > i.start_column)) {
            inner = Some(block);
        }
    }
    inner
}

fn has_namespace(file: &str, path: &str, ctx: &Ctx) -> bool {
    namespace_blocks(file, ctx).iter().any(|b| b.path == path || b.path.starts_with(&format!("{path}.")))
}

fn declarations_in(file: &str, scope: &[String], name: &str, ctx: &Ctx) -> Vec<NodeRef> {
    let blocks = namespace_blocks(file, ctx);
    let want = scope.join(".");
    let mut found: Vec<NodeRef> = ctx
        .get_nodes_in_file_named(file, name)
        .into_iter()
        .filter(|n| {
            n.qualified_name == name
                && MEMBER_DECLARATION_KINDS.contains(&n.kind)
                && innermost_block(n.start_line, n.start_column, &blocks).map_or("", |b| b.path.as_str()) == want
        })
        .collect();
    found.sort_by_key(|n| types_first_key(n));
    found
}

fn container_member(container: &Node, path: &[String], ctx: &Ctx) -> Option<NodeRef> {
    if path.is_empty() {
        return None;
    }
    let mut found: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&format!("{}::{}", container.qualified_name, path.join("::")))
        .iter()
        .filter(|n| n.file_path == container.file_path)
        .cloned()
        .collect();
    found.sort_by_key(|n| types_first_key(n));
    found.into_iter().next()
}

fn type_member_in(file: &str, scope: &[String], path: &[String], ctx: &Ctx) -> Option<NodeRef> {
    let (name, rest) = path.split_first()?;
    if !rest.is_empty() {
        let mut deeper = scope.to_vec();
        deeper.push(name.clone());
        if has_namespace(file, &deeper.join("."), ctx) {
            return type_member_in(file, &deeper, rest, ctx);
        }
    }
    let declared = declarations_in(file, scope, name, ctx);
    if rest.is_empty() {
        return declared.into_iter().next();
    }
    let container = declared.iter().find(|n| is_static_member_container_kind(n.kind))?;
    container_member(container, rest, ctx)
}

fn namespace_declaring_file(file: &str, name: &str, language: Language, ctx: &Ctx, depth: usize) -> Option<(String, String)> {
    if depth > REEXPORT_MAX_DEPTH {
        return None;
    }
    if namespace_blocks(file, ctx).iter().any(|b| b.path == name || b.path.starts_with(&format!("{name}."))) {
        return Some((file.to_string(), name.to_string()));
    }
    for rex in ctx.get_re_exports(file, language).iter() {
        let next_name = match rex {
            crate::types::ReExport::Wildcard { .. } => name.to_string(),
            crate::types::ReExport::Named { exported_name, original_name, .. } if exported_name == name => original_name.clone(),
            _ => continue,
        };
        let next = resolve_import_path(rex.source(), file, language, ctx);
        if let Some(found) = next.and_then(|n| namespace_declaring_file(&n, &next_name, language, ctx, depth + 1)) {
            return Some(found);
        }
    }
    None
}

fn type_qualifier(r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<String>> {
    let line = ctx.get_file_lines(&r.file_path)?.get((r.line as usize).wrapping_sub(1))?.clone();
    let units: Vec<u16> = line.encode_utf16().collect();
    let name_units: Vec<u16> = r.reference_name.encode_utf16().collect();
    let col = r.column as usize;
    if units.len() < col + name_units.len() || units[col..col + name_units.len()] != name_units[..] {
        return None;
    }
    let is_blank = |u: u16| u == b' ' as u16 || u == b'\t' as u16;
    let is_word = |u: u16| u < 128 && ((u as u8).is_ascii_alphanumeric() || u == b'_' as u16 || u == b'$' as u16);
    let mut names: Vec<String> = Vec::new();
    let mut i = col as isize - 1;
    loop {
        while i >= 0 && is_blank(units[i as usize]) {
            i -= 1;
        }
        if i < 0 || units[i as usize] != b'.' as u16 {
            break;
        }
        i -= 1;
        while i >= 0 && is_blank(units[i as usize]) {
            i -= 1;
        }
        let end = (i + 1) as usize;
        while i >= 0 && is_word(units[i as usize]) {
            i -= 1;
        }
        let name = String::from_utf16_lossy(&units[(i + 1) as usize..end]);
        if !re!(r"^[A-Za-z_$][\w$]*$").is_match(&name) {
            return None;
        }
        names.insert(0, name);
    }
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

fn imported_type_member(imp: &ImportMapping, path: &[String], r: &UnresolvedRef, ctx: &Ctx) -> Tri<NodeRef> {
    let file = resolve_import_path(&imp.source, &r.file_path, r.language, ctx);
    let bare = is_bare_module_specifier(&imp.source, r.language);
    let found: Option<NodeRef>;
    let rest: Vec<String>;
    if imp.is_namespace {
        let w = want(false, true, &path[0], Some(&path[0]));
        let amb = AmbientWant { is_default: false, is_namespace: true, exported_name: &path[0], member_name: Some(&path[0]) };
        found = (if bare { find_ambient_module_export(&imp.source, &amb, r, ctx) } else { None })
            .or_else(|| file.as_deref().and_then(|f| exported(f, &w, r.language, ctx)));
        if found.is_some() && path.len() == 1 {
            return Tri::Value(found.unwrap());
        }
        rest = path[1..].to_vec();
    } else {
        let exported_name = if imp.is_default { "default" } else { imp.exported_name.as_str() };
        let w = want(imp.is_default, false, exported_name, None);
        let amb = AmbientWant { is_default: imp.is_default, is_namespace: false, exported_name, member_name: None };
        found = (if bare { find_ambient_module_export(&imp.source, &amb, r, ctx) } else { None })
            .or_else(|| file.as_deref().and_then(|f| exported(f, &w, r.language, ctx)));
        rest = path.to_vec();
    }
    if let Some(found) = found {
        let member = if is_static_member_container_kind(found.kind) { container_member(&found, &rest, ctx) } else { None };
        let mut tail = vec![found.name.clone()];
        tail.extend(rest.iter().cloned());
        return match member.or_else(|| type_member_in(&found.file_path, &[], &tail, ctx)) {
            Some(m) => Tri::Value(m),
            None => Tri::Null,
        };
    }
    let namespace: Option<&str> = if imp.is_namespace {
        Some(path[0].as_str())
    } else if imp.is_default {
        None
    } else {
        Some(imp.exported_name.as_str())
    };
    let declaring = match (&file, namespace) {
        (Some(f), Some(ns)) => namespace_declaring_file(f, ns, r.language, ctx, 0),
        _ => None,
    };
    if let Some((dfile, dname)) = declaring {
        let mut tail = vec![dname];
        tail.extend(rest.iter().cloned());
        return match type_member_in(&dfile, &[], &tail, ctx) {
            Some(m) => Tri::Value(m),
            None => Tri::Null,
        };
    }
    let outside = file.is_none() && find_ambient_module_file(&imp.source, &r.file_path, ctx).is_none() && ctx.is_out_of_repo_import(&imp.source, &r.file_path, r.language);
    if outside {
        Tri::Null
    } else {
        Tri::Undefined
    }
}

fn own_type_member(head: &str, path: &[String], r: &UnresolvedRef, ctx: &Ctx) -> Tri<NodeRef> {
    let blocks = namespace_blocks(&r.file_path, ctx);
    let mut scopes: Vec<Vec<String>> =
        blocks.iter().filter(|b| in_block(r.line, r.column, *b)).map(|b| b.path.split('.').map(|s| s.to_string()).collect()).collect();
    scopes.sort_by(|a, b| b.len().cmp(&a.len()));
    scopes.push(Vec::new());
    for scope in scopes {
        let mut qualified = scope.clone();
        qualified.push(head.to_string());
        if !has_namespace(&r.file_path, &qualified.join("."), ctx) && declarations_in(&r.file_path, &scope, head, ctx).is_empty() {
            continue;
        }
        let mut full = vec![head.to_string()];
        full.extend(path.iter().cloned());
        return match type_member_in(&r.file_path, &scope, &full, ctx) {
            Some(m) => Tri::Value(m),
            None => Tri::Null,
        };
    }
    Tri::Undefined
}

fn global_type_member(namespace: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let from_test = is_test_file(&r.file_path);
    let mut found: Vec<NodeRef> = ctx
        .get_nodes_by_name(&r.reference_name)
        .iter()
        .filter(|n| {
            if !is_ts_js_file(&n.file_path) || n.qualified_name != n.name || !MEMBER_DECLARATION_KINDS.contains(&n.kind) {
                return false;
            }
            if !from_test && is_test_file(&n.file_path) {
                return false;
            }
            let blocks = namespace_blocks(&n.file_path, ctx);
            innermost_block(n.start_line, n.start_column, &blocks).is_some_and(|b| b.global && b.path == namespace)
        })
        .cloned()
        .collect();
    found.sort_by_key(|n| types_first_key(n));
    found.into_iter().next()
}

/// `Ns.Inner.Type` written as a type reference, resolved through the namespace it names.
pub fn resolve_ts_qualified_type_ref(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if !r.is_kind(EdgeKind::References) || !is_esm_import_language(r.language) || !is_ts_js_file(&r.file_path) {
        return Tri::Undefined;
    }
    let Some(qualifier) = type_qualifier(r, ctx) else { return Tri::Undefined };
    let head = qualifier[0].clone();
    let mut path: Vec<String> = qualifier[1..].to_vec();
    path.push(r.reference_name.clone());
    let imports = ctx.get_import_mappings(&r.file_path, r.language);
    let imp = imports.iter().find(|m| m.local_name == head);
    let target: Tri<NodeRef> = if let Some(imp) = imp {
        imported_type_member(imp, &path, r, ctx)
    } else {
        if ts_global_scope(&r.file_path, ctx).is_some_and(|s| s.import_aliases.contains(&head)) {
            return Tri::Undefined;
        }
        match own_type_member(&head, &path, r, ctx) {
            Tri::Undefined => match global_type_member(&qualifier.join("."), r, ctx) {
                Some(m) => Tri::Value(m),
                None => Tri::Null,
            },
            other => other,
        }
    };
    match target {
        Tri::Undefined => Tri::Undefined,
        Tri::Null => Tri::Null,
        Tri::Value(t) => Tri::Value(ResolvedRef::new(r, t.id.clone(), 0.9, if imp.is_some() { ResolvedBy::Import } else { ResolvedBy::QualifiedName })),
    }
}

// -- Python --------------------------------------------------------------------------------------

fn python_package_binds(imp: &ImportMapping, from_file: &str, ctx: &Ctx) -> bool {
    if imp.is_namespace || imp.exported_name == "*" {
        return false;
    }
    let Some(package_file) = resolve_import_path(&imp.source, from_file, Language::Python, ctx) else { return false };
    if !re!(r"(?:^|/)__init__\.py$").is_match(&package_file) {
        return false;
    }
    exported(&package_file, &want(false, false, &imp.exported_name, None), Language::Python, ctx).is_some()
}

fn resolve_python_module_member(r: &UnresolvedRef, imports: &[ImportMapping], ctx: &Ctx) -> Option<ResolvedRef> {
    let dot_idx = r.reference_name.find('.').filter(|d| *d > 0)?;
    let receiver = &r.reference_name[..dot_idx];
    let member = r.reference_name[dot_idx + 1..].split('.').next().unwrap_or("");
    if member.is_empty() {
        return None;
    }
    if let Some(local) = python_function_module_alias(receiver, r, ctx) {
        let local_path = resolve_import_path(&local, &r.file_path, r.language, ctx);
        let target = match local_path {
            Some(p) if p != r.file_path => exported(&p, &want(false, false, member, None), Language::Python, ctx),
            _ => None,
        };
        return target.map(|t| imported(r, &t.id, 0.85));
    }
    for imp in imports {
        let spelled_out = imp.is_namespace
            && Some(imp.local_name.as_str()) == imp.source.rsplit('.').next()
            && imp.source.contains('.')
            && r.reference_name.starts_with(&format!("{}.", imp.source));
        if imp.local_name != receiver && !spelled_out {
            continue;
        }
        let module_name = if imp.exported_name == "*" { imp.local_name.as_str() } else { imp.exported_name.as_str() };
        let mut module_path = if imp.is_namespace {
            imp.source.clone()
        } else if imp.source.ends_with('.') {
            format!("{}{module_name}", imp.source)
        } else {
            format!("{}.{module_name}", imp.source)
        };
        let from = if spelled_out { imp.source.len() + 1 } else { dot_idx + 1 };
        let mut rest: Vec<String> = r.reference_name[from..].split('.').map(|s| s.to_string()).collect();
        if !imp.is_namespace && python_package_binds(imp, &r.file_path, ctx) {
            continue;
        }
        let Some(mut resolved_path) = resolve_import_path(&module_path, &r.file_path, r.language, ctx) else { continue };
        if resolved_path == r.file_path {
            continue;
        }
        let bound = |file: &str, name: &str| -> Option<NodeRef> { exported(file, &want(false, false, name, None), Language::Python, ctx) };
        if r.is_kind(EdgeKind::Calls) && rest.len() > 1 {
            while rest.len() > 1 {
                let Some(sub) = resolve_import_path(&format!("{module_path}.{}", rest[0]), &r.file_path, r.language, ctx) else { break };
                module_path = format!("{module_path}.{}", rest.remove(0));
                resolved_path = sub;
            }
            let head = bound(&resolved_path, &rest[0]);
            let mut target: Option<NodeRef> = None;
            if rest.len() == 1 {
                target = head.filter(|h| matches!(h.kind, NodeKind::Function | NodeKind::Class));
            } else if rest.len() == 2 {
                if let Some(h) = head.filter(|h| h.kind == NodeKind::Class) {
                    target = ctx
                        .get_nodes_in_file(&h.file_path)
                        .iter()
                        .find(|n| n.kind == NodeKind::Method && n.name == rest[1] && n.qualified_name == format!("{}::{}", h.qualified_name, rest[1]))
                        .cloned();
                }
            }
            if let Some(t) = target {
                return Some(imported(r, &t.id, 0.85));
            }
            continue;
        }
        if let Some(t) = bound(&resolved_path, &rest[0]) {
            return Some(imported(r, &t.id, 0.85));
        }
    }
    None
}

fn resolve_module_import_to_file(r: &UnresolvedRef, imports: &[ImportMapping], ctx: &Ctx) -> Option<ResolvedRef> {
    if !r.is_kind(EdgeKind::Imports) || r.reference_name.contains('.') {
        return None;
    }
    for imp in imports {
        if imp.local_name != r.reference_name {
            continue;
        }
        let module_path: String;
        if imp.is_namespace || imp.is_default {
            module_path = imp.source.clone();
        } else if r.language == Language::Python {
            if python_package_binds(imp, &r.file_path, ctx) {
                continue;
            }
            let module_name = if imp.exported_name == "*" { imp.local_name.as_str() } else { imp.exported_name.as_str() };
            module_path = if imp.source.ends_with('.') { format!("{}{module_name}", imp.source) } else { format!("{}.{module_name}", imp.source) };
        } else {
            continue;
        }
        if is_bare_module_specifier(&module_path, r.language) {
            if let Some(declaring) = find_ambient_module_file(&module_path, &r.file_path, ctx) {
                return Some(imported(r, &declaring.id, 0.9));
            }
        }
        if let Some(resolved) = resolve_import_path(&module_path, &r.file_path, r.language, ctx).filter(|p| *p != r.file_path) {
            if let Some(file_node) = file_node_of(&resolved, ctx) {
                return Some(imported(r, &file_node.id, 0.9));
            }
        }
    }
    None
}

fn resolve_python_absolute_module(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !r.is_kind(EdgeKind::Imports) || !r.reference_name.contains('.') {
        return None;
    }
    python_module_file_node(&r.reference_name, &r.file_path, ctx).map(|hit| imported(r, &hit.id, 0.9))
}

// -- Lua -----------------------------------------------------------------------------------------

fn resolve_lua_require(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let name = &r.reference_name;
    if name.is_empty() {
        return None;
    }
    let base = if name.contains('.') { name.replace('.', "/") } else { name.clone() };
    let suffixes = [format!("{base}.lua"), format!("{base}.luau"), format!("{base}/init.lua"), format!("{base}/init.luau")];
    let by_basename = lua_basename_index(ctx);
    let shared = |a: &str, b: &str| a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    for suffix in &suffixes {
        let key = suffix.rsplit('/').next().unwrap_or("");
        let candidates = by_basename.get(key).cloned().unwrap_or_default();
        let mut matches: Vec<String> = candidates.into_iter().filter(|f| f == suffix || f.ends_with(&format!("/{suffix}"))).collect();
        if matches.is_empty() {
            continue;
        }
        matches.sort_by(|x, y| shared(y, &r.file_path).cmp(&shared(x, &r.file_path)));
        let best = &matches[0];
        if *best == r.file_path {
            continue;
        }
        if let Some(file_node) = file_node_of(best, ctx) {
            return Some(imported(r, &file_node.id, 0.9));
        }
    }
    None
}

// -- Object literals -----------------------------------------------------------------------------

fn resolve_object_literal_alias(container: &Node, member: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !matches!(container.kind, NodeKind::Constant | NodeKind::Variable) || !super::exports::is_js_family_path(&container.file_path) {
        return None;
    }
    resolve_object_literal_binding(container, member, r, ctx).map(|mut resolved| {
        resolved.confidence = 0.9;
        resolved.resolved_by = ResolvedBy::Import;
        resolved
    })
}

fn resolve_imported_instance_member(value: &Node, r: &UnresolvedRef, local_name: &str, ctx: &Ctx) -> Option<ResolvedRef> {
    if !r.is_kind(EdgeKind::Calls) || !matches!(value.kind, NodeKind::Constant | NodeKind::Variable) {
        return None;
    }
    let member = &r.reference_name[local_name.len() + 1..];
    if member.is_empty() || member.contains('.') {
        return None;
    }
    let source = ctx.read_file(&value.file_path)?;
    if source.is_empty() {
        return None;
    }
    let lines: Vec<&str> = source.split('\n').collect();
    let from = (value.start_line as usize).saturating_sub(1).min(lines.len());
    let to = (value.end_line as usize).min(lines.len());
    let decl_slice = if from < to { lines[from..to].join("\n") } else { String::new() };
    let receiver = codegraph_jsre::escape_regex(&value.name);
    for pattern in local_receiver_type_patterns(value.language, &receiver).iter() {
        let Some(m) = pattern.captures(&decl_slice) else { continue };
        let Some(raw) = m.text(1).filter(|s| !s.is_empty()) else { continue };
        let Some(type_name) = normalize_inferred_type_name(raw) else { continue };
        let declaring = if is_esm_import_language(value.language) {
            let mut at = r.clone();
            at.file_path = value.file_path.clone();
            at.language = value.language;
            Some(ts_imported_type_file(raw, &at, ctx).unwrap_or_else(|| value.file_path.clone()))
        } else {
            None
        };
        if let Some(resolved) = resolve_method_on_type(&type_name, member, r, ctx, 0.85, ResolvedBy::InstanceMethod, declaring.as_deref(), 0) {
            return Some(resolved);
        }
    }
    None
}

// -- Rust ------------------------------------------------------------------------------------

fn rust_crate_root_dir(from_file: &str, ctx: &Ctx) -> Option<String> {
    let mut dir = posix::dirname(from_file);
    if dir == "." {
        dir = String::new();
    }
    for _ in 0..64 {
        let lib = posix::join(&[&dir, "lib.rs"]);
        let main = posix::join(&[&dir, "main.rs"]);
        if ctx.file_exists(&lib) || ctx.file_exists(&main) {
            return Some(dir);
        }
        if dir.is_empty() {
            return None;
        }
        let parent = posix::dirname(&dir);
        dir = if parent == "." { String::new() } else { parent };
    }
    None
}

fn rust_self_module_dir(from_file: &str) -> String {
    let base = posix::basename(from_file);
    let dir = posix::dirname(from_file);
    let dir = if dir == "." { String::new() } else { dir };
    if base == "mod.rs" || base == "lib.rs" || base == "main.rs" {
        return dir;
    }
    posix::join(&[&dir, base.strip_suffix(".rs").unwrap_or(&base)])
}

fn resolve_rust_module_file(segments: &[String], from_file: &str, ctx: &Ctx) -> Option<String> {
    if segments.is_empty() {
        return None;
    }
    let resolve_under = |start_dir: Option<String>, rest: &[String]| -> Option<String> {
        let mut dir = start_dir?;
        let mut target_file: Option<String> = None;
        for seg in rest {
            if seg == "self" || seg == "crate" || seg == "super" {
                continue;
            }
            let as_file = posix::join(&[&dir, &format!("{seg}.rs")]);
            let as_mod = posix::join(&[&dir, seg, "mod.rs"]);
            if ctx.file_exists(&as_file) {
                target_file = Some(as_file);
            } else if ctx.file_exists(&as_mod) {
                target_file = Some(as_mod);
            } else {
                return None;
            }
            dir = posix::join(&[&dir, seg]);
        }
        target_file
    };
    let first = segments[0].as_str();
    if first == "crate" {
        return resolve_under(rust_crate_root_dir(from_file, ctx), &segments[1..]);
    }
    if first == "self" {
        return resolve_under(Some(rust_self_module_dir(from_file)), &segments[1..]);
    }
    if first == "super" {
        let supers = segments.iter().take_while(|s| *s == "super").count();
        let mut dir: Option<String> = Some(rust_self_module_dir(from_file));
        for _ in 0..supers {
            dir = match dir {
                Some(d) if !d.is_empty() => {
                    let p = posix::dirname(&d);
                    Some(if p == "." { String::new() } else { p })
                }
                _ => None,
            };
            if dir.is_none() {
                break;
            }
        }
        return resolve_under(dir, &segments[supers..]);
    }
    resolve_under(Some(rust_self_module_dir(from_file)), segments).or_else(|| resolve_under(rust_crate_root_dir(from_file, ctx), segments))
}

fn resolve_rust_path_reference(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let segments: Vec<String> = r.reference_name.split("::").filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
    if segments.len() < 2 {
        return None;
    }
    let leaf = &segments[segments.len() - 1];
    let mod_segs = &segments[..segments.len() - 1];
    let file = resolve_rust_module_file(mod_segs, &r.file_path, ctx).filter(|f| *f != r.file_path)?;
    let in_file = ctx.get_nodes_in_file(&file);
    let target = in_file.iter().find(|n| {
        &n.name == leaf
            && !is_rust_associated_item(n)
            && matches!(
                n.kind,
                NodeKind::Function
                    | NodeKind::Struct
                    | NodeKind::Union
                    | NodeKind::Enum
                    | NodeKind::Trait
                    | NodeKind::TypeAlias
                    | NodeKind::Constant
                    | NodeKind::Method
                    | NodeKind::Class
                    | NodeKind::Interface
            )
    })?;
    Some(imported(r, &target.id, 0.9))
}

// -- Go --------------------------------------------------------------------------------------

fn find_go_package_member(member_name: &str, imp: &ImportMapping, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let pkg_dir = ctx.get_go_package_dir(&imp.source, Some(&r.file_path))?;
    for node in ctx.get_nodes_by_name(member_name).iter() {
        if node.language != Language::Go || node.is_exported != Some(true) {
            continue;
        }
        if posix::dirname(&node.file_path.replace('\\', "/")) == pkg_dir {
            return Some(imported(r, &node.id, 0.9));
        }
    }
    None
}

fn resolve_go_cross_package_reference(r: &UnresolvedRef, imports: &[ImportMapping], ctx: &Ctx) -> Option<ResolvedRef> {
    let Some(dot_idx) = r.reference_name.find('.').filter(|d| *d > 0) else {
        let imp = go_ref_qualifier(r, ctx)?;
        return find_go_package_member(&r.reference_name, &imp, r, ctx);
    };
    let receiver = &r.reference_name[..dot_idx];
    let member_name = &r.reference_name[dot_idx + 1..];
    if member_name.is_empty() {
        return None;
    }
    go_ref_qualifier(r, ctx)?;
    for imp in imports {
        if imp.local_name != receiver {
            continue;
        }
        if let Some(found) = find_go_package_member(member_name, imp, r, ctx) {
            return Some(found);
        }
    }
    None
}

// -- Reachability of ambient modules ---------------------------------------------------------

/// Can a reference written in its file reach the ambient module `specifier` declared in
/// `declaring_file`?
pub fn reaches_ambient_module(r: &UnresolvedRef, specifier: &str, declaring_file: &str, ctx: &Ctx) -> bool {
    if is_in_module_block(&r.file_path, r.line, r.column, specifier, ctx) {
        return true;
    }
    let mappings = ctx.get_import_mappings(&r.file_path, r.language);
    let relative = |source: &str| source.starts_with('.') || source.starts_with('/');
    let leads_to = |source: &str, through: &dyn Fn(&str) -> bool| -> bool {
        if source == specifier {
            return true;
        }
        if !relative(source) {
            return false;
        }
        match resolve_import_path(source, &r.file_path, r.language, ctx) {
            Some(file) => file == declaring_file || through(&file),
            None => false,
        }
    };
    let head = r.reference_name.split('.').next().unwrap_or("");
    if let Some(binding) = mappings.iter().find(|m| m.local_name == head) {
        return leads_to(&binding.source, &|file| ctx.get_import_mappings(file, r.language).iter().any(|m| m.source == specifier));
    }
    let last_name_kind = matches!(
        r.reference_kind,
        codegraph_types::ReferenceKind::Edge(
            EdgeKind::References | EdgeKind::Extends | EdgeKind::Implements | EdgeKind::TypeOf | EdgeKind::Returns | EdgeKind::Instantiates
        )
    );
    last_name_kind && mappings.iter().any(|m| leads_to(&m.source, &|_| false))
}

// -- The resolver --------------------------------------------------------------------------------

/// Resolve a reference through the file's imports.
pub fn resolve_via_import(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if let Some(module_file) = js_module_file(r, ctx) {
        if let Some(file_node) = file_node_of(&module_file, ctx) {
            return Some(imported(r, &file_node.id, 0.9));
        }
    }
    if is_dart_import_ref(r) {
        let file = dart_directive_file(&r.file_path, &r.reference_name, ctx);
        let file_node = file.filter(|f| *f != r.file_path).and_then(|f| file_node_of(&f, ctx));
        return file_node.map(|n| imported(r, &n.id, 0.9));
    }
    if matches!(r.language, Language::C | Language::Cpp) && r.is_kind(EdgeKind::Imports) {
        let slash = r.file_path.rfind('/');
        let from_dir = slash.map_or("", |s| &r.file_path[..s]);
        let sibling_path = posix::normalize(&if from_dir.is_empty() { r.reference_name.clone() } else { format!("{from_dir}/{}", r.reference_name) });
        let sibling_base = sibling_path.rsplit('/').next().unwrap_or("").to_string();
        if let Some(sibling) = ctx.get_nodes_by_name(&sibling_base).iter().find(|n| n.kind == NodeKind::File && n.file_path == sibling_path) {
            return Some(imported(r, &sibling.id, 0.92));
        }
        let resolved_path = resolve_import_path(&r.reference_name, &r.file_path, r.language, ctx)?;
        return file_node_by_path(&resolved_path, ctx).map(|n| imported(r, &n.id, 0.9));
    }
    if is_cobol_copybook_ref(r) {
        let resolved_path = resolve_import_path(&r.reference_name, &r.file_path, r.language, ctx)?;
        return file_node_by_path(&resolved_path, ctx).map(|n| imported(r, &n.id, 0.9));
    }
    if is_php_include_path_ref(r) {
        if let Some(resolved_path) = resolve_php_include_path(&r.reference_name, &r.file_path, ctx) {
            if let Some(n) = file_node_by_path(&resolved_path, ctx) {
                return Some(imported(r, &n.id, 0.9));
            }
        }
        return None;
    }
    if is_nix_path_import_ref(r) {
        let resolved_path = resolve_import_path(&r.reference_name, &r.file_path, r.language, ctx)?;
        return file_node_by_path(&resolved_path, ctx).map(|n| imported(r, &n.id, 0.9));
    }
    let imports = ctx.get_import_mappings(&r.file_path, r.language);
    if imports.is_empty() && ctx.read_file(&r.file_path).is_none() {
        return None;
    }
    if r.language == Language::Go {
        if let Some(go) = resolve_go_cross_package_reference(r, &imports, ctx) {
            return Some(go);
        }
    }
    if matches!(r.language, Language::Java | Language::Kotlin) {
        if let Some(java) = resolve_java_imported_reference(r, &imports, ctx) {
            return Some(java);
        }
    }
    if r.language == Language::Python {
        if let Some(py) = resolve_python_module_member(r, &imports, ctx) {
            return Some(py);
        }
        if let Some(py) = resolve_python_absolute_module(r, ctx) {
            return Some(py);
        }
    }
    if r.language == Language::Rust && r.reference_name.contains("::") {
        if let Some(rs) = resolve_rust_path_reference(r, ctx) {
            return Some(rs);
        }
    }
    if matches!(r.language, Language::Lua | Language::Luau) && r.is_kind(EdgeKind::Imports) {
        if let Some(lua) = resolve_lua_require(r, ctx) {
            return Some(lua);
        }
    }
    if matches!(r.language, Language::Python | Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx | Language::ArkTs) {
        if let Some(module_file) = resolve_module_import_to_file(r, &imports, ctx) {
            return Some(module_file);
        }
    }
    if r.language == Language::Kotlin && !r.is_kind(EdgeKind::Imports) {
        if let Some(aliased) = resolve_jvm_alias(r, &imports, ctx) {
            return Some(aliased);
        }
    }
    for imp in imports.iter() {
        if imp.local_name == r.reference_name || r.reference_name.starts_with(&format!("{}.", imp.local_name)) {
            let resolved_path = resolve_import_path(&imp.source, &r.file_path, r.language, ctx);
            let bare = is_bare_module_specifier(&imp.source, r.language);
            if resolved_path.is_some() || bare {
                let exported_name = if imp.is_default { "default" } else { imp.exported_name.as_str() };
                let member_name = if imp.is_namespace { Some(r.reference_name.replacen(&format!("{}.", imp.local_name), "", 1)) } else { None };
                let w = want(imp.is_default, imp.is_namespace, exported_name, member_name.as_deref());
                let amb = AmbientWant { is_default: imp.is_default, is_namespace: imp.is_namespace, exported_name, member_name: member_name.as_deref() };
                let target = (if bare { find_ambient_module_export(&imp.source, &amb, r, ctx) } else { None })
                    .or_else(|| resolved_path.as_deref().and_then(|p| exported(p, &w, r.language, ctx)));
                if let Some(target) = target {
                    if !imp.is_namespace && r.reference_name.starts_with(&format!("{}.", imp.local_name)) {
                        if let Some(member) = resolve_static_member(&target, r, &imp.local_name, ctx) {
                            return Some(imported(r, &member.id, 0.9));
                        }
                        if matches!(target.kind, NodeKind::Constant | NodeKind::Variable) {
                            let member = r.reference_name[imp.local_name.len() + 1..].split('.').next().unwrap_or("");
                            if !member.is_empty() {
                                if let Some(literal) = resolve_object_literal_member(&target, member, r, ctx, 0.9, ResolvedBy::Import) {
                                    return Some(literal);
                                }
                                if let Some(alias) = resolve_object_literal_alias(&target, member, r, ctx) {
                                    return Some(alias);
                                }
                            }
                        }
                        if let Some(instance) = resolve_imported_instance_member(&target, r, &imp.local_name, ctx) {
                            return Some(instance);
                        }
                        if r.language == Language::Python {
                            return None;
                        }
                    }
                    return Some(imported(r, &target.id, 0.9));
                }
            }
        }
    }
    if r.is_kind(EdgeKind::Imports) && is_esm_import_language(r.language) {
        let source = re_export_source(&r.reference_name, r, ctx);
        let resolved_path = source.and_then(|s| resolve_import_path(&s, &r.file_path, r.language, ctx));
        let target = resolved_path.and_then(|p| exported(&p, &want(false, false, &r.reference_name, None), r.language, ctx));
        if let Some(target) = target {
            return Some(imported(r, &target.id, 0.9));
        }
    }
    None
}

/// `context.resolveImport`: the import resolution a reference's own edge gets, falling back to
/// the file-path matcher for a C/C++ `#include` the import resolver cannot place — linked by
/// the end of its path, as name matching does. A standard header is never the project's.
pub fn resolve_import(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    resolve_via_import(r, ctx).or_else(|| {
        if matches!(r.language, Language::C | Language::Cpp)
            && r.is_kind(EdgeKind::Imports)
            && !is_external_import(&r.reference_name, r.language, ctx, ExternalOpts::default())
        {
            match_by_file_path(r, ctx)
        } else {
            None
        }
    })
}

// -- Out-of-repo bindings and binding tails ----------------------------------------------------

/// Does the file bind this reference's name to an import from outside the repo?
pub fn is_bound_to_out_of_repo_import(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let name = &r.reference_name;
    if name.contains("::") || name.contains('.') {
        return false;
    }
    if r.language == Language::Rust {
        let Some(content) = ctx.read_file(&r.file_path).filter(|c| !c.is_empty()) else { return false };
        let bindings = collect_rust_use_bindings(&content);
        let Some(use_path) = bindings.get(name.as_str()).filter(|p| !p.is_empty()) else { return false };
        let segments: Vec<String> = use_path.split("::").map(|s| s.to_string()).collect();
        if segments.len() < 2 || !is_rust_stdlib_root(&segments[0]) {
            return false;
        }
        return resolve_rust_module_file(&segments[..segments.len() - 1], &r.file_path, ctx).is_none();
    }
    if !is_esm_import_language(r.language) {
        return false;
    }
    for imp in ctx.get_import_mappings(&r.file_path, r.language).iter() {
        if &imp.local_name != name {
            continue;
        }
        return is_external_import(&imp.source, r.language, ctx, ExternalOpts::default())
            && resolve_import_path(&imp.source, &r.file_path, r.language, ctx).is_none()
            && !ambient_module_declares(&imp.source, &imp.exported_name, imp.is_default, r, ctx);
    }
    false
}

fn is_identifier(s: &str) -> bool {
    re!(r"^[A-Za-z_$][\w$]*$").is_match(s)
}

crate::name_matcher::util::memo_map!(ProjectFolderSuffixMemo, String, std::rc::Rc<HashSet<String>>);

fn project_folder_suffixes(ctx: &Ctx) -> std::rc::Rc<HashSet<String>> {
    crate::name_matcher::util::memoized::<ProjectFolderSuffixMemo>(ctx, String::new(), || {
        let mut folders: HashSet<String> = HashSet::new();
        for file in ctx.get_all_files() {
            let mut i = file.rfind('/');
            while let Some(at) = i.filter(|a| *a > 0) {
                let folder = &file[..at];
                if folders.contains(folder) {
                    break;
                }
                folders.insert(folder.to_string());
                i = file[..at].rfind('/');
            }
        }
        let mut suffixes: HashSet<String> = HashSet::new();
        for folder in &folders {
            let mut start = 0usize;
            loop {
                suffixes.insert(folder[start..].to_string());
                match folder[start..].find('/') {
                    Some(p) => start += p + 1,
                    None => break,
                }
            }
        }
        std::rc::Rc::new(suffixes)
    })
}

fn is_project_python_module(source: &str, ctx: &Ctx) -> bool {
    if source.starts_with('.') {
        return true;
    }
    let folders = project_folder_suffixes(ctx);
    let module_path = source.replace('.', "/");
    let slash = module_path.rfind('/');
    folders.contains(&module_path) || slash.is_some_and(|s| s > 0 && folders.contains(&module_path[..s]))
}

/// A Python import source as a project-relative module path (`.sibling` → `dir/sibling`).
pub fn python_module_path(source: &str, from_file: &str) -> Option<String> {
    let name = source.trim_start_matches('.');
    let dots = source.len() - name.len();
    if dots == 0 {
        return Some(name.replace('.', "/"));
    }
    let up = "../".repeat(dots - 1);
    let module_path = posix::join(&[&posix::dirname(from_file), &up, &name.replace('.', "/")]);
    if module_path == ".." || module_path.starts_with("../") {
        None
    } else {
        Some(module_path)
    }
}

fn python_import_binding_tail(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if !is_identifier(&r.reference_name) {
        return None;
    }
    let imports = ctx.get_import_mappings(&r.file_path, Language::Python);
    let binding = imports.iter().find(|m| m.local_name == r.reference_name)?;
    if is_external_import(&binding.source, Language::Python, ctx, ExternalOpts::default()) {
        return None;
    }
    let declared = if binding.is_namespace { binding.source.rsplit('.').next().unwrap_or("").to_string() } else { binding.exported_name.clone() };
    if declared == binding.local_name && (binding.is_namespace || !is_project_python_module(&binding.source, ctx)) {
        return None;
    }
    let module_path = python_module_path(&binding.source, &r.file_path)?;
    let tail = module_tail(&module_path);
    if tail.is_empty() {
        None
    } else {
        Some(tail)
    }
}

fn local_type_binding(receiver: &str, r: &UnresolvedRef, ctx: &Ctx, mappings: &[ImportMapping]) -> Option<ImportMapping> {
    if mappings.is_empty() {
        return None;
    }
    let mut decl = GoReceiverDecl::default();
    let ty = infer_local_receiver_type(receiver, r, ctx, Some(&mut decl))?;
    let raw = decl.raw.filter(|s| !s.is_empty())?;
    if array_element_type(&ty).is_some()
        && crate::js_builtins::JS_ARRAY_METHODS.contains(r.reference_name.get(receiver.len() + 1..).unwrap_or(""))
    {
        return None;
    }
    let head = re!(r"[.<\[\s]").split(&raw).into_iter().next().unwrap_or("").to_string();
    mappings.iter().find(|m| m.local_name == head).cloned()
}

/// The tail a reference that failed to resolve is parked under, when not the one its name gives.
pub fn import_binding_tail(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if r.language == Language::Python {
        return python_import_binding_tail(r, ctx);
    }
    if !is_esm_import_language(r.language) {
        return None;
    }
    let receiver: Option<String> = if r.is_kind(EdgeKind::Imports) {
        None
    } else {
        re!(r"^([A-Za-z_$][\w$]*)\.[A-Za-z_$][\w$]*$").captures(&r.reference_name).and_then(|c| c.text(1).map(|s| s.to_string()))
    };
    let name = receiver.clone().unwrap_or_else(|| r.reference_name.clone());
    if receiver.is_none() && !is_identifier(&name) {
        return None;
    }
    let mappings = ctx.get_import_mappings(&r.file_path, r.language);
    let binding = mappings.iter().find(|m| m.local_name == name).cloned().or_else(|| match &receiver {
        Some(rc) if r.is_kind(EdgeKind::Calls) => local_type_binding(rc, r, ctx, &mappings),
        _ => None,
    })?;
    if is_external_import(&binding.source, r.language, ctx, ExternalOpts::default())
        && !ctx.get_nearest_aliases(&r.file_path).is_some_and(|a| a.patterns.iter().any(|p| !p.prefix.is_empty() && binding.source.starts_with(&p.prefix)))
        && !names_own_config_module(&binding.source, &r.file_path, r.language, ctx)
    {
        return None;
    }
    let module_path = if binding.source.starts_with('.') { posix::join(&[&posix::dirname(&r.file_path), &binding.source]) } else { binding.source.clone() };
    let tail = module_tail(&module_path);
    if tail.is_empty() {
        return None;
    }
    if receiver.is_none() && !binding.is_default && !binding.is_namespace && binding.exported_name == binding.local_name {
        let module_file = resolve_import_path(&binding.source, &r.file_path, r.language, ctx)?;
        if !ctx.is_indexed_file(&module_file) || !module_reference_keys(&module_file).contains(&tail) {
            return None;
        }
        if exported(&module_file, &want(false, false, &binding.exported_name, None), r.language, ctx).is_some() {
            return None;
        }
    }
    Some(tail)
}

/// The module's default export.
pub fn module_default_export(file_path: &str, language: Language, ctx: &Ctx) -> Option<NodeRef> {
    exported(file_path, &want(true, false, "default", None), language, ctx)
}

pub fn is_js_family_file(file_path: &str) -> bool {
    super::exports::is_js_family_path(file_path)
}

pub fn is_module_binding_name(name: &str) -> bool {
    is_identifier(name)
}
