//! `matchReference`: the strategy order for one unresolved reference, and the post-filters
//! every result passes through.

use super::call_chain::{match_dotted_call_chain, match_scoped_call_chain};
use super::calls::is_collapsed_non_recursion;
use super::cpp_text::{cpp_paren_list_after, split_cpp_top_level};
use super::exact::{match_by_exact_name, match_by_file_path, match_by_qualified_name, match_fuzzy, prefer_call_site_file};
use super::function_ref::match_function_ref;
use super::gate::gate_language_match;
use super::js_scope::is_outside_js_local;
use super::js_store::{is_unresolved_js_member_call, match_store_accessor_chain};
use super::lua::{lua_alias_target, LUA_GLOBAL_FUNCTIONS};
use super::method::match_method_call;
use super::object_literal::{match_collapsed_object_call, match_object_path_call};
use super::python_attr::match_python_factory_chain;
use super::pending::*;
use super::python_scope::is_python_name_imported_from_outside;
use super::rust::match_rust_as_mut_chain_call;
use super::same_language_family;
use super::util::{find_best_match, compute_path_proximity, lines_of, memo_map, memoized};
use crate::ctx::Ctx;
use crate::swift_type_visibility::{is_swift_type_path_call, resolve_swift_type_path_call};
use crate::types::{is_inheritance_ref, is_supertype_target, NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use std::collections::HashSet;
use std::rc::Rc;

const R_BASE_FUNCTIONS: &[&str] = &[
    "c", "t", "q", "df", "dt", "data", "list", "length", "names", "max", "min", "sum", "mean", "range", "rev", "sort", "order", "rep", "seq", "cat", "print",
    "paste", "paste0", "format", "levels", "factor", "matrix", "vector", "table", "scale", "sample", "exp", "log", "abs", "all", "any", "which", "nchar",
    "summary", "file", "dir", "identity", "unique", "nrow", "ncol", "rownames", "colnames", "array", "character", "numeric", "integer", "logical", "mode",
    "class", "body", "args", "environment", "search", "diff", "round", "sign", "trunc", "var", "sd", "median", "quantile", "weights",
];

fn is_js_local_ref_kind(k: ReferenceKind) -> bool {
    matches!(
        k,
        ReferenceKind::Edge(EdgeKind::Calls) | ReferenceKind::Edge(EdgeKind::References) | ReferenceKind::FunctionRef | ReferenceKind::Edge(EdgeKind::Instantiates)
    )
}

fn is_value_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Variable | NodeKind::Constant | NodeKind::Field | NodeKind::Property)
}

fn is_r_function_value(n: &Node, ctx: &Ctx) -> bool {
    let lines = lines_of(ctx, &n.file_path);
    let line = lines.get((n.start_line as usize).wrapping_sub(1)).map(String::as_str).unwrap_or("");
    re!(r"(?:<<?-|=)\s*(?:function\b|\\\s*\()").is_match(line)
}

pub fn match_reference(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let result = gate_language_match(match_reference_inner(r, ctx), r, ctx);
    if let Some(res) = &result {
        if res.target_node_id == r.from_node_id && is_collapsed_non_recursion(r, ctx) {
            return None;
        }
        if is_js_local_ref_kind(r.reference_kind) {
            if let Some(target) = ctx.node_by_id(&res.target_node_id) {
                if target.name == r.reference_name && is_outside_js_local(&target, r, ctx) {
                    return None;
                }
            }
        }
        if r.language == Language::Cpp && r.is_kind(EdgeKind::Calls) {
            let target = ctx.node_by_id(&res.target_node_id);
            let caller = ctx.node_by_id(&r.from_node_id);
            let own = match (&target, &caller) {
                (Some(t), Some(c)) => t.file_path == c.file_path && t.start_line >= c.start_line && t.start_line <= c.end_line.max(c.start_line),
                _ => false,
            };
            if !own && is_cpp_local_callable_call(r, ctx) {
                return None;
            }
        }
        if r.language == Language::R && r.is_kind(EdgeKind::Calls) && R_BASE_FUNCTIONS.contains(&r.reference_name.as_str()) {
            if let Some(target) = ctx.node_by_id(&res.target_node_id) {
                if matches!(target.kind, NodeKind::Variable | NodeKind::Constant) && !is_r_function_value(&target, ctx) {
                    return None;
                }
            }
        }
        if r.language == Language::C && r.is_kind(EdgeKind::Calls) && ctx.node_by_id(&res.target_node_id).is_some_and(|t| t.kind == NodeKind::Method) {
            return None;
        }
        if res.target_node_id == r.from_node_id && is_inheritance_ref(r) {
            return other_supertype_named(r, ctx);
        }
        if res.target_node_id == r.from_node_id
            && r.is_kind(EdgeKind::Calls)
            && ctx.node_by_id(&r.from_node_id).is_some_and(|n| is_value_kind(n.kind))
        {
            return None;
        }
    }
    result.map(|res| retarget_self_overload(res, r, ctx))
}

