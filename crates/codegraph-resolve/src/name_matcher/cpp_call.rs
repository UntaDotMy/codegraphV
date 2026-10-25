//! C++ member-call matchers — port of `matchCppHeldTypeCall`, `matchCppDeclaredTypeCall`,
//! `matchCppMemberChainCall`, `matchCppUnspelledReceiverCall`, `matchCppConstruction` and
//! `matchCppCallChain`, with the predicates they share.

use super::cpp_method::{cpp_lacks_method, cpp_method_of};
use super::cpp_recv::resolve_cpp_call_result_type;
use super::cpp_scope::{cpp_member_chain_receiver, cpp_unspelled_receiver};
use super::cpp_types::{
    cpp_arrow_target, cpp_class_of_type, cpp_dereferenced, cpp_expression_type, cpp_follow_written_aliases,
    cpp_library_element_of, cpp_member_object, cpp_methods_of, cpp_pointee, cpp_return_type_of, cpp_test_fixture, cpp_types_named,
    cpp_written_class, is_c_family_node, is_cpp_class_kind, is_cpp_library_type, is_cpp_template_dependent_type, is_cpp_template_parameter_type,
    CppWrittenType,
};
use super::method::resolve_method_on_type;
use super::cpp_tu::c_source_ext;
use super::util::after_col;
use crate::cpp_namespaces::{cpp_macro_namespace_frames, cpp_macro_namespace_heads, cpp_name_macros, cpp_namespace_aliases};
use crate::cpp_supertypes::{cpp_class_written, CppNameSite};
use crate::cpp_type_aliases::{
    cpp_aliased_class, cpp_declares_local_alias, cpp_scopes_within, cpp_template_parameters, cpp_type_segments, is_cpp_pointer_type,
    resolve_cpp_aliased_type, strip_cpp_template_arguments,
};
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use super::cpp_recv::CppReceiverDecl;
use codegraph_jsre::re;
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Language, Node, NodeKind};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

/// `cppCallSite`: where the call is written, for looking its type names up.
pub fn cpp_call_site(caller: &Node, r: &UnresolvedRef, ctx: &Ctx) -> CppNameSite {
    CppNameSite {
        scope: if caller.kind == NodeKind::File { String::new() } else { caller.qualified_name.clone() },
        file: r.file_path.clone(),
        line: r.line as usize,
        parameters: cpp_template_parameters(caller, ctx),
        self_id: None,
        class_scope: cpp_test_fixture(caller, r, ctx),
    }
}

fn instance_method(r: &UnresolvedRef, method: &Node, confidence: f64) -> ResolvedRef {
    ResolvedRef::new(r, method.id.clone(), confidence, ResolvedBy::InstanceMethod)
}

/// `cppMemberOperator`: the `.` or `->` that follows the receiver at the reference ("" if neither).
pub fn cpp_member_operator(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> &'static str {
    let Some(lines) = ctx.get_file_lines(&r.file_path) else { return "" };
    let Some(line) = lines.get((r.line as usize).wrapping_sub(1)) else { return "" };
    let at = after_col(line, r.column);
    if at.is_empty() || !at.starts_with(receiver) {
        return "";
    }
    let mut rest = at[receiver.len()..].to_string();
    let mut next = r.line as usize;
    while js_trim(&rest).is_empty() && next < lines.len().min(r.line as usize + 3) {
        rest = lines[next].clone();
        next += 1;
    }
    match re!(r"^\s*(\.|->)").captures(&rest).and_then(|c| c.text(1).map(String::from)).as_deref() {
        Some(".") => ".",
        Some("->") => "->",
        _ => "",
    }
}

/// `isCppDotCall`.
pub fn is_cpp_dot_call(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    cpp_member_operator(receiver, r, ctx) == "."
}

/// `isCppTemplateMember`: the receiver type's head is a template parameter of the caller.
pub fn is_cpp_template_member(raw: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let head = cpp_type_segments(raw).and_then(|s| s.into_iter().next());
    let caller = ctx.node_by_id(&r.from_node_id);
    match (head, caller) {
        (Some(head), Some(caller)) => cpp_template_parameters(&caller, ctx).contains(&head),
        _ => false,
    }
}

