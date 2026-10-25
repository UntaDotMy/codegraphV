//! Dart call chains: `a.b(c).d` read backwards from the call being resolved, and typed link by
//! link from the head, so `d` resolves against the type `b(c)` returns.

use super::cpp_text::split_cpp_top_level;
use super::dart::*;
use super::dart_code::*;
use super::js_store::enclosing_scope_start_line;
use super::util::*;
use crate::ctx::Ctx;
use crate::dart_libraries::dart_import_prefixes;
use crate::types::{ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::{Language, Node, NodeKind};

const DART_TYPE_ARGUMENT_LOOKUPS: [&str; 12] = [
    "read", "watch", "get", "find", "of", "maybeOf", "call", "dependOnInheritedWidgetOfExactType", "getInheritedWidgetOfExactType",
    "findAncestorStateOfType", "findAncestorWidgetOfExactType", "findRootAncestorStateOfType",
];

const DART_NON_TYPE_WORDS: [&str; 38] = [
    "final", "var", "late", "const", "required", "covariant", "static", "external", "return", "await", "yield", "throw", "new", "in", "is", "as",
    "case", "else", "if", "for", "while", "do", "switch", "assert", "this", "super", "get", "set", "operator", "async", "sync", "show", "hide",
    "typedef", "extends", "with", "implements", "on",
];

/// A keyword that can precede a name without that name being a type.
pub fn is_non_type_word(w: &str) -> bool {
    DART_NON_TYPE_WORDS.contains(&w)
}

#[derive(Clone)]
pub struct DartChainLink {
    pub name: String,
    pub type_args: Vec<String>,
    pub call: bool,
    pub index: bool,
}

pub enum DartChainHead {
    Name { name: String, type_args: Vec<String>, call: bool, keyword: String, at: usize },
    Str,
    Paren { open: i64, close: i64 },
}

pub struct DartChain {
    pub head: DartChainHead,
    pub links: Vec<DartChainLink>,
}

#[derive(Clone)]
pub struct DartValue {
    pub ty: String,
    pub is_static: bool,
    pub via_super: bool,
    pub owner: Option<String>,
}

impl DartValue {
    fn instance(ty: impl Into<String>) -> DartValue {
        DartValue { ty: ty.into(), is_static: false, via_super: false, owner: None }
    }
}

fn owner_check(value: &DartValue) -> impl Fn(&Node) -> bool + '_ {
    move |n: &Node| value.owner.as_deref().is_none_or(|o| n.file_path == o)
}

fn is_identifier(name: &str) -> bool {
    re!(r"^[A-Za-z_$][\w$]*$").is_match(name)
}

/// Reads the chain that ends at `end` (inclusive) of `text`, backwards.
pub fn read_dart_chain(text: &str, end: i64) -> Option<DartChain> {
    let bytes = text.as_bytes();
    let mut links: Vec<DartChainLink> = Vec::new();
    let mut i = end;
    let skip = |i: &mut i64| {
        while *i >= 0 && is_ws(tb(bytes, *i)) {
            *i -= 1;
        }
    };
    for _step in 0..32 {
        skip(&mut i);
        while tb(bytes, i) == b'!' {
            i -= 1;
            skip(&mut i);
        }
        if tb(bytes, i) == b']' {
            let open = dart_opening(bytes, i, b'[', b']', 20000);
            if open < 0 {
                return None;
            }
            links.push(DartChainLink { name: String::new(), type_args: Vec::new(), call: false, index: true });
            i = open - 1;
            continue;
        }
        if tb(bytes, i) == b'\'' || tb(bytes, i) == b'"' {
            links.reverse();
            return Some(DartChain { head: DartChainHead::Str, links });
        }
        let mut call = false;
        let mut type_args: Vec<String> = Vec::new();
        if tb(bytes, i) == b')' {
            let close = i;
            let open = dart_opening(bytes, i, b'(', b')', 20000);
            if open < 0 {
                return None;
            }
            i = open - 1;
            skip(&mut i);
            if tb(bytes, i) == b'>' {
                let lt = dart_opening(bytes, i, b'<', b'>', 400);
                if lt < 0 {
                    return None;
                }
                type_args = split_cpp_top_level(sl(text, lt + 1, i));
                i = lt - 1;
                skip(&mut i);
            }
            if !is_wordish(tb(bytes, i)) {
                if !type_args.is_empty() || tb(bytes, i) == b')' || tb(bytes, i) == b']' {
                    return None;
                }
                links.reverse();
                return Some(DartChain { head: DartChainHead::Paren { open, close }, links });
            }
            call = true;
        } else if tb(bytes, i) == b'>' {
            let lt = dart_opening(bytes, i, b'<', b'>', 400);
            if lt < 0 {
                return None;
            }
            type_args = split_cpp_top_level(sl(text, lt + 1, i));
            i = lt - 1;
            skip(&mut i);
        }
        let name_end = i + 1;
        while i >= 0 && is_wordish(tb(bytes, i)) {
            i -= 1;
        }
        let name = sl(text, i + 1, name_end).to_string();
        if !is_identifier(&name) {
            return None;
        }
        let mut j = i;
        while j >= 0 && is_ws(tb(bytes, j)) {
            j -= 1;
        }
        if tb(bytes, j) == b'.' {
            if tb(bytes, j - 1) == b'.' {
                return None;
            }
            links.push(DartChainLink { name, type_args, call, index: false });
            i = j - 1;
            if tb(bytes, i) == b'?' {
                i -= 1;
            }
            continue;
        }
        let keyword = re!(r"([A-Za-z_]\w*)$").captures(sl(text, (j - 10).max(0), j + 1)).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        links.reverse();
        return Some(DartChain { head: DartChainHead::Name { name, type_args, call, keyword, at: (i + 1) as usize }, links });
    }
    None
}

