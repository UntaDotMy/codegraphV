//! VB.NET call shape: the receiver a call is written on (`Me.Foo`, `Customer.Create`), and which
//! same-named candidates that shape can reach.

use super::lang_shape::RefShape;
use super::line_scope::has_no_receiver_on_line;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, UnresolvedRef};
use crate::vbnet_receivers::{is_vb_member_in_scope, is_vb_nested_type_in_scope, is_vb_type_qualified_by};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{EdgeKind, Language, Node, NodeKind};

fn is_vb_member_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Property | NodeKind::Field | NodeKind::EnumMember | NodeKind::Constant | NodeKind::Variable)
}

/// The receiver a VB call is written on — `a.b` for `a.b.Name(…)`, call arguments dropped, or
/// the `(Of T)` type argument for `X(Of T).Name` — or none when it is not written on one.
pub fn vb_receiver_of(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let line = line_at(ctx, &r.file_path, r.line).or_else(|| {
        let idx = (r.line as usize).checked_sub(1)?;
        ctx.read_file(&r.file_path).and_then(|s| s.split('\n').nth(idx).map(String::from))
    })?;
    let lower = line.to_lowercase();
    let name = r.reference_name.to_lowercase();
    let mut start: Option<usize> = if starts_with_at(&lower, &name, r.column) { Some(col_byte(&lower, r.column)) } else { None };
    if start.is_none() {
        let at = Re::cached(&format!(r"(?<![\w]){}(?![\w])", escape_regex(&name)), "g");
        let from = col_byte(&lower, r.column);
        let found = at.captures_at(&lower, from).or_else(|| at.captures_at(&lower, 0));
        start = found.and_then(|c| c.get(0)).map(|m| m.start());
    }
    let start = start?;
    // The prefix of the original-case line (the lowered copy has the same shape for the ASCII source this reads).
    let before: &str = if line.len() == lower.len() && line.is_char_boundary(start) { &line[..start] } else { &lower[..start] };
    let dot = re!(r"([\w.()]*?)\s*\.\s*$").captures(before)?;
    if let Some(type_arg) = re!(r"\(\s*Of\s+([\w.]+)\s*\)\s*\.\s*$", "i").captures(before).and_then(|c| c.text(1).map(String::from)) {
        return Some(type_arg);
    }
    Some(re!(r"\([^()]*\)", "g").replace_all_str(dot.text(1).unwrap_or(""), ""))
}

/// Can a member (or any non-member) candidate be reached through this receiver?
pub fn is_vb_member_reachable(n: &Node, receiver: &str) -> bool {
    if !is_vb_member_kind(n.kind) {
        return true;
    }
    let Some(cut) = n.qualified_name.rfind("::") else { return true };
    let last = receiver.rsplit('.').next().unwrap_or("").to_lowercase();
    if re!(r"^(?:me|mybase|myclass)$").is_match(&last) && !receiver.contains('.') {
        return true;
    }
    let owner_path = &n.qualified_name[..cut];
    let by_colons = owner_path.rfind("::").map(|i| i + 2);
    let by_dot = owner_path.rfind('.').map(|i| i + 1);
    let owner = owner_path[by_colons.into_iter().chain(by_dot).max().unwrap_or(0)..].to_lowercase();
    !last.is_empty() && last == owner
}

fn is_vb_scoped_call(r: &UnresolvedRef, receiver: Option<&str>, ctx: &Ctx) -> bool {
    if r.language != Language::VbNet || !is_calls(r) || !re!(r"^\w+$").is_match(&r.reference_name) {
        return false;
    }
    match receiver {
        None => has_no_receiver_on_line(r, ctx),
        Some(recv) => re!(r"^(?:me|mybase|myclass)$", "i").is_match(recv),
    }
}

fn is_vb_unqualified_name(r: &UnresolvedRef, receiver: Option<&str>, ctx: &Ctx) -> bool {
    if r.language != Language::VbNet || !(is_calls(r) || super::util::is_kind(r, EdgeKind::Instantiates)) {
        return false;
    }
    receiver.is_none() && re!(r"^\w+$").is_match(&r.reference_name) && has_no_receiver_on_line(r, ctx)
}

/// The VB.NET contribution to [`super::lang_shape::LangShape`].
pub struct VbShape {
    receiver: Option<String>,
    scoped: bool,
    unqualified: bool,
}

impl VbShape {
    pub fn of(r: &UnresolvedRef, ctx: &Ctx) -> Option<VbShape> {
        if r.language != Language::VbNet {
            return None;
        }
        let receiver = if (is_calls(r) || is_kind(r, EdgeKind::Instantiates)) && re!(r"^\w+$").is_match(&r.reference_name) { vb_receiver_of(r, ctx) } else { None };
        let scoped = is_vb_scoped_call(r, receiver.as_deref(), ctx);
        let unqualified = is_vb_unqualified_name(r, receiver.as_deref(), ctx);
        Some(VbShape { receiver, scoped, unqualified })
    }
}

impl RefShape for VbShape {
    fn excludes(&self, n: &NodeRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
        if let Some(receiver) = &self.receiver {
            if !is_vb_member_reachable(n, receiver) {
                return true;
            }
            if !receiver.is_empty() && !re!(r"^(?:me|mybase|myclass)$", "i").is_match(receiver) && !is_vb_type_qualified_by(n, receiver, &r.file_path, ctx) {
                return true;
            }
        }
        (self.scoped && !is_vb_member_in_scope(n, r, ctx)) || (self.unqualified && !is_vb_nested_type_in_scope(n, r, ctx))
    }
}
