//! Resolving a Rust method call from what is known of its receiver's type: wrappers, traits,
//! impls on std types, `self` and `Self::` paths, pinned and lent receivers.

use super::impls::*;
use super::types::*;
use super::uses::*;
use crate::ctx::Ctx;
use crate::name_matcher::exact::prefer_call_site_file;
use crate::name_matcher::method::resolve_method_on_type;
use crate::name_matcher::pending::RustTyped;
use crate::name_matcher::util::{after_col, compute_path_proximity, lines_of, memo_map, memoized};
use crate::rust_scopes::*;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Language, Node, NodeKind};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

fn trim(s: &str) -> &str {
    js_trim(s)
}

fn strip(r: &Re, s: &str) -> String {
    r.replace_first(s, |_| String::new())
}

/// `rustTypedMethodCall`'s answer: no opinion because the type could not be read, a decided
/// "nothing", or the target.
#[derive(Debug, Clone)]
pub enum CallRes {
    Unread,
    Null,
    Hit(ResolvedRef),
}

impl CallRes {
    fn of(r: Option<ResolvedRef>) -> CallRes {
        r.map_or(CallRes::Null, CallRes::Hit)
    }
}

fn instance(r: &UnresolvedRef, id: &str, confidence: f64) -> ResolvedRef {
    ResolvedRef::new(r, id.to_string(), confidence, ResolvedBy::InstanceMethod)
}

fn unique_files(nodes: &[NodeRef]) -> Vec<String> {
    let mut seen = HashSet::new();
    nodes.iter().filter(|n| seen.insert(n.file_path.clone())).map(|n| n.file_path.clone()).collect()
}

fn slice_before_last_sep(qn: &str) -> &str {
    match qn.rfind("::") {
        Some(i) => &qn[..i],
        None => {
            let mut end = qn.len().saturating_sub(1);
            while end > 0 && !qn.is_char_boundary(end) {
                end -= 1;
            }
            &qn[..end]
        }
    }
}

// -- Typed refs ----------------------------------------------------------------------------

memo_map!(TypedRefs, String, bool);

fn ref_key(r: &UnresolvedRef) -> String {
    format!("{}\0{}\0{}\0{}\0{}", r.file_path, r.line, r.column, r.reference_name, r.from_node_id)
}

fn mark_typed(r: &UnresolvedRef, ctx: &Ctx) {
    use crate::name_matcher::util::MemoMap;
    ctx.memo::<TypedRefs>().store(ref_key(r), true);
}

/// Whether a typed receiver call already resolved `r` (so a private trait method is reachable).
pub fn is_rust_typed_ref(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    use crate::name_matcher::util::MemoMap;
    ctx.memo::<TypedRefs>().lookup(&ref_key(r)).unwrap_or(false)
}

/// The Rust branch of `isVisibleAcrossFiles`.
pub fn is_rust_visible_across_files(candidate: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if candidate.visibility != Some(codegraph_types::Visibility::Private) {
        return true;
    }
    if is_rust_trait_impl_method(candidate, ctx) {
        return true;
    }
    if candidate.kind == NodeKind::Method
        && is_rust_typed_ref(r, ctx)
        && rust_trait_of(candidate, ctx).is_none_or(|t| t.visibility != Some(codegraph_types::Visibility::Private))
    {
        return true;
    }
    let owner = rust_module_dir(&candidate.file_path);
    r.file_path.starts_with(&format!("{owner}/"))
}

// -- Picking -------------------------------------------------------------------------------

fn rust_pick_method(methods: &[NodeRef], type_files: &[String], r: &UnresolvedRef, confidence: f64) -> ResolvedRef {
    let chosen = methods
        .iter()
        .find(|m| m.file_path == r.file_path)
        .or_else(|| methods.iter().find(|m| type_files.contains(&m.file_path)))
        .cloned()
        .unwrap_or_else(|| {
            let mut best = methods[0].clone();
            for m in &methods[1..] {
                if compute_path_proximity(&r.file_path, &m.file_path) > compute_path_proximity(&r.file_path, &best.file_path) {
                    best = m.clone();
                }
            }
            best
        });
    instance(r, &chosen.id, confidence)
}

struct ImplSelfArgs {
    args: String,
    generics: String,
    by_ref: bool,
}

fn rust_impl_self_args(method: &Node, ctx: &Ctx) -> Option<ImplSelfArgs> {
    let scopes = rust_scopes_of(&method.file_path, ctx)?;
    let block = rust_impl_at(&scopes, rust_node_offset(method, ctx))?;
    if block.kind != ImplKind::Impl {
        return None;
    }
    let header = strip(re!(r"\bwhere\b[\s\S]*$"), code_slice(&scopes, block.at, block.from));
    let head = strip(re!(r"^impl\s*(?:<(?:[^<>]|<(?:[^<>]|<[^<>]*>)*>)*>)?"), &header);
    let self_ = re!(r"\bfor\s+([\s\S]+)$").captures(&head).and_then(|c| c.text(1).map(String::from)).unwrap_or(head);
    let type_ = rust_path_head(&strip(re!(r"^&\s*(?:'\w+\s+)?(?:mut\s+)?"), trim(&self_)))?;
    Some(ImplSelfArgs { args: type_.args, generics: block.generics.clone(), by_ref: trim(&self_).starts_with('&') })
}

fn rust_specialized(methods: Vec<NodeRef>, args: &str, ctx: &Ctx) -> Vec<NodeRef> {
    if methods.len() < 2 || args.is_empty() {
        return methods;
    }
    let norm = |text: &str| -> Vec<String> {
        split_rust_top_level(text, ',')
            .iter()
            .map(|a| strip(re!(r"^(?:[A-Za-z_]\w*::)+"), &re!(r"\s+", "g").replace_all(a, |_| Some(String::new()))))
            .collect()
    };
    let want = norm(args);
    let mut exact: Vec<NodeRef> = Vec::new();
    let mut generic: Vec<NodeRef> = Vec::new();
    for m in &methods {
        let Some(impl_) = rust_impl_self_args(m, ctx) else {
            generic.push(m.clone());
            continue;
        };
        let have = norm(&impl_.args);
        if have.len() != want.len() {
            continue;
        }
        if have.iter().zip(&want).all(|(a, b)| a == b) {
            exact.push(m.clone());
        } else if have.iter().zip(&want).all(|(a, b)| a == b || rust_bounds_of(a, &[(impl_.generics.as_str(), "")]).is_some()) {
            generic.push(m.clone());
        }
    }
    if !exact.is_empty() {
        exact
    } else if !generic.is_empty() {
        generic
    } else {
        methods
    }
}

fn is_rust_trait_callable(trait_: &str, r: &UnresolvedRef, sample: &Node, ctx: &Ctx) -> bool {
    !is_rust_project_named_in(trait_, &RUST_TRAIT_KIND, ctx) || is_rust_trait_in_scope(trait_, r, sample, ctx)
}

// -- Trait impls ---------------------------------------------------------------------------

memo_map!(TraitImplsMemo, String, bool);

#[allow(clippy::too_many_arguments)]
fn rust_trait_implemented(
    trait_: &str,
    trait_file: Option<&str>,
    name: Option<&str>,
    type_files: &[String],
    receiver_traits: &[String],
    ctx: &Ctx,
    depth: u32,
) -> bool {
    use crate::name_matcher::util::MemoMap;
    let ident = re!(r"^[A-Za-z_]\w*$");
    if !ident.is_match(trait_) || name.is_some_and(|n| !ident.is_match(n)) {
        return false;
    }
    let mut files: Vec<String> = Vec::new();
    if let Some(f) = trait_file {
        files.push(f.to_string());
    }
    for f in type_files {
        if !files.contains(f) {
            files.push(f.clone());
        }
    }
    let mut key_parts: Vec<String> =
        vec![trait_.to_string(), trait_file.unwrap_or("").to_string(), name.unwrap_or("").to_string(), receiver_traits.join(","), depth.to_string()];
    key_parts.extend(files.iter().cloned());
    let key = key_parts.join("\0");
    let memo = ctx.memo::<TraitImplsMemo>();
    if let Some(hit) = memo.lookup(&key) {
        return hit;
    }
    memo.store(key.clone(), false);
    let trait_part = format!(
        r"\bimpl\b\s*(?:<((?:[^<>{{;]|<(?:[^<>{{;]|<[^<>{{;]*>)*>)*)>)?\s*(?:[\w:]+::)?{trait_}\b(?:\s*<[^{{;]*?>)?\s+for\s+(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)?(?:[\w:]+::)?"
    );
    let blanket = Re::cached(&format!(r"{trait_part}([A-Z]\w*)\s*(?:where\b([^{{]*))?\{{"), "g");
    let direct = name.map(|n| Re::cached(&format!("{trait_part}{n}\\b"), ""));
    let meets = |bound: &str| -> bool {
        let head = rust_path_head(bound).map(|h| h.name).unwrap_or_default();
        if is_rust_marker_trait(&head) || receiver_traits.iter().any(|t| rust_path_head(t).is_some_and(|h| h.name == head)) {
            return true;
        }
        if name.is_none() || depth >= 2 {
            return false;
        }
        let declared = ctx.get_nodes_by_name(&head).iter().find(|n| n.kind == NodeKind::Trait && n.language == Language::Rust).cloned();
        rust_trait_implemented(&head, declared.as_ref().map(|d| d.file_path.as_str()), name, type_files, receiver_traits, ctx, depth + 1)
    };
    let mut found = false;
    for file in &files {
        let Some(scopes) = rust_scopes_of(file, ctx) else { continue };
        let code = scopes.code.as_str();
        if !code.contains(trait_) {
            continue;
        }
        if direct.as_ref().is_some_and(|d| d.is_match(code)) {
            found = true;
            break;
        }
        for m in blanket.captures_iter(code) {
            let bounds = m.text(1).and_then(|g| rust_bounds_of(m.text(2).unwrap_or(""), &[(g, m.text(3).unwrap_or(""))]));
            if bounds.is_some_and(|b| b.iter().all(|x| meets(x))) {
                found = true;
                break;
            }
        }
        if found {
            break;
        }
    }
    memo.store(key, found);
    found
}