/// A supertype reference that resolved to its own declaration: the *other* type of that name
/// (`class Foo : Foo` in another namespace), when exactly one is in reach.
fn other_supertype_named(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let name = re!(r"::|\.").split(&r.reference_name).into_iter().last().unwrap_or("").to_string();
    let lines = lines_of(ctx, &r.file_path);
    let line = lines.get((r.line as usize).wrapping_sub(1)).map(String::as_str).unwrap_or("");
    let rest = super::util::after_col(line, r.column);
    let written = Re::cached(&format!(r"^\s*((?:[\w$]+\.)+){}\b", escape_regex(&name)), "").captures(rest);
    let qualifier = written.and_then(|w| w.text(1).map(|s| s[..s.len() - 1].to_string())).unwrap_or_default();
    let mut pool: Vec<NodeRef> = ctx
        .get_nodes_by_name(&name)
        .iter()
        .filter(|n| {
            n.id != r.from_node_id
                && is_supertype_target(n)
                && same_language_family(n.language, r.language)
                && (qualifier.is_empty() || {
                    let path = owner_path_of(n, ctx);
                    path == qualifier || path.ends_with(&format!(".{qualifier}"))
                })
        })
        .cloned()
        .collect();
    if pool.len() > 1 {
        let same_file: Vec<NodeRef> = pool.iter().filter(|n| n.file_path == r.file_path).cloned().collect();
        if !same_file.is_empty() {
            pool = same_file;
        }
    }
    if pool.len() > 1 && r.is_kind(EdgeKind::Implements) {
        let conformable: Vec<NodeRef> = pool.iter().filter(|n| matches!(n.kind, NodeKind::Protocol | NodeKind::Interface | NodeKind::Trait)).cloned().collect();
        if !conformable.is_empty() {
            pool = conformable;
        }
    }
    if pool.len() == 1 {
        Some(ResolvedRef::new(r, pool[0].id.clone(), 0.8, ResolvedBy::QualifiedName))
    } else {
        None
    }
}

fn owner_path_of(n: &Node, ctx: &Ctx) -> String {
    let text = ctx.read_file(&n.file_path).unwrap_or_default();
    let pkg: Vec<String> = re!(r"^\s*package\s+([\w.]+)\s*;?\s*$", "gm")
        .captures_iter(&text)
        .into_iter()
        .filter_map(|m| m.text(1).map(|s| s.to_string()))
        .collect();
    let pkg = pkg.join(".");
    let owners = match n.qualified_name.rfind("::").filter(|c| *c > 0) {
        Some(cut) => n.qualified_name[..cut].replace("::", "."),
        None => String::new(),
    };
    [pkg.clone(), owners.clone()]
        .into_iter()
        .filter(|p| !p.is_empty() && !(!pkg.is_empty() && *p == owners && owners.starts_with(&pkg)))
        .collect::<Vec<_>>()
        .join(".")
}

// -- Overload retargeting ----------------------------------------------------------------------

fn overloads(l: Language) -> bool {
    matches!(
        l,
        Language::CSharp | Language::Java | Language::Kotlin | Language::Swift | Language::Cpp | Language::Scala | Language::Dart | Language::VbNet | Language::Solidity
    )
}

#[derive(Clone)]
pub struct Arity {
    min: usize,
    max: Option<usize>,
}

