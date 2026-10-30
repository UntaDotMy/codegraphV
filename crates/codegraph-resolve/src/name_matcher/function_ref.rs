//! Function references: a function passed as a value (`register(handler)`, `target=self.run`).

use super::infer::{infer_local_receiver_type, GoReceiverDecl};
use super::js_imports::{import_binds_other, is_out_of_repo_binding};
use super::js_scope::js_function_local_scope;
use super::method::{receiver_link, resolve_method_on_type, shares_receiver_word};
use super::go::*;
use super::pending::*;
use super::python::{
    is_python_module_global, is_python_property, python_binds_locally, python_derives_from, python_field_type, python_global_bindings, python_global_members,
    python_global_receiver_target, python_import_keys, python_local_type, python_members, python_ref_class, PythonBinding,
};
use super::python_scope::{is_fixture_in_reach, is_python_locally_bound, python_from_imports};
use super::reach::is_lexically_reachable;
use super::same_language_family;
use super::util::is_js_family;
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, NodeKind, ReferenceKind};
use std::collections::HashSet;

pub fn match_member_function_ref(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let dot = r.reference_name.rfind('.')?;
    let receiver = &r.reference_name[..dot];
    let member = &r.reference_name[dot + 1..];
    let result = |nodes: &[NodeRef], confidence: f64| -> Option<ResolvedRef> {
        let pool: Vec<&NodeRef> = nodes.iter().filter(|n| same_language_family(n.language, r.language)).collect();
        let target = if pool.len() == 1 { Some(pool[0]) } else { None };
        target
            .filter(|t| matches!(t.kind, NodeKind::Function | NodeKind::Method) && t.id != r.from_node_id && !is_python_property(t, ctx))
            .map(|t| ResolvedRef::new(r, t.id.clone(), confidence, ResolvedBy::FunctionRef))
    };
    let imports = ctx.get_import_mappings(&r.file_path, r.language);
    if imports.iter().any(|i| Some(i.local_name.as_str()) == receiver.split('.').next()) {
        if r.language == Language::Python {
            if let Some(cls) = python_ref_class(receiver, r, ctx) {
                return result(&python_members(&cls, member, r, ctx, &mut HashSet::new()), 0.9);
            }
            let segments: Vec<&str> = receiver.split('.').collect();
            let hit = if segments.len() <= 2 {
                let mut as_ref = r.clone();
                as_ref.reference_name = receiver.to_string();
                as_ref.reference_kind = ReferenceKind::Edge(codegraph_types::EdgeKind::References);
                crate::import_resolver::resolve_import(&as_ref, ctx)
            } else {
                None
            };
            let global = hit.and_then(|h| ctx.node_by_id(&h.target_node_id));
            if let Some(global) = global {
                let head = segments[0];
                if Some(&global.name.as_str()) == segments.last()
                    && is_python_module_global(&global, ctx)
                    && python_import_keys(head, &r.file_path, ctx).len() == 1
                    && !python_global_bindings(head, &r.file_path, ctx).iter().any(|b| !matches!(b, PythonBinding::Import { .. }))
                    && !python_binds_locally(head, r, ctx, false)
                {
                    return result(&python_global_members(&global, member, r, ctx), 0.9);
                }
            }
        }
        let imported = crate::import_resolver::resolve_import(r, ctx);
        let node = imported.and_then(|i| ctx.node_by_id(&i.target_node_id));
        return node.and_then(|node| {
            let same: Vec<NodeRef> = ctx.get_nodes_by_qualified_name(&node.qualified_name).iter().filter(|n| n.file_path == node.file_path).cloned().collect();
            result(&same, 0.9)
        });
    }
    if r.language == Language::Go {
        if receiver.contains('.') {
            return match_go_field_chain_call(receiver, member, r, ctx);
        }
        let mut decl = GoReceiverDecl::default();
        let ty = infer_local_receiver_type(receiver, r, ctx, Some(&mut decl));
        match &decl.asserted {
            Tri::Value(a) => return resolve_method_on_type(&a.name, member, r, ctx, 0.9, ResolvedBy::FunctionRef, Some(&a.dir), 0),
            Tri::Null => return None,
            Tri::Undefined => {}
        }
        match &decl.declared {
            Tri::Null => return None,
            Tri::Value(d) if go_has_no_project_methods(d, r, ctx) => return None,
            _ => {}
        }
        // Nor one a call or a conversion gives a value of such a type.
        match &decl.valued {
            Tri::Null => return None,
            Tri::Value(v) if go_valued_has_no_project_methods(v, ctx) => return None,
            _ => {}
        }
        if let Some(ty) = ty {
            let pkg = go_declared_type_package(decl.raw.as_deref(), decl.file.as_deref().unwrap_or(&r.file_path), ctx);
            return resolve_method_on_type(&ty, member, r, ctx, 0.9, ResolvedBy::FunctionRef, pkg.as_deref(), 0);
        }
        let types: Vec<NodeRef> = ctx
            .get_nodes_by_name(receiver)
            .iter()
            .filter(|n| n.language == Language::Go && matches!(n.kind, NodeKind::Struct | NodeKind::Interface))
            .cloned()
            .collect();
        if !types.is_empty() {
            return if types.len() == 1 { resolve_method_on_type(receiver, member, r, ctx, 0.9, ResolvedBy::FunctionRef, None, 0) } else { None };
        }
    } else {
        let mut classes: Vec<NodeRef> = ctx
            .get_nodes_in_file(&r.file_path)
            .iter()
            .filter(|n| n.kind == NodeKind::Class && n.start_line <= r.line && n.end_line >= r.line)
            .cloned()
            .collect();
        classes.sort_by(|a, b| b.start_line.cmp(&a.start_line));
        let owner = classes.into_iter().next();
        let mut ty: Option<String>;
        if receiver == "self" || receiver == "cls" {
            return owner.and_then(|o| result(&python_members(&o, member, r, ctx, &mut HashSet::new()), 0.9));
        }
        if re!(r"^(self|cls)\.\w+$").is_match(receiver) {
            let owner = owner?;
            ty = python_field_type(receiver, &owner, r, ctx);
        } else {
            ty = python_local_type(receiver, r, ctx);
            if ty.is_none() {
                match python_global_receiver_target(receiver, member, r, ctx, false) {
                    Tri::Undefined => {}
                    Tri::Null => return None,
                    Tri::Value(via) => return Some(ResolvedRef::new(r, via.id.clone(), 0.9, ResolvedBy::FunctionRef)),
                }
            }
        }
        if ty.is_none() && re!(r"^[A-Z]\w*$").is_match(receiver) {
            ty = Some(receiver.to_string());
        }
        if let Some(ty) = ty.filter(|t| t != "object" && t != "Any") {
            let cls = python_ref_class(&ty, r, ctx)?;
            let members = python_members(&cls, member, r, ctx, &mut HashSet::new());
            if !members.is_empty() {
                return result(&members, 0.9);
            }
            let descendants: Vec<NodeRef> = ctx
                .get_nodes_by_name(member)
                .iter()
                .filter(|n| n.kind == NodeKind::Method && n.language == Language::Python)
                .filter(|n| {
                    let parent = ctx
                        .get_nodes_in_file(&n.file_path)
                        .iter()
                        .find(|c| c.kind == NodeKind::Class && n.qualified_name == format!("{}::{member}", c.qualified_name))
                        .cloned();
                    parent.is_some_and(|p| python_derives_from(&p, &cls, r, ctx, &mut HashSet::new()))
                })
                .cloned()
                .collect();
            return result(&descendants, 0.8);
        }
    }
    let by_name: Vec<NodeRef> = ctx.get_nodes_by_name(member).iter().cloned().collect();
    let unique = result(&by_name, 0.8);
    let target = unique.as_ref().and_then(|u| ctx.node_by_id(&u.target_node_id));
    match target {
        Some(t) if t.kind == NodeKind::Method && !shares_receiver_word(receiver_link(receiver), &t.qualified_name) => None,
        _ => unique,
    }
}

