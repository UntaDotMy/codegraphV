//! The name-keyed matchers: by file path, by exact name, by qualified name and by fuzzy
//! (case-folded) name — the core of `src/resolution/name-matcher.ts`.

use super::calls::{is_bare_go_call, is_bare_js_call, is_bare_php_call, is_bare_r_call};
use super::js_imports::{is_out_of_repo_binding, is_this_call_in_own_file, is_vue_component_method};
use super::js_scope::is_locally_bound_js_name;
use super::js_store::{match_js_store_binding_call, match_destructured_call_result};
use super::lang_shape::LangShape;
use super::pending::{
    cpp_function_overload, cpp_qualifier_specialization_pool, cpp_specialization_pool, within_cpp_translation_unit,
    CASE_INSENSITIVE_LANGUAGES,
};
use super::visibility::{is_visible_across_files, is_visible_by_language};
use super::reach::{is_cross_file_reachable, is_lexically_reachable, is_sealed_module, is_ts_bare_type_name, is_ts_type_in_scope};
use super::util::*;
use super::{ambiguous_name_ceiling, same_language_family};
use crate::cpp_namespaces::cpp_macro_namespace_frames;
use crate::ctx::Ctx;
use crate::types::{is_cpp_define_signature, is_importable_kind, is_inheritance_ref, is_supertype_target, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
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

pub fn match_by_exact_name(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let bare_js = is_bare_js_call(r, ctx);
    let bare_no_members = is_bare_go_call(r, ctx) || is_bare_r_call(r, ctx);
    let bare_php = is_bare_php_call(r, ctx);
    let lua_bare_call = matches!(r.language, Language::Lua | Language::Luau) && is_calls(r) && re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name);
    if !bare_js && is_js_family(r.language) && !is_calls(r) && is_plain_name(&r.reference_name) && is_out_of_repo_binding(&r.reference_name, r, ctx) {
        return None;
    }
    if bare_js {
        if let Some(store) = match_js_store_binding_call(r, ctx) {
            return Some(store);
        }
        if let Some(returned) = match_destructured_call_result(r, ctx) {
            return Some(returned);
        }
        if is_out_of_repo_binding(&r.reference_name, r, ctx) {
            return None;
        }
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
    let c_macro_call = is_calls(r)
        && is_c_family(r.language)
        && same_name.iter().any(|n| n.kind == NodeKind::Constant && is_cpp_define_signature(n.signature.as_deref().unwrap_or("")));
    let shape = LangShape::compute(r, ctx);
    let scala_symbolic_reference = r.language == Language::Scala && is_kind(r, EdgeKind::References) && re!(r"^(?:[A-Z]|[^\w\s]+$)").is_match(&r.reference_name);
    let filtered: Vec<NodeRef> = same_name
        .iter()
        .filter(|n| {
            !shape.excludes(n, r, ctx)
                && !(c_macro_call && !matches!(n.kind, NodeKind::Function | NodeKind::Method))
                && !(scala_symbolic_reference && matches!(n.kind, NodeKind::Method | NodeKind::Function))
                && (!value_ref || same_language_family(n.language, r.language))
                && n.kind != NodeKind::Import
                && !((bare_js || bare_no_members) && is_type_member_kind(n.kind))
                && !(bare_php && n.kind != NodeKind::Function)
                && !(lua_bare_call && n.kind == NodeKind::Method)
                && !(is_calls(r) && is_js_family(r.language) && is_vue_component_method(n) && !is_this_call_in_own_file(n, r, ctx))
                && (!inheritance_ref || is_supertype_target(n))
                && !(n.file_path != r.file_path && ts_bare_type_name() && !is_ts_type_in_scope(n, r, ctx))
                && (!import_ref || is_importable_kind(n.kind))
                && is_lexically_reachable(n, r, ctx)
                && is_visible_by_language(n, r, ctx)
                && (!import_ref || n.file_path == r.file_path || !is_esm_family(n.language) || !is_sealed_module(&n.file_path, ctx))
                && !(bare_js && n.file_path != r.file_path && js_locally_bound())
        })
        .cloned()
        .collect();
    let ranked = match shape.rank_exact(&filtered, r, ctx) {
        Some(pool) => pool,
        None => cpp_specialization_pool(filtered, r, ctx),
    };
    let candidates = if ranked.len() <= ambiguous_name_ceiling() { within_cpp_translation_unit(ranked, r, ctx, true) } else { ranked };
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        if !is_cross_file_reachable(&candidates[0], r, ctx) {
            return None;
        }
        let cross_language = candidates[0].language != r.language;
        return Some(ResolvedRef::new(r, candidates[0].id.clone(), if cross_language { 0.5 } else { 0.9 }, ResolvedBy::ExactMatch));
    }
    if candidates.len() > ambiguous_name_ceiling() {
        return None;
    }
    let nearest = find_best_match(r, &candidates, ctx)?;
    let best = cpp_function_overload(&nearest, &candidates, r, ctx);
    if is_cross_file_reachable(&best, r, ctx) {
        let proximity = compute_path_proximity(&r.file_path, &best.file_path);
        let confidence = if proximity >= 30 { 0.7 } else { 0.4 };
        return Some(ResolvedRef::new(r, best.id.clone(), confidence, ResolvedBy::ExactMatch));
    }
    None
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
            let cross_language = only.language != r.language;
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
