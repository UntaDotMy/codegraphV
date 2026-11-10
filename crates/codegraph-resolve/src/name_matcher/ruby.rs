//! Ruby scope rules: a bare call reaches a method of the enclosing class, its superclasses and
//! the modules it mixes in.

use super::lang_shape::RefShape;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

memo_map!(RubyAncestryMemo, String, Rc<indexmap::IndexSet<String>>);
memo_map!(RubyMixinsMemo, String, Rc<RubyMixins>);
memo_map!(RubyMethodOwnersMemo, String, Rc<HashMap<String, Vec<NodeRef>>>);
memo_map!(RubyFileScopesMemo, String, Rc<Vec<NodeRef>>);

/// What one Ruby class or module declaration mixes in, found by constant lookup from its own body.
#[derive(Clone)]
pub struct RubyMixins {
    pub prepended: Vec<String>,
    pub included: Vec<String>,
    pub extended: Vec<String>,
    pub superclass: Option<String>,
}

/// Is the method `method` one a bare call inside the class around the reference can reach?
pub fn is_ruby_method_in_scope(method: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(cut) = method.qualified_name.rfind("::") else { return true };
    let in_file = ctx.get_nodes_in_file(&r.file_path);
    let mut around: Vec<&NodeRef> = in_file.iter().filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Module) && n.start_line <= r.line && n.end_line >= r.line).collect();
    around.sort_by_key(|n| n.end_line.saturating_sub(n.start_line));
    let Some(here) = around.first() else { return true };
    if here.kind != NodeKind::Class {
        return true;
    }
    ruby_ancestry(&here.qualified_name, ctx, false).contains(&method.qualified_name[..cut])
}

fn is_ruby_namespace(n: &Node) -> bool {
    n.language == Language::Ruby && matches!(n.kind, NodeKind::Class | NodeKind::Module)
}

fn unshift(list: &mut Vec<String>, names: Vec<String>) {
    if names.is_empty() {
        return;
    }
    let mut combined = names;
    combined.append(list);
    *list = combined;
}

/// Mixins of one declaration, from its own body only. A nested class keeps its own. The module
/// mixed in last is first in the list.
fn ruby_mixins_of(decl: &Node, ctx: &Ctx) -> Rc<RubyMixins> {
    memoized::<RubyMixinsMemo>(ctx, decl.id.clone(), || {
        let q = decl.qualified_name.as_str();
        let mut found = RubyMixins { prepended: Vec::new(), included: Vec::new(), extended: Vec::new(), superclass: None };
        let lines = lines_of(ctx, &decl.file_path);
        let head = lines.get((decl.start_line as usize).wrapping_sub(1)).cloned().unwrap_or_default();
        if let Some(sup) = re!(r"^\s*class\s+[\w:]+\s*<\s*(::)?([A-Z][\w:]*)").captures(&head) {
            let scope = if sup.text(1).is_some() || !q.contains("::") { "" } else { q.rfind("::").map(|i| &q[..i]).unwrap_or("") };
            found.superclass = Some(ruby_constant_qn(sup.text(2).unwrap_or(""), scope, ctx));
        }
        let nested: Vec<NodeRef> = ctx
            .get_nodes_in_file(&decl.file_path)
            .iter()
            .filter(|n| {
                matches!(n.kind, NodeKind::Class | NodeKind::Module)
                    && n.id != decl.id
                    && n.start_line > decl.start_line
                    && n.end_line < decl.end_line
            })
            .cloned()
            .collect();
        let start = decl.start_line as usize;
        let end = (decl.end_line as usize).min(lines.len());
        if start < end {
            for (offset, line) in lines[start..end].iter().enumerate() {
                // A module a call returns (`include RouteSet.new.tap { … }.url_helpers`)
                // is not a mixin of the constant the call starts at.
                let Some(mix) = re!(r"^\s*(include|extend|prepend)\s+([A-Z:][\w:]*(?:\s*,\s*[A-Z:][\w:]*)*)(?![\w:]|\s*[.(])").captures(line) else {
                    continue;
                };
                let line_no = (start + offset + 1) as u32;
                if nested.iter().any(|n| n.start_line <= line_no && n.end_line >= line_no) {
                    continue;
                }
                let mut names = Vec::new();
                for name in re!(r"\s*,\s*").split(mix.text(2).unwrap_or("")) {
                    if name.is_empty() {
                        continue;
                    }
                    names.push(match name.strip_prefix("::") {
                        Some(rest) => ruby_constant_qn(rest, "", ctx),
                        None => ruby_constant_qn(name, q, ctx),
                    });
                }
                match mix.text(1).unwrap_or("") {
                    "include" => unshift(&mut found.included, names),
                    "prepend" => unshift(&mut found.prepended, names),
                    _ => unshift(&mut found.extended, names),
                }
            }
        }
        Rc::new(found)
    })
}