/// The value type of the whole chain.
pub fn type_dart_chain(chain: &DartChain, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<DartValue> {
    if depth > 3 {
        return None;
    }
    let mut links: &[DartChainLink] = &chain.links;
    let mut value: Option<DartValue>;
    match &chain.head {
        DartChainHead::Str => value = Some(DartValue::instance("String")),
        DartChainHead::Paren { open, close } => value = type_dart_paren(*open, *close, r, ctx, depth),
        DartChainHead::Name { name, type_args, call, keyword, at } => {
            let first = links.first();
            if !*call
                && type_args.is_empty()
                && first.is_some_and(|f| !f.index)
                && dart_import_prefixes(&r.file_path, ctx).contains(name.as_str())
                && !is_dart_name_hidden(name, r, *at, ctx)
            {
                value = type_dart_prefixed_head(name, &links[0], r, ctx, depth);
                links = &links[1..];
            } else {
                if (keyword == "const" || keyword == "new") && !*call && links.first().is_some_and(|l| l.call) {
                    links = &links[1..];
                }
                value = type_dart_head(name, type_args, *call, keyword, r, ctx, depth);
            }
        }
    }
    for link in links {
        if link.name == "toString" && link.call && link.type_args.is_empty() {
            value = Some(DartValue::instance("String"));
            continue;
        }
        let next = value.as_ref().and_then(|v| type_dart_link(v, link, r, ctx, depth));
        let given = if next.is_none()
            && link.call
            && link.type_args.len() == 1
            && DART_TYPE_ARGUMENT_LOOKUPS.contains(&link.name.as_str())
            && !value.as_ref().is_some_and(|v| dart_link_declared(v, link, r, ctx))
        {
            dart_simple_type_name(&link.type_args[0])
        } else {
            None
        };
        value = next.or_else(|| given.map(DartValue::instance));
    }
    value
}

/// Does the value's type declare the member the link names?
fn dart_link_declared(value: &DartValue, link: &DartChainLink, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if value.is_static {
        dart_static_member(&value.ty, &link.name, r, ctx, &owner_check(value)).is_some()
    } else {
        dart_member_of(&value.ty, &link.name, r, ctx, &|n| !is_dart_setter(n, ctx), if value.via_super { 1 } else { 0 }).is_some()
    }
}

/// `(expr as T)` is a `T`; `(expr)` is the type of `expr`.
fn type_dart_paren(open: i64, close: i64, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<DartValue> {
    let code = dart_code_of(&r.file_path, ctx)?;
    let inner = sl(&code.text, open + 1, close);
    if let Some(cast) = re!(r"\bas\s+((?:[A-Za-z_$][\w$]*\.)?[A-Za-z_$][\w$]*)\s*(?:<[^()]*>)?\s*\??\s*$").captures(inner) {
        return dart_simple_type_name(cast.text(1).unwrap_or("")).map(DartValue::instance);
    }
    let mut end = close - 1;
    while end > open && is_ws(tb(code.bytes(), end)) {
        end -= 1;
    }
    dart_expression_value(&code.text, end, r, ctx, depth)
}

/// The instance type of the expression ending at `end`.
fn dart_expression_value(text: &str, end: i64, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<DartValue> {
    let chain = read_dart_chain(text, end)?;
    if matches!(&chain.head, DartChainHead::Name { keyword, .. } if keyword == "await") {
        return None;
    }
    type_dart_chain(&chain, r, ctx, depth + 1).filter(|v| !v.is_static)
}

fn is_dart_type_named(name: &str, ctx: &Ctx) -> bool {
    ctx.get_nodes_by_name(name).iter().any(|n| n.language == Language::Dart && is_dart_type_kind(n.kind))
}

fn type_dart_head(name: &str, type_args: &[String], call: bool, keyword: &str, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<DartValue> {
    if name == "this" || name == "super" {
        let own = dart_enclosing_type(r, ctx)?;
        return Some(DartValue { ty: own.name.clone(), is_static: false, via_super: name == "super", owner: None });
    }
    if keyword == "const" || keyword == "new" || keyword == "as" {
        return Some(DartValue::instance(name));
    }
    if keyword == "is" {
        return None;
    }
    let is_type = is_dart_type_named(name, ctx);
    if call {
        if is_type {
            return Some(DartValue::instance(name));
        }
        if let Some(f) = dart_function_named(name, r, ctx) {
            return dart_return_type(&f, type_args, ctx).map(DartValue::instance);
        }
        let held = dart_variable_type(name, r, ctx, depth)?;
        return dart_call_result(&held, type_args, r, ctx);
    }
    if is_type
        || (name.starts_with(|c: char| c.is_ascii_uppercase())
            && !ctx.get_nodes_by_name(name).iter().any(|n| n.language == Language::Dart && matches!(n.kind, NodeKind::Constant | NodeKind::Variable)))
    {
        return Some(DartValue { ty: name.to_string(), is_static: true, via_super: false, owner: None });
    }
    dart_variable_type(name, r, ctx, depth).map(DartValue::instance)
}

/// `p.Name` / `p.name(…)` / `p.constant` where `p` is an import prefix.
fn type_dart_prefixed_head(prefix: &str, link: &DartChainLink, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<DartValue> {
    let found = dart_prefixed_decls(prefix, &link.name, r, ctx);
    let types: Vec<&Node> = found.iter().filter(|n| is_dart_type_kind(n.kind)).map(|n| &**n).collect();
    if !types.is_empty() {
        return Some(DartValue {
            ty: link.name.clone(),
            is_static: !link.call,
            via_super: false,
            owner: if types.len() == 1 { Some(types[0].file_path.clone()) } else { None },
        });
    }
    if found.len() != 1 {
        return None;
    }
    let decl = &found[0];
    if decl.kind == NodeKind::Function {
        let ty = if link.call { dart_return_type(decl, &link.type_args, ctx) } else { None };
        return ty.map(DartValue::instance);
    }
    if decl.kind != NodeKind::Constant && decl.kind != NodeKind::Variable {
        return None;
    }
    let held = dart_constant_type(decl, r, ctx, depth)?;
    if link.call {
        dart_call_result(&held, &link.type_args, r, ctx)
    } else {
        Some(DartValue::instance(held))
    }
}

/// The type of `value.link`.
fn type_dart_link(value: &DartValue, link: &DartChainLink, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<DartValue> {
    if link.index {
        return None;
    }
    if value.is_static {
        let Some(member) = dart_static_member(&value.ty, &link.name, r, ctx, &owner_check(value)) else {
            return if link.call && is_dart_type_named(&value.ty, ctx) { Some(DartValue::instance(value.ty.clone())) } else { None };
        };
        if member.kind == NodeKind::EnumMember {
            return if link.call { None } else { Some(DartValue::instance(value.ty.clone())) };
        }
        if member.kind == NodeKind::Constant {
            let held = dart_constant_type(&member, r, ctx, depth)?;
            return if link.call { dart_call_result(&held, &link.type_args, r, ctx) } else { Some(DartValue::instance(held)) };
        }
        return type_dart_member_use(&member, link, r, ctx);
    }
    let member = dart_member_of(&value.ty, &link.name, r, ctx, &|n| !is_dart_setter(n, ctx), if value.via_super { 1 } else { 0 }).map(|(n, _)| n);
    if let Some(member) = member {
        return type_dart_member_use(&member, link, r, ctx);
    }
    if link.call {
        return None;
    }
    dart_field_type(&value.ty, &link.name, r, ctx, depth).map(DartValue::instance)
}

/// The type of calling / reading a member the link names.
fn type_dart_member_use(member: &Node, link: &DartChainLink, r: &UnresolvedRef, ctx: &Ctx) -> Option<DartValue> {
    let getter = is_dart_getter(member, ctx);
    let ty = dart_return_type(member, if getter { &[] } else { &link.type_args }, ctx)?;
    if getter {
        return if link.call { dart_call_result(&ty, &link.type_args, r, ctx) } else { Some(DartValue::instance(ty)) };
    }
    if link.call {
        Some(DartValue::instance(ty))
    } else {
        None
    }
}

/// The type a value of `ty` returns when called (`ty.call(…)`).
fn dart_call_result(ty: &str, type_args: &[String], r: &UnresolvedRef, ctx: &Ctx) -> Option<DartValue> {
    let (call, _) = dart_member_of(ty, "call", r, ctx, &|n| !is_dart_getter(n, ctx), 0)?;
    dart_return_type(&call, type_args, ctx).map(DartValue::instance)
}

// -- Types of names ----------------------------------------------------------------------------

/// The type of the variable, field, getter or constant `name` where the reference sits.
pub fn dart_variable_type(name: &str, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<String> {
    if let Some(local) = dart_local_type(name, r, ctx, depth) {
        return local;
    }
    if let Some(field) = super::members::infer_member_receiver_type(name, r, ctx) {
        return dart_simple_type_name(&field);
    }
    let getter = dart_enclosing_type(r, ctx).and_then(|own| dart_member_of(&own.name, name, r, ctx, &|n| is_dart_getter(n, ctx), 0)).map(|(n, _)| n);
    if let Some(getter) = getter {
        return dart_return_type(&getter, &[], ctx);
    }
    let constants: Vec<_> = ctx
        .get_nodes_by_name(name)
        .iter()
        .filter(|n| n.language == Language::Dart && n.kind == NodeKind::Constant && !n.qualified_name.contains("::"))
        .cloned()
        .collect();
    let top = dart_library_pick(&constants, r, ctx)?;
    dart_constant_type(&top, r, ctx, depth)
}

/// The type of the local `name` where the reference sits: `None` when the scan found no local
/// binding (the caller keeps looking), `Some(None)` when it found one of unknown type.
fn dart_local_type(name: &str, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<Option<String>> {
    let code = dart_code_of(&r.file_path, ctx)?;
    let at = dart_name_start(r, name, &code);
    if at < 0 {
        return None;
    }
    let at = at as usize;
    let start = code.line_start(enclosing_scope_start_line(r, ctx)).unwrap_or(0);
    let scope = sl(&code.text, start as i64, at as i64);
    let uses = dart_name_uses(&code, &r.file_path, name, ctx);
    let lo = uses.partition_point(|u| *u < at);
    let mut u = lo as i64 - 1;
    while u >= 0 && uses[u as usize] >= start {
        let pos = uses[u as usize] - start;
        u -= 1;
        let before = sl(scope, (pos as i64 - 120).max(0), pos as i64);
        let after = sl(scope, (pos + name.len()) as i64, scope.len() as i64);
        if let Some(declared) = re!(r"([A-Za-z_$][\w$]*)\s*(<[^;{}()=]*>)?\s*\??\s+$").captures(before) {
            let word = declared.text(1).unwrap_or("");
            if !is_non_type_word(word) && re!(r"^\s*(?:[=;,)}]|in\b)").is_match(after) && !re!(r"^\s*==").is_match(after) {
                return Some(Some(word.to_string()));
            }
        }
        if re!(r"(?:^|[^\w$])(?:final|var|late|const)\s+$").is_match(before) {
            let Some(init) = re!(r"^\s*=(?![=>])").find(after) else { return Some(None) };
            return Some(dart_initializer_type(&code.text, start + pos + name.len() + init.end(), r, ctx, depth));
        }
        if re!(r"^\s*[,)]").is_match(after) && re!(r"[(,]\s*$").is_match(before) {
            let open = dart_group_open(code.bytes(), start + pos);
            let close = if open < 0 { -1 } else { dart_group_close(code.bytes(), open as usize) };
            let window = sl(&code.text, (open - 60).max(0), open.max(0));
            let callee = re!(r"([\w$]*)\s*(?:<[^;{}()]*>)?\s*$").captures(window).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
            if callee == "catch" {
                return Some(None);
            }
            if close >= 0
                && !re!(r"[\w$>)\]]\s*$").is_match(window)
                && re!(r"^\)\s*(?:async\s*\*?|sync\s*\*)?\s*(?:=>|\{)").is_match(sl(&code.text, close, close + 30))
            {
                return Some(None);
            }
        }
    }
    None
}

/// The type of the initializer expression starting at `from`, up to the `;` / `,` / closing bracket.
pub fn dart_initializer_type(text: &str, from: usize, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut nest = 0i32;
    let mut k = from;
    while k < bytes.len() && k - from < 4000 {
        let ch = bytes[k];
        if ch == b'<' && k > 0 && is_wordish(bytes[k - 1]) {
            let close = dart_type_arguments_end(bytes, k);
            if close > 0 {
                k = close as usize + 1;
                continue;
            }
        }
        if matches!(ch, b'(' | b'[' | b'{') {
            nest += 1;
        } else if matches!(ch, b')' | b']' | b'}') {
            if nest == 0 {
                break;
            }
            nest -= 1;
        } else if (ch == b';' || ch == b',') && nest == 0 {
            break;
        }
        k += 1;
    }
    let mut end = k as i64 - 1;
    while end >= from as i64 && is_ws(tb(bytes, end)) {
        end -= 1;
    }
    if end < from as i64 {
        return None;
    }
    dart_expression_value(text, end, r, ctx, depth).map(|v| v.ty)
}

/// The type of a top-level constant: its declared type, or the type of its initializer.
pub fn dart_constant_type(constant: &Node, r: &UnresolvedRef, ctx: &Ctx, depth: usize) -> Option<String> {
    let code = dart_code_of(&constant.file_path, ctx)?;
    let from = code.line_start(constant.start_line)?;
    let head = Re::cached(
        &format!(r"\b(?:const|final|var)\s+(?:([A-Za-z_$][\w$.]*)\s*(?:<[^;=]*>)?\s*\??\s+)?{}\s*=(?![=>])", escape_regex(&constant.name)),
        "",
    );
    let window = sl(&code.text, from as i64, from as i64 + 600);
    let m = head.captures(window)?;
    if let Some(ty) = m.text(1).filter(|t| !t.is_empty()) {
        if !is_non_type_word(ty) {
            return dart_simple_type_name(ty);
        }
    }
    let whole = m.get(0)?;
    let site = UnresolvedRef { file_path: constant.file_path.clone(), line: constant.start_line, column: 0, ..r.clone() };
    dart_initializer_type(&code.text, from + whole.end(), &site, ctx, depth + 1)
}

// -- Chain links -------------------------------------------------------------------------------

/// A call written on a receiver (`x.name(…)`), whose target depends on the receiver's type.
pub fn is_dart_chain_link(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    r.language == Language::Dart && is_calls(r) && is_identifier(&r.reference_name) && !is_receiver_less_dart_call(r, ctx)
}

/// Resolves `receiver.name(…)` by typing the receiver chain.
pub fn match_dart_chain_link(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !ctx.get_nodes_by_name(&r.reference_name).iter().any(|n| n.language == Language::Dart) {
        return None;
    }
    let code = dart_code_of(&r.file_path, ctx)?;
    let start = dart_name_start(r, &r.reference_name, &code);
    if start < 0 {
        return None;
    }
    let text = code.bytes();
    let mut end = start - 1;
    while end >= 0 && is_ws(tb(text, end)) {
        end -= 1;
    }
    if tb(text, end) != b'.' || tb(text, end - 1) == b'.' {
        return None;
    }
    end -= 1;
    if tb(text, end) == b'?' {
        end -= 1;
    }
    while end >= 0 && is_ws(tb(text, end)) {
        end -= 1;
    }
    let chain = read_dart_chain(&code.text, end)?;
    let receiver = type_dart_chain(&chain, r, ctx, 0)?;
    if receiver.is_static {
        let member = dart_static_member(&receiver.ty, &r.reference_name, r, ctx, &owner_check(&receiver))?;
        return (member.kind != NodeKind::EnumMember).then(|| ResolvedRef::new(r, member.id.clone(), 0.85, ResolvedBy::QualifiedName));
    }
    let (member, _) = dart_member_of(&receiver.ty, &r.reference_name, r, ctx, &|n| !is_dart_setter(n, ctx), if receiver.via_super { 1 } else { 0 })?;
    Some(ResolvedRef::new(r, member.id.clone(), 0.85, ResolvedBy::InstanceMethod))
}
