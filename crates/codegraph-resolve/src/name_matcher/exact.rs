//! The name-keyed matchers: by file path, by exact name, by qualified name and by fuzzy
//! (case-folded) name — the core of `src/resolution/name-matcher.ts`.

use super::enclosing_member::match_enclosing_scope_member;
use super::calls::{is_bare_go_call, is_bare_js_call, is_bare_php_call, is_bare_r_call};
use super::cpp_tu::c_source_ext;
use super::js_imports::{is_out_of_repo_binding, is_this_call_in_own_file, is_vue_component_method};
use super::js_scope::is_locally_bound_js_name;
use super::js_store::{match_js_store_binding_call, match_destructured_call_result};
use super::lang_shape::LangShape;
use super::pending::{
    cpp_function_overload, cpp_qualifier_specialization_pool, cpp_specialization_pool, within_cpp_translation_unit,
    CASE_INSENSITIVE_LANGUAGES,
};
use super::visibility::{is_visible_across_files, is_visible_by_language};
use super::csharp::is_csharp_type_visible;
use super::java::java_file_scope;
use super::kotlin::kotlin_file_scope;
use super::reach::{is_cross_file_reachable, is_lexically_reachable, is_sealed_module, is_ts_bare_type_name, is_ts_type_in_scope};
use super::util::*;
use super::{ambiguous_name_ceiling, same_language_family};
use crate::ambient_modules::{ambient_module_declarations, declaring_file_answers};
use crate::cpp_namespaces::cpp_macro_namespace_frames;
use crate::ctx::Ctx;
use crate::js_super_calls::is_js_super_call_site;
use crate::types::{is_cpp_define_signature, is_importable_kind, is_inheritance_ref, is_supertype_target, NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{EdgeKind, Language, NodeKind, ReferenceKind};
use std::cell::Cell;

pub fn match_by_file_path(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !r.reference_name.contains('/') && !re!(r"\.[A-Za-z][A-Za-z0-9]{0,3}$").is_match(&r.reference_name) {
        return None;
    }
    let file_name = r.reference_name.rsplit('/').next().filter(|s| !s.is_empty())?;
    let file_nodes: Vec<NodeRef> = ctx.get_nodes_by_name(file_name).iter().filter(|n| n.kind == NodeKind::File).cloned().collect();
    if file_nodes.is_empty() {
        return None;
    }
    if let Some(exact) = file_nodes.iter().find(|n| n.qualified_name == r.reference_name || n.file_path == r.reference_name) {
        return Some(ResolvedRef::new(r, exact.id.clone(), 0.95, ResolvedBy::FilePath));
    }
    let suffix: Vec<NodeRef> =
        file_nodes.iter().filter(|n| n.qualified_name.ends_with(&r.reference_name) || n.file_path.ends_with(&r.reference_name)).cloned().collect();
    if !suffix.is_empty() {
        return Some(ResolvedRef::new(r, pick_closest_file_node(&suffix, r).id.clone(), 0.85, ResolvedBy::FilePath));
    }
    if file_nodes.len() == 1 {
        return Some(ResolvedRef::new(r, file_nodes[0].id.clone(), 0.7, ResolvedBy::FilePath));
    }
    None
}

/// Of several same-named files, the one nearest the reference (same directory first).
fn pick_closest_file_node(candidates: &[NodeRef], r: &UnresolvedRef) -> NodeRef {
    let dir_of = |p: &str| p.rfind('/').map_or("", |i| &p[..i]).to_string();
    let ref_dir = dir_of(&r.file_path);
    let same_dir: Vec<&NodeRef> = candidates.iter().filter(|c| dir_of(&c.file_path) == ref_dir).collect();
    let pool: Vec<&NodeRef> = if same_dir.is_empty() { candidates.iter().collect() } else { same_dir };
    let mut best = pool[0];
    let mut best_score = i64::MIN;
    for c in pool {
        let score = compute_path_proximity(&r.file_path, &c.file_path) + if same_language_family(c.language, r.language) { 5 } else { 0 };
        if score > best_score {
            best_score = score;
            best = c;
        }
    }
    best.clone()
}

fn is_plain_name(s: &str) -> bool {
    re!(r"^[A-Za-z_$][\w$]*$").is_match(s)
}

/// The calling file's declarations of an out-of-repo imported name that sit inside a function or
/// method — only those can shadow the import. A top-level one is the import's own binding
/// (`request = require('supertest')`), not a shadow of it.
fn inner_scope_shadows(nodes: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    let local: Vec<NodeRef> = nodes.iter().filter(|n| n.file_path == r.file_path).cloned().collect();
    if local.is_empty() {
        return local;
    }
    let in_file = ctx.get_nodes_in_file(&r.file_path);
    let scopes: Vec<&NodeRef> = in_file.iter().filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method)).collect();
    local
        .into_iter()
        .filter(|n| {
            scopes.iter().any(|s| {
                s.id != n.id
                    && s.start_line <= n.start_line
                    && s.end_line >= n.end_line
                    && (s.start_line < n.start_line || s.end_line > n.end_line)
            })
        })
        .collect()
}