/// A class's qualified name, its superclasses, and every module mixed into them, in the order
/// Ruby looks a method up. For an instance, a module the class `extend`s is left out.
pub fn ruby_ancestry(qn: &str, ctx: &Ctx, instance: bool) -> Rc<indexmap::IndexSet<String>> {
    let key = if instance { format!("instance {qn}") } else { qn.to_string() };
    memoized::<RubyAncestryMemo>(ctx, key, || {
        let mut seen: HashSet<String> = HashSet::new();
        let mut order: indexmap::IndexSet<String> = indexmap::IndexSet::new();
        fn visit(q: &str, instance: bool, seen: &mut HashSet<String>, order: &mut indexmap::IndexSet<String>, ctx: &Ctx) {
            if seen.contains(q) || seen.len() >= 250 {
                return;
            }
            seen.insert(q.to_string());
            let mixins: Vec<Rc<RubyMixins>> = ctx
                .get_nodes_by_qualified_name(q)
                .iter()
                .filter(|decl| is_ruby_namespace(decl))
                .map(|decl| ruby_mixins_of(decl, ctx))
                .collect();
            for mixin in &mixins {
                for name in &mixin.prepended {
                    visit(name, instance, seen, order, ctx);
                }
            }
            order.insert(q.to_string());
            for mixin in &mixins {
                for name in &mixin.included {
                    visit(name, instance, seen, order, ctx);
                }
                if !instance {
                    for name in &mixin.extended {
                        visit(name, instance, seen, order, ctx);
                    }
                }
            }
            for mixin in &mixins {
                if let Some(sup) = &mixin.superclass {
                    visit(sup, instance, seen, order, ctx);
                }
            }
        }
        visit(qn, instance, &mut seen, &mut order, ctx);
        Rc::new(order)
    })
}

/// The Ruby methods `name` whose owner is exactly `owner`.
fn ruby_methods_of(owner: &str, name: &str, ctx: &Ctx) -> Vec<NodeRef> {
    let owners = memoized::<RubyMethodOwnersMemo>(ctx, name.to_string(), || {
        let mut by_owner: HashMap<String, Vec<NodeRef>> = HashMap::new();
        for n in ctx.get_nodes_by_name(name).iter() {
            if n.kind != NodeKind::Method || n.language != Language::Ruby {
                continue;
            }
            let Some(at) = n.qualified_name.rfind("::") else { continue };
            by_owner.entry(n.qualified_name[..at].to_string()).or_default().push(Rc::clone(n));
        }
        Rc::new(by_owner)
    });
    owners.get(owner).cloned().unwrap_or_default()
}

/// The qualified name of the innermost class or module around `r`, or "" at the top of the file.
pub fn ruby_scope_at(r: &UnresolvedRef, ctx: &Ctx) -> String {
    let scopes = memoized::<RubyFileScopesMemo>(ctx, r.file_path.clone(), || {
        let mut scopes: Vec<NodeRef> = ctx
            .get_nodes_in_file(&r.file_path)
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Module))
            .cloned()
            .collect();
        scopes.sort_by_key(|n| n.end_line.saturating_sub(n.start_line));
        Rc::new(scopes)
    });
    scopes.iter().find(|n| n.start_line <= r.line && n.end_line >= r.line).map(|n| n.qualified_name.clone()).unwrap_or_default()
}

