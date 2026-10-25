//! The classes a C++ call site sits in, and the implicit-`this` / unspelled-receiver readings of
//! a call — port of `matchCppImplicitThisCall`, `cppImplicitReceiver`, `cppMemberOwner`,
//! `cppEnclosingClasses` and `cppScopeMethod`.

use super::cpp_decl::{cpp_skip_space, esc};
use super::cpp_locals::is_cpp_local_name;
use super::cpp_method::{cpp_member_macro_arguments, cpp_method_in, cpp_using_declares, is_cpp_dependent_base};
use super::cpp_overload::cpp_overload_for;
use super::cpp_types::{cpp_closing_bracket, cpp_last_segment, cpp_test_fixture, is_c_family_node, is_cpp_class_kind};
use super::exact::prefer_call_site_file;
use super::util::{after_col, before_col, range_within, same_range};
use crate::cpp_includers::cpp_includers;
use crate::cpp_raw_strings::{cpp_literals_blanked, cpp_non_code_lines};
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{EdgeKind, Node, NodeKind};
use std::collections::HashSet;

/// `'bare' | 'this'`: how a member call reaches the current object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Receiver {
    Bare,
    This,
}

/// `cppClassDecls`: the class/struct/union declarations of `qualified_name`.
pub fn cpp_class_decls(qualified_name: &str, ctx: &Ctx) -> Vec<NodeRef> {
    ctx.get_nodes_by_qualified_name(qualified_name).iter().filter(|n| is_cpp_class_kind(n.kind) && is_c_family_node(n)).cloned().collect()
}

/// `cppMemberOwner`: the class whose member the caller is (or defines out of line).
pub fn cpp_member_owner(caller: &Node, ctx: &Ctx) -> Option<String> {
    if is_cpp_class_kind(caller.kind) {
        return Some(caller.qualified_name.clone());
    }
    if !matches!(caller.kind, NodeKind::Method | NodeKind::Function) {
        return None;
    }
    let cut = caller.qualified_name.rfind("::").filter(|c| *c > 0)?;
    let owner = caller.qualified_name[..cut].to_string();
    if caller.kind == NodeKind::Function {
        let head = ctx
            .get_file_lines(&caller.file_path)
            .map(|lines| {
                let from = (caller.start_line as usize).saturating_sub(1).min(lines.len());
                let to = (caller.start_line as usize + 2).min(lines.len());
                lines[from..to].join(" ")
            })
            .unwrap_or_default();
        let cls = esc(cpp_last_segment(&owner));
        let f = esc(&caller.name);
        if !Re::cached(&format!(r"\b{cls}\s*(?:<[^<>;{{}}]*>\s*)?::\s*~?{f}\s*\("), "").is_match(&head) {
            return None;
        }
    }
    if !cpp_class_decls(&owner, ctx).is_empty() {
        return Some(owner);
    }
    let outer = owner.rfind("::").filter(|o| *o > 0)?;
    if !cpp_class_decls(&owner[..outer], ctx).is_empty() {
        Some(owner)
    } else {
        None
    }
}

/// `CppClassScope`: one enclosing class and the declarations of it that are in reach.
#[derive(Clone, Debug)]
pub struct CppClassScope {
    pub qualified_name: String,
    pub decls: Vec<NodeRef>,
    /// The scope's declaration contains the inner node.
    pub holds: bool,
    /// Some declarations of the class were left out.
    pub narrowed: bool,
}

/// `cppScopeDeclarations`.
pub fn cpp_scope_declarations(qualified_name: &str, decls: Vec<NodeRef>, inner: &Node, ctx: &Ctx) -> CppClassScope {
    let total = decls.len();
    let scope = |kept: Vec<NodeRef>, holds: bool| CppClassScope {
        qualified_name: qualified_name.to_string(),
        narrowed: kept.len() < total,
        decls: kept,
        holds,
    };
    let holding: Vec<NodeRef> = decls.iter().filter(|d| d.file_path == inner.file_path && range_within(inner, d)).cloned().collect();
    if !holding.is_empty() {
        return scope(holding, true);
    }
    let local: Vec<NodeRef> = decls.iter().filter(|d| d.file_path == inner.file_path).cloned().collect();
    if !local.is_empty() || decls.len() < 2 {
        return scope(if local.is_empty() { decls } else { local }, false);
    }
    let included: Vec<NodeRef> = decls.iter().filter(|d| cpp_includers(&d.file_path, ctx).contains(&inner.file_path)).cloned().collect();
    scope(if included.is_empty() { decls } else { included }, false)
}

