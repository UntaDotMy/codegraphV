//! Solidity scope rules: a receiver-less call inside a contract reaches the methods of that
//! contract and of the contracts and interfaces it inherits.

use super::lang_shape::RefShape;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::{Language, Node, NodeKind};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

fn is_solidity_type_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Interface | NodeKind::Struct | NodeKind::Module | NodeKind::Trait)
}

memo_map!(SoliditySupersMemo, String, Rc<Vec<String>>);
memo_map!(SolidityHierarchyMemo, String, Rc<HashSet<String>>);

/// The contracts and interfaces a Solidity type lists after `is`.
fn solidity_supertypes_of(name: &str, ctx: &Ctx) -> Rc<Vec<String>> {
    memoized::<SoliditySupersMemo>(ctx, name.to_string(), || {
        let mut supers: Vec<String> = Vec::new();
        for decl in ctx.get_nodes_by_name(name).iter() {
            if decl.language != Language::Solidity || !is_solidity_type_kind(decl.kind) {
                continue;
            }
            let lines = lines_of(ctx, &decl.file_path);
            let joined = js_slice(&lines, decl.start_line as i64 - 1, decl.start_line as i64 + 10).join(" ");
            let text = re!(r"//[^\n]*|/\*.*?\*/", "g").replace_all_str(&joined, " ");
            let mut depth = 0i32;
            let mut head = String::new();
            for ch in text.chars() {
                if ch == '{' && depth == 0 {
                    break;
                }
                if ch == '(' {
                    depth += 1;
                } else if ch == ')' {
                    depth = (depth - 1).max(0);
                } else if depth == 0 {
                    head.push(ch);
                }
            }
            let clause = re!(r"\bis\b([\s\S]*)$").captures(&head).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
            for m in re!(r"([A-Za-z_]\w*)\s*(?=,|$)", "g").captures_iter(&clause) {
                supers.push(m.text(1).unwrap_or("").to_string());
            }
        }
        Rc::new(supers)
    })
}

/// Is the method `n` one a receiver-less call at the reference can reach (the enclosing
/// contract's own, or an inherited one)?
pub fn is_solidity_member_in_scope(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if n.kind != NodeKind::Method || n.language != Language::Solidity {
        return true;
    }
    let Some(cut) = n.qualified_name.rfind("::").filter(|c| *c > 0) else { return true };
    let owner = n.qualified_name[..cut].rsplit("::").next().unwrap_or("");
    let names = memoized::<SolidityHierarchyMemo>(ctx, format!("{}:{}", r.file_path, r.line), || {
        let mut names: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = ctx
            .get_nodes_in_file(&r.file_path)
            .iter()
            .filter(|c| is_solidity_type_kind(c.kind) && c.start_line <= r.line && c.end_line >= r.line)
            .map(|c| c.name.clone())
            .collect();
        while let Some(name) = queue.pop_front() {
            if names.len() >= 60 {
                break;
            }
            if !names.insert(name.clone()) {
                continue;
            }
            queue.extend(solidity_supertypes_of(&name, ctx).iter().cloned());
        }
        Rc::new(names)
    });
    names.contains(owner)
}

/// Is the call written `name(…)` with nothing in front of it?
fn is_receiver_less_solidity_call(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if r.language != Language::Solidity || !is_calls(r) || !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        return false;
    }
    let Some(line) = line_at(ctx, &r.file_path, r.line).or_else(|| {
        let idx = (r.line as usize).checked_sub(1)?;
        ctx.read_file(&r.file_path).and_then(|s| s.split('\n').nth(idx).map(String::from))
    }) else {
        return false;
    };
    if line.is_empty() {
        return false;
    }
    Re::cached(&format!(r"(?<![\w$]){}\s*\(", r.reference_name), "").find(&line).is_some_and(|m| !re!(r"\.\s*$").is_match(&line[..m.start()]))
}

/// The Solidity contribution to [`super::lang_shape::LangShape`]: a receiver-less call.
pub struct SolidityShape;

impl SolidityShape {
    pub fn of(r: &UnresolvedRef, ctx: &Ctx) -> Option<SolidityShape> {
        is_receiver_less_solidity_call(r, ctx).then_some(SolidityShape)
    }
}

impl RefShape for SolidityShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        !is_solidity_member_in_scope(n, r, ctx)
    }
}