/// What an Objective-C struct's member or an alias can be declared with.
fn is_objc_declared_type_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class | NodeKind::Protocol | NodeKind::Struct | NodeKind::Union | NodeKind::Enum | NodeKind::TypeAlias
    )
}

/// Does `ref` name the type an Objective-C struct's member or an alias is declared with?
/// Extraction writes no other `references` from either: a value read is its function's. (A
/// node's id starts with its kind.)
pub fn is_objc_declared_type_ref(r: &UnresolvedRef) -> bool {
    r.language == Language::ObjC
        && r.is_kind(EdgeKind::References)
        && (r.from_node_id.starts_with("struct:")
            || r.from_node_id.starts_with("union:")
            || r.from_node_id.starts_with("type_alias:"))
}

/// The declarations an Objective-C type name stands for: a class's own header (`NSString.h`),
/// then any header or the file itself. A class has a node per `@implementation` and category,
/// and the one in some other source file is the least of them: GNUstep's `NSString` has 14, in
/// test files too.
fn objc_type_declarations(candidates: Vec<NodeRef>, r: &UnresolvedRef) -> Vec<NodeRef> {
    let visible: Vec<NodeRef> =
        candidates.iter().filter(|n| n.file_path == r.file_path || !c_source_ext(&n.file_path)).cloned().collect();
    let own: Vec<NodeRef> = visible
        .iter()
        .filter(|n| {
            n.file_path.ends_with(&format!("/{}.h", r.reference_name)) || n.file_path == format!("{}.h", r.reference_name)
        })
        .cloned()
        .collect();
    if !own.is_empty() {
        own
    } else if !visible.is_empty() {
        visible
    } else {
        candidates
    }
}