#[derive(Default)]
struct ProjectScopeNames(RefCell<Option<Rc<HashSet<String>>>>);

/// `cppProjectScopeNames`: every namespace/type segment the project declares.
fn cpp_project_scope_names(ctx: &Ctx) -> Rc<HashSet<String>> {
    let memo = ctx.memo::<ProjectScopeNames>();
    if let Some(hit) = memo.0.borrow().as_ref() {
        return Rc::clone(hit);
    }
    let mut names: HashSet<String> = cpp_macro_namespace_heads(ctx).into_iter().collect();
    for kind in [NodeKind::Class, NodeKind::Struct, NodeKind::Union, NodeKind::Enum, NodeKind::TypeAlias] {
        ctx.for_each_node_of_kind(kind, |n| {
            if !is_c_family_node(&n) {
                return;
            }
            for segment in n.qualified_name.split("::") {
                if !segment.is_empty() {
                    names.insert(segment.to_string());
                }
            }
        });
    }
    for (macro_name, name) in cpp_name_macros(ctx).aliases.iter() {
        if names.contains(macro_name) {
            names.insert(name.clone());
        } else if names.contains(name) {
            names.insert(macro_name.clone());
        }
    }
    let names = Rc::new(names);
    *memo.0.borrow_mut() = Some(Rc::clone(&names));
    names
}

/// `cppNamespaceSpellings`: a qualified name as written, and with name macros expanded.
fn cpp_namespace_spellings(qualified: &str, ctx: &Ctx) -> Vec<String> {
    let macros = cpp_name_macros(ctx);
    if macros.aliases.is_empty() {
        return vec![qualified.to_string()];
    }
    let named: String = qualified.split("::").map(|s| macros.aliases.get(s).cloned().unwrap_or_else(|| s.to_string())).collect::<Vec<_>>().join("::");
    if named == qualified {
        vec![qualified.to_string()]
    } else {
        vec![qualified.to_string(), named]
    }
}

/// `cppFindsOnlyNamesakes`: the written type's qualification matches no declaration of its name.
pub fn cpp_finds_only_namesakes(raw: &str, ctx: &Ctx, method: Option<&str>) -> bool {
    if raw.matches('<').count() != raw.matches('>').count() {
        return false;
    }
    let Some(segments) = cpp_type_segments(raw) else { return false };
    if segments.len() < 2 {
        return false;
    }
    if segments[..segments.len() - 1].iter().any(|s| !cpp_types_named(s, ctx).is_empty()) {
        return false;
    }
    let name = segments.last().unwrap().clone();
    let alias = cpp_namespace_aliases(ctx).get(&segments[0]).cloned();
    let mut spellings: Vec<Vec<String>> = vec![segments.clone()];
    if let Some(alias) = alias {
        let mut expanded: Vec<String> = alias.split("::").map(String::from).collect();
        expanded.extend(segments[1..].iter().cloned());
        spellings.push(expanded);
    }
    let written: Vec<String> = spellings.iter().flat_map(|s| cpp_namespace_spellings(&s.join("::"), ctx)).collect();
    let std = segments[0] == "std";
    let fits = |owner: &str| written.iter().any(|w| owner == w || owner.ends_with(&format!("::{w}")) || (!std && w.ends_with(&format!("::{owner}"))));
    let mut owners: Vec<String> = cpp_types_named(&name, ctx).iter().map(|n| n.qualified_name.clone()).collect();
    if let Some(method) = method.filter(|m| !m.is_empty()) {
        for m in ctx.get_method_matches(&name, method, Language::Cpp).iter() {
            owners.push(m.qualified_name[..m.qualified_name.len().saturating_sub(method.len() + 2)].to_string());
        }
    }
    !owners.is_empty() && !owners.iter().any(|owner| cpp_namespace_spellings(owner, ctx).iter().any(|s| fits(s)))
}