impl Arity {
    fn fits(&self, argc: usize) -> bool {
        argc >= self.min && self.max.map_or(true, |m| argc <= m)
    }
}

pub struct OverloadSet {
    name: String,
    own: Option<Arity>,
    siblings: Vec<(String, Option<Arity>)>,
}

memo_map!(OverloadSetMemo, String, Option<Rc<OverloadSet>>);

fn is_parameter_pack(p: &str) -> bool {
    let flat = re!(r"<[^<>]*>", "g").replace_all_str(p, "");
    re!(r"\.\.\.|\bparams\s|\bvararg\s").is_match(&flat)
}

fn overload_set_of(id: &str, ctx: &Ctx) -> Option<Rc<OverloadSet>> {
    let this = ctx.node_by_id(id)?;
    if !matches!(this.kind, NodeKind::Method | NodeKind::Function) {
        return None;
    }
    let name = this.name.clone();
    let owner_of = |qn: &str| -> String { qn[..qn.rfind("::").unwrap_or(0)].to_string() };
    let owner = owner_of(&this.qualified_name);
    let siblings: Vec<NodeRef> = ctx
        .get_nodes_in_file_named(&this.file_path, &name)
        .into_iter()
        .filter(|n| n.id != this.id && matches!(n.kind, NodeKind::Method | NodeKind::Function) && owner_of(&n.qualified_name) == owner)
        .collect();
    if siblings.is_empty() {
        return None;
    }
    let arity = |n: &Node| -> Option<Arity> {
        let list = cpp_paren_list_after(&n.file_path, n.start_line, 0, &name, ctx)?;
        let params: Vec<String> = split_cpp_top_level(&list).into_iter().filter(|p| !p.is_empty() && p != "void").collect();
        let min = params.iter().filter(|p| !p.contains('=') && !is_parameter_pack(p)).count();
        let max = if params.iter().any(|p| is_parameter_pack(p)) { None } else { Some(params.len()) };
        Some(Arity { min, max })
    };
    Some(Rc::new(OverloadSet { own: arity(&this), siblings: siblings.iter().map(|n| (n.id.clone(), arity(n))).collect(), name }))
}

fn swift_labels_fit(labels: &[String], param_list: &str) -> bool {
    let params: Vec<(String, bool)> = if param_list.trim().is_empty() {
        Vec::new()
    } else {
        split_cpp_top_level(param_list)
            .iter()
            .map(|p| {
                let head = re!(r"^\s*(?:@\w+(?:\([^)]*\))?\s+)*(?:inout\s+)?([A-Za-z_]\w*)(?:\s+([A-Za-z_]\w*))?\s*:").captures(p);
                let label = head.and_then(|h| h.text(1).map(|s| s.to_string())).unwrap_or_else(|| "_".to_string());
                (label, p.contains('=') || p.contains("..."))
            })
            .collect()
    };
    let mut i = 0usize;
    for (label, optional) in &params {
        if i < labels.len() && &labels[i] == label {
            i += 1;
            continue;
        }
        if !optional {
            return false;
        }
    }
    i == labels.len()
}