pub fn match_by_exact_name(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let bare_js = is_bare_js_call(r, ctx);
    let bare_no_members = is_bare_go_call(r, ctx) || is_bare_r_call(r, ctx);
    let bare_php = is_bare_php_call(r, ctx);
    let lua_bare_call = matches!(r.language, Language::Lua | Language::Luau) && is_calls(r) && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name);
    if !bare_js && is_js_family(r.language) && !is_calls(r) && is_plain_name(&r.reference_name) && is_out_of_repo_binding(&r.reference_name, r, ctx) {
        return None;
    }
    // `import { useQuery } from '@tanstack/react-query'`: the call means the package's, and no
    // other file's same-named symbol. A declaration of the name in this file's inner scope still
    // shadows the import.
    let mut out_of_repo_call = false;
    if bare_js {
        if let Some(store) = match_js_store_binding_call(r, ctx) {
            return Some(store);
        }
        if let Some(returned) = match_destructured_call_result(r, ctx) {
            return Some(returned);
        }
        out_of_repo_call = is_out_of_repo_binding(&r.reference_name, r, ctx);
    }
    let value_ref = is_value_ref(r);
    let import_ref = is_kind(r, EdgeKind::Imports);
    let inheritance_ref = is_inheritance_ref(r);
    let bare_type_name: Cell<Option<bool>> = Cell::new(None);
    let ts_bare_type_name = || {
        if bare_type_name.get().is_none() {
            bare_type_name.set(Some(is_ts_bare_type_name(r, ctx)));
        }
        bare_type_name.get().unwrap()
    };
    // Depends on the reference only, so it is asked once, not per same-named candidate.
    let locally_bound: Cell<Option<bool>> = Cell::new(None);
    let js_locally_bound = || {
        if locally_bound.get().is_none() {
            locally_bound.set(Some(is_locally_bound_js_name(&r.reference_name, &r.file_path, ctx)));
        }
        locally_bound.get().unwrap()
    };
    let same_name = ctx.get_nodes_by_name(&r.reference_name);
    let same_name: crate::types::NodeList = if out_of_repo_call { std::rc::Rc::new(inner_scope_shadows(&same_name, r, ctx)) } else { same_name };
    let objc_declared_type = is_objc_declared_type_ref(r);
    let c_macro_call = is_calls(r)
        && is_c_family(r.language)
        && same_name.iter().any(|n| n.kind == NodeKind::Constant && is_cpp_define_signature(n.signature.as_deref().unwrap_or("")));
    let shape = LangShape::compute(r, ctx);
    let ambient_declarations = ambient_module_declarations(ctx);
    let scala_symbolic_reference = r.language == Language::Scala && is_kind(r, EdgeKind::References) && re!(r"^(?:[A-Z]|[^\w\s]+$)").is_match(&r.reference_name);
    let filtered: Vec<NodeRef> = same_name
        .iter()
        .filter(|n| {
            !shape.excludes(n, r, ctx)
                && !(r.language == Language::Dart && super::dart::is_dart_private_of_another_library(n, r, ctx))
                && !(c_macro_call && !matches!(n.kind, NodeKind::Function | NodeKind::Method))
                && !(scala_symbolic_reference && matches!(n.kind, NodeKind::Method | NodeKind::Function))
                && !(objc_declared_type && !is_objc_declared_type_kind(n.kind))
                && (!value_ref || same_language_family(n.language, r.language))
                && n.kind != NodeKind::Import
                && !((bare_js || bare_no_members) && is_type_member_kind(n.kind))
                && !(bare_php && n.kind != NodeKind::Function)
                && !(lua_bare_call && n.kind == NodeKind::Method)
                && !(is_calls(r) && is_js_family(r.language) && is_vue_component_method(n) && !is_this_call_in_own_file(n, r, ctx))
                // A Kotlin object expression's `instantiates` names a supertype, never an enum entry.
                && (!(inheritance_ref || (r.language == Language::Kotlin && is_kind(r, EdgeKind::Instantiates))) || is_supertype_target(n))
                && !(n.file_path != r.file_path && ts_bare_type_name() && !is_ts_type_in_scope(n, r, ctx))
                && !(ambient_declarations.contains_key(&n.id) && !declaring_file_answers(&n.file_path, &r.file_path, ctx))
                && (!import_ref || is_importable_kind(n.kind))
                && is_lexically_reachable(n, r, ctx)
                && is_visible_by_language(n, r, ctx)
                && (!import_ref || n.file_path == r.file_path || !is_esm_family(n.language) || !is_sealed_module(&n.file_path, ctx))
                && !(bare_js && n.file_path != r.file_path && js_locally_bound())
        })
        .cloned()
        .collect();
    let ranked = if objc_declared_type {
        objc_type_declarations(filtered, r)
    } else {
        match shape.rank_exact(&filtered, r, ctx) {
            Some(pool) => pool,
            None => cpp_specialization_pool(filtered, r, ctx),
        }
    };
    let candidates = if ranked.len() <= ambiguous_name_ceiling() { within_cpp_translation_unit(ranked, r, ctx, true) } else { ranked };
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        if !is_cross_file_reachable(&candidates[0], r, ctx) {
            return None;
        }
        // A `.tsx` component calling the one `.ts` helper is the same family, not a cross-language guess.
        let cross_language = !super::same_language_family(candidates[0].language, r.language);
        return Some(ResolvedRef::new(r, candidates[0].id.clone(), if cross_language { 0.5 } else { 0.9 }, ResolvedBy::ExactMatch));
    }
    // A member of the caller's own class (or what it inherits / is nested in) is what the name
    // means there, however far away it is declared. Not for a call written on `super`: the one in a
    // class expression's method gets here (js-super-calls declines to place it), and its `super` is
    // that expression's parent, never the enclosing class's (#2497).
    let member = if is_js_super_call_site(r, ctx) { None } else { match_enclosing_scope_member(r, &candidates, ctx) };
    // The scope names the member, not the overload: when the class declares the name more than
    // once, the one the arguments fit, else the nearest, is chosen below — among those overloads
    // only, so a sibling class's namesake stays out of it.
    let overloads: Vec<NodeRef>;
    let mut pool: &[NodeRef] = &candidates;
    if let Some(member) = &member {
        overloads = candidates.iter().filter(|c| c.qualified_name == member.qualified_name && c.file_path == member.file_path).cloned().collect();
        if overloads.len() <= 1 {
            return Some(ResolvedRef::new(r, member.id.clone(), 0.9, ResolvedBy::ExactMatch));
        }
        pool = &overloads;
    }
    if pool.len() > ambiguous_name_ceiling() {
        return None;
    }
    let nearest = find_best_match(r, pool, ctx)?;
    let best = cpp_function_overload(&nearest, pool, r, ctx);
    if is_cross_file_reachable(&best, r, ctx) {
        let proximity = compute_path_proximity(&r.file_path, &best.file_path);
        // Every candidate is a copy of one symbol and this is the caller's own copy: the name
        // itself is not ambiguous, only which copy — so rank it above a proximity guess, but below
        // a unique name or the file's own definition (0.9). The candidates are those
        // `find_best_match` weighed: a namesake in another ranking language never wins while one
        // in the reference's own ranking language exists (C, C++ and Objective-C count as one).
        // The caller's own copy being the caller itself is no evidence, as for a same-file match.
        let is_closest_copy = || {
            let weighed: Vec<NodeRef> = if candidates.iter().any(|c| super::same_ranking_language(c.language, r.language)) {
                candidates.iter().filter(|c| super::same_ranking_language(c.language, r.language)).cloned().collect()
            } else {
                candidates.clone()
            };
            best.id != r.from_node_id && closest_copy(r, &weighed).is_some_and(|copy| copy.id == best.id)
        };
        let confidence = if is_decisive_same_file_match(&best, r, &candidates, ctx) {
            0.9
        } else if is_closest_copy() {
            0.8
        } else if proximity >= 30 {
            0.7
        } else {
            0.4
        };
        return Some(ResolvedRef::new(r, best.id.clone(), confidence, ResolvedBy::ExactMatch));
    }
    None
}