/// `isCppOutsideQualifiedType`: `Foo::Bar` whose qualifier no project scope or macro explains.
pub fn is_cpp_outside_qualified_type(raw: &str, site: &CppNameSite, ctx: &Ctx) -> bool {
    let Some(segments) = cpp_type_segments(raw) else { return false };
    if segments.len() < 2 {
        return false;
    }
    let head = &segments[0];
    if head == "std" || !re!(r"^[A-Z]").is_match(segments.last().unwrap()) {
        return false;
    }
    if site.parameters.contains(head) || cpp_namespace_aliases(ctx).contains_key(head) {
        return false;
    }
    if cpp_macro_namespace_frames(&site.file, ctx).iter().any(|f| f.path.contains(head)) {
        return false;
    }
    !cpp_project_scope_names(ctx).contains(head)
}

/// `matchCppHeldTypeCall`: a call through `->` on a smart pointer / iterator.
pub fn match_cpp_held_type_call(raw: &str, method_name: &str, r: &UnresolvedRef, ctx: &Ctx, declared: Option<&CppWrittenType>) -> Tri<ResolvedRef> {
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return Tri::Undefined };
    let Some(target) = cpp_arrow_target(raw, &cpp_call_site(&caller, r, ctx), declared, r, ctx) else { return Tri::Undefined };
    if is_cpp_pointer_type(&target.text) {
        return Tri::Undefined;
    }
    let held = target.text.clone();
    if is_cpp_library_type(&held, ctx) {
        return Tri::Null;
    }
    let found = match cpp_class_of_type(&target, r, ctx) {
        Tri::Undefined => return Tri::Undefined,
        Tri::Null => return Tri::Null,
        Tri::Value(f) => f,
    };
    if !is_cpp_class_kind(found.node.kind) {
        return match resolve_cpp_aliased_type(&held, r, ctx) {
            Tri::Null => Tri::Null,
            Tri::Value(a) if is_cpp_library_type(&a.target.join("::"), ctx) => Tri::Null,
            _ => Tri::Undefined,
        };
    }
    if let Some(method) = cpp_method_of(&found.node, method_name, r, ctx) {
        return Tri::Value(instance_method(r, &method, if found.exact { 0.9 } else { 0.85 }));
    }
    if cpp_lacks_method(&found.node, ctx) {
        Tri::Null
    } else {
        Tri::Undefined
    }
}

/// `matchCppDeclaredTypeCall`: a call on a receiver whose declaration was read.
pub fn match_cpp_declared_type_call(receiver: &str, method_name: &str, decl: &CppReceiverDecl, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let Some(raw) = decl.raw.as_deref().filter(|r| !r.is_empty()) else { return Tri::Undefined };
    if !decl.callers || decl.shadowed {
        return Tri::Undefined;
    }
    if cpp_member_operator(receiver, r, ctx) != if decl.pointer { "->" } else { "." } {
        return Tri::Undefined;
    }
    if !decl.aliased && is_cpp_library_type(raw, ctx) {
        return Tri::Null;
    }
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return Tri::Undefined };
    let aliased_class = decl.alias.as_ref().and_then(|a| cpp_aliased_class(a, ctx, None));
    if decl.aliased && aliased_class.is_none() {
        return Tri::Undefined;
    }
    let site = match &decl.declared {
        Some(d) => d.site(),
        None => cpp_call_site(&caller, r, ctx),
    };
    let written = match &aliased_class {
        Some(c) => format!("::{c}"),
        None => raw.to_string(),
    };
    let found = cpp_class_written(&written, &site, r, ctx);
    if found.is_undefined() && aliased_class.is_none() && is_cpp_outside_qualified_type(raw, &site, ctx) {
        return Tri::Null;
    }
    let found = match found {
        Tri::Value(f) if f.exact && is_cpp_class_kind(f.node.kind) => f,
        _ => return Tri::Undefined,
    };
    if let Some(method) = cpp_method_of(&found.node, method_name, r, ctx) {
        return Tri::Value(instance_method(r, &method, 0.9));
    }
    if cpp_lacks_method(&found.node, ctx) {
        Tri::Null
    } else {
        Tri::Undefined
    }
}