/// A call that resolved to one overload may belong to a sibling that takes its argument count
/// (or, in Swift, its argument labels).
fn retarget_self_overload(result: ResolvedRef, r: &UnresolvedRef, ctx: &Ctx) -> ResolvedRef {
    if !r.is_kind(EdgeKind::Calls) || !overloads(r.language) {
        return result;
    }
    if r.language == Language::Cpp && result.target_node_id != r.from_node_id {
        return result;
    }
    let set = memoized::<OverloadSetMemo>(ctx, result.target_node_id.clone(), || overload_set_of(&result.target_node_id, ctx));
    let Some(set) = set else { return result };
    let Some(own) = &set.own else { return result };
    let Some(args) = cpp_paren_list_after(&r.file_path, r.line, r.column, &set.name, ctx) else { return result };
    if r.language == Language::Swift {
        let labels: Vec<String> = if args.trim().is_empty() {
            Vec::new()
        } else {
            split_cpp_top_level(&args)
                .iter()
                .map(|a| re!(r"^\s*([A-Za-z_]\w*)\s*:(?!:)").captures(a).and_then(|c| c.text(1).map(|s| s.to_string())).unwrap_or_else(|| "_".to_string()))
                .collect()
        };
        let fits_labels = |id: &str| -> Option<bool> {
            let decl = ctx.node_by_id(id)?;
            let list = cpp_paren_list_after(&decl.file_path, decl.start_line, 0, &set.name, ctx)?;
            Some(swift_labels_fit(&labels, &list))
        };
        if fits_labels(&result.target_node_id) != Some(false) {
            return result;
        }
        let fit: Vec<&(String, Option<Arity>)> = set.siblings.iter().filter(|(id, _)| fits_labels(id) == Some(true)).collect();
        return if fit.len() == 1 {
            let mut moved = result;
            moved.target_node_id = fit[0].0.clone();
            moved
        } else {
            result
        };
    }
    let argc = if args.trim().is_empty() { 0 } else { split_cpp_top_level(&args).len() };
    if own.fits(argc) {
        return result;
    }
    let fits: Vec<&(String, Option<Arity>)> = set.siblings.iter().filter(|(_, a)| a.as_ref().is_some_and(|a| a.fits(argc))).collect();
    if fits.len() == 1 {
        let mut moved = result;
        moved.target_node_id = fits[0].0.clone();
        moved
    } else {
        result
    }
}

// -- The strategy order ------------------------------------------------------------------------

const ARKUI_ATTRIBUTE_DECORATORS: [&str; 4] = ["Extend", "Styles", "AnimatableExtend", "Builder"];

fn is_scala_type_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Trait | NodeKind::Interface | NodeKind::Enum | NodeKind::Struct | NodeKind::Module | NodeKind::Namespace)
}

memo_map!(JvmPackagesMemo, String, Rc<HashSet<String>>);

/// `isJavaOutsideImport`: the name is bound by an import of a package the project does not declare.
fn is_java_outside_import(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let imports = ctx.get_import_mappings(&r.file_path, r.language);
    let Some(binding) = imports.iter().find(|m| m.local_name == name) else { return false };
    let call = r.is_kind(EdgeKind::Calls) && name == r.reference_name;
    let declared_here = ctx.get_nodes_in_file_named(&r.file_path, name).iter().any(|n| {
        if call {
            matches!(n.kind, NodeKind::Method | NodeKind::Function)
        } else {
            matches!(
                n.kind,
                NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Struct | NodeKind::Trait | NodeKind::TypeAlias
            ) || n.kind.as_str() == "annotation"
        }
    });
    if declared_here {
        return false;
    }
    let packages = memoized::<JvmPackagesMemo>(ctx, String::new(), || {
        let mut set = HashSet::new();
        for n in ctx.get_nodes_by_kind(NodeKind::Namespace).iter() {
            if matches!(n.language, Language::Java | Language::Kotlin | Language::Scala) {
                set.insert(n.qualified_name.clone());
            }
        }
        Rc::new(set)
    });
    let parts: Vec<&str> = binding.source.split('.').collect();
    for i in 1..parts.len() {
        if packages.contains(&parts[..i].join(".")) {
            return false;
        }
    }
    true
}

fn erlang_imported_module(name: &str, arity: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let source = ctx.read_file(&r.file_path)?;
    if !source.contains("-import") {
        return None;
    }
    let fun = escape_regex(name);
    let listed = Re::cached(&format!(r"(?:^|[\s,])'?{fun}'?\s*/\s*{arity}\b"), "");
    for m in re!(r"^-import\(\s*'?([A-Za-z_][\w@]*)'?\s*,\s*\[([^\]]*)\]\s*\)\s*\.", "gm").captures_iter(&source) {
        if listed.is_match(m.text(2).unwrap_or("")) {
            return m.text(1).map(|s| s.to_string());
        }
    }
    None
}