/// isDecisiveSameFileMatch (name-matcher.ts, cde4cd74): `best` won because it is
/// THE definition in the reference's own file — the only same-file candidate,
/// not the caller itself, and (as a member of a declared type, module or
/// namespace) inside the owner the caller is in. Scores like a lone candidate.
fn is_decisive_same_file_match(best: &NodeRef, r: &UnresolvedRef, candidates: &[NodeRef], ctx: &Ctx) -> bool {
    if best.file_path != r.file_path || best.id == r.from_node_id {
        return false;
    }
    if candidates.iter().any(|c| c.id != best.id && c.file_path == r.file_path) {
        return false;
    }
    let Some(sep) = best.qualified_name.rfind("::").filter(|&s| s > 0) else {
        return best.kind != NodeKind::Method;
    };
    let owner = &best.qualified_name[..sep];
    if best.kind != NodeKind::Method {
        let owner_name = owner.rfind("::").map_or(owner, |cut| &owner[cut + 2..]);
        let declared_only_functions = ctx
            .get_nodes_in_file_named(&best.file_path, owner_name)
            .iter()
            .filter(|n| n.qualified_name == owner)
            .all(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method));
        if declared_only_functions {
            return true;
        }
    }
    ctx.node_by_id(&r.from_node_id)
        .is_some_and(|from| from.qualified_name == owner || from.qualified_name.starts_with(&format!("{owner}::")))
}

