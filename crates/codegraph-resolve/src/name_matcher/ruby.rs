//! Ruby scope rules: a bare call reaches a method of the enclosing class, its superclasses and
//! the modules it mixes in.

use super::lang_shape::RefShape;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

memo_map!(RubyAncestryMemo, String, Rc<HashSet<String>>);

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
    ruby_ancestry(&here.qualified_name, ctx).contains(&method.qualified_name[..cut])
}

/// A class or module, its superclass chain and every module it includes, extends or prepends.
fn ruby_ancestry(qn: &str, ctx: &Ctx) -> Rc<HashSet<String>> {
    memoized::<RubyAncestryMemo>(ctx, qn.to_string(), || {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::from([qn.to_string()]);
        while let Some(q) = queue.pop_front() {
            if seen.len() >= 60 {
                break;
            }
            if !seen.insert(q.clone()) {
                continue;
            }
            for decl in ctx.get_nodes_by_qualified_name(&q).iter() {
                if decl.language != Language::Ruby || !matches!(decl.kind, NodeKind::Class | NodeKind::Module) {
                    continue;
                }
                let lines = lines_of(ctx, &decl.file_path);
                let outer = q.rfind("::").map(|i| q[..i].to_string()).unwrap_or_default();
                let head = lines.get((decl.start_line as usize).wrapping_sub(1)).cloned().unwrap_or_default();
                if let Some(sup) = re!(r"^\s*class\s+[\w:]+\s*<\s*(::)?([A-Z][\w:]*)").captures(&head) {
                    let scope = if sup.text(1).is_some() { "" } else { outer.as_str() };
                    queue.push_back(ruby_constant_qn(sup.text(2).unwrap_or(""), scope, ctx));
                }
                for line in js_slice(&lines, decl.start_line as i64, decl.end_line as i64) {
                    let Some(mix) = re!(r"^\s*(?:include|extend|prepend)\s+([A-Z:][\w:]*(?:\s*,\s*[A-Z:][\w:]*)*)").captures(line) else { continue };
                    for name in re!(r"\s*,\s*").split(mix.text(1).unwrap_or("")) {
                        queue.push_back(match name.strip_prefix("::") {
                            Some(rest) => ruby_constant_qn(rest, "", ctx),
                            None => ruby_constant_qn(name, &q, ctx),
                        });
                    }
                }
            }
        }
        Rc::new(seen)
    })
}

/// The qualified name a constant written in `scope` names: the innermost enclosing scope that declares it.
fn ruby_constant_qn(name: &str, scope: &str, ctx: &Ctx) -> String {
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
}