fn match_reference_inner(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if r.reference_kind == ReferenceKind::FunctionRef {
        return match_function_ref(r, ctx);
    }
    if r.language == Language::Python && r.is_kind(EdgeKind::Imports) && is_python_name_imported_from_outside(&r.reference_name, r, ctx) {
        return None;
    }
    if r.language == Language::ArkTs && r.reference_name.starts_with('.') {
        let base = &r.reference_name[1..];
        let candidates: Vec<NodeRef> = ctx
            .get_nodes_by_name(base)
            .iter()
            .filter(|n| {
                n.language == Language::ArkTs
                    && n.kind == NodeKind::Function
                    && n.decorators.as_ref().is_some_and(|ds| ds.iter().any(|d| ARKUI_ATTRIBUTE_DECORATORS.contains(&d.as_str())))
            })
            .cloned()
            .collect();
        let chosen = if candidates.len() > 1 { prefer_call_site_file(candidates, &r.file_path) } else { candidates };
        if chosen.len() != 1 {
            return None;
        }
        return Some(ResolvedRef::new(r, chosen[0].id.clone(), 0.85, ResolvedBy::ExactMatch));
    }
    if matches!(r.language, Language::Java | Language::Kotlin)
        && !r.is_kind(EdgeKind::Imports)
        && is_java_outside_import(r.reference_name.split('.').next().unwrap_or(""), r, ctx)
    {
        return None;
    }
    if r.language == Language::Scala && r.is_kind(EdgeKind::References) && re!(r"^[^\w\s]+$").is_match(&r.reference_name) {
        let types: Vec<NodeRef> = ctx
            .get_nodes_by_name(&r.reference_name)
            .iter()
            .filter(|n| n.language == Language::Scala && (is_scala_type_kind(n.kind) || n.kind == NodeKind::TypeAlias))
            .cloned()
            .collect();
        let chosen = if types.len() > 1 { prefer_call_site_file(types, &r.file_path) } else { types };
        return if chosen.len() == 1 { Some(ResolvedRef::new(r, chosen[0].id.clone(), 0.8, ResolvedBy::ExactMatch)) } else { None };
    }
    if matches!(r.language, Language::Lua | Language::Luau) && r.is_kind(EdgeKind::Calls) && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        match lua_alias_target(r, ctx) {
            Tri::Undefined => {}
            Tri::Null => return None,
            Tri::Value(v) => return Some(v),
        }
        if LUA_GLOBAL_FUNCTIONS.contains(r.reference_name.as_str())
            && !ctx.get_nodes_in_file(&r.file_path).iter().any(|n| n.name == r.reference_name && n.kind == NodeKind::Function)
        {
            return None;
        }
    }
    if r.language == Language::Erlang && (r.is_kind(EdgeKind::Implements) || re!(r"\.app(?:\.src)?$", "i").is_match(&r.file_path)) {
        let modules: Vec<NodeRef> =
            ctx.get_nodes_by_name(&r.reference_name).iter().filter(|n| n.language == Language::Erlang && n.kind == NodeKind::Namespace).cloned().collect();
        let chosen = prefer_call_site_file(modules, &r.file_path).into_iter().next()?;
        return Some(ResolvedRef::new(r, chosen.id.clone(), 0.9, ResolvedBy::ExactMatch));
    }
    if r.language == Language::Erlang && !r.reference_name.contains("::") && (r.is_kind(EdgeKind::Calls) || r.is_kind(EdgeKind::References)) {
        if let Some(am) = re!(r"^(.+)/(\d{1,3})$").captures(&r.reference_name) {
            let (fname, arity) = (am.text(1).unwrap_or("").to_string(), am.text(2).unwrap_or("").to_string());
            let arity_tail = format!("/{arity}");
            let mut candidates: Vec<NodeRef> = ctx
                .get_nodes_by_name(&fname)
                .iter()
                .filter(|n| n.language == Language::Erlang && n.kind == NodeKind::Function && n.qualified_name.ends_with(&arity_tail))
                .cloned()
                .collect();
            if !candidates.is_empty() {
                if let Some(same) = candidates.iter().find(|n| n.file_path == r.file_path) {
                    return Some(ResolvedRef::new(r, same.id.clone(), 0.95, ResolvedBy::ExactMatch));
                }
                if let Some(imported) = erlang_imported_module(&fname, &arity, r, ctx) {
                    let chosen = candidates.iter().find(|n| n.qualified_name.starts_with(&format!("{imported}::")));
                    return chosen.map(|c| ResolvedRef::new(r, c.id.clone(), 0.9, ResolvedBy::ExactMatch));
                }
                if !r.file_path.ends_with(".hrl") {
                    let included: Vec<NodeRef> = candidates.iter().filter(|n| n.file_path.ends_with(".hrl")).cloned().collect();
                    if included.is_empty() {
                        return None;
                    }
                    candidates = included;
                }
                if candidates.len() == 1 {
                    return Some(ResolvedRef::new(r, candidates[0].id.clone(), 0.8, ResolvedBy::ExactMatch));
                }
                if let Some(best) = find_best_match(r, &candidates, ctx) {
                    let proximity = compute_path_proximity(&r.file_path, &best.file_path);
                    return Some(ResolvedRef::new(r, best.id.clone(), if proximity >= 30 { 0.7 } else { 0.4 }, ResolvedBy::ExactMatch));
                }
            }
            return None;
        }
    }
    if is_unresolved_js_member_call(r) {
        return match_object_path_call(r, ctx);
    }
    if let Some(collapsed) = match_collapsed_object_call(r, ctx) {
        return Some(collapsed);
    }
    if r.language == Language::Swift && r.is_kind(EdgeKind::Calls) && is_swift_type_path_call(&r.reference_name) {
        return resolve_swift_type_path_call(r, ctx);
    }
    if r.language == Language::Dart && r.is_kind(EdgeKind::Calls) {
        let prefixed = match_dart_prefixed_call(r, ctx);
        if !prefixed.is_undefined() {
            return prefixed.value();
        }
    }
    if r.language == Language::Cpp && r.is_kind(EdgeKind::Calls) && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        let unspelled = match_cpp_unspelled_receiver_call(r, ctx);
        if !unspelled.is_undefined() {
            return unspelled.value();
        }
        let chained = match_cpp_member_chain_call(r, ctx);
        if !chained.is_undefined() {
            return chained.value();
        }
    }
    if r.language == Language::Rust && r.is_kind(EdgeKind::Calls) && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        let lent = match_rust_as_mut_chain_call(r, ctx);
        if !lent.is_undefined() {
            return lent.value();
        }
    }
    if let Some(result) = match_by_file_path(r, ctx) {
        return Some(result);
    }
    if let Some(result) = match_by_qualified_name(r, ctx) {
        return Some(result);
    }
    if matches!(r.language, Language::Cpp | Language::C) {
        if let Some(result) = match_cpp_call_chain(r, ctx) {
            return Some(result);
        }
    }
    if r.language == Language::Cpp && r.is_kind(EdgeKind::Instantiates) {
        let constructed = match_cpp_construction(r, ctx);
        if !constructed.is_undefined() {
            return constructed.value();
        }
    }
    if matches!(r.language, Language::Php | Language::Rust) {
        if let Some(result) = match_scoped_call_chain(r, ctx) {
            return Some(result);
        }
    }
    if matches!(
        r.language,
        Language::Java | Language::Kotlin | Language::CSharp | Language::Swift | Language::Go | Language::Scala | Language::Dart | Language::ObjC | Language::Pascal
    ) {
        if let Some(result) = match_dotted_call_chain(r, ctx) {
            return Some(result);
        }
    }
    if r.reference_name.contains("().")
        && matches!(r.language, Language::TypeScript | Language::JavaScript | Language::Tsx | Language::Jsx | Language::Python)
    {
        if r.language == Language::Python {
            if let Some(chained) = match_python_factory_chain(r, ctx) {
                return Some(chained);
            }
        }
        return match_store_accessor_chain(r, ctx);
    }
    if let Some(result) = match_method_call(r, ctx) {
        return Some(result);
    }
    if let Some(result) = match_by_exact_name(r, ctx) {
        return Some(result);
    }
    if let Some(result) = match_fuzzy(r, ctx) {
        return Some(result);
    }
    if matches!(r.language, Language::Cpp | Language::C) && r.reference_name.contains("::") {
        return match_cpp_macro_namespaced(r, ctx);
    }
    None
}