fn rust_has_body(method: &Node, ctx: &Ctx) -> bool {
    let Some(scopes) = rust_scopes_of(&method.file_path, ctx) else { return false };
    let from = rust_node_offset(method, ctx);
    scopes.fns.iter().any(|f| f.at >= from && rust_line_at(&scopes, f.at) <= method.end_line.max(method.start_line))
}

/// A default method of a trait the receiver implements and that is in scope.
fn rust_provided_method(
    name: Option<&str>,
    type_files: &[String],
    receiver_traits: &[String],
    method: &str,
    r: &UnresolvedRef,
    ctx: &Ctx,
) -> Tri<ResolvedRef> {
    let declared: Vec<NodeRef> = ctx
        .get_nodes_by_name(method)
        .iter()
        .filter(|m| m.kind == NodeKind::Method && m.language == Language::Rust && m.name == method && rust_trait_of(m, ctx).is_some() && rust_has_body(m, ctx))
        .cloned()
        .collect();
    if declared.is_empty() {
        return Tri::Undefined;
    }
    let reached: Vec<NodeRef> = declared
        .into_iter()
        .filter(|m| {
            let trait_ = rust_trait_of(m, ctx).expect("filtered to trait methods");
            rust_trait_implemented(&trait_.name, Some(&trait_.file_path), name, type_files, receiver_traits, ctx, 0)
                && is_rust_trait_in_scope(&trait_.name, r, m, ctx)
        })
        .collect();
    let traits: HashSet<String> = reached.iter().map(|m| rust_trait_of(m, ctx).expect("trait method").id.clone()).collect();
    if traits.len() == 1 {
        return Tri::Value(instance(r, &reached[0].id, 0.85));
    }
    if traits.len() > 1 {
        Tri::Null
    } else {
        Tri::Undefined
    }
}

memo_map!(AliasImplMemo, String, Rc<Vec<NodeRef>>);

fn rust_alias_impl_methods(name: &str, method: &str, ctx: &Ctx) -> Rc<Vec<NodeRef>> {
    memoized::<AliasImplMemo>(ctx, format!("{name}\0{method}"), || {
        let mut alias_of: HashMap<String, bool> = HashMap::new();
        let mut is_alias_of_type = |owner: &str| -> bool {
            if let Some(yes) = alias_of.get(owner) {
                return *yes;
            }
            let yes = owner != name
                && ctx.get_nodes_by_name(owner).iter().any(|n| {
                    n.kind == NodeKind::TypeAlias
                        && n.language == Language::Rust
                        && rust_path_head(&rust_alias_target(n, ctx).map(|t| t.text).unwrap_or_default()).is_some_and(|h| h.name == name)
                });
            alias_of.insert(owner.to_string(), yes);
            yes
        };
        let found: Vec<NodeRef> = ctx
            .get_nodes_by_name(method)
            .iter()
            .filter(|m| {
                if m.kind != NodeKind::Method
                    || m.language != Language::Rust
                    || m.name != method
                    || rust_trait_of(m, ctx).is_some()
                    || is_rust_trait_impl_method(m, ctx)
                {
                    return false;
                }
                match m.qualified_name.rfind("::") {
                    Some(cut) if cut > 0 => is_alias_of_type(m.qualified_name[..cut].rsplit("::").next().unwrap_or("")),
                    _ => false,
                }
            })
            .cloned()
            .collect();
        Rc::new(found)
    })
}

fn rust_deref_target(name: &str, type_files: &[String], ctx: &Ctx) -> Option<RustWrittenType> {
    let header = Re::cached(
        &format!(
            r"\bimpl\b\s*(?:<[^{{;]*?>)?\s*(?:(?:std|core)\s*::\s*ops\s*::\s*|ops\s*::\s*)?Deref\s+for\s+(?:[\w:]+::)?{}\b[^{{;]*\{{",
            escape_regex(name)
        ),
        "g",
    );
    for file in type_files {
        let Some(scopes) = rust_scopes_of(file, ctx) else { continue };
        let code = scopes.code.as_str();
        if !code.contains("Deref") {
            continue;
        }
        for m in header.captures_iter(code) {
            let body = m.get(0).expect("group 0").end();
            if let Some(target) = re!(r"\btype\s+Target\s*=\s*([^;]+);").captures(code_slice(&scopes, body, body + 600)) {
                return Some(RustWrittenType::plain(
                    trim(target.text(1).unwrap_or("")).to_string(),
                    file.clone(),
                    body + target.get(0).expect("group 0").start(),
                ));
            }
        }
    }
    None
}

fn rust_alias_target(alias: &Node, ctx: &Ctx) -> Option<RustWrittenType> {
    let scopes = rust_scopes_of(&alias.file_path, ctx)?;
    let start = rust_node_offset(alias, ctx);
    let re = Re::cached(&format!(r"\btype\s+{}\b\s*(?:<([^=;]*?)>)?\s*=\s*([^;]+);", escape_regex(&alias.name)), "");
    let m = re.captures(code_slice(&scopes, start.saturating_sub(64), start + 2000))?;
    Some(RustWrittenType {
        text: trim(m.text(2).unwrap_or("")).to_string(),
        file: alias.file_path.clone(),
        at: start,
        generics: Some(m.text(1).unwrap_or("").to_string()),
        ..Default::default()
    })
}

fn rust_trait_methods(traits: &[NodeRef], method: &str, ctx: &Ctx, depth: u32) -> Vec<NodeRef> {
    let mut found: Vec<NodeRef> = Vec::new();
    for trait_ in traits {
        let own: Vec<NodeRef> = ctx
            .get_nodes_by_name(method)
            .iter()
            .filter(|m| {
                m.kind == NodeKind::Method
                    && m.language == Language::Rust
                    && m.file_path == trait_.file_path
                    && m.start_line >= trait_.start_line
                    && m.start_line <= trait_.end_line.max(m.start_line)
                    && rust_trait_of(m, ctx).is_some_and(|t| t.id == trait_.id)
            })
            .cloned()
            .collect();
        if let Some(first) = own.first() {
            found.push(first.clone());
            continue;
        }
        if depth >= 4 {
            continue;
        }
        let Some(scopes) = rust_scopes_of(&trait_.file_path, ctx) else { continue };
        let start = rust_node_offset(trait_, ctx);
        let re = Re::cached(
            &format!(
                r"\btrait\s+{}\b\s*(?:<(?:[^<>]|<(?:[^<>]|<[^<>]*>)*>)*>)?\s*:([^{{;]*?)(?:\bwhere\b[^{{]*)?\{{",
                escape_regex(&trait_.name)
            ),
            "",
        );
        let Some(header) = re.captures(code_slice(&scopes, start, start + 2000)) else { continue };
        for bound in split_rust_top_level(header.text(1).unwrap_or(""), '+') {
            let Some(head) = rust_path_head(&bound) else { continue };
            if bound.starts_with('\'') {
                continue;
            }
            let supers: Vec<NodeRef> = rust_type_lookup(&head.name, &head.segments, &trait_.file_path, ctx, Some(rust_node_offset(trait_, ctx)))
                .nodes
                .into_iter()
                .filter(|n| n.kind == NodeKind::Trait)
                .collect();
            found.extend(rust_trait_methods(&supers, method, ctx, depth + 1));
        }
    }
    found
}