/// `qn` ends with `name` at a `::` or `.` boundary.
fn ends_at_scope_boundary(qn: &str, name: &str) -> bool {
    if qn == name {
        return true;
    }
    if !qn.ends_with(name) {
        return false;
    }
    let before = &qn[..qn.len() - name.len()];
    before.ends_with("::") || before.ends_with('.')
}

/// Same-file declarations first, the rest in their given order.
pub fn prefer_call_site_file(nodes: Vec<NodeRef>, call_site_file: &str) -> Vec<NodeRef> {
    if nodes.len() < 2 {
        return nodes;
    }
    let (same, other): (Vec<NodeRef>, Vec<NodeRef>) = nodes.iter().cloned().partition(|n| n.file_path == call_site_file);
    if same.is_empty() {
        nodes
    } else {
        same.into_iter().chain(other).collect()
    }
}

fn is_dotted_supertype_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Interface | NodeKind::Struct | NodeKind::Trait | NodeKind::Enum)
}

fn dotted_written_path(name: &str) -> String {
    let stripped = re!(r"<[^<>]*(?:<[^<>]*>[^<>]*)*>").replace_all_str(name, "");
    re!(r"\s+").replace_all_str(&stripped, "")
}

fn directory_closeness(from: &str, to: &str) -> usize {
    let from = from.replace('\\', "/");
    let to = to.replace('\\', "/");
    let from_dirs: Vec<&str> = from.rsplit_once('/').map(|(dirs, _)| dirs.split('/').collect()).unwrap_or_default();
    let to_dirs: Vec<&str> = to.rsplit_once('/').map(|(dirs, _)| dirs.split('/').collect()).unwrap_or_default();
    let mut shared = 0;
    while shared < from_dirs.len() && shared < to_dirs.len() && from_dirs[shared] == to_dirs[shared] {
        shared += 1;
    }
    shared
}

/// A supertype written through its enclosing type — `Outer.Inner`, `Api.Listener`,
/// `object : KOuter.KListener` — or in full. The written path is the tail of the
/// nested type's qualified name, and the file must be able to name the path's
/// first type. Of several, the closest in the tree, or nothing on a tie.
/// `Undefined` when no project type has the path (a library supertype).
pub fn match_dotted_supertype(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if !matches!(r.language, Language::Java | Language::Kotlin | Language::CSharp) {
        return Tri::Undefined;
    }
    if !(is_inheritance_ref(r) || (r.language == Language::Kotlin && is_kind(r, EdgeKind::Instantiates))) {
        return Tri::Undefined;
    }
    let written = dotted_written_path(&r.reference_name);
    if !re!(r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)+$").is_match(&written) {
        return Tri::Undefined;
    }
    let parts: Vec<&str> = written.split('.').collect();
    let last = parts[parts.len() - 1];
    let candidates: Vec<NodeRef> = ctx
        .get_nodes_by_name(last)
        .iter()
        .filter(|n| {
            if !is_dotted_supertype_kind(n.kind) || !same_language_family(n.language, r.language) || n.id == r.from_node_id {
                return false;
            }
            let dotted = n.qualified_name.replace("::", ".");
            dotted == written || dotted.ends_with(&format!(".{written}"))
        })
        .cloned()
        .collect();
    if candidates.is_empty() {
        return Tri::Undefined;
    }
    let visible: Vec<NodeRef> = candidates
        .iter()
        .filter(|n| {
            let dotted = n.qualified_name.replace("::", ".");
            if dotted == written || n.file_path == r.file_path {
                return true;
            }
            let segments: Vec<&str> = n.qualified_name.split("::").collect();
            let root_len = segments.len().saturating_sub(parts.len() - 1);
            let root_qn = segments[..root_len].join("::");
            let pkg = root_qn.split_once("::").map(|(pkg, _)| pkg).unwrap_or("");
            let root_fqn = root_qn.replace("::", ".");
            match r.language {
                Language::Java => {
                    let scope = java_file_scope(&r.file_path, ctx);
                    scope.pkg == pkg || scope.single.contains(&root_fqn) || scope.demand.contains(pkg)
                }
                Language::Kotlin => {
                    let scope = kotlin_file_scope(&r.file_path, ctx);
                    scope.pkg == pkg || scope.imports.contains(&root_fqn) || scope.stars.contains(pkg)
                }
                _ => {
                    let mut named = r.clone();
                    named.reference_name = parts[0].to_string();
                    ctx.get_nodes_by_qualified_name(&root_qn)
                        .iter()
                        .find(|root| is_dotted_supertype_kind(root.kind))
                        .is_some_and(|root| is_csharp_type_visible(root, &named, ctx))
                }
            }
        })
        .cloned()
        .collect();
    if visible.is_empty() {
        return Tri::Null;
    }
    let chosen = if visible.len() == 1 {
        visible[0].clone()
    } else {
        let mut ranked: Vec<(usize, NodeRef)> = visible.iter().map(|n| (directory_closeness(&r.file_path, &n.file_path), n.clone())).collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0));
        if ranked[0].0 == ranked[1].0 {
            return Tri::Null;
        }
        ranked[0].1.clone()
    };
    Tri::Value(ResolvedRef::new(r, chosen.id.clone(), 0.9, ResolvedBy::QualifiedName))
}

