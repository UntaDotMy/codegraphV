//! A TS/JS call written `super.run(…)` runs the method of the class's parent (#2497) — port of
//! `src/resolution/js-super-calls.ts`.
//!
//! The parent is the one the class `extends`, or the nearest ancestor of that one that has the
//! method; a static method's `super` reaches the parent's static methods. The parent comes from
//! the class's resolved `extends` edge, which the resolver persists before it resolves calls. A
//! parent outside the project, or an ancestry without the method, links nothing.

use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_extract::strip_comments::{blank_string_contents, strip_comments_for_regex};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{utf16_from, utf16_prefix};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::collections::HashSet;
use std::rc::Rc;

/// How far up a class's ancestry a `super` call is looked for.
const MAX_ANCESTRY: usize = 8;

/// Whether `super` calls of `language` are read where each call starts (isJsSuperCallSite).
pub fn is_js_super_call_language(language: Language) -> bool {
    matches!(language, Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx)
}

/// The file's lines, shared with the resolver's per-file cache (a call site must not copy the file).
fn file_lines(file_path: &str, ctx: &Ctx) -> Option<Rc<Vec<String>>> {
    match ctx.get_file_lines(file_path) {
        Some(lines) => Some(lines),
        None => ctx
            .read_file(file_path)
            .map(|s| Rc::new(re!(r"\r?\n").split(&s).into_iter().map(String::from).collect())),
    }
}

/// Whether the call `r` records is written on `super` where it starts: `super.run(…)`, the
/// member on the next line or two included. A `this.run()` on the same line is not one.
pub fn is_js_super_call_site(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if !r.is_kind(EdgeKind::Calls) || !is_js_super_call_language(r.language) || !re!(r"^[A-Za-z_$][\w$]*$").is_match(&r.reference_name) {
        return false;
    }
    let Some(lines) = file_lines(&r.file_path, ctx) else { return false };
    let Some(line) = lines.get((r.line as usize).wrapping_sub(1)) else { return false };
    let column = r.column as usize;
    if line.is_empty() || !utf16_from(line, column).starts_with("super") {
        return false;
    }
    let written = Re::cached(&format!(r"^super\s*\.\s*{}(?![\w$])", escape_regex(&r.reference_name)), "");
    if written.is_match(utf16_from(line, column)) {
        return true;
    }
    let from = r.line as usize - 1;
    let joined = lines[from..(from + 3).min(lines.len())].join("\n");
    written.is_match(utf16_from(&joined, column))
}

/// The method a TS/JS `super.name(…)` call runs. `Undefined` when the call isn't written on
/// `super`, or the index can't place it in a member of a class; `Null` when no ancestor the
/// project declares has the method.
pub fn match_js_super_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if !is_js_super_call_site(r, ctx) {
        return Tri::Undefined;
    }
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return Tri::Undefined };
    let Some((member, cls)) = class_member_around(&caller, ctx) else { return Tri::Undefined };
    if writes_class_before(&member, r, ctx) {
        return Tri::Undefined;
    }
    match inherited_method(&cls, &r.reference_name, member.is_static == Some(true), ctx) {
        Some(target) => Tri::Value(ResolvedRef::new(r, target.id.clone(), 0.9, ResolvedBy::InstanceMethod)),
        None => Tri::Null,
    }
}

/// The class member a call made in `caller` belongs to, and its class: the caller itself when
/// it is a method or a field of a class, or the member an arrow function nested in it is
/// written in, whose `super` is the member's.
fn class_member_around(caller: &NodeRef, ctx: &Ctx) -> Option<(NodeRef, NodeRef)> {
    let mut member = caller.clone();
    let mut depth = 0;
    while member.kind == NodeKind::Function && depth < 4 {
        member = owner_of(&member, ctx, |n| matches!(n.kind, NodeKind::Function | NodeKind::Method | NodeKind::Property))?;
        depth += 1;
    }
    if member.kind != NodeKind::Method && member.kind != NodeKind::Property {
        return None;
    }
    let cls = owner_of(&member, ctx, |n| n.kind == NodeKind::Class)?;
    Some((member, cls))
}

/// The node `node` is declared in, by its qualified name, in its file and around it.
fn owner_of(node: &Node, ctx: &Ctx, accept: impl Fn(&Node) -> bool) -> Option<NodeRef> {
    let cut = node.qualified_name.rfind("::").filter(|&c| c > 0)?;
    ctx.get_nodes_by_qualified_name(&node.qualified_name[..cut])
        .iter()
        .find(|n| accept(n) && n.file_path == node.file_path && n.start_line <= node.start_line && n.end_line >= node.end_line)
        .cloned()
}

/// Whether a `class` is written between where `member` starts and the call, outside comments
/// and strings: a class expression's methods have no nodes, so its `super` is that class's.
fn writes_class_before(member: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(lines) = file_lines(&r.file_path, ctx) else { return false };
    if member.start_line > r.line {
        return false;
    }
    let from = (member.start_line as usize).saturating_sub(1);
    let to = (r.line as usize).min(lines.len());
    let mut span: Vec<String> = if from < to { lines[from..to].to_vec() } else { Vec::new() };
    if span.is_empty() {
        span.push(String::new());
    }
    let last = span.len() - 1;
    span[last] = utf16_prefix(&span[last], r.column as usize).to_string();
    let first_cut = if member.start_line == r.line { member.start_column.min(r.column) } else { member.start_column };
    span[0] = utf16_from(&span[0], first_cut as usize).to_string();
    let text = span.join("\n");
    let class_word = re!(r"(?<![\w$.])class(?![\w$])");
    if !class_word.is_match(&text) {
        return false;
    }
    class_word.is_match(&blank_string_contents(&strip_comments_for_regex(&text, "javascript"), None))
}

/// The nearest method named `name` of `cls`'s ancestors, static or instance as asked.
fn inherited_method(cls: &NodeRef, name: &str, is_static: bool, ctx: &Ctx) -> Option<NodeRef> {
    let mut seen: HashSet<String> = HashSet::from([cls.id.clone()]);
    let mut level = superclasses_of(cls, ctx);
    let mut depth = 0;
    while depth < MAX_ANCESTRY && !level.is_empty() {
        let mut next: Vec<NodeRef> = Vec::new();
        for parent in level {
            if !seen.insert(parent.id.clone()) {
                continue;
            }
            if let Some(own) = own_method(&parent, name, is_static, ctx) {
                return Some(own);
            }
            next.extend(superclasses_of(&parent, ctx));
        }
        level = next;
        depth += 1;
    }
    None
}

fn superclasses_of(cls: &Node, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_superclass_nodes(&cls.id).iter().filter(|n| n.kind == NodeKind::Class && is_js_super_call_language(n.language)).cloned().collect()
}

/// A method `cls` itself declares under `name`, static or instance as asked; of a TypeScript
/// method's overload signatures and its body, the body, which comes last.
fn own_method(cls: &Node, name: &str, is_static: bool, ctx: &Ctx) -> Option<NodeRef> {
    let mut own: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&format!("{}::{name}", cls.qualified_name))
        .iter()
        .filter(|m| {
            m.kind == NodeKind::Method
                && m.file_path == cls.file_path
                && m.start_line >= cls.start_line
                && m.end_line <= cls.end_line
                && (m.is_static == Some(true)) == is_static
        })
        .cloned()
        .collect();
    own.sort_by_key(|m| (m.start_line, m.start_column));
    own.pop()
}