/// The instance method a value of the class written `type_name` answers from its real ancestors.
pub fn ruby_instance_method_of(type_name: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let looked = ruby_constant_qn(type_name, &ruby_scope_at(r, ctx), ctx);
    let classes: Vec<String> = if ctx.get_nodes_by_qualified_name(&looked).iter().any(|n| is_ruby_namespace(n)) {
        vec![looked]
    } else {
        let short = type_name.rsplit("::").next().unwrap_or(type_name);
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for n in ctx.get_nodes_by_name(short).iter() {
            if is_ruby_namespace(n) && seen.insert(n.qualified_name.clone()) {
                out.push(n.qualified_name.clone());
            }
        }
        out
    };
    for qn in classes {
        for owner in ruby_ancestry(&qn, ctx, true).iter() {
            let methods = ruby_methods_of(owner, name, ctx);
            if !methods.is_empty() {
                return ruby_methods_of_kind_first(methods, Some(RubyMethodKind::Instance)).into_iter().next();
            }
        }
    }
    None
}

/// The class method `name` a Ruby class answers past its own `def self`.
fn ruby_class_method_of(qn: &str, name: &str, ctx: &Ctx) -> Option<NodeRef> {
    let named = |owner: &str, is_static: bool| -> Option<NodeRef> {
        ruby_methods_of(owner, name, ctx).into_iter().find(|n| (n.is_static == Some(true)) == is_static)
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut cls = Some(qn.to_string());
    while let Some(current) = cls {
        if !seen.insert(current.clone()) {
            break;
        }
        let decls: Vec<NodeRef> = ctx.get_nodes_by_qualified_name(&current).iter().filter(|n| is_ruby_namespace(n)).cloned().collect();
        if current != qn {
            if let Some(own) = named(&current, true) {
                return Some(own);
            }
        }
        for decl in &decls {
            let mixins = ruby_mixins_of(decl, ctx);
            for module in &mixins.extended {
                for owner in ruby_ancestry(module, ctx, true).iter() {
                    if let Some(method) = named(owner, false) {
                        return Some(method);
                    }
                }
            }
            for module in &mixins.included {
                for owner in ruby_ancestry(module, ctx, true).iter() {
                    if let Some(method) = named(&format!("{owner}::ClassMethods"), false) {
                        return Some(method);
                    }
                }
            }
        }
        cls = decls.iter().find_map(|decl| ruby_mixins_of(decl, ctx).superclass.clone());
    }
    None
}

/// The method the first ancestor of `qn` declares, past `qn` itself. A class-method call walks
/// `def self`, extended modules, and included `ClassMethods` modules.
pub fn ruby_ancestor_method(qn: &str, name: &str, ctx: &Ctx, kind: Option<RubyMethodKind>) -> Option<NodeRef> {
    if kind == Some(RubyMethodKind::Class) {
        return ruby_class_method_of(qn, name, ctx);
    }
    for owner in ruby_ancestry(qn, ctx, false).iter() {
        if owner == qn {
            continue;
        }
        let methods = ruby_methods_of(owner, name, ctx);
        let class_owner = ctx.get_nodes_by_qualified_name(owner).iter().any(|d| d.kind == NodeKind::Class);
        let reached = methods.iter().find(|n| n.is_static != Some(true)).cloned().or_else(|| if class_owner { methods.first().cloned() } else { None });
        if reached.is_some() {
            return reached;
        }
    }
    None
}

/// The qualified name a constant written in `scope` names: the innermost enclosing scope that declares it.
pub fn ruby_constant_qn(name: &str, scope: &str, ctx: &Ctx) -> String {
    let mut prefix = scope.to_string();
    loop {
        let qn = if prefix.is_empty() { name.to_string() } else { format!("{prefix}::{name}") };
        if ctx.get_nodes_by_qualified_name(&qn).iter().any(|n| n.language == Language::Ruby && matches!(n.kind, NodeKind::Class | NodeKind::Module)) {
            return qn;
        }
        if prefix.is_empty() {
            return name.to_string();
        }
        prefix = prefix.rfind("::").map(|i| prefix[..i].to_string()).unwrap_or_default();
    }
}

/// The Ruby contribution to [`super::lang_shape::LangShape`]: a bare call.
pub struct RubyShape;

impl RubyShape {
    pub fn of(r: &UnresolvedRef) -> Option<RubyShape> {
        (r.language == Language::Ruby && is_calls(r) && re!(r"^[A-Za-z_]\w*[?!]?$").is_match(&r.reference_name)).then_some(RubyShape)
    }
}

impl RefShape for RubyShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        n.kind == NodeKind::Method && !is_ruby_method_in_scope(n, r, ctx)
    }

    fn rank_exact(&self, pool: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<Vec<NodeRef>> {
        Some(ruby_methods_of_caller_kind(pool, r, ctx))
    }
}