/// `cppEnclosingClasses`: the classes around the caller, innermost first.
pub fn cpp_enclosing_classes(owner: &str, caller: &Node, ctx: &Ctx) -> Vec<CppClassScope> {
    let mut scopes = Vec::new();
    let mut qualified_name = owner.to_string();
    let mut inner: NodeRef = std::rc::Rc::new(caller.clone());
    let mut hops = 0;
    while hops < 8 && !qualified_name.is_empty() {
        let decls = cpp_class_decls(&qualified_name, ctx);
        if decls.is_empty() && hops > 0 {
            let f = ctx
                .get_nodes_in_file_named(&inner.file_path, cpp_last_segment(&qualified_name))
                .into_iter()
                .find(|n| matches!(n.kind, NodeKind::Method | NodeKind::Function) && range_within(&inner, n) && !same_range(&inner, n));
            let f_owner = f.as_ref().and_then(|f| cpp_member_owner(f, ctx));
            match (f, f_owner) {
                (Some(f), Some(f_owner)) => {
                    qualified_name = f_owner;
                    inner = f;
                    hops += 1;
                    continue;
                }
                _ => break,
            }
        }
        let scope = cpp_scope_declarations(&qualified_name, decls, &inner, ctx);
        if let Some(first) = scope.decls.first() {
            inner = first.clone();
        }
        scopes.push(scope);
        qualified_name = match qualified_name.rfind("::").filter(|c| *c > 0) {
            Some(cut) => qualified_name[..cut].to_string(),
            None => String::new(),
        };
        hops += 1;
    }
    scopes
}

/// `cppScopeMethod`: the method `name` a scope's class (or its bases) provides. `Null` means
/// "not here, look outward"; `Undefined` means "stop, no opinion".
pub fn cpp_scope_method(scope: &CppClassScope, name: &str, receiver: Receiver, r: &UnresolvedRef, ctx: &Ctx) -> Tri<NodeRef> {
    let named: Vec<NodeRef> = ctx
        .get_nodes_by_qualified_name(&format!("{}::{}", scope.qualified_name, name))
        .iter()
        .filter(|n| {
            n.kind == NodeKind::Method
                && is_c_family_node(n)
                && (!scope.narrowed || scope.decls.iter().any(|d| d.file_path == n.file_path || cpp_includers(&d.file_path, ctx).contains(&n.file_path)))
        })
        .cloned()
        .collect();
    let own = super::cpp_overload::cpp_specialization_pool(named, r, ctx);
    let holder = if scope.holds { scope.decls.first().cloned() } else { None };
    let inline: Vec<NodeRef> = match &holder {
        Some(h) => own.iter().filter(|n| n.file_path == h.file_path && range_within(n, h)).cloned().collect(),
        None => Vec::new(),
    };
    let out_of_line: Vec<NodeRef> = if cpp_last_segment(&scope.qualified_name) == "<anonymous>" {
        Vec::new()
    } else {
        own.iter().filter(|n| !inline.iter().any(|i| i.id == n.id)).cloned().collect()
    };
    if inline.len() + out_of_line.len() > 0 {
        let mut all = inline;
        all.extend(prefer_call_site_file(out_of_line, &r.file_path));
        return Tri::Value(cpp_overload_for(all, name, r, ctx));
    }
    if scope.decls.iter().any(|d| cpp_member_macro_arguments(d, ctx).iter().any(|a| name.starts_with(a.as_str()) || name.ends_with(a.as_str()))) {
        return Tri::Undefined;
    }
    for decl in &scope.decls {
        for base in ctx.get_supertype_nodes(&decl.id).iter() {
            if !is_cpp_class_kind(base.kind) {
                continue;
            }
            if receiver == Receiver::Bare && is_cpp_dependent_base(decl, base, ctx) && !cpp_using_declares(decl, base, name, ctx) {
                continue;
            }
            let mut seen = HashSet::new();
            seen.insert(scope.qualified_name.clone());
            let Some(inherited) = cpp_method_in(base, name, r, ctx, 1, &mut seen) else { continue };
            let siblings: Vec<NodeRef> = ctx
                .get_nodes_by_qualified_name(&inherited.qualified_name)
                .iter()
                .filter(|n| n.id != inherited.id && n.kind == NodeKind::Method && is_c_family_node(n))
                .cloned()
                .collect();
            let mut pool = vec![inherited];
            pool.extend(prefer_call_site_file(siblings, &r.file_path));
            let pool = super::cpp_overload::cpp_specialization_pool(pool, r, ctx);
            return Tri::Value(cpp_overload_for(pool, name, r, ctx));
        }
    }
    Tri::Null
}