pub fn match_by_qualified_name(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !r.reference_name.contains("::") && !r.reference_name.contains('.') {
        return None;
    }
    let keep_for_ref = |nodes: Vec<NodeRef>| -> Vec<NodeRef> {
        if is_calls(r) {
            nodes.into_iter().filter(|n| !(n.kind == NodeKind::Constant && matches!(n.language, Language::Yaml | Language::Properties))).collect()
        } else {
            nodes
        }
    };
    if is_c_family(r.language) && r.reference_name.starts_with("::") {
        let frames_of = |n: &NodeRef| cpp_macro_namespace_frames(&n.file_path, ctx);
        let pool: Vec<NodeRef> = keep_for_ref(ctx.get_nodes_by_qualified_name(&r.reference_name[2..]).iter().cloned().collect())
            .into_iter()
            .filter(|n| is_c_family(n.language) && !frames_of(n).iter().any(|f| f.start as u32 <= n.start_line && f.end as u32 >= n.start_line))
            .collect();
        let global = cpp_specialization_pool(pool, r, ctx);
        let ordered = prefer_call_site_file(global.clone(), &r.file_path);
        let chosen = ordered.first()?;
        let confidence = if global.len() == 1 || chosen.file_path == r.file_path { 0.95 } else { 0.85 };
        return Some(ResolvedRef::new(r, cpp_function_overload(chosen, &global, r, ctx).id.clone(), confidence, ResolvedBy::QualifiedName));
    }
    let direct = keep_for_ref(ctx.get_nodes_by_qualified_name(&r.reference_name).iter().cloned().collect());
    let mut candidates = cpp_qualifier_specialization_pool(cpp_specialization_pool(direct, r, ctx), r, ctx);
    if r.language == Language::CSharp && is_kind(r, EdgeKind::Imports) {
        let namespaces: Vec<NodeRef> = candidates.iter().filter(|n| n.kind == NodeKind::Namespace).cloned().collect();
        candidates = if !namespaces.is_empty() {
            prefer_call_site_file(namespaces, &r.file_path).into_iter().take(1).collect()
        } else {
            candidates.into_iter().filter(|n| n.kind != NodeKind::Import || n.file_path == r.file_path).collect()
        };
    }
    if candidates.len() == 1 {
        return Some(ResolvedRef::new(r, candidates[0].id.clone(), 0.95, ResolvedBy::QualifiedName));
    }
    if candidates.len() > 1 {
        let ordered = prefer_call_site_file(candidates.clone(), &r.file_path);
        if ordered[0].file_path == r.file_path {
            return Some(ResolvedRef::new(r, cpp_function_overload(&ordered[0], &ordered, r, ctx).id.clone(), 0.95, ResolvedBy::QualifiedName));
        }
    }
    if r.language == Language::Erlang && r.reference_name.contains("::") {
        if re!(r"/\d{1,3}$").is_match(&r.reference_name) {
            return None;
        }
        let base = &r.reference_name[r.reference_name.rfind("::").unwrap() + 2..];
        let prefix = format!("{}/", r.reference_name);
        let arity: Vec<NodeRef> = keep_for_ref(ctx.get_nodes_by_name(base).iter().cloned().collect())
            .into_iter()
            .filter(|n| n.qualified_name.starts_with(&prefix) && re!(r"^\d{1,3}$").is_match(&n.qualified_name[prefix.len()..]))
            .collect();
        return if arity.len() == 1 { Some(ResolvedRef::new(r, arity[0].id.clone(), 0.85, ResolvedBy::QualifiedName)) } else { None };
    }
    let parts: Vec<&str> = r.reference_name.split([':', '.']).collect();
    let last_name = parts[parts.len() - 1];
    if !last_name.is_empty() {
        let named: Vec<NodeRef> = keep_for_ref(ctx.get_nodes_by_name(last_name).iter().cloned().collect())
            .into_iter()
            .filter(|c| ends_at_scope_boundary(&c.qualified_name, &r.reference_name))
            .collect();
        let partial = cpp_qualifier_specialization_pool(cpp_specialization_pool(named, r, ctx), r, ctx);
        let own: Vec<NodeRef> = partial.iter().filter(|c| same_language_family(c.language, r.language)).cloned().collect();
        let pool = if !own.is_empty() { own } else { partial };
        let ordered = prefer_call_site_file(pool.clone(), &r.file_path);
        if let Some(chosen) = ordered.first() {
            return Some(ResolvedRef::new(r, cpp_function_overload(chosen, &pool, r, ctx).id.clone(), 0.85, ResolvedBy::QualifiedName));
        }
    }
    None
}

