//! A Ruby constant written with its namespace — `Billing::ChargeCard`, `::Foo` — is the class,
//! module or constant Ruby's lookup finds from where it is written, and a call on one
//! (`Billing::ChargeCard.call`) a method of that class or module (#2503) — port of
//! `src/resolution/ruby-constant-paths.ts`.
//!
//! Both resolve here alone: a path the project doesn't declare is a library's, and a class the
//! path names without the method has it from somewhere the index can't see. Production code never
//! names a class a test declares. A bare constant is left to the name strategies.

use crate::ctx::Ctx;
use crate::name_matcher::ruby::{ruby_ancestry, ruby_methods_of_kind_first, RubyMethodKind};
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_search::query_utils::is_test_path;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};

fn is_namespace_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Class | NodeKind::Module)
}

fn is_value_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Variable | NodeKind::Constant)
}

/// What a Ruby `references` ref written as a constant path names, or the method a call on one
/// reaches. Undefined for any other ref; Null when the project declares no such constant, or the
/// class or module it names has no method of the name.
pub fn match_ruby_constant_path_ref(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if r.language != Language::Ruby {
        return Tri::Undefined;
    }
    if r.is_kind(EdgeKind::References) {
        let Some(m) = re!(r"^(::)?([A-Z]\w*(?:::[A-Z]\w*)*)$").captures(&r.reference_name) else { return Tri::Undefined };
        let (absolute, path) = (m.text(1).is_some(), m.text(2).unwrap_or(""));
        if !absolute && !path.contains("::") {
            return Tri::Undefined;
        }
        return match constants_named(path, absolute, r, ctx).first() {
            Some(t) => Tri::Value(ResolvedRef::new(r, t.id.clone(), 0.9, ResolvedBy::QualifiedName)),
            None => Tri::Null,
        };
    }
    if r.is_kind(EdgeKind::Calls) {
        let Some(m) = re!(r"^(::)?([A-Z]\w*(?:::[A-Z]\w*)*)\.([A-Za-z_]\w*[?!=]?)$").captures(&r.reference_name) else { return Tri::Undefined };
        let (absolute, path, name) = (m.text(1).is_some(), m.text(2).unwrap_or(""), m.text(3).unwrap_or(""));
        if !absolute && !path.contains("::") {
            return Tri::Undefined;
        }
        let owner = constants_named(path, absolute, r, ctx).into_iter().next();
        let method = owner.filter(|o| is_namespace_kind(o.kind)).and_then(|o| method_of(&o.qualified_name, name, r, ctx));
        return match method {
            Some((node, own)) => Tri::Value(ResolvedRef::new(r, node.id.clone(), if own { 0.85 } else { 0.8 }, ResolvedBy::QualifiedName)),
            None => Tri::Null,
        };
    }
    Tri::Undefined
}

/// The declarations of the constant `path` names where `r` is written: the first of
/// `<scope>::<path>`, `<outer scope>::<path>`, … `<path>` the project declares.
fn constants_named(path: &str, absolute: bool, r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    let production = !is_test_path(&r.file_path);
    let mut prefix = if absolute { String::new() } else { lexical_scope(r, ctx) };
    loop {
        let qn = if prefix.is_empty() { path.to_string() } else { format!("{prefix}::{path}") };
        let found: Vec<NodeRef> = ctx
            .get_nodes_by_qualified_name(&qn)
            .iter()
            .filter(|n| n.language == Language::Ruby && (is_namespace_kind(n.kind) || is_value_kind(n.kind)) && !(production && is_test_path(&n.file_path)))
            .cloned()
            .collect();
        if !found.is_empty() {
            return ordered(found, r);
        }
        if prefix.is_empty() {
            return Vec::new();
        }
        prefix = prefix.rfind("::").map_or_else(String::new, |c| prefix[..c].to_string());
    }
}

/// The qualified name of the innermost class or module around `r`, or '' at the top level.
fn lexical_scope(r: &UnresolvedRef, ctx: &Ctx) -> String {
    let mut inner: Option<NodeRef> = None;
    for n in ctx.get_nodes_in_file(&r.file_path).iter() {
        if !is_namespace_kind(n.kind) || n.language != Language::Ruby || n.start_line > r.line || n.end_line < r.line {
            continue;
        }
        let later = inner.as_ref().is_none_or(|i| n.start_line > i.start_line || (n.start_line == i.start_line && n.start_column > i.start_column));
        if later {
            inner = Some(n.clone());
        }
    }
    inner.map_or_else(String::new, |n| n.qualified_name.clone())
}

/// The method `name` of the class or module `owner`: its own, else the first of its ancestry
/// that declares one. Of a class's `def self.m` and `def m`, which share one qualified name, the
/// call on the class reaches the class method. Production code never reaches one a test declares.
fn method_of(owner: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<(NodeRef, bool)> {
    let production = !is_test_path(&r.file_path);
    for qn in ruby_ancestry(owner, ctx).iter() {
        let methods: Vec<NodeRef> = ctx
            .get_nodes_by_qualified_name(&format!("{qn}::{name}"))
            .iter()
            .filter(|n| n.kind == NodeKind::Method && n.language == Language::Ruby && !(production && is_test_path(&n.file_path)))
            .cloned()
            .collect();
        if !methods.is_empty() {
            return ruby_methods_of_kind_first(ordered(methods, r), Some(RubyMethodKind::Class)).into_iter().next().map(|m| (m, qn == owner));
        }
    }
    None
}

/// Anything before a value, then the reference's own file, then production code; the index's order
/// within each.
fn ordered(mut nodes: Vec<NodeRef>, r: &UnresolvedRef) -> Vec<NodeRef> {
    let rank = |n: &Node| -> u8 {
        (if is_value_kind(n.kind) { 4 } else { 0 }) + (if n.file_path == r.file_path { 0 } else { 2 }) + u8::from(is_test_path(&n.file_path))
    };
    if nodes.len() >= 2 {
        nodes.sort_by_key(|n| rank(n));
    }
    nodes
}