pub fn match_function_ref(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if r.reference_name.starts_with("this.") {
        return None;
    }
    if matches!(r.language, Language::Python | Language::Go) && r.reference_name.contains('.') {
        return match_member_function_ref(r, ctx);
    }
    let bare_fn_only = matches!(
        r.language,
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx | Language::ArkTs | Language::Cpp | Language::Python | Language::Php
    );
    let bare_class_ok = r.language == Language::Python;
    if r.reference_name.contains("::") {
        let member_name = &r.reference_name[r.reference_name.rfind("::").unwrap() + 2..];
        let scoped: Vec<NodeRef> = ctx
            .get_nodes_by_name(member_name)
            .iter()
            .filter(|n| {
                matches!(n.kind, NodeKind::Function | NodeKind::Method)
                    && same_language_family(n.language, r.language)
                    && n.id != r.from_node_id
                    && (n.qualified_name == r.reference_name || n.qualified_name.ends_with(&format!("::{}", r.reference_name)))
            })
            .cloned()
            .collect();
        if scoped.is_empty() {
            return None;
        }
        let same_file: Vec<NodeRef> = scoped.iter().filter(|n| n.file_path == r.file_path).cloned().collect();
        let pool = if !same_file.is_empty() { &same_file } else { &scoped };
        if same_file.is_empty() && scoped.len() > 1 {
            return None;
        }
        let target = pool.iter().fold(pool[0].clone(), |a, b| if a.start_line <= b.start_line { a } else { b.clone() });
        return Some(ResolvedRef::new(r, target.id.clone(), 0.9, ResolvedBy::FunctionRef));
    }
    let named: Vec<NodeRef> = ctx
        .get_nodes_by_name(&r.reference_name)
        .iter()
        .filter(|n| {
            (n.kind == NodeKind::Function || (!bare_fn_only && n.kind == NodeKind::Method) || (bare_class_ok && n.kind == NodeKind::Class))
                && same_language_family(n.language, r.language)
                && n.id != r.from_node_id
        })
        .cloned()
        .collect();
    let mut candidates: Vec<NodeRef> = named.iter().filter(|n| is_lexically_reachable(n, r, ctx) && !is_dart_constructor(n, ctx)).cloned().collect();
    if candidates.is_empty() {
        return None;
    }
    if r.language == Language::Python
        && !candidates.iter().any(|n| is_fixture_in_reach(n, &r.file_path, ctx))
        && is_python_locally_bound(&r.reference_name, r, ctx)
    {
        return None;
    }
    if let Some(js_local) = js_function_local_scope(&r.reference_name, r, ctx) {
        candidates.retain(|n| n.file_path == r.file_path && n.start_line >= js_local.start && n.start_line <= js_local.end);
        if candidates.is_empty() {
            return None;
        }
    }
    if r.language == Language::Swift && candidates.iter().any(|n| n.kind == NodeKind::Method) {
        let from_node = ctx.node_by_id(&r.from_node_id);
        let sep = from_node.as_ref().and_then(|f| f.qualified_name.rfind("::"));
        let class_prefix = match (&from_node, sep) {
            (Some(f), Some(s)) if s > 0 => Some(f.qualified_name[..s].to_string()),
            _ => None,
        };
        candidates.retain(|n| {
            if n.kind != NodeKind::Method {
                return true;
            }
            let Some(class_prefix) = &class_prefix else { return false };
            let Some(m_sep) = n.qualified_name.rfind("::").filter(|s| *s > 0) else { return false };
            let method_prefix = &n.qualified_name[..m_sep];
            method_prefix == class_prefix
                || method_prefix.ends_with(&format!("::{class_prefix}"))
                || class_prefix.ends_with(&format!("::{method_prefix}"))
        });
        if candidates.is_empty() {
            return None;
        }
    }
    let same_file: Vec<NodeRef> = candidates.iter().filter(|n| n.file_path == r.file_path).cloned().collect();
    if !same_file.is_empty() {
        if r.language == Language::Swift && same_file.len() > 1 && same_file.iter().all(|n| n.kind == NodeKind::Method) {
            return None;
        }
        let target = same_file.iter().fold(same_file[0].clone(), |a, b| if a.start_line <= b.start_line { a } else { b.clone() });
        return Some(ResolvedRef::new(r, target.id.clone(), if same_file.len() == 1 { 0.95 } else { 0.9 }, ResolvedBy::FunctionRef));
    }
    if candidates.len() == 1
        && (named.len() == 1 || (r.language == Language::Python && python_from_imports(&r.file_path, ctx).contains_key(&r.reference_name)))
        && !(is_js_family(r.language) && is_out_of_repo_binding(&r.reference_name, r, ctx))
        && !(is_js_family(r.language) && import_binds_other(&candidates[0], r, ctx))
    {
        return Some(ResolvedRef::new(r, candidates[0].id.clone(), 0.8, ResolvedBy::FunctionRef));
    }
    None
}