pub fn match_fuzzy(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let lower = js_lower_name(&r.reference_name);
    let candidates = ctx.get_nodes_by_lower_name(&lower);
    let shape = LangShape::compute(r, ctx);
    let bare_r = is_bare_r_call(r, ctx);
    let case_insensitive = CASE_INSENSITIVE_LANGUAGES.contains(r.language.as_str());
    let instantiates = is_kind(r, EdgeKind::Instantiates);
    let callable: Vec<NodeRef> = candidates
        .iter()
        .filter(|n| {
            matches!(n.kind, NodeKind::Function | NodeKind::Method | NodeKind::Class)
                && !shape.excludes(n, r, ctx)
                && !(bare_r && n.kind == NodeKind::Method)
                && !(instantiates && n.kind == NodeKind::Method)
                && !(!case_insensitive && n.name != r.reference_name)
        })
        .filter(|n| !is_value_ref(r) || same_language_family(n.language, r.language))
        .cloned()
        .collect();
    let same_language: Vec<NodeRef> = callable.iter().filter(|n| n.language == r.language).cloned().collect();
    let pool = if !same_language.is_empty() { same_language } else { callable };
    let language_candidates = cpp_specialization_pool(pool, r, ctx);
    let final_candidates = shape.rank_fuzzy(&language_candidates, r, ctx).unwrap_or(language_candidates);
    if final_candidates.len() == 1 {
        let only = &final_candidates[0];
        let bare_js = is_bare_js_call(r, ctx);
        if is_visible_across_files(only, r, ctx)
            && is_cross_file_reachable(only, r, ctx)
            && !(bare_js && (is_type_member_kind(only.kind) || (only.file_path != r.file_path && is_locally_bound_js_name(&r.reference_name, &r.file_path, ctx))))
            && !(is_js_family(r.language) && is_out_of_repo_binding(&r.reference_name, r, ctx))
            && !(only.file_path != r.file_path && is_ts_bare_type_name(r, ctx) && !is_ts_type_in_scope(only, r, ctx))
            && !(only.kind == NodeKind::Method && is_bare_go_call(r, ctx))
            && !(only.kind != NodeKind::Function && is_bare_php_call(r, ctx))
            && is_lexically_reachable(only, r, ctx)
        {
            let cross_language = !super::same_language_family(only.language, r.language);
            return Some(ResolvedRef::new(r, only.id.clone(), if cross_language { 0.3 } else { 0.5 }, ResolvedBy::Fuzzy));
        }
    }
    None
}

fn js_lower_name(s: &str) -> String {
    crate::ctx::js_lower(s)
}

#[allow(dead_code)]
fn unused(_: ReferenceKind) {}