/// `cppImplicitThisMethod`: the method a bare (or `this->`) call names in the caller's classes.
pub fn cpp_implicit_this_method(name: &str, receiver: Receiver, caller: &Node, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let owner = cpp_member_owner(caller, ctx).or_else(|| cpp_test_fixture(caller, r, ctx))?;
    let mut found: Tri<NodeRef> = Tri::Null;
    for scope in cpp_enclosing_classes(&owner, caller, ctx) {
        found = cpp_scope_method(&scope, name, receiver, r, ctx);
        if !matches!(found, Tri::Null) || receiver == Receiver::This {
            break;
        }
    }
    found.value()
}

/// `matchCppImplicitThisCall`.
pub fn match_cpp_implicit_this_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let name = r.reference_name.as_str();
    if !r.is_kind(EdgeKind::Calls) || !re!(r"^[A-Za-z_]\w*$").is_match(name) {
        return None;
    }
    let receiver = cpp_implicit_receiver(r, ctx)?;
    let caller = ctx.node_by_id(&r.from_node_id)?;
    let found = cpp_implicit_this_method(name, receiver, &caller, r, ctx)?;
    if receiver == Receiver::Bare && name != caller.name && is_cpp_local_name(name, &caller, r, ctx) {
        return None;
    }
    Some(ResolvedRef::new(r, found.id.clone(), 0.9, ResolvedBy::InstanceMethod))
}

/// `cppUtf8Column`: the UTF-16 index of a UTF-8 byte `column` of `line`, when it is a boundary
/// that differs from the byte column.
fn cpp_utf8_column(line: &str, column: u32) -> Option<usize> {
    let mut bytes = 0usize;
    let mut index = 0usize;
    for c in line.chars() {
        if bytes >= column as usize {
            break;
        }
        bytes += c.len_utf8();
        index += c.len_utf16();
    }
    if bytes == column as usize && index != column as usize {
        Some(index)
    } else {
        None
    }
}

fn has_non_ascii(line: &str) -> bool {
    !line.is_ascii()
}

/// `cppImplicitReceiver`: does the call at the reference read `name(` (bare) or `this->name(`?
pub fn cpp_implicit_receiver(r: &UnresolvedRef, ctx: &Ctx) -> Option<Receiver> {
    let lines = ctx.get_file_lines(&r.file_path)?;
    let line = lines.get((r.line as usize).wrapping_sub(1))?;
    let at = |column: u32| -> Option<Receiver> {
        let text = after_col(line, column);
        let self_len = re!(r"^(?:this\s*->|\(\s*\*\s*this\s*\)\s*\.)\s*(?:template\s+)?").find(text).map(|m| m.end()).unwrap_or(0);
        let before = re!(r"\s+$").replace_all_str(before_col(line, column), "");
        let on_this = self_len > 0 || re!(r"(?:^|[^\w$])(?:this\s*->|\(\s*\*\s*this\s*\)\s*\.)$").is_match(&before);
        if !on_this && re!(r"(?:\.|->|::)$").is_match(&before) {
            return None;
        }
        let rest = &text[self_len..];
        let name = r.reference_name.as_str();
        if !rest.starts_with(name) || rest[name.len()..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
            return None;
        }
        if cpp_argument_list_follows(rest, name.len()) {
            Some(if on_this { Receiver::This } else { Receiver::Bare })
        } else {
            None
        }
    };
    let verdict = at(r.column);
    if verdict.is_some() || !has_non_ascii(line) {
        return verdict;
    }
    cpp_utf8_column(line, r.column).and_then(|i| at(i as u32))
}

/// `cppArgumentListFollows`: `(` (after optional template arguments) follows `from` in `text`.
pub fn cpp_argument_list_follows(text: &str, from: usize) -> bool {
    let b = text.as_bytes();
    let mut i = cpp_skip_space(text, from);
    if b.get(i) == Some(&b'<') {
        let mut depth = 0i64;
        while i < b.len() {
            let c = b[i];
            if c == b'<' {
                depth += 1;
            } else if c == b'>' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            } else if matches!(c, b';' | b'{' | b'}') {
                return false;
            }
            i += 1;
        }
        if depth != 0 {
            return false;
        }
        i += 1;
        i = cpp_skip_space(text, i);
    }
    b.get(i) == Some(&b'(')
}