/// `isCppArrowThroughOutsideType`.
pub fn is_cpp_arrow_through_outside_type(receiver: &str, decl: &CppReceiverDecl, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(raw) = decl.raw.as_deref().filter(|r| !r.is_empty()) else { return false };
    if !decl.callers || decl.shadowed || decl.pointer || decl.aliased {
        return false;
    }
    if cpp_member_operator(receiver, r, ctx) != "->" || is_cpp_library_type(raw, ctx) {
        return false;
    }
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return false };
    let site = match &decl.declared {
        Some(d) => d.site(),
        None => cpp_call_site(&caller, r, ctx),
    };
    cpp_class_written(raw, &site, r, ctx).is_undefined() && is_cpp_outside_qualified_type(raw, &site, ctx)
}

/// The shared tail of the two receiver-expression matchers.
fn match_cpp_receiver_expression(receiver: &super::cpp_scope::CppReceiverExpr, r: &UnresolvedRef, ctx: &Ctx, outside_is_null: bool) -> Tri<ResolvedRef> {
    let Some(object) = cpp_member_object(&receiver.expression, &receiver.op, r, ctx, 0) else { return Tri::Undefined };
    if is_cpp_pointer_type(&object.text) {
        return Tri::Undefined;
    }
    if is_cpp_library_type(&object.text, ctx) {
        return Tri::Null;
    }
    let found = match cpp_class_of_type(&object, r, ctx) {
        Tri::Undefined => {
            return if outside_is_null && is_cpp_outside_qualified_type(&object.text, &object.site(), ctx) { Tri::Null } else { Tri::Undefined };
        }
        Tri::Null => {
            let caller_scope = ctx.node_by_id(&r.from_node_id).map(|c| c.qualified_name.clone());
            return if is_cpp_template_parameter_type(&object) && Some(&object.scope) != caller_scope.as_ref() { Tri::Undefined } else { Tri::Null };
        }
        Tri::Value(f) => f,
    };
    if !found.exact || !is_cpp_class_kind(found.node.kind) {
        return Tri::Undefined;
    }
    if let Some(method) = cpp_method_of(&found.node, &r.reference_name, r, ctx) {
        return Tri::Value(instance_method(r, &method, 0.9));
    }
    if cpp_lacks_method(&found.node, ctx) {
        Tri::Null
    } else {
        Tri::Undefined
    }
}

/// `matchCppMemberChainCall`: `a.b->f()`.
pub fn match_cpp_member_chain_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    match cpp_member_chain_receiver(r, ctx) {
        Some(receiver) => match_cpp_receiver_expression(&receiver, r, ctx, true),
        None => Tri::Undefined,
    }
}

/// `matchCppUnspelledReceiverCall`: `(*p)->f()`, `v[i].f()`, `pair.first->f()`.
pub fn match_cpp_unspelled_receiver_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    match cpp_unspelled_receiver(r, ctx) {
        Some(receiver) => match_cpp_receiver_expression(&receiver, r, ctx, false),
        None => Tri::Undefined,
    }
}

// -- Construction -----------------------------------------------------------------------------------------

/// `cppTypeWrittenAt`: the type name written at the start of `text`.
fn cpp_type_written_at(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut end = 0usize;
    let mut first = true;
    loop {
        let segment = if first { re!(r"^(?:::\s*)?[A-Za-z_]\w*") } else { re!(r"^\s*::\s*[A-Za-z_]\w*") };
        first = false;
        let Some(name) = segment.find(&text[end..]) else {
            return if end > 0 { Some(text[..end].to_string()) } else { None };
        };
        end += name.end();
        let Some(open) = re!(r"^\s*<").find(&text[end..]) else { continue };
        end += open.end();
        let (mut angles, mut parens) = (1i64, 0i64);
        while angles > 0 {
            let Some(&c) = b.get(end) else { return None };
            if c == b'(' {
                parens += 1;
            } else if c == b')' {
                parens -= 1;
            } else if parens > 0 {
            } else if c == b'<' {
                angles += 1;
            } else if c == b'>' {
                angles -= 1;
            }
            end += 1;
        }
    }
}