/// Which of a Ruby class's two methods of one name a call reaches: a call on the class
/// (`Widget.build`) its class method, `def self.build` or one in `class << self`; a call on an
/// instance (`widget.build`) its instance method, `def build`. Both share one qualified name,
/// `Widget::build`, and only `is_static` tells them apart (#2834).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RubyMethodKind {
    Class,
    Instance,
}

impl RubyMethodKind {
    pub fn key(kind: Option<RubyMethodKind>) -> &'static str {
        match kind {
            None => "",
            Some(RubyMethodKind::Class) => "class",
            Some(RubyMethodKind::Instance) => "instance",
        }
    }
}

/// `methods` with the ones of `kind` first, in their own order: a pick of the first then takes one
/// of the kind wherever there is one, and still the first otherwise. Methods of other languages
/// are never static-split.
pub fn ruby_methods_of_kind_first(methods: Vec<NodeRef>, kind: Option<RubyMethodKind>) -> Vec<NodeRef> {
    let Some(kind) = kind else { return methods };
    if methods.len() < 2 {
        return methods;
    }
    let fits = |m: &NodeRef| m.language == Language::Ruby && (m.is_static == Some(true)) == (kind == RubyMethodKind::Class);
    let first: Vec<NodeRef> = methods.iter().filter(|m| fits(m)).cloned().collect();
    if first.is_empty() || first.len() == methods.len() {
        return methods;
    }
    let mut out = first;
    out.extend(methods.into_iter().filter(|m| !fits(m)));
    out
}

/// Of a Ruby class's two methods of one name, `def build` and `def self.build` (both
/// `Widget::build`), the one a bare call means: a bare call is a call on `self`, so in an instance
/// method the instance method, and in a class method the class method. Only such pairs are split,
/// and only when the call is written in a method.
fn ruby_methods_of_caller_kind(candidates: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Vec<NodeRef> {
    if candidates.len() < 2 {
        return candidates.to_vec();
    }
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return candidates.to_vec() };
    if caller.kind != NodeKind::Method || caller.language != Language::Ruby {
        return candidates.to_vec();
    }
    let want_class = caller.is_static == Some(true);
    let is_ruby_method = |n: &Node| n.kind == NodeKind::Method && n.language == Language::Ruby;
    // Per qualified name, which kinds the candidates declare: 1 class, 2 instance.
    let mut kinds: std::collections::HashMap<&str, u8> = std::collections::HashMap::new();
    for n in candidates {
        if is_ruby_method(n) {
            *kinds.entry(n.qualified_name.as_str()).or_insert(0) |= if n.is_static == Some(true) { 1 } else { 2 };
        }
    }
    let twin_of_other_kind =
        |n: &Node| is_ruby_method(n) && kinds.get(n.qualified_name.as_str()) == Some(&3) && (n.is_static == Some(true)) != want_class;
    if candidates.iter().any(|n| twin_of_other_kind(n)) {
        candidates.iter().filter(|n| !twin_of_other_kind(n)).cloned().collect()
    } else {
        candidates.to_vec()
    }
}