pub fn rust_traits_method_call(traits: &[String], written: &RustWrittenType, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> CallRes {
    let mut found: Vec<NodeRef> = Vec::new();
    let mut project_traits = 0;
    for bound in traits {
        let Some(head) = rust_path_head(bound) else { continue };
        let nodes: Vec<NodeRef> = rust_type_lookup(&head.name, &head.segments, &written.file, ctx, Some(written.at))
            .nodes
            .into_iter()
            .filter(|n| n.kind == NodeKind::Trait)
            .collect();
        if !nodes.is_empty() {
            project_traits += 1;
        }
        found.extend(rust_trait_methods(&nodes, method, ctx, 0));
    }
    if project_traits == 0 {
        return CallRes::Unread;
    }
    let owners: HashSet<Option<String>> = found.iter().map(|m| rust_trait_of(m, ctx).map(|t| t.id.clone())).collect();
    if owners.len() == 1 {
        return CallRes::Hit(instance(r, &found[0].id, 0.9));
    }
    if owners.len() > 1 {
        return CallRes::Null;
    }
    match rust_provided_method(None, &[], traits, method, r, ctx) {
        Tri::Value(v) => CallRes::Hit(v),
        _ => CallRes::Null,
    }
}

fn rust_delegates_to_own(method: &Node, type_name: &str, ctx: &Ctx) -> bool {
    let Some(scopes) = rust_scopes_of(&method.file_path, ctx) else { return false };
    let start = (method.start_line as usize).checked_sub(1).and_then(|i| scopes.line_starts.get(i)).copied().unwrap_or(0);
    let end = scopes.line_starts.get(method.end_line.max(method.start_line) as usize).copied().unwrap_or(scopes.code.len());
    Re::cached(&format!(r"\b(?:Self|{})\s*::\s*{}\s*\(", escape_regex(type_name), escape_regex(&method.name)), "")
        .is_match(code_slice(&scopes, start, end))
}

fn rust_outside_type_call(name: &str, args: &str, by_ref: bool, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let for_outside = |m: &Node| -> bool {
        let written = rust_trait_impl_header(m, ctx)
            .map(|h| strip(re!(r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)+"), &h.self_text))
            .unwrap_or_default();
        let head = rust_path_head(&written);
        let lookup = rust_type_lookup(
            head.as_ref().map(|h| h.name.as_str()).unwrap_or(name),
            head.as_ref().map(|h| h.segments.as_slice()).unwrap_or(&[]),
            &m.file_path,
            ctx,
            Some(rust_node_offset(m, ctx)),
        );
        lookup.outside || !lookup.nodes.iter().any(|n| RUST_OWNER_KINDS.contains(&n.kind))
    };
    let candidates: Vec<NodeRef> = rust_methods_named(name, method, ctx)
        .into_iter()
        .filter(|m| {
            let Some(impl_) = rust_trait_impl_header(m, ctx) else { return false };
            impl_.self_ == name
                && is_rust_project_named_in(&impl_.trait_, &RUST_TRAIT_KIND, ctx)
                && is_rust_trait_in_scope(&impl_.trait_, r, m, ctx)
                && !rust_delegates_to_own(m, name, ctx)
                && for_outside(m)
        })
        .collect();
    let impls = rust_specialized(candidates, args, ctx);
    if impls.len() == 1 {
        return Some(instance(r, &impls[0].id, 0.85));
    }
    if impls.len() > 1 {
        let matching: Vec<NodeRef> =
            impls.iter().filter(|m| rust_impl_self_args(m, ctx).is_some_and(|a| a.by_ref) == by_ref).cloned().collect();
        let pool = if matching.is_empty() { impls } else { matching };
        if pool.len() == 1 {
            return Some(instance(r, &pool[0].id, 0.85));
        }
        let traits: HashSet<String> = pool
            .iter()
            .map(|m| {
                let h = rust_trait_impl_header(m, ctx);
                format!("{}<{}>", h.as_ref().map(|h| h.trait_.as_str()).unwrap_or("undefined"), h.as_ref().map(|h| h.trait_args.join(",")).unwrap_or_default())
            })
            .collect();
        return if traits.len() == 1 { Some(rust_pick_method(&pool, &[], r, 0.85)) } else { None };
    }
    if is_rust_primitive(name) {
        let declared: Vec<NodeRef> = ctx
            .get_nodes_by_name(method)
            .iter()
            .filter(|m| m.kind == NodeKind::Method && m.language == Language::Rust && m.name == method && rust_trait_of(m, ctx).is_some())
            .filter(|m| {
                let t = rust_trait_of(m, ctx).expect("trait method");
                rust_trait_implemented(&t.name, Some(&t.file_path), Some(name), &[], &[], ctx, 0) && is_rust_trait_in_scope(&t.name, r, m, ctx)
            })
            .cloned()
            .collect();
        let traits: HashSet<String> = declared.iter().map(|m| rust_trait_of(m, ctx).expect("trait method").id.clone()).collect();
        if traits.len() == 1 {
            return Some(instance(r, &declared[0].id, 0.8));
        }
        if traits.len() > 1 {
            return None;
        }
    }
    let provided_name = if re!(r"^[A-Za-z_]\w*$").is_match(name) { Some(name) } else { None };
    rust_provided_method(provided_name, &[], &[], method, r, ctx).value()
}

/// `rustTypedMethodCall`: the method `method` called on a value of the type `read`.
pub fn rust_typed_method_call(read: &RustTypeRead, method: &str, r: &UnresolvedRef, ctx: &Ctx, depth: u32) -> CallRes {
    let RustTypeRead::Read(body) = read else { return CallRes::Unread };
    if body.wrappers.iter().any(|w| rust_wrapper_has_own(w, method)) {
        return CallRes::Null;
    }
    for wrapper in &body.wrappers {
        let own: Vec<NodeRef> = rust_methods_named(wrapper, method, ctx)
            .into_iter()
            .filter(|m| !is_rust_trait_impl_method(m, ctx) && rust_trait_of(m, ctx).is_none())
            .collect();
        if own.is_empty() {
            continue;
        }
        let declared = rust_type_lookup(wrapper, &[], &body.written.file, ctx, Some(body.written.at));
        if !declared.outside && !declared.nodes.is_empty() {
            return CallRes::Hit(rust_pick_method(&own, &unique_files(&declared.nodes), r, 0.9));
        }
    }
    if let Some(outer) = body.wrappers.first() {
        if RUST_SELF_WRAPPERS.contains(&outer.as_str()) && !is_rust_project_named(outer, ctx) {
            let held = match &body.kind {
                RustReadKind::Named { name, .. } => Some(name.as_str()),
                _ => None,
            };
            let on_wrapper: Vec<NodeRef> = ctx
                .get_nodes_by_name(method)
                .iter()
                .filter(|n| {
                    n.language == Language::Rust
                        && n.kind == NodeKind::Method
                        && (n.qualified_name == format!("{outer}::{method}") || n.qualified_name.ends_with(&format!("::{outer}::{method}")))
                        && rust_self_wrapper(n).is_none()
                        && rust_impl_for(n, held, ctx)
                })
                .cloned()
                .collect();
            if !on_wrapper.is_empty() {
                let mut inner_body = body.clone();
                inner_body.wrappers = body.wrappers[1..].to_vec();
                let inner = rust_typed_method_call(&RustTypeRead::Read(inner_body), method, r, ctx, depth);
                if let CallRes::Hit(h) = &inner {
                    if ctx.node_by_id(&h.target_node_id).is_some_and(|t| rust_self_wrapper(&t).as_deref() == Some(outer.as_str())) {
                        return inner;
                    }
                }
                let first = prefer_call_site_file(on_wrapper, &r.file_path).remove(0);
                return CallRes::Hit(instance(r, &first.id, 0.9));
            }
        }
    }
    let (name, path, args) = match &body.kind {
        RustReadKind::Outside(name) => return CallRes::of(rust_outside_type_call(name, "", body.by_ref, method, r, ctx)),
        RustReadKind::Traits(traits) => return rust_traits_method_call(traits, &body.written, method, r, ctx),
        RustReadKind::Named { name, path, args } => (name, path, args),
    };
    let lookup = rust_type_lookup(name, path, &body.written.file, ctx, Some(body.written.at));
    if lookup.outside {
        return CallRes::of(rust_outside_type_call(&lookup.name, args, body.by_ref, method, r, ctx));
    }
    if lookup.undeclared && rust_methods_named(&lookup.name, method, ctx).is_empty() {
        return CallRes::of(rust_outside_type_call(&lookup.name, args, body.by_ref, method, r, ctx));
    }
    if !lookup.nodes.is_empty() && lookup.nodes.iter().all(|n| n.kind == NodeKind::Trait) {
        return rust_traits_method_call(&[name.clone()], &body.written, method, r, ctx);
    }
    if lookup.nodes.len() == 1 && lookup.nodes[0].kind == NodeKind::TypeAlias {
        let alias = &lookup.nodes[0];
        let own = rust_methods_named(&alias.name, method, ctx);
        let inherent: Vec<NodeRef> = own.into_iter().filter(|m| !is_rust_trait_impl_method(m, ctx) && rust_trait_of(m, ctx).is_none()).collect();
        if !inherent.is_empty() {
            return CallRes::Hit(rust_pick_method(&inherent, &[alias.file_path.clone()], r, 0.9));
        }
        let target = if depth < 4 { rust_alias_target(alias, ctx) } else { None };
        return match target {
            Some(mut t) => {
                t.wrappers = Some(body.wrappers.clone());
                rust_typed_method_call(&rust_read_type(&t, r, ctx), method, r, ctx, depth + 1)
            }
            None => CallRes::Unread,
        };
    }
    let mut types: Vec<NodeRef> = lookup.nodes.iter().filter(|n| RUST_OWNER_KINDS.contains(&n.kind)).cloned().collect();
    if types.len() > 1 && unique_files(&types).len() > 1 {
        let having: Vec<NodeRef> = types
            .iter()
            .filter(|t| {
                let one = RustTypeLookup { nodes: vec![(*t).clone()], ..lookup.clone() };
                !rust_owned_methods(&lookup.name, method, &one, ctx).is_empty()
            })
            .cloned()
            .collect();
        if having.len() != 1 {
            return CallRes::Unread;
        }
        types = having;
    }
    let type_files = unique_files(&types);
    let scoped = RustTypeLookup { nodes: if lookup.nodes.is_empty() { Vec::new() } else { types.clone() }, ..lookup.clone() };
    let matches = rust_owned_methods(&lookup.name, method, &scoped, ctx);
    let inherent: Vec<NodeRef> = matches.iter().filter(|m| !is_rust_trait_impl_method(m, ctx)).cloned().collect();
    if !inherent.is_empty() {
        let specialized = rust_specialized(inherent, args, ctx);
        return CallRes::Hit(rust_pick_method(&specialized, &type_files, r, 0.9));
    }
    let mut via_traits: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
    for m in &matches {
        let trait_ = rust_trait_impl_header(m, ctx).map(|h| h.trait_.clone()).unwrap_or_default();
        via_traits.entry(trait_).or_default().push(m.clone());
    }
    if !via_traits.is_empty() {
        let mut traits: Vec<String> = via_traits.keys().cloned().collect();
        if traits.len() > 1 {
            traits.retain(|t| is_rust_trait_callable(t, r, &via_traits[t][0], ctx));
        }
        if traits.len() != 1 {
            return CallRes::Null;
        }
        let mut methods = rust_specialized(via_traits[&traits[0]].clone(), args, ctx);
        let trait_args = |m: &Node| -> String { rust_trait_impl_header(m, ctx).map(|h| h.trait_args.join(",")).unwrap_or_default() };
        if methods.iter().map(|m| trait_args(m)).collect::<HashSet<_>>().len() > 1 {
            let fit: Vec<NodeRef> = methods
                .iter()
                .filter(|m| {
                    let args = rust_trait_impl_header(m, ctx).map(|h| h.trait_args.clone()).unwrap_or_default();
                    let head = args.iter().find(|a| re!(r"^[A-Z]").is_match(a)).or_else(|| args.first());
                    head.is_some_and(|h| rust_first_arg_is(h, r, ctx))
                })
                .cloned()
                .collect();
            if fit.is_empty() || fit.iter().map(|m| trait_args(m)).collect::<HashSet<_>>().len() > 1 {
                return CallRes::Null;
            }
            methods = fit;
        }
        return CallRes::Hit(rust_pick_method(&methods, &type_files, r, 0.9));
    }
    let via_alias = rust_alias_impl_methods(&lookup.name, method, ctx);
    if !via_alias.is_empty() {
        let owners: HashSet<&str> = via_alias.iter().map(|m| slice_before_last_sep(&m.qualified_name)).collect();
        return if owners.len() == 1 { CallRes::Hit(rust_pick_method(&via_alias, &type_files, r, 0.85)) } else { CallRes::Null };
    }
    match rust_provided_method(Some(&lookup.name), &type_files, &[], method, r, ctx) {
        Tri::Value(v) => return CallRes::Hit(v),
        Tri::Null => return CallRes::Null,
        Tri::Undefined => {}
    }
    if depth < 3 {
        if let Some(target) = rust_deref_target(&lookup.name, &type_files, ctx) {
            return rust_typed_method_call(&rust_read_type(&target, r, ctx), method, r, ctx, depth + 1);
        }
    }
    CallRes::Null
}

// -- Entry points ----------------------------------------------------------------------------

/// `matchRustTypedReceiverCall`.
pub fn match_rust_typed_receiver_call(receiver: &str, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> RustTyped {
    let Some(scopes) = rust_scopes_of(&r.file_path, ctx) else { return RustTyped::Undefined };
    let offset = rust_ref_offset(&scopes, r);
    let Some((binding, ambiguous)) = rust_binding_at(&scopes, receiver, offset) else { return RustTyped::Undefined };
    let unread = RustTyped::Unread(rust_line_at(&scopes, binding.at));
    if ambiguous {
        return unread;
    }
    let Some(written) = rust_binding_type(&binding, &scopes, r, ctx, 0) else { return unread };
    match rust_typed_method_call(&rust_read_type(&written, r, ctx), method, r, ctx, 0) {
        CallRes::Unread => unread,
        CallRes::Null => RustTyped::Resolved(None),
        CallRes::Hit(h) => {
            mark_typed(r, ctx);
            RustTyped::Resolved(Some(h))
        }
    }
}

/// `matchRustTypedSelfFieldCall`: `self.field.method()` through the field's declared type.
pub fn match_rust_typed_self_field_call(field: &str, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if !re!(r"^(?:[A-Za-z_]\w*|\d+)$").is_match(field) {
        return Tri::Undefined;
    }
    let Some(scopes) = rust_scopes_of(&r.file_path, ctx) else { return Tri::Undefined };
    let at = rust_ref_offset(&scopes, r);
    let Some(written) = rust_field_value("self", field, at, &scopes, r, ctx, 0) else { return Tri::Undefined };
    match rust_typed_method_call(&rust_read_type(&written, r, ctx), method, r, ctx, 0) {
        CallRes::Unread => Tri::Undefined,
        CallRes::Null => Tri::Null,
        CallRes::Hit(h) => {
            mark_typed(r, ctx);
            Tri::Value(h)
        }
    }
}

/// `matchRustGenericPath`: `T::method()` for a generic `T` bounded by project traits.
pub fn match_rust_generic_path(name: &str, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some(scopes) = rust_scopes_of(&r.file_path, ctx) else { return Tri::Undefined };
    let written = RustWrittenType::plain(name, r.file_path.clone(), rust_ref_offset(&scopes, r));
    let Some(bounds) = rust_generic_bounds(name, &written, ctx) else { return Tri::Undefined };
    let traits: Vec<String> = bounds
        .into_iter()
        .filter(|b| !re!(r"^['?]").is_match(trim(b)) && !is_rust_marker_trait(&rust_path_head(b).map(|h| h.name).unwrap_or_default()))
        .collect();
    if traits.is_empty() {
        return Tri::Null;
    }
    match rust_traits_method_call(&traits, &written, method, r, ctx) {
        CallRes::Unread | CallRes::Null => Tri::Null,
        CallRes::Hit(h) => {
            mark_typed(r, ctx);
            Tri::Value(h)
        }
    }
}

const RUST_SELF_PATH_KINDS: [NodeKind; 3] = [NodeKind::Method, NodeKind::Function, NodeKind::EnumMember];

/// `matchRustSelfPath`: `Self::name` inside an impl (or trait).
pub fn match_rust_self_path(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some(self_) = rust_self_type(r, ctx) else { return Tri::Undefined };
    let outside = !self_.is_trait && is_rust_outside_self(&self_, r, ctx);
    let in_trait = |n: &Node, trait_: &str| -> bool {
        ctx.get_nodes_in_file_named(&n.file_path, trait_)
            .iter()
            .any(|t| t.kind == NodeKind::Trait && t.name == trait_ && t.start_line <= n.start_line && t.end_line >= n.start_line)
    };
    let pick = |owner: &str, trait_: bool| -> Option<NodeRef> {
        let all: Vec<NodeRef> = ctx
            .get_nodes_by_name(name)
            .iter()
            .filter(|n| {
                n.language == Language::Rust
                    && RUST_SELF_PATH_KINDS.contains(&n.kind)
                    && (n.qualified_name == format!("{owner}::{name}") || n.qualified_name.ends_with(&format!("::{owner}::{name}")))
                    && (!trait_ || in_trait(n, owner))
                    && (trait_ || !outside || n.file_path == r.file_path)
                    && (self_.trait_.is_none() || n.id != r.from_node_id)
            })
            .cloned()
            .collect();
        let inherent: Vec<NodeRef> = all.iter().filter(|n| !is_rust_trait_impl_method(n, ctx)).cloned().collect();
        let candidates = if !inherent.is_empty() {
            inherent
        } else if all.len() == 1 {
            all
        } else {
            Vec::new()
        };
        if candidates.is_empty() {
            return None;
        }
        if let Some(own) = candidates.iter().find(|n| n.file_path == r.file_path) {
            return Some(own.clone());
        }
        let mut best = candidates[0].clone();
        let mut score = compute_path_proximity(&r.file_path, &best.file_path);
        for c in &candidates[1..] {
            let s = compute_path_proximity(&r.file_path, &c.file_path);
            if s > score {
                score = s;
                best = c.clone();
            }
        }
        Some(best)
    };
    let delegates = ctx.node_by_id(&r.from_node_id).is_some_and(|n| n.name == name);
    let target = pick(&self_.type_, self_.is_trait).or_else(|| match &self_.trait_ {
        Some(t) if !outside && !delegates => pick(t, true),
        _ => None,
    });
    match target {
        None => Tri::Null,
        Some(t) => Tri::Value(ResolvedRef::new(r, t.id.clone(), if t.file_path == r.file_path { 0.9 } else { 0.8 }, ResolvedBy::QualifiedName)),
    }
}

// -- `self.method()` ---------------------------------------------------------------------------

fn rust_impl_for(method: &Node, inner: Option<&str>, ctx: &Ctx) -> bool {
    rust_trait_impl_header(method, ctx).is_some_and(|impl_| impl_.self_args.iter().any(|a| impl_.params.contains(a) || Some(a.as_str()) == inner))
}

enum Through {
    Nodes(Vec<NodeRef>),
    One(NodeRef),
    None,
}

fn rust_through_wrapper(wrapper: &str, name: &str, own: Vec<NodeRef>, inner: Option<&str>, r: &UnresolvedRef, ctx: &Ctx) -> Through {
    let through: Vec<NodeRef> = own.iter().filter(|n| rust_self_wrapper(n).as_deref() == Some(wrapper)).cloned().collect();
    if !through.is_empty() {
        return Through::Nodes(through);
    }
    if rust_wrapper_has_own(wrapper, name) {
        return Through::None;
    }
    let on_wrapper: Vec<NodeRef> = ctx
        .get_nodes_by_name(name)
        .iter()
        .filter(|n| {
            n.language == Language::Rust
                && n.kind == NodeKind::Method
                && (n.qualified_name == format!("{wrapper}::{name}") || n.qualified_name.ends_with(&format!("::{wrapper}::{name}")))
                && rust_self_wrapper(n).is_none()
                && rust_impl_for(n, inner, ctx)
        })
        .cloned()
        .collect();
    if !on_wrapper.is_empty() {
        return Through::One(prefer_call_site_file(on_wrapper, &r.file_path).remove(0));
    }
    Through::Nodes(own)
}

fn pick_rust_own_method(owned: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    if owned.len() <= 1 {
        return owned.first().cloned();
    }
    let inherent: Vec<&NodeRef> = owned.iter().filter(|n| !is_rust_trait_impl_method(n, ctx)).collect();
    if !inherent.is_empty() {
        return if inherent.len() == 1 { Some(inherent[0].clone()) } else { None };
    }
    let header = rust_enclosing_impl_line(r, ctx);
    let block: Vec<&NodeRef> =
        owned.iter().filter(|n| n.file_path == r.file_path && rust_impl_line_at(&n.file_path, n.start_line, ctx) == header).collect();
    if block.len() == 1 {
        Some(block[0].clone())
    } else {
        None
    }
}

/// `matchRustSelfCall`: `self.method()` inside an impl.
pub fn match_rust_self_call(method_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let caller = ctx.node_by_id(&r.from_node_id)?;
    if caller.qualified_name.is_empty() {
        return None;
    }
    let sep = caller.qualified_name.rfind("::").filter(|s| *s > 0)?;
    let owner = &caller.qualified_name[..sep];
    let want = format!("{owner}::{method_name}");
    let mut owned: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&want)
        .iter()
        .filter(|n| n.kind == NodeKind::Method && n.language == Language::Rust && n.qualified_name == want)
        .cloned()
        .collect();
    let owners: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(owner)
        .iter()
        .filter(|n| {
            n.language == Language::Rust && matches!(n.kind, NodeKind::Struct | NodeKind::Enum | NodeKind::Union | NodeKind::Trait | NodeKind::Class)
        })
        .cloned()
        .collect();
    if owners.len() > 1 {
        if owners.iter().filter(|n| n.file_path == caller.file_path).count() != 1 {
            return None;
        }
        owned.retain(|n| n.file_path == caller.file_path);
    }
    let lines = lines_of(ctx, &r.file_path);
    let at = lines.get((r.line as usize).wrapping_sub(1)).map(|l| after_col(l, r.column).to_string());
    if let Some(at) = &at {
        if at.starts_with('(') && Re::cached(&format!(r"^\(\s*self\s*\.\s*{}\s*\)", escape_regex(method_name)), "").is_match(at) {
            return None;
        }
    }
    if caller.name == method_name && is_rust_trait_impl_method(&caller, ctx) {
        let inherent: Vec<NodeRef> = owned.iter().filter(|n| !is_rust_trait_impl_method(n, ctx)).cloned().collect();
        if !inherent.is_empty() {
            owned = inherent;
        } else {
            let self_ = rust_self_type(r, ctx)?;
            if is_rust_outside_self(&self_, r, ctx) {
                return None;
            }
        }
    }
    if let Some(wrapper) = rust_self_wrapper(&caller) {
        if !is_rust_project_named(&wrapper, ctx) {
            let inner = owner.rsplit("::").next();
            match rust_through_wrapper(&wrapper, method_name, owned, inner, r, ctx) {
                Through::None => return None,
                Through::One(n) => return Some(ResolvedRef::new(r, n.id.clone(), 0.9, ResolvedBy::QualifiedName)),
                Through::Nodes(nodes) => owned = nodes,
            }
        }
    }
    let target = pick_rust_own_method(&owned, r, ctx)?;
    Some(ResolvedRef::new(r, target.id.clone(), 0.9, ResolvedBy::QualifiedName))
}

// -- Pinned and lent receivers ---------------------------------------------------------------

/// The type text up to the first top-level `,` `=` `;` `{` or unmatched closer.
pub fn rust_type_text_at(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    for (i, &ch) in bytes.iter().enumerate() {
        if ch == b'<' || ch == b'(' || ch == b'[' {
            depth += 1;
        } else if ch == b'>' && (i == 0 || bytes[i - 1] != b'-') {
            depth = (depth - 1).max(0);
        } else if ch == b')' || ch == b']' {
            if depth == 0 {
                return trim(&text[..i]).to_string();
            }
            depth -= 1;
        } else if depth == 0 && matches!(ch, b',' | b'=' | b';' | b'{') {
            return trim(&text[..i]).to_string();
        }
    }
    trim(text).to_string()
}

fn rust_declared_type_text(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let scopes = rust_scopes_of(&r.file_path, ctx)?;
    let offset = rust_ref_offset(&scopes, r);
    let (binding, ambiguous) = rust_binding_at(&scopes, name, offset)?;
    if ambiguous {
        return None;
    }
    binding.ty.as_deref().map(rust_type_text_at)
}

fn rust_pinned_pointee(type_: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let name = re!(
        r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)?(?:(?:std|core)\s*::\s*pin\s*::\s*)?Pin\s*<\s*(?:&\s*(?:'\w+\s+)?(?:mut\s+)?|(?:(?:std|alloc)\s*::\s*boxed\s*::\s*)?Box\s*<\s*)?(?:dyn\s+|impl\s+)?((?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*)"
    )
    .captures(type_)
    .and_then(|c| c.text(1).map(|s| trim(s.rsplit("::").next().unwrap_or("")).to_string()));
    match name {
        Some(n) if n == "Self" => rust_self_type(r, ctx).map(|s| s.type_),
        other => other,
    }
}

/// `matchRustPinnedCall`: a method on a receiver declared `Pin<&mut T>`.
pub fn match_rust_pinned_call(receiver: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let pointee = rust_declared_type_text(receiver, r, ctx).and_then(|t| rust_pinned_pointee(&t, r, ctx));
    rust_pinned_call_on(pointee, name, r, ctx)
}

fn rust_pinned_call_on(pointee: Option<String>, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let inner = pointee.filter(|p| !is_rust_type_param(p, r, ctx));
    let own: Vec<NodeRef> = match &inner {
        None => Vec::new(),
        Some(inner) => ctx
            .get_nodes_by_name(name)
            .iter()
            .filter(|n| {
                n.language == Language::Rust
                    && n.kind == NodeKind::Method
                    && (n.qualified_name == format!("{inner}::{name}") || n.qualified_name.ends_with(&format!("::{inner}::{name}")))
            })
            .cloned()
            .collect(),
    };
    let target: Option<NodeRef> = match rust_through_wrapper("Pin", name, own, inner.as_deref(), r, ctx) {
        Through::None => None,
        Through::One(n) => Some(n),
        Through::Nodes(nodes) => {
            let inherent: Vec<NodeRef> = nodes.iter().filter(|n| !is_rust_trait_impl_method(n, ctx)).cloned().collect();
            let pool = if inherent.is_empty() { nodes } else { inherent };
            prefer_call_site_file(pool, &r.file_path).into_iter().next()
        }
    };
    target.map(|t| instance(r, &t.id, 0.9))
}

const RUST_LENDING_POINTERS: [&str; 5] = ["Box", "NonNull", "Rc", "Arc", "Cow"];

struct TypeHead {
    outer: String,
    args: String,
    text: String,
}

fn rust_type_head(type_: &str) -> Option<TypeHead> {
    let a = strip(re!(r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)+"), type_);
    let text = trim(&strip(re!(r"^(?:dyn|impl)\s+"), &a)).to_string();
    let m = re!(r"^(?:[A-Za-z_]\w*\s*::\s*)*([A-Za-z_]\w*)\s*(?:<([\s\S]*)>)?").captures(&text)?;
    Some(TypeHead { outer: m.text(1).unwrap_or("").to_string(), args: m.text(2).unwrap_or("").to_string(), text: text.clone() })
}

fn rust_lent_call(type_: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx, tried: bool) -> Tri<ResolvedRef> {
    let Some(head) = rust_type_head(type_) else { return Tri::Undefined };
    if head.outer == "Pin" && !is_rust_project_named("Pin", ctx) {
        return if tried { Tri::Undefined } else { opt_to_tri(rust_pinned_call_on(rust_pinned_pointee(&head.text, r, ctx), name, r, ctx)) };
    }
    if is_rust_project_named(&head.outer, ctx) {
        return Tri::Undefined;
    }
    if RUST_LENDING_POINTERS.contains(&head.outer.as_str()) {
        return if tried { Tri::Undefined } else { rust_call_on_type(&rust_type_text_at(&head.args), name, r, ctx) };
    }
    if head.outer == "Option" || head.outer == "Result" {
        return if tried { rust_call_on_type(&rust_type_text_at(&head.args), name, r, ctx) } else { Tri::Null };
    }
    if is_rust_std_type(&head.outer) || is_rust_type_param(&head.outer, r, ctx) {
        return Tri::Null;
    }
    Tri::Undefined
}

fn opt_to_tri(v: Option<ResolvedRef>) -> Tri<ResolvedRef> {
    v.map_or(Tri::Null, Tri::Value)
}

fn rust_call_on_type(type_: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some(head) = rust_type_head(type_) else { return Tri::Undefined };
    if head.outer == "Pin" && !is_rust_project_named("Pin", ctx) {
        return opt_to_tri(rust_pinned_call_on(rust_pinned_pointee(&head.text, r, ctx), name, r, ctx));
    }
    let outer = if head.outer == "Self" { rust_self_type(r, ctx).map(|s| s.type_) } else { Some(head.outer.clone()) };
    let Some(outer) = outer.filter(|o| !is_rust_type_param(o, r, ctx)) else { return Tri::Null };
    if RUST_LENDING_POINTERS.contains(&outer.as_str()) && !is_rust_project_named(&outer, ctx) {
        return if rust_wrapper_has_own(&outer, name) { Tri::Null } else { rust_call_on_type(&rust_type_text_at(&head.args), name, r, ctx) };
    }
    if is_rust_outside_self(&RustSelfType { type_: outer.clone(), root: None, trait_: None, is_trait: false }, r, ctx) {
        return Tri::Null;
    }
    if !is_rust_project_named(&outer, ctx) {
        return Tri::Undefined;
    }
    opt_to_tri(resolve_method_on_type(&outer, name, r, ctx, 0.85, ResolvedBy::InstanceMethod, None, 0))
}

fn rust_projects_self(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let lines = lines_of(ctx, &r.file_path);
    let start = (crate::name_matcher::js_store::enclosing_scope_start_line(r, ctx) as usize).saturating_sub(1);
    let bound = Re::cached(&format!(r"\blet\s+(?:mut\s+)?{}\b\s*=\s*(.*)$", escape_regex(name)), "");
    let mut i = lines.len().min(r.line as usize) as i64 - 1;
    while i >= start as i64 {
        if let Some(m) = bound.captures(lines.get(i as usize).map(String::as_str).unwrap_or("")) {
            return re!(r"^self\s*(?:\.\s*as_mut\s*\(\s*\)\s*)?\.\s*project\s*\(\s*\)").is_match(m.text(1).unwrap_or(""));
        }
        i -= 1;
    }
    false
}

struct FieldDeclaration {
    type_: String,
    pinned: bool,
}

fn rust_field_declaration(owner: &str, field: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<FieldDeclaration> {
    let mut spans: Vec<Vec<String>> = prefer_call_site_file(
        ctx.get_nodes_by_name(owner)
            .iter()
            .filter(|n| n.language == Language::Rust && matches!(n.kind, NodeKind::Struct | NodeKind::Union))
            .cloned()
            .collect(),
        &r.file_path,
    )
    .iter()
    .map(|s| {
        let lines = lines_of(ctx, &s.file_path);
        let from = (s.start_line as usize).saturating_sub(1).min(lines.len());
        let to = (s.end_line as usize).min(lines.len());
        if from < to {
            lines[from..to].to_vec()
        } else {
            Vec::new()
        }
    })
    .collect();
    if spans.is_empty() {
        let lines = lines_of(ctx, &r.file_path);
        let header = Re::cached(&format!(r"\bstruct\s+{}\b", escape_regex(owner)), "");
        let strip_comment = |l: &str| strip(re!(r"//.*$"), l);
        if let Some(at) = lines.iter().position(|l| header.is_match(&strip_comment(l))) {
            let mut depth = 0i32;
            let mut end = at;
            let mut opened = false;
            while end < lines.len() {
                for ch in strip_comment(&lines[end]).chars() {
                    if ch == '{' {
                        depth += 1;
                        opened = true;
                    } else if ch == '}' {
                        depth -= 1;
                    }
                }
                if opened && depth <= 0 {
                    break;
                }
                end += 1;
            }
            spans.push(lines[at..(end + 1).min(lines.len())].to_vec());
        }
    }
    let declared = Re::cached(&format!(r"(?:^|[\s{{(,])(?:pub(?:\([^)]*\))?\s+)?{}\s*:(?!:)\s*(.*)$", escape_regex(field)), "");
    for span in &spans {
        for (i, raw) in span.iter().enumerate() {
            let line = strip(re!(r"//.*$"), raw);
            let Some(m) = declared.captures(&line) else { continue };
            let idx = m.get(0).expect("group 0").start();
            let before: &str = &line[..(idx + 1).min(line.len())];
            let pinned = re!(r"#\[pin\]").is_match(before) || (i > 0 && re!(r"^\s*#\[pin\]\s*$").is_match(&span[i - 1]));
            return Some(FieldDeclaration { type_: rust_type_text_at(m.text(1).unwrap_or("")), pinned });
        }
    }
    None
}

/// `matchRustAsMutChainCall`: `self.field.as_mut().method()` / `x.as_ref()?.method()`.
pub fn match_rust_as_mut_chain_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let lines = lines_of(ctx, &r.file_path);
    let Some(line) = lines.get((r.line as usize).wrapping_sub(1)).filter(|l| !l.is_empty()) else { return Tri::Undefined };
    if !re!(r"\.\s*as_(?:mut|ref)\s*\(\s*\)").is_match(line) {
        return Tri::Undefined;
    }
    let name = &r.reference_name;
    let lent = Re::cached(
        &format!(
            r"^(self|[a-z_]\w*)(?:\s*\.\s*([a-z_]\w*))?\s*\.\s*as_(?:mut|ref)\s*\(\s*\)\s*(\?)?\s*\.\s*{}\s*(?:\(|::<)",
            escape_regex(name)
        ),
        "",
    );
    let Some(m) = lent.captures(after_col(line, r.column)) else { return Tri::Undefined };
    let base = m.text(1).unwrap_or("");
    let field = m.text(2);
    let tried = m.text(3).is_some();
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return Tri::Undefined };
    let Some(field) = field else {
        if base == "self" {
            return if rust_self_wrapper(&caller).as_deref() == Some("Pin") && !tried {
                opt_to_tri(match_rust_self_call(name, r, ctx))
            } else {
                Tri::Undefined
            };
        }
        return match rust_declared_type_text(base, r, ctx) {
            None => Tri::Undefined,
            Some(declared) => rust_lent_call(&declared, name, r, ctx, tried),
        };
    };
    let projected = base != "self";
    if projected && !rust_projects_self(base, r, ctx) {
        return Tri::Undefined;
    }
    let owner = caller.qualified_name.rfind("::").filter(|s| *s > 0).and_then(|s| caller.qualified_name[..s].rsplit("::").next());
    let decl = owner.and_then(|o| rust_field_declaration(o, field, r, ctx));
    let Some(decl) = decl else { return Tri::Undefined };
    let type_ = if projected && decl.pinned { format!("Pin<&mut {}>", decl.type_) } else { decl.type_ };
    rust_lent_call(&type_, name, r, ctx, tried)
}

const RUST_NON_PROJECT_FIELD_TYPES: [&str; 20] = [
    "bool", "char", "str", "String", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize", "f32", "f64",
    "Self", "self",
];

/// The project type name a field is declared as (`Box<dyn Foo>` → `Foo`), or none for a std/generic one.
pub fn rust_field_type_name(raw: &str) -> Option<String> {
    let mut t = trim(raw).to_string();
    loop {
        let before = t.clone();
        t = strip(re!(r"^&\s*(?:'\w+\s+)?(?:mut\s+)?"), &t);
        t = strip(re!(r"^(?:Box|Rc|Arc)\s*<\s*"), &t);
        t = strip(re!(r"^(?:dyn|impl)\s+"), &t);
        if t == before {
            break;
        }
    }
    let t = trim(&strip(re!(r"[<>+].*$"), &t)).to_string();
    let seg = t.split("::").filter(|s| !s.is_empty()).last()?;
    if !re!(r"^[A-Za-z_]\w*$").is_match(seg) {
        return None;
    }
    if RUST_NON_PROJECT_FIELD_TYPES.contains(&seg) || re!(r"^[A-Z]$").is_match(seg) {
        return None;
    }
    Some(seg.to_string())
}

/// `matchRustSelfFieldCall`: `self.field.method()` through the field's declared type name.
pub fn match_rust_self_field_call(field: &str, method_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if field.is_empty() || field.contains('.') {
        return None;
    }
    let caller = ctx.node_by_id(&r.from_node_id)?;
    let sep = caller.qualified_name.rfind("::").filter(|s| *s > 0)?;
    let owner = caller.qualified_name[..sep].rsplit("::").next().filter(|o| !o.is_empty())?;
    let owners: Vec<NodeRef> = prefer_call_site_file(ctx.get_nodes_by_name(owner).iter().cloned().collect(), &r.file_path)
        .into_iter()
        .filter(|n| matches!(n.kind, NodeKind::Struct | NodeKind::Union | NodeKind::Class) && n.language == Language::Rust)
        .collect();
    let field_re = Re::cached(&format!(r"\b{}\s*:\s*([^,{{}}]+)", escape_regex(field)), "");
    for s in &owners {
        let Some(source) = ctx.read_file(&s.file_path).filter(|t| !t.is_empty()) else { continue };
        let from = (s.start_line as usize).saturating_sub(1);
        for raw in source.split('\n').skip(from).take((s.end_line as usize).saturating_sub(from)) {
            let line = re!(r"/\*.*?\*/", "g").replace_all(&strip(re!(r"//.*$"), raw), |_| Some(String::new()));
            let Some(m) = field_re.captures(&line) else { continue };
            let Some(text) = m.text(1).filter(|t| !t.is_empty()) else { continue };
            let field_type = rust_field_type_name(text)?;
            return resolve_method_on_type(&field_type, method_name, r, ctx, 0.85, ResolvedBy::InstanceMethod, None, 0);
        }
    }
    None
}

// -- Written receivers: field chains and struct literals (#2614) -------------------------------

/// Whether a Rust call through a type's path can mean `target` (isRustPathCallReachable): a type
/// the path names from std or a dependency is that crate's own, and the project holds a function
/// of it only as its own impl of a trait for it; a call through a trait's path reaches the
/// project's impls of that trait.
pub fn is_rust_path_call_reachable(target: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if target.language != Language::Rust {
        return true;
    }
    let segments: Vec<&str> = r.reference_name.split("::").collect();
    if segments.len() < 2 || !segments.iter().all(|s| re!(r"^[A-Za-z_]\w*$").is_match(s)) {
        return true;
    }
    let ty = segments[segments.len() - 2];
    if !ty.starts_with(|c: char| c.is_ascii_uppercase()) || ty == "Self" {
        return true;
    }
    let path: Vec<String> = segments[..segments.len() - 2].iter().map(|s| s.to_string()).collect();
    let Some(outside) = rust_outside_path_type(ty, &path, r, ctx) else { return true };
    let header = if target.kind == NodeKind::Method { rust_trait_impl_header(target, ctx) } else { None };
    let Some(header) = header else { return false };
    if header.trait_ == outside {
        return true;
    }
    if header.self_ != outside {
        return false;
    }
    // The type the impl is for, as its own file names it: the outside one, not a project namesake.
    let self_text = strip(re!(r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)+"), &header.self_text);
    let head = rust_path_head(&self_text);
    let (name, segs) = match &head {
        Some(h) => (h.name.clone(), h.segments.clone()),
        None => (header.self_.clone(), Vec::new()),
    };
    let lookup = rust_type_lookup(&name, &segs, &target.file_path, ctx, Some(rust_node_offset(target, ctx)));
    lookup.outside || !lookup.nodes.iter().any(|n| RUST_OWNER_KINDS.contains(&n.kind))
}

/// The name of the type a Rust path `path::type` names from std or a dependency, where the file's
/// `use`s or the path itself say so — `J` under `use serde_json::Value as J` is `Value` — or None.
fn rust_outside_path_type(ty: &str, path: &[String], r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let at = rust_scopes_of(&r.file_path, ctx).map(|s| s.line_starts.get((r.line as usize).wrapping_sub(1)).copied().unwrap_or(0) + r.column as usize);
    let (root, real) = if path.is_empty() {
        let bound = rust_use_path(ty, &r.file_path, at, ctx)?;
        if bound.len() < 2 {
            return None;
        }
        (bound[0].clone(), bound[bound.len() - 1].clone())
    } else {
        let root = rust_use_path(&path[0], &r.file_path, at, ctx).and_then(|b| b.first().cloned()).unwrap_or_else(|| path[0].clone());
        (root, ty.to_string())
    };
    is_rust_outside_crate(&root, r, ctx).then_some(real)
}

/// Whether a Rust path's first segment names std, or a crate the manifests depend on that the
/// project doesn't hold.
fn is_rust_outside_crate(root: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if RUST_STD_ROOTS.contains(&root) {
        return true;
    }
    if root == "crate" || root == "self" || root == "super" || rust_project_crates(ctx).contains(root) {
        return false;
    }
    if !rust_dependency_crates(ctx).contains(root) {
        return false;
    }
    // A path dependency the project holds, a workspace member or not, is the project's.
    let graph = rust_crate_graph(ctx);
    if let Some(own) = rust_crate_dir_of(&r.file_path, &graph) {
        if graph.crates.get(&own).is_some_and(|c| c.keys.contains_key(root)) {
            return false;
        }
    }
    !graph.crates.values().any(|c| c.name == root)
}

/// A Rust call on a field chain or a struct literal, which the extractor records by its bare name
/// — `vis.map.get("a")`, `self.0.len()`, `Xyz { x: 1.0 }.to_val()` — is a call on the type that
/// receiver is written with (matchRustWrittenReceiverCall). Undefined where the receiver is
/// anything else or its type can't be read.
pub fn match_rust_written_receiver_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some(scopes) = rust_scopes_of(&r.file_path, ctx) else { return Tri::Undefined };
    let Some(&line_start) = scopes.line_starts.get((r.line as usize).wrapping_sub(1)) else { return Tri::Undefined };
    let start = line_start + r.column as usize;
    if start >= scopes.line_starts.get(r.line as usize).copied().unwrap_or(scopes.code.len()) {
        return Tri::Undefined;
    }
    let Some(receiver) = rust_written_call_receiver(&scopes, start, &r.reference_name) else { return Tri::Undefined };
    let Some(written) = rust_value_type(&receiver, start, &scopes, r, ctx, 0) else { return Tri::Undefined };
    let read = rust_read_type(&written, r, ctx);
    // A type no declaration the index holds names says nothing of what the call reaches, unless a
    // wrapper around it has the method itself (`Arc::clone`).
    if let RustTypeRead::Read(body) = &read {
        if let RustReadKind::Named { name, path, .. } = &body.kind {
            if !body.wrappers.iter().any(|w| rust_wrapper_has_own(w, &r.reference_name)) {
                let lookup = rust_type_lookup(name, path, &body.written.file, ctx, Some(body.written.at));
                if lookup.undeclared && rust_methods_named(&lookup.name, &r.reference_name, ctx).is_empty() {
                    return Tri::Undefined;
                }
            }
        }
    }
    let mut result = match rust_typed_method_call(&read, &r.reference_name, r, ctx, 0) {
        CallRes::Unread => return Tri::Undefined,
        CallRes::Null => None,
        CallRes::Hit(hit) => Some(hit),
    };
    if result.is_none() {
        if let RustTypeRead::Read(body) = &read {
            if matches!(body.kind, RustReadKind::Named { .. }) {
                result = rust_derived_trait_method(body, &r.reference_name, r, ctx);
            }
        }
    }
    match result {
        Some(hit) => {
            mark_typed(r, ctx);
            Tri::Value(hit)
        }
        None => Tri::Null,
    }
}

/// The method a Rust project trait that a type derives provides, for a call on a value of the
/// type: `#[derive(TypedPath)]` reaches `TypedPath::with_query_params` through the impl the derive
/// writes. None unless exactly one derived trait in scope declares the method with a body.
fn rust_derived_trait_method(body: &RustReadBody, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let RustReadKind::Named { name, path, .. } = &body.kind else { return None };
    let lookup = rust_type_lookup(name, path, &body.written.file, ctx, Some(body.written.at));
    let types: Vec<&NodeRef> = if lookup.outside { Vec::new() } else { lookup.nodes.iter().filter(|n| RUST_OWNER_KINDS.contains(&n.kind)).collect() };
    if types.len() != 1 {
        return None;
    }
    let mut found: Vec<NodeRef> = Vec::new();
    for derived in rust_derived_names(types[0], ctx) {
        let traits: Vec<NodeRef> =
            ctx.get_nodes_by_name(&derived).iter().filter(|n| n.kind == NodeKind::Trait && n.language == Language::Rust && n.name == derived).cloned().collect();
        for m in rust_trait_methods(&traits, method, ctx, 0) {
            let Some(trait_) = rust_trait_of(&m, ctx) else { continue };
            if rust_has_body(&m, ctx) && is_rust_trait_in_scope(&trait_.name, r, &m, ctx) {
                found.push(m);
            }
        }
    }
    let owners: HashSet<Option<String>> = found.iter().map(|m| rust_trait_of(m, ctx).map(|t| t.id.clone())).collect();
    (owners.len() == 1).then(|| ResolvedRef::new(r, found[0].id.clone(), 0.85, ResolvedBy::InstanceMethod))
}

/// The names a Rust type's `#[derive(…)]` attributes write, each by its path's last segment.
fn rust_derived_names(decl: &Node, ctx: &Ctx) -> Vec<String> {
    let Some(scopes) = rust_scopes_of(&decl.file_path, ctx) else { return Vec::new() };
    let start = rust_node_offset(decl, ctx);
    let keyword = Re::cached(&format!(r"\b(?:struct|enum|union)\s+{}\b", escape_regex(&decl.name)), "");
    let Some(k) = keyword.find(code_slice(&scopes, start, start + 2000)) else { return Vec::new() };
    let at = start + k.start();
    // The attributes right before the keyword.
    let before = code_slice(&scopes, at.saturating_sub(2000), at);
    let attrs = re!(r"((?:#\s*\[(?:[^\[\]]|\[[^\[\]]*\])*\]\s*)+)(?:pub(?:\s*\([^()]*\))?\s+)?$").captures(before).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
    let mut names = Vec::new();
    for m in re!(r"\bderive\s*\(([^()]*)\)").captures_iter(&attrs) {
        for part in m.text(1).unwrap_or("").split(',') {
            let name = js_trim(part).rsplit("::").next().map(js_trim).unwrap_or("");
            if re!(r"^[A-Za-z_]\w*$").is_match(name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

fn code_byte(code: &str, i: usize) -> Option<u8> {
    code.as_bytes().get(i).copied()
}

/// Past the whitespace at `i` in a Rust file's blanked text.
fn rust_space_end(code: &str, mut i: usize) -> usize {
    let b = code.as_bytes();
    while i < b.len() && (b[i] == b' ' || (9..=13).contains(&b[i])) {
        i += 1;
    }
    i
}

fn ident_at(code: &str, at: usize, limit: usize, digits: bool) -> Option<&str> {
    let text = code_slice_str(code, at, limit.min(at + 128));
    let m = if digits { re!(r"^(?:[A-Za-z_]\w*|\d+)").find(text) } else { re!(r"^[A-Za-z_]\w*").find(text) };
    m.map(|m| m.as_str())
}

fn code_slice_str(code: &str, from: usize, to: usize) -> &str {
    let to = to.min(code.len());
    if from >= to {
        ""
    } else {
        &code[from..to]
    }
}

/// Past the arguments of a call at `p` — `(…)`, after a turbofish `::<…>` — or None.
fn rust_call_args_end(scopes: &RustScopes, p: usize) -> Option<usize> {
    let code = &scopes.code;
    let mut q = rust_space_end(code, p);
    if code_slice_str(code, q, q + 2) == "::" {
        q = rust_space_end(code, q + 2);
        if code_byte(code, q) != Some(b'<') {
            return None;
        }
        let end = rust_angle_end(code_slice_str(code, q, q + 512), 0)?;
        q = rust_space_end(code, q + end + 1);
    }
    if code_byte(code, q) != Some(b'(') {
        return None;
    }
    scopes.close.get(&q).map(|e| e + 1)
}

struct FieldChain {
    end: usize,
    hops: usize,
    typed: bool,
}

/// A field chain or a struct literal at `from`, up to `limit` (rustFieldChainAt).
fn rust_field_chain_at(scopes: &RustScopes, from: usize, limit: usize) -> Option<FieldChain> {
    let code = &scopes.code;
    let mut p = from;
    let mut typed = false;
    if code_byte(code, p) == Some(b'(') {
        let end = *scopes.close.get(&p)?;
        if end >= limit {
            return None;
        }
        let mut q = rust_space_end(code, p + 1);
        if code_byte(code, q) == Some(b'&') {
            q = rust_space_end(code, q + 1);
            if re!(r"^mut\b").is_match(code_slice_str(code, q, q + 4)) {
                q = rust_space_end(code, q + 3);
            }
        }
        let inner = rust_field_chain_at(scopes, q, end)?;
        if rust_space_end(code, inner.end) != end || (inner.hops == 0 && !inner.typed) {
            return None;
        }
        p = end + 1;
        typed = true;
    } else {
        let path = re!(r"^(?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*").find(code_slice_str(code, p, limit.min(p + 512)))?.as_str();
        let segments: Vec<&str> = path.split("::").map(|s| s.trim()).collect();
        let last = segments[segments.len() - 1];
        let mut q = rust_space_end(code, p + path.len());
        // `Wrapper::<u8> { … }`: the literal's type arguments.
        if code_slice_str(code, q, q + 2) == "::" {
            let lt = rust_space_end(code, q + 2);
            let end = if code_byte(code, lt) == Some(b'<') { rust_angle_end(code_slice_str(code, lt, lt + 512), 0) } else { None }?;
            q = rust_space_end(code, lt + end + 1);
        }
        if last.starts_with(|c: char| c.is_ascii_uppercase()) && code_byte(code, q) == Some(b'{') {
            let end = *scopes.close.get(&q)?;
            if end >= limit {
                return None;
            }
            p = end + 1;
            typed = true;
        } else if segments.len() == 1 && last.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') {
            // A local, a parameter or `self`.
            p += path.len();
        } else {
            return None;
        }
    }
    let mut hops = 0;
    loop {
        let mut q = rust_space_end(code, p);
        if q >= limit || code_byte(code, q) != Some(b'.') || code_byte(code, q + 1) == Some(b'.') {
            break;
        }
        q = rust_space_end(code, q + 1);
        let Some(id) = ident_at(code, q, limit, true) else { break };
        // `.await` and a method call end the chain.
        if id == "await" || rust_call_args_end(scopes, q + id.len()).is_some() {
            break;
        }
        p = q + id.len();
        hops += 1;
    }
    Some(FieldChain { end: p, hops, typed })
}

/// The receiver of the Rust call `name(…)` whose expression starts at `start`, when it is a field
/// chain or a struct literal (rustWrittenCallReceiver). None for any other receiver, and when a
/// later link of the chain calls `name` again.
fn rust_written_call_receiver(scopes: &RustScopes, start: usize, name: &str) -> Option<String> {
    let code = &scopes.code;
    if start > 0 && code_byte(code, start - 1).is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
        return None;
    }
    let chain = rust_field_chain_at(scopes, start, code.len())?;
    // `x.name(…)` and `self.name(…)` are recorded with their receiver.
    if chain.hops == 0 && !chain.typed {
        return None;
    }
    let dot = rust_space_end(code, chain.end);
    if code_byte(code, dot) != Some(b'.') || code_byte(code, dot + 1) == Some(b'.') {
        return None;
    }
    let at = rust_space_end(code, dot + 1);
    if !code_slice_str(code, at, code.len()).starts_with(name) || code_byte(code, at + name.len()).is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
        return None;
    }
    let mut p = rust_call_args_end(scopes, at + name.len())?;
    // The links after the call: `?`, `[…]`, `.field`, `.method(…)`.
    loop {
        p = rust_space_end(code, p);
        match code_byte(code, p) {
            Some(b'?') => {
                p += 1;
                continue;
            }
            Some(b'[') => {
                p = scopes.close.get(&p)? + 1;
                continue;
            }
            Some(b'.') if code_byte(code, p + 1) != Some(b'.') => {}
            _ => break,
        }
        p = rust_space_end(code, p + 1);
        let Some(id) = ident_at(code, p, code.len(), true) else { break };
        let id = id.to_string();
        p += id.len();
        let Some(args) = rust_call_args_end(scopes, p) else { continue };
        if id == name {
            return None;
        }
        p = args;
    }
    Some(js_trim(code_slice_str(code, start, dot)).to_string())
}