/// `cppEnumWritten`.
fn cpp_enum_written(written: &str, site: &CppNameSite, ctx: &Ctx) -> Option<NodeRef> {
    let global = js_trim(written).starts_with("::");
    let stripped = strip_cpp_template_arguments(written);
    let path = re!(r"\s+", "g").replace_all_str(&stripped, "");
    let path = path.strip_prefix("::").unwrap_or(&path).to_string();
    let scopes: Vec<String> = if global { vec![String::new()] } else { cpp_scopes_within(&site.scope) };
    for scope in scopes {
        let qualified = if scope.is_empty() { path.clone() } else { format!("{scope}::{path}") };
        let enums: Vec<NodeRef> = ctx
            .get_nodes_by_qualified_name(&qualified)
            .iter()
            .filter(|n| n.kind == NodeKind::Enum && (n.file_path == site.file || !c_source_ext(&n.file_path)))
            .cloned()
            .collect();
        if !enums.is_empty() {
            return Some(enums.iter().find(|n| n.file_path == site.file).cloned().unwrap_or_else(|| enums[0].clone()));
        }
    }
    None
}

/// `cppNamesakeAround`: of same-named types in the file, the one the reference can see.
fn cpp_namesake_around(namesakes: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let holders: Vec<NodeRef> = ctx
        .get_nodes_in_file(&r.file_path)
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method | NodeKind::Class | NodeKind::Struct | NodeKind::Union))
        .cloned()
        .collect();
    let around: Vec<&NodeRef> = namesakes
        .iter()
        .filter(|cls| {
            let mut holder: Option<&NodeRef> = None;
            for n in &holders {
                if n.id == cls.id || n.start_line > cls.start_line || n.end_line < cls.end_line {
                    continue;
                }
                if holder.is_none_or(|h| n.end_line - n.start_line < h.end_line - h.start_line) {
                    holder = Some(n);
                }
            }
            holder.is_none_or(|h| h.start_line <= r.line && r.line <= h.end_line)
        })
        .collect();
    if around.len() == 1 {
        Some(around[0].clone())
    } else {
        None
    }
}

/// `matchCppConstruction`: `new T(...)` / `T(...)` / `T{...}` names the class `T`.
pub fn match_cpp_construction(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let caller = ctx.node_by_id(&r.from_node_id);
    let lines = ctx.get_file_lines(&r.file_path);
    let text = lines.as_ref().and_then(|l| l.get((r.line as usize).wrapping_sub(1))).map(|l| after_col(l, r.column).to_string());
    let (Some(caller), Some(text)) = (caller, text.filter(|t| !t.is_empty())) else { return Tri::Undefined };
    let specifiers = re!(r"^(?:(?:new|const|volatile|static|thread_local|constexpr|constinit|mutable|struct|class|union|typename)\s+)*")
        .find(&text)
        .map(|m| m.end())
        .unwrap_or(0);
    let Some(written) = cpp_type_written_at(&text[specifiers..]) else { return Tri::Undefined };
    let made = |node: &Node, exact: bool| -> ResolvedRef {
        if exact {
            ResolvedRef::new(r, node.id.clone(), 0.9, ResolvedBy::QualifiedName)
        } else {
            ResolvedRef::new(r, node.id.clone(), 0.7, ResolvedBy::ExactMatch)
        }
    };
    if cpp_declares_local_alias(&written, r, ctx) {
        let aliased = resolve_cpp_aliased_type(&written, r, ctx);
        let named = match &aliased {
            Tri::Value(a) if !a.pointer => cpp_aliased_class(a, ctx, Some(r)),
            _ => None,
        };
        let cls = named.and_then(|n| ctx.get_nodes_by_qualified_name(&n).iter().find(|c| is_cpp_class_kind(c.kind)).cloned());
        return match cls {
            Some(c) => Tri::Value(made(&c, true)),
            None => Tri::Null,
        };
    }
    let site = cpp_call_site(&caller, r, ctx);
    let found = match cpp_class_written(&written, &site, r, ctx) {
        Tri::Null => {
            return match cpp_enum_written(&written, &site, ctx) {
                Some(named) => Tri::Value(made(&named, true)),
                None => Tri::Null,
            };
        }
        Tri::Undefined => return Tri::Undefined,
        Tri::Value(f) => f,
    };
    let namesakes: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&found.node.qualified_name)
        .iter()
        .filter(|n| n.file_path == r.file_path && (is_cpp_class_kind(n.kind) || n.kind == NodeKind::TypeAlias))
        .cloned()
        .collect();
    let ty = if namesakes.len() > 1 { cpp_namesake_around(&namesakes, r, ctx) } else { Some(found.node.clone()) };
    match ty {
        Some(t) => Tri::Value(made(&t, found.exact)),
        None => Tri::Undefined,
    }
}