/// What `cppUnspelledReceiverText` reads from the start of an expression.
struct UnspelledReceiver {
    expression: String,
    op: String,
    length: usize,
}

/// `cppUnspelledReceiverText`: `(*p)->`, `v[i].`, `pair.first->` — a receiver whose type is not
/// written at its declaration.
fn cpp_unspelled_receiver_text(text: &str) -> Option<UnspelledReceiver> {
    let head = re!(r"^(?:\(\s*\*\s*([A-Za-z_]\w*)\s*\)|[A-Za-z_]\w*)").captures(text)?;
    let mut end = head.get(0)?.end();
    let mut unspelled = head.text(1).is_some_and(|h| h != "this");
    while let Some(open) = re!(r"^\s*\[").find(&text[end..]) {
        let close = cpp_closing_bracket(text, end + open.end() - 1)?;
        end = close + 1;
        unspelled = true;
    }
    if let Some(member) = re!(r"^\s*(?:->|\.)\s*(?:first|second)\b").find(&text[end..]) {
        end += member.end();
        unspelled = true;
    }
    let op = if unspelled { re!(r"^\s*(->|\.)\s*(?:template\s+)?").captures(&text[end..]) } else { None };
    let op = op?;
    Some(UnspelledReceiver {
        expression: js_trim(&text[..end]).to_string(),
        op: op.text(1)?.to_string(),
        length: end + op.get(0)?.end(),
    })
}

/// A receiver expression and the member operator after it.
#[derive(Debug, Clone)]
pub struct CppReceiverExpr {
    pub expression: String,
    pub op: String,
}

fn code_line_of(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let lines = ctx.get_file_lines(&r.file_path)?;
    let raw = lines.get((r.line as usize).wrapping_sub(1))?;
    let code = super::cpp_decl::cpp_code_of(&cpp_literals_blanked(raw, r.line as usize, &cpp_non_code_lines(&r.file_path, ctx)))?;
    if code.is_empty() {
        None
    } else {
        Some(code)
    }
}

/// `cppUnspelledReceiver`.
pub fn cpp_unspelled_receiver(r: &UnresolvedRef, ctx: &Ctx) -> Option<CppReceiverExpr> {
    let line = code_line_of(r, ctx)?;
    let at = |column: u32| -> Option<CppReceiverExpr> {
        let text = after_col(&line, column);
        let receiver = cpp_unspelled_receiver_text(text)?;
        let rest = &text[receiver.length..];
        let name = r.reference_name.as_str();
        if !rest.starts_with(name) || rest[name.len()..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
            return None;
        }
        if cpp_argument_list_follows(rest, name.len()) {
            Some(CppReceiverExpr { expression: receiver.expression, op: receiver.op })
        } else {
            None
        }
    };
    let found = at(r.column);
    if found.is_some() || !has_non_ascii(&line) {
        return found;
    }
    cpp_utf8_column(&line, r.column).and_then(|i| at(i as u32))
}

/// `cppMemberChainReceiver`: `a.b->` before a call.
pub fn cpp_member_chain_receiver(r: &UnresolvedRef, ctx: &Ctx) -> Option<CppReceiverExpr> {
    let line = code_line_of(r, ctx)?;
    let at = |column: u32| -> Option<CppReceiverExpr> {
        let text = after_col(&line, column);
        let chain = re!(r"^([A-Za-z_]\w*(?:\s*(?:\.|->)\s*[A-Za-z_]\w*)*)\s*(\.|->)\s*(?:template\s+)?").captures(text)?;
        if !re!(r"\.|->").is_match(chain.text(1)?) {
            return None;
        }
        let rest = &text[chain.get(0)?.end()..];
        let name = r.reference_name.as_str();
        if !rest.starts_with(name) || rest[name.len()..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
            return None;
        }
        if cpp_argument_list_follows(rest, name.len()) {
            Some(CppReceiverExpr { expression: chain.text(1)?.to_string(), op: chain.text(2)?.to_string() })
        } else {
            None
        }
    };
    let found = at(r.column);
    if found.is_some() || !has_non_ascii(&line) {
        return found;
    }
    cpp_utf8_column(&line, r.column).and_then(|i| at(i as u32))
}