// -- Call chains --------------------------------------------------------------------------------------------

/// `cppChainInnerType`: the type of the call expression a `f().g()` chain starts with.
fn cpp_chain_inner_type(inner: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<CppWrittenType> {
    let Some(dot) = inner.rfind('.') else { return cpp_expression_type(&format!("{inner}()"), r, ctx, 1) };
    let recv = &inner[..dot];
    if !re!(r"^[A-Za-z_]\w*$").is_match(recv) {
        return None;
    }
    let op = cpp_member_operator(recv, r, ctx);
    let recv_type = if op.is_empty() { None } else { cpp_expression_type(recv, r, ctx, 1) };
    let followed = recv_type.and_then(|t| cpp_follow_written_aliases(&t, r, ctx))?;
    let object = if op == "->" {
        cpp_dereferenced(&followed, r, ctx)
    } else if is_cpp_pointer_type(&followed.text) {
        None
    } else {
        Some(followed)
    }?;
    let member = &inner[dot + 1..];
    if let Some(element) = cpp_library_element_of(&object, member, r, ctx) {
        return Some(element);
    }
    let cls = cpp_written_class(&object, r, ctx)?;
    cpp_return_type_of(&cpp_methods_of(&cls, member, r, ctx), ctx)
}

/// `matchCppChainOnWrittenType`.
fn match_cpp_chain_on_written_type(inner: &str, method: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let ty = cpp_chain_inner_type(inner, r, ctx);
    let followed = ty.and_then(|t| cpp_follow_written_aliases(&t, r, ctx));
    let Some(followed) = followed.filter(|f| !is_cpp_template_dependent_type(f, ctx)) else { return Tri::Undefined };
    let object = cpp_pointee(&followed).unwrap_or(followed);
    if is_cpp_library_type(&object.text, ctx) {
        return Tri::Null;
    }
    let found = cpp_class_of_type(&object, r, ctx);
    if found.is_undefined() && is_cpp_outside_qualified_type(&object.text, &object.site(), ctx) {
        return Tri::Null;
    }
    let found = match found {
        Tri::Value(f) if f.exact && is_cpp_class_kind(f.node.kind) => f,
        _ => return Tri::Undefined,
    };
    if let Some(target) = cpp_method_of(&found.node, method, r, ctx) {
        return Tri::Value(instance_method(r, &target, 0.85));
    }
    if cpp_lacks_method(&found.node, ctx) {
        Tri::Null
    } else {
        Tri::Undefined
    }
}

/// `matchCppCallChain`: `f().g()` — type `f`'s return value, then look `g` up on it.
pub fn match_cpp_call_chain(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let m = re!(r"^(.+)\(\)\.(\w+)$").captures(&r.reference_name)?;
    let (inner, method) = (m.text(1).filter(|s| !s.is_empty())?, m.text(2).filter(|s| !s.is_empty())?);
    let looked = match_cpp_chain_on_written_type(inner, method, r, ctx);
    if !looked.is_undefined() {
        return looked.value();
    }
    let cls = resolve_cpp_call_result_type(inner, r, ctx, 0)?;
    resolve_method_on_type(&cls, method, r, ctx, 0.85, ResolvedBy::InstanceMethod, None, 0)
}
