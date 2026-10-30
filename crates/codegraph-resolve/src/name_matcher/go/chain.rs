//! The type of a Go call chain's receiver, read from source text: `a.b().c[i].d()`.
//!
//! A chain is split into links (operand, selections, calls, indexes, assertions, composite
//! literals); the head's type comes from a local declaration, a package-level variable, an
//! imported package's function or a literal, and each later link is followed through the
//! project's struct fields and method signatures. Anything the text cannot settle is
//! `Undefined`; a type known to be outside the project is `Null`.

use super::methods::{go_alias_target, go_package_value_type};
use super::qualify::*;
use super::scope::*;
use crate::ctx::Ctx;
use crate::name_matcher::method::resolve_method_on_type;
use crate::name_matcher::util::*;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_len};
use codegraph_types::{Language, NodeKind};
use indexmap::IndexMap;
use std::rc::Rc;

/// Most links a chain may span, and lines a call's chain may be continued over.
const GO_CHAIN_LINES: usize = 30;
const GO_VALUE_DEPTH: u32 = 12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GoChainKind {
    Operand,
    Select,
    Assert,
    Call,
    Index,
    Composite,
}

#[derive(Clone, Debug)]
pub struct GoChainLink {
    pub kind: GoChainKind,
    pub text: String,
}

fn at_byte(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// The offset of the bracket closing the one at `open`, skipping strings and comments; None when
/// it never closes.
pub fn go_closing_bracket(text: &str, open: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c && b[i] != b'\n' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
        } else if c == b'`' {
            i = i + 1 + text[i + 1..].find('`')?;
        } else if c == b'/' && at_byte(b, i + 1) == b'/' {
            i += text[i..].find('\n')?;
        } else if c == b'/' && at_byte(b, i + 1) == b'*' {
            i = i + 2 + text[i + 2..].find("*/")? + 1;
        } else if matches!(c, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(c, b')' | b']' | b'}') {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// The links of the chain starting at `start`: an operand (a name or a parenthesised
/// expression), then selections `.x`, assertions `.(T)`, calls `(…)`, indexes `[…]` and — with
/// `composite` — a trailing `{…}`. `end` receives the offset the chain stops at.
pub fn go_chain_links(text: &str, start: usize, mut end: Option<&mut usize>, composite: bool) -> Vec<GoChainLink> {
    let b = text.as_bytes();
    let mut links: Vec<GoChainLink> = Vec::new();
    let mut i = start;
    let name = |i: &mut usize| -> Option<String> {
        let s = *i;
        if !(at_byte(b, s).is_ascii_alphabetic() || at_byte(b, s) == b'_') {
            return None;
        }
        let mut e = s + 1;
        while at_byte(b, e).is_ascii_alphanumeric() || at_byte(b, e) == b'_' {
            e += 1;
        }
        *i = e;
        Some(text[s..e].to_string())
    };
    let group = |i: &mut usize| -> Option<String> {
        let close = go_closing_bracket(text, *i)?;
        let inner = text[*i + 1..close].to_string();
        *i = close + 1;
        Some(inner)
    };
    let operand = if at_byte(b, i) == b'(' { group(&mut i) } else { name(&mut i) };
    let Some(operand) = operand else {
        if let Some(e) = end.as_deref_mut() {
            *e = i;
        }
        return links;
    };
    links.push(GoChainLink { kind: GoChainKind::Operand, text: operand });
    loop {
        while matches!(at_byte(b, i), b' ' | b'\t') {
            i += 1;
        }
        let c = at_byte(b, i);
        if c == b'.' {
            i += 1;
            while i < b.len() && text[i..].chars().next().is_some_and(is_js_space) {
                i += text[i..].chars().next().map_or(1, char::len_utf8);
            }
            let ty = if at_byte(b, i) == b'(' { group(&mut i) } else { None };
            let member = if ty.is_none() { name(&mut i) } else { None };
            if let Some(t) = ty {
                links.push(GoChainLink { kind: GoChainKind::Assert, text: js_trim(&t).to_string() });
            } else if let Some(m) = member {
                links.push(GoChainLink { kind: GoChainKind::Select, text: m });
            } else {
                break;
            }
        } else if c == b'(' || c == b'[' {
            // A call or an index: its text is the bracketed part.
            let idx = links.len();
            links.push(GoChainLink { kind: if c == b'(' { GoChainKind::Call } else { GoChainKind::Index }, text: String::new() });
            let Some(inner) = group(&mut i) else { break };
            links[idx].text = inner;
        } else if composite
            && c == b'{'
            && at_byte(b, start) != b'('
            && links.len() <= 3
            && links.iter().all(|l| matches!(l.kind, GoChainKind::Operand | GoChainKind::Select | GoChainKind::Index))
        {
            let Some(inner) = group(&mut i) else { break };
            links.push(GoChainLink { kind: GoChainKind::Composite, text: inner });
        } else {
            break;
        }
    }
    if let Some(e) = end {
        *e = i;
    }
    links
}

/// The type a call's chain asserts the receiver to: `x.(pkg.T).Method()` → `pkg.T`.
pub fn go_asserted_type(r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    let lines = ctx.get_file_lines(&r.file_path)?;
    let line = lines.get((r.line as usize).checked_sub(1)?)?;
    if r.column as usize >= utf16_len(line) {
        return None;
    }
    if !after_col(line, r.column).contains(".(") && !re!(r"\.\s*$").is_match(line) {
        return None;
    }
    let from = r.line as usize - 1;
    let joined = lines[from..lines.len().min(r.line as usize + GO_CHAIN_LINES)].join("\n");
    let links = go_chain_links(&joined, col_byte(line, r.column), None, false);
    for k in 1..links.len().saturating_sub(1) {
        if links[k - 1].kind == GoChainKind::Assert && links[k].kind == GoChainKind::Select && links[k].text == r.reference_name && links[k + 1].kind == GoChainKind::Call {
            return Some(links[k - 1].text.clone());
        }
    }
    None
}

// -- Types as text ---------------------------------------------------------------------------------

type Params = Rc<IndexMap<String, String>>;

/// A type as written, with the file it is written in (to resolve its package names) and the
/// type parameters in scope there.
#[derive(Clone, Debug)]
pub struct GoTypeText {
    pub text: String,
    pub file: String,
    pub params: Option<Params>,
}

type GoTyped = Tri<GoTypeText>;

pub struct GoTypeSpec {
    #[allow(dead_code)]
    alias: bool,
    params: Params,
    underlying: String,
}

pub struct GoStructMembers {
    fields: IndexMap<String, String>,
    embedded: Vec<String>,
    params: Params,
}

memo_map!(TypeSpecMemo, String, Option<Rc<GoTypeSpec>>);
memo_map!(StructMembersMemo, String, Option<Rc<GoStructMembers>>);
memo_map!(FuncTypeParamsMemo, String, Params);

fn go_is_type_literal(s: &str) -> bool {
    re!(r"^(?:\[|map\b|chan\b|<-|func\b|struct\b|interface\b)").is_match(s)
}

pub fn go_normalize_type(text: &str) -> String {
    let a = re!(r"\s+", "g").replace_all_str(js_trim(text), " ");
    let b = re!(r"([*\]])\s+", "g").replace_all(&a, |m| Some(m.text(1).unwrap_or("").to_string()));
    re!(r"\s*\.\s*", "g").replace_all_str(&b, ".")
}

/// Split `s` at the characters of `seps` that sit outside brackets.
pub fn go_split_top_level(s: &str, seps: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut from = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ if depth == 0 && seps.contains(c) => {
                parts.push(s[from..i].to_string());
                from = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(s[from..].to_string());
    parts
}

fn go_is_slice_index(inner: &str) -> bool {
    go_split_top_level(inner, ":").len() > 1
}

fn go_header_value_end(code: &str, at: usize) -> usize {
    let b = code.as_bytes();
    let mut depth = 0i32;
    for (i, &c) in b.iter().enumerate().skip(at) {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => {
                depth -= 1;
                if depth < 0 {
                    return i;
                }
            }
            b';' | b'{' | b'\n' if depth == 0 => return i,
            _ => {}
        }
    }
    code.len()
}

/// Where the type written at the start of `s` ends (-1 when it is not a type).
fn go_type_text_end(s: &str, mut i: usize) -> i64 {
    let b = s.as_bytes();
    while matches!(at_byte(b, i), b' ' | b'\t') {
        i += 1;
    }
    if at_byte(b, i) == b'*' {
        return go_type_text_end(s, i + 1);
    }
    if at_byte(b, i) == b'[' {
        return match go_closing_bracket(s, i) {
            Some(close) => go_type_text_end(s, close + 1),
            None => -1,
        };
    }
    if let Some(m) = re!(r"^map\s*\[").find(slice_bytes(s, i, i + 8)) {
        return match go_closing_bracket(s, i + m.end() - 1) {
            Some(close) => go_type_text_end(s, close + 1),
            None => -1,
        };
    }
    if let Some(m) = re!(r"^(?:struct|interface)\s*\{").find(slice_bytes(s, i, i + 16)) {
        return match go_closing_bracket(s, i + m.end() - 1) {
            Some(close) => close as i64 + 1,
            None => -1,
        };
    }
    let Some(name) = re!(r"^[A-Za-z_]\w*(?:\s*\.\s*[A-Za-z_]\w*)?").find(slice_bytes(s, i, s.len())) else { return -1 };
    if re!(r"^(?:func|chan|range|make|new|append)$").is_match(name.as_str()) {
        return -1;
    }
    let mut end = i + name.end();
    if at_byte(b, end) == b'[' {
        if let Some(close) = go_closing_bracket(s, end) {
            if close > end && !go_is_slice_index(&s[end + 1..close]) {
                end = close + 1;
            }
        }
    }
    end as i64
}

fn go_type_text_at(text: &str, file_path: &str, at: usize, index: Option<&GoScopeIndex>, ctx: &Ctx) -> GoTypeText {
    GoTypeText { text: go_normalize_type(text), file: file_path.to_string(), params: index.and_then(|ix| go_type_params_at(ix, file_path, at, ctx)) }
}

fn go_type_param_list(inner: &str) -> IndexMap<String, String> {
    let mut params = IndexMap::new();
    let mut pending: Vec<String> = Vec::new();
    for part in go_split_top_level(inner, ",") {
        let Some(m) = re!(r"^\s*([A-Za-z_]\w*)\s*([\s\S]*?)\s*$").captures(&part) else { continue };
        pending.push(m.text(1).unwrap_or("").to_string());
        let constraint = m.text(2).unwrap_or("");
        if constraint.is_empty() {
            continue;
        }
        for p in pending.drain(..) {
            params.insert(p, constraint.to_string());
        }
    }
    for p in pending {
        params.insert(p, String::new());
    }
    params
}

/// The type parameters of the generic function (or method on a generic type) enclosing `at`.
fn go_type_params_at(index: &GoScopeIndex, file_path: &str, at: usize, ctx: &Ctx) -> Option<Params> {
    if index.generic.get().is_none() {
        index.generic.set(Some(
            re!(r"(?<![\w.])func\s*(?:\(\s*(?:[A-Za-z_]\w*\s+)?\*?\s*[A-Za-z_]\w*\s*\[|[A-Za-z_]\w*\s*\[)").is_match(&index.code),
        ));
    }
    if index.generic.get() != Some(true) {
        return None;
    }
    let (mut lo, mut hi) = (0usize, index.line_starts.len() - 1);
    while lo < hi {
        let mid = (lo + hi + 1) >> 1;
        if index.line_starts[mid] <= at {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let line = lo as u32 + 1;
    let fn_node = ctx.get_nodes_in_file(file_path).iter().find(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method) && n.start_line <= line && n.end_line >= line).cloned()?;
    let params = memoized::<FuncTypeParamsMemo>(ctx, fn_node.id.clone(), || {
        let mut params: IndexMap<String, String> = IndexMap::new();
        let line_start = index.line_starts.get(fn_node.start_line as usize - 1).copied().unwrap_or(0);
        let head_end = index.line_starts.get((fn_node.end_line.min(fn_node.start_line + 40)) as usize).copied().unwrap_or(index.code.len());
        let head = &index.code[line_start..head_end.max(line_start)];
        if let Some(generic) = re!(r"^\s*func\s*(?:\(\s*(?:[A-Za-z_]\w*\s+)?\*?\s*([A-Za-z_]\w*)\s*(\[)|[A-Za-z_]\w*\s*(\[))").captures(head) {
            let whole = generic.get(0).map_or(0, |m| m.end());
            let open = whole - 1;
            let inner = match go_closing_bracket(head, open) {
                Some(close) if close > open => head[open + 1..close].to_string(),
                _ => String::new(),
            };
            match generic.text(1).filter(|s| !s.is_empty()) {
                None => params = go_type_param_list(&inner),
                Some(receiver) => {
                    // A method of a generic type: the receiver's own type parameters, named by
                    // the type declaration, carry the constraints.
                    let declared = go_package_types(receiver, &go_package_dir(file_path), ctx)
                        .iter()
                        .filter_map(|n| go_type_spec(n, ctx).map(|s| s.params.clone()))
                        .find(|p| !p.is_empty());
                    let constraints: Vec<String> = declared.map(|d| d.values().cloned().collect()).unwrap_or_default();
                    for (k, p) in go_split_top_level(&inner, ",").iter().enumerate() {
                        if let Some(ident) = re!(r"^\s*([A-Za-z_]\w*)\s*$").captures(p) {
                            params.insert(ident.text(1).unwrap_or("").to_string(), constraints.get(k).cloned().unwrap_or_default());
                        }
                    }
                }
            }
        }
        Rc::new(params)
    });
    if params.is_empty() {
        None
    } else {
        Some(params)
    }
}

/// The right-hand side of a `type` declaration: its type parameters and what it is defined as.
fn go_type_spec(node: &NodeRef, ctx: &Ctx) -> Option<Rc<GoTypeSpec>> {
    memoized::<TypeSpecMemo>(ctx, node.id.clone(), || {
        let lines = lines_of(ctx, &node.file_path);
        let from = (node.start_line as usize).saturating_sub(1);
        let to = node.end_line.max(node.start_line) as usize;
        let slice = if from < lines.len() { &lines[from..lines.len().min(to)] } else { &lines[0..0] };
        let code = blank_go_code(&slice.join("\n"));
        let first = slice.first().map_or("", |s| s.as_str());
        let start = col_byte(first, node.start_column).min(code.len());
        let name = re!(r"^[A-Za-z_]\w*").find(&code[start..])?;
        let b = code.as_bytes();
        let mut i = start + name.end();
        let skip = |i: &mut usize| {
            while matches!(at_byte(b, *i), b' ' | b'\t') {
                *i += 1;
            }
        };
        skip(&mut i);
        let mut params: IndexMap<String, String> = IndexMap::new();
        // `type T[K comparable, V any] …`: type parameters, not an array length.
        if at_byte(b, i) == b'[' {
            if let Some(close) = go_closing_bracket(&code, i).filter(|c| *c > i) {
                let inner = &code[i + 1..close];
                if re!(r"^\s*[A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*\s+[^\s,\]]").is_match(inner) {
                    params = go_type_param_list(inner);
                    i = close + 1;
                    skip(&mut i);
                }
            }
        }
        let alias = at_byte(b, i) == b'=';
        if alias {
            i += 1;
        }
        let underlying = js_trim(&code[i..go_statement_end(&code, i).max(i)]).to_string();
        if underlying.is_empty() {
            None
        } else {
            Some(Rc::new(GoTypeSpec { alias, params: Rc::new(params), underlying }))
        }
    })
}

/// A struct declaration's named fields and embedded types.
fn go_struct_members(node: &NodeRef, ctx: &Ctx) -> Option<Rc<GoStructMembers>> {
    memoized::<StructMembersMemo>(ctx, node.id.clone(), || {
        let spec = go_type_spec(node, ctx)?;
        let open = re!(r"^struct\s*\{").find(&spec.underlying)?;
        let close = go_closing_bracket(&spec.underlying, open.end() - 1).filter(|c| *c > 0)?;
        let mut members = GoStructMembers { fields: IndexMap::new(), embedded: Vec::new(), params: Rc::clone(&spec.params) };
        for raw in go_split_top_level(&spec.underlying[open.end()..close], "\n;") {
            // A field tag is not part of the member.
            let member = re!(r#"(?:`[^`]*`|"[^"]*")\s*$"#).replace_first(&raw, |_| String::new());
            let member = js_trim(&member);
            if member.is_empty() {
                continue;
            }
            if let Some(embedded) = re!(r"^\*?\s*([A-Za-z_]\w*(?:\s*\.\s*[A-Za-z_]\w*)?(?:\s*\[[\s\S]*\])?)$").captures(member) {
                members.embedded.push(embedded.text(1).unwrap_or("").to_string());
                continue;
            }
            let Some(named) = re!(r"^([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)\s+(\S[\s\S]*)$").captures(member) else { continue };
            for n in named.text(1).unwrap_or("").split(',') {
                members.fields.insert(js_trim(n).to_string(), named.text(2).unwrap_or("").to_string());
            }
        }
        Some(Rc::new(members))
    })
}

// -- Named types ------------------------------------------------------------------------------------

/// A named type's declarations in its package.
pub struct GoNamedDecls {
    pub name: String,
    pub dir: String,
    pub nodes: Vec<NodeRef>,
}

fn go_named_type_decls(t: &GoTypeText, ctx: &Ctx) -> Tri<GoNamedDecls> {
    let Some(m) = re!(r"^([A-Za-z_]\w*)(?:\.([A-Za-z_]\w*))?(?:\[[\s\S]*\])?$").captures(&t.text) else { return Tri::Undefined };
    let first = m.text(1).unwrap_or("");
    let second = m.text(2).filter(|s| !s.is_empty());
    let (name, dir) = match second {
        None => {
            if t.params.as_ref().is_some_and(|p| p.contains_key(first)) {
                return Tri::Undefined;
            }
            if GO_BUILTIN_FIELD_TYPES.contains(first) {
                return Tri::Null;
            }
            match go_bare_type_dir(first, &t.file, ctx) {
                Some(dir) => (first.to_string(), dir),
                None => return Tri::Null,
            }
        }
        Some(second) => match go_import_package_dir(first, &t.file, ctx) {
            Tri::Undefined => return Tri::Undefined,
            Tri::Null => return if is_go_outside_package(first, &t.file, ctx) { Tri::Null } else { Tri::Undefined },
            Tri::Value(dir) => (second.to_string(), dir),
        },
    };
    let nodes = go_package_types(&name, &dir, ctx);
    if nodes.is_empty() {
        Tri::Undefined
    } else {
        Tri::Value(GoNamedDecls { name, dir, nodes })
    }
}

/// The method a call names on a receiver of type `t` (an element, a field, a call's result).
fn go_method_on_element(t: &GoTypeText, r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    let mut text = t.text.strip_prefix('*').unwrap_or(&t.text).to_string();
    if text.starts_with('*') || go_is_type_literal(&text) {
        return Tri::Null;
    }
    // A type parameter has the methods its constraint names.
    if let Some(constraint) = t.params.as_ref().and_then(|p| p.get(&text)) {
        if constraint.is_empty() {
            return Tri::Undefined;
        }
        text = go_normalize_type(constraint);
        if !re!(r"^[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)?(?:\[[\s\S]*\])?$").is_match(&text) || text == "comparable" {
            return Tri::Null;
        }
    }
    let probe = GoTypeText { text, ..t.clone() };
    let named = match go_named_type_decls(&probe, ctx) {
        Tri::Value(n) => n,
        Tri::Null => return Tri::Null,
        Tri::Undefined => return Tri::Undefined,
    };
    if named.nodes.iter().all(|n| matches!(go_alias_target(n, ctx), Tri::Null)) {
        return Tri::Null;
    }
    Tri::from(resolve_method_on_type(&named.name, &r.reference_name, r, ctx, 0.9, ResolvedBy::InstanceMethod, Some(&named.dir), 0))
}

// -- Values ------------------------------------------------------------------------------------------

fn kind_at(links: &[GoChainLink], i: usize) -> Option<GoChainKind> {
    links.get(i).map(|l| l.kind)
}

/// `[T]` of a generic call's explicit type arguments: `f[int](x)`, `T[int]{…}`.
fn go_type_args_link(links: &[GoChainLink], k: usize) -> String {
    let Some(link) = links.get(k) else { return String::new() };
    let next = kind_at(links, k + 1);
    if link.kind == GoChainKind::Index && matches!(next, Some(GoChainKind::Call | GoChainKind::Composite)) {
        format!("[{}]", link.text)
    } else {
        String::new()
    }
}

/// The type of `links` read as a value: the receiver of the call that follows them.
#[allow(clippy::too_many_arguments)]
pub fn go_receiver_chain_type(
    links: &[GoChainLink],
    paren: bool,
    at: usize,
    file_path: &str,
    index: Option<&GoScopeIndex>,
    ctx: &Ctx,
    depth: u32,
    result: usize,
) -> GoTyped {
    let Some(head) = links.first().filter(|h| h.kind == GoChainKind::Operand) else { return Tri::Undefined };
    if depth > GO_VALUE_DEPTH {
        return Tri::Undefined;
    }
    // `….(T)` is a T, whatever the expression before it: prometheus's
    // `rw.converterPool.Get().(*otlptranslator.PrometheusConverter)` asserts what an outside
    // `sync.Pool` hands back.
    if let Some(last) = links.last().filter(|l| l.kind == GoChainKind::Assert && l.text != "type") {
        if result == 0 && links.len() > 1 {
            return Tri::Value(go_type_text_at(&last.text, file_path, at, index, ctx));
        }
    }
    let result_of = |k: usize| if k + 1 == links.len() { result } else { 0 };
    let mut t: GoTyped;
    let mut i = 1usize;
    if paren && kind_at(links, 1) == Some(GoChainKind::Call) && re!(r"^\*?\s*[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)?$").is_match(js_trim(&head.text)) {
        // A conversion `(*T)(x)` / `(T)(x)`.
        let ty = go_type_text_at(&head.text, file_path, at, index, ctx);
        let bare = GoTypeText { text: ty.text.strip_prefix('*').unwrap_or(&ty.text).to_string(), ..ty.clone() };
        match go_named_type_decls(&bare, ctx) {
            Tri::Null => return Tri::Null,
            Tri::Undefined => return Tri::Undefined,
            Tri::Value(_) => {}
        }
        t = Tri::Value(ty);
        i = 2;
    } else if paren {
        t = go_expr_type(&head.text, at, file_path, index, ctx, depth + 1, 0);
    } else {
        let decl = index.and_then(|ix| go_local_decl_at(ix, &head.text, at));
        if let (Some(decl), Some(ix)) = (decl, index) {
            // A local called as a function value: not settled here.
            if kind_at(links, 1) == Some(GoChainKind::Call) {
                return Tri::Undefined;
            }
            t = go_declared_value_type(&decl, file_path, ix, ctx, depth + 1);
        } else if ctx.get_import_mappings(file_path, Language::Go).iter().any(|m| m.local_name == head.text) {
            // `pkg.Name…`: a function, a variable or a type of an imported package.
            if kind_at(links, 1) != Some(GoChainKind::Select) {
                return Tri::Undefined;
            }
            let dir = match go_import_package_dir(&head.text, file_path, ctx) {
                Tri::Value(d) => d,
                Tri::Null => return if is_go_outside_package(&head.text, file_path, ctx) { Tri::Null } else { Tri::Undefined },
                Tri::Undefined => return Tri::Undefined,
            };
            let args = go_type_args_link(links, 2);
            let written = format!("{}.{}{}", head.text, links[1].text, args);
            let at2 = if args.is_empty() { 2 } else { 3 };
            let next = kind_at(links, at2);
            t = match next {
                Some(GoChainKind::Composite) => Tri::Value(go_type_text_at(&written, file_path, at, index, ctx)),
                Some(GoChainKind::Call) => go_called_type(&links[1].text, &dir, &written, file_path, true, at, index, ctx, depth + 1, result_of(at2)),
                _ => go_package_var_type_text(&links[1].text, &dir, file_path, true, ctx, depth + 1),
            };
            i = if matches!(next, Some(GoChainKind::Composite | GoChainKind::Call)) { at2 + 1 } else { 2 };
        } else if kind_at(links, 1) == Some(GoChainKind::Composite) || (kind_at(links, 1) == Some(GoChainKind::Index) && kind_at(links, 2) == Some(GoChainKind::Composite)) {
            // A composite literal `T{…}` / `T[int]{…}`.
            let args = go_type_args_link(links, 1);
            t = Tri::Value(go_type_text_at(&format!("{}{}", head.text, args), file_path, at, index, ctx));
            i = if args.is_empty() { 2 } else { 3 };
        } else if kind_at(links, 1) == Some(GoChainKind::Call) || (kind_at(links, 1) == Some(GoChainKind::Index) && kind_at(links, 2) == Some(GoChainKind::Call)) {
            // A call of a function of this package, or a builtin.
            let args = go_type_args_link(links, 1);
            t = if args.is_empty() && matches!(head.text.as_str(), "new" | "make" | "append") {
                go_expr_type(&format!("{}({})", head.text, links[1].text), at, file_path, index, ctx, depth + 1, 0)
            } else {
                go_called_type(&head.text, &go_package_dir(file_path), &format!("{}{}", head.text, args), file_path, false, at, index, ctx, depth + 1, result_of(if args.is_empty() { 1 } else { 2 }))
            };
            i = if args.is_empty() { 2 } else { 3 };
        } else {
            t = go_package_var_type_text(&head.text, &go_package_dir(file_path), file_path, false, ctx, depth + 1);
        }
    }
    if result > 0 && kind_at(links, links.len() - 1) != Some(GoChainKind::Call) {
        return Tri::Undefined;
    }
    while i < links.len() {
        let Tri::Value(cur) = &t else { break };
        let link = &links[i];
        let next_t: GoTyped = if link.kind == GoChainKind::Select && kind_at(links, i + 1) == Some(GoChainKind::Call) {
            let n = go_method_result_type(cur, &link.text, file_path, ctx, depth + 1, result_of(i + 1));
            i += 1;
            n
        } else if link.kind == GoChainKind::Select {
            go_field_type_of(cur, &link.text, ctx, depth + 1)
        } else if link.kind == GoChainKind::Index {
            if go_is_slice_index(&link.text) {
                go_sliced_type(cur, ctx, depth + 1)
            } else {
                go_element_of(&t, ctx, depth + 1)
            }
        } else if link.kind == GoChainKind::Assert && link.text != "type" {
            Tri::Value(go_type_text_at(&link.text, file_path, at, index, ctx))
        } else {
            return Tri::Undefined;
        };
        t = next_t;
        i += 1;
    }
    t
}

/// The type of a call of the function `name` of package `dir` (`result` picks which of several
/// results). A call of a type is a conversion.
#[allow(clippy::too_many_arguments)]
fn go_called_type(name: &str, dir: &str, written: &str, from_file: &str, imported: bool, at: usize, index: Option<&GoScopeIndex>, ctx: &Ctx, depth: u32, result: usize) -> GoTyped {
    if depth > GO_VALUE_DEPTH {
        return Tri::Undefined;
    }
    let clause = if imported { None } else { go_package_clause(from_file, ctx) };
    let test_file = |f: &str| f.ends_with("_test.go");
    let fns: Vec<NodeRef> = ctx
        .get_nodes_by_name(name)
        .iter()
        .filter(|n| {
            n.language == Language::Go
                && n.kind == NodeKind::Function
                && go_package_dir(&n.file_path) == dir
                && if imported { !test_file(&n.file_path) } else { go_package_clause(&n.file_path, ctx) == clause && (test_file(from_file) || !test_file(&n.file_path)) }
        })
        .cloned()
        .collect();
    // A bare call the file's own package doesn't declare may be a package's the file dot-imports:
    // kubernetes' `package watch_test` calls `NewStreamWatcher(…)` through
    // `. "k8s.io/apimachinery/pkg/watch"`, the package of its own directory.
    if fns.is_empty() && !imported && go_package_types(name, dir, ctx).is_empty() {
        for d in go_dot_imports(from_file, ctx).dirs.iter() {
            let t = go_called_type(name, d, written, from_file, true, at, index, ctx, depth + 1, result);
            if !t.is_undefined() {
                return t;
            }
        }
    }
    if fns.is_empty() {
        // A conversion `T(x)`: the type itself.
        return if result == 0 && !go_package_types(name, dir, ctx).is_empty() { Tri::Value(go_type_text_at(written, from_file, at, index, ctx)) } else { Tri::Undefined };
    }
    let mut found: GoTyped = Tri::Undefined;
    for f in &fns {
        let Some(t) = go_signature_result_type(f, ctx, result) else { return Tri::Undefined };
        if let Tri::Value(prev) = &found {
            if prev.text != t.text {
                return Tri::Undefined;
            }
        }
        found = Tri::Value(t);
    }
    found
}

/// The result type of the method `name` of a receiver of type `t`.
fn go_method_result_type(t: &GoTypeText, name: &str, file_path: &str, ctx: &Ctx, depth: u32, result: usize) -> GoTyped {
    if depth > GO_VALUE_DEPTH {
        return Tri::Undefined;
    }
    let probe = UnresolvedRef {
        from_node_id: String::new(),
        reference_name: name.to_string(),
        reference_kind: codegraph_types::ReferenceKind::Edge(codegraph_types::EdgeKind::Calls),
        line: 1,
        column: 0,
        file_path: file_path.to_string(),
        language: Language::Go,
        candidates: None,
        row_id: None,
        name_tail: None,
    };
    let method = go_method_on_element(t, &probe, ctx);
    if let Tri::Value(method) = &method {
        return match ctx.node_by_id(&method.target_node_id) {
            Some(node) => go_signature_result_type(&node, ctx, result).map_or(Tri::Undefined, Tri::Value),
            None => Tri::Undefined,
        };
    }
    let via_field = go_func_field_result_type(t, name, ctx, depth + 1, result);
    if !via_field.is_undefined() || method.is_undefined() {
        return via_field;
    }
    let text = t.text.strip_prefix('*').unwrap_or(&t.text);
    if re!(r"^(?:struct|interface)\b").is_match(text) || (!go_is_type_literal(text) && text.ends_with(']')) {
        return Tri::Undefined;
    }
    match go_named_type_decls(&GoTypeText { text: text.to_string(), ..t.clone() }, ctx) {
        Tri::Value(_) => Tri::Undefined,
        _ => Tri::Null,
    }
}

/// What calling a func-typed field `name` of a value of type `t` gives: result `result` of the func
/// type the struct declares the field with. Undefined for no such field, or one of another type.
fn go_func_field_result_type(t: &GoTypeText, name: &str, ctx: &Ctx, depth: u32, result: usize) -> GoTyped {
    let Tri::Value(field) = go_field_type_of(t, name, ctx, depth + 1) else { return Tri::Undefined };
    if !re!(r"^func\s*\(").is_match(&field.text) {
        return Tri::Undefined;
    }
    let signature = re!(r"^func\s*").replace_first(&field.text, |_| String::new());
    match go_result_text(&signature, result) {
        Some(text) => Tri::Value(GoTypeText { text: go_normalize_type(&text), file: field.file, params: field.params }),
        None => Tri::Undefined,
    }
}

/// The type a function's signature declares for result number `result`.
fn go_signature_result_type(f: &NodeRef, ctx: &Ctx, result: usize) -> Option<GoTypeText> {
    let text = go_result_text(f.signature.as_deref().unwrap_or(""), result)?;
    let index = go_scope_index(&f.file_path, ctx);
    let line_start = index.as_ref().map_or(0, |ix| ix.line_starts.get(f.start_line as usize - 1).copied().unwrap_or(0));
    let params = index.as_ref().and_then(|ix| go_type_params_at(ix, &f.file_path, line_start, ctx));
    Some(GoTypeText { text: go_normalize_type(&text), file: f.file_path.clone(), params })
}

/// Result `result` of a Go signature written `(params) results`, as written: `CardDo` of
/// `(value int) CardDo`, `*Response` of `(ctx context.Context) (*Response, error)`, `int` of
/// `() (n int, err error)`.
fn go_result_text(signature: &str, result: usize) -> Option<String> {
    let sig = js_trim(signature);
    if !sig.starts_with('(') {
        return None;
    }
    let close = go_closing_bracket(sig, 0)?;
    let mut text = js_trim(&sig[close + 1..]).to_string();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('(') {
        let end = go_closing_bracket(&text, 0)?;
        let parts: Vec<String> = go_split_top_level(&text[1..end], ",").iter().map(|p| js_trim(p).to_string()).filter(|p| !p.is_empty()).collect();
        // `(n int, err error)`: named results; the wanted one is the first named from `result`.
        let named = |p: &str| -> Option<String> {
            let m = re!(r"^([A-Za-z_]\w*)\s+(\S[\s\S]*)$").captures(p)?;
            if re!(r"^(?:chan|func|map|struct|interface)$").is_match(m.text(1).unwrap_or("")) {
                None
            } else {
                m.text(2).map(String::from)
            }
        };
        let wanted: Option<String> = if parts.iter().any(|p| named(p).is_some()) {
            (result..parts.len()).find_map(|r| named(&parts[r]))
        } else {
            parts.get(result).cloned()
        };
        text = js_trim(&wanted.filter(|w| !w.is_empty())?).to_string();
    } else if result > 0 {
        return None;
    }
    Some(text)
}

/// The type of an expression written as text: `&T{…}`, `*p`, `make(T, n)`, `T(x)`, a chain.
#[allow(clippy::too_many_arguments)]
pub fn go_expr_type(expr: &str, at: usize, file_path: &str, index: Option<&GoScopeIndex>, ctx: &Ctx, depth: u32, result: usize) -> GoTyped {
    let s = js_trim(expr);
    if s.is_empty() || depth > GO_VALUE_DEPTH {
        return Tri::Undefined;
    }
    if result > 0 {
        // A multi-value call: `a, b := f()`.
        if !re!(r"\)\s*$").is_match(s) {
            return Tri::Undefined;
        }
        let mut end = 0usize;
        let links = go_chain_links(s, 0, Some(&mut end), true);
        if end != s.len() || links.last().map(|l| l.kind) != Some(GoChainKind::Call) {
            return Tri::Undefined;
        }
        return go_receiver_chain_type(&links, s.starts_with('('), at, file_path, index, ctx, depth + 1, result);
    }
    if let Some(rest) = s.strip_prefix('&') {
        return match go_expr_type(rest, at, file_path, index, ctx, depth + 1, 0) {
            Tri::Value(t) => Tri::Value(GoTypeText { text: format!("*{}", t.text), ..t }),
            other => other,
        };
    }
    if let Some(rest) = s.strip_prefix('*') {
        return match go_expr_type(rest, at, file_path, index, ctx, depth + 1, 0) {
            Tri::Value(t) => match t.text.strip_prefix('*') {
                Some(inner) => Tri::Value(GoTypeText { text: inner.to_string(), ..t.clone() }),
                None => Tri::Undefined,
            },
            other => other,
        };
    }
    // `make(T, …)`, `new(T)`, `append(s, …)`.
    if let Some(builtin) = re!(r"^(make|new|append)\s*\(").captures(s) {
        let open = builtin.get(0).map_or(0, |m| m.end()) - 1;
        if go_closing_bracket(s, open) != Some(s.len() - 1) {
            return Tri::Undefined;
        }
        let first = go_split_top_level(&s[open + 1..s.len() - 1], ",").into_iter().next().map(|f| js_trim(&f).to_string()).unwrap_or_default();
        if first.is_empty() {
            return Tri::Undefined;
        }
        return match builtin.text(1).unwrap_or("") {
            "make" => Tri::Value(go_type_text_at(&first, file_path, at, index, ctx)),
            "new" => Tri::Value(go_type_text_at(&format!("*{first}"), file_path, at, index, ctx)),
            _ => go_expr_type(&first, at, file_path, index, ctx, depth + 1, 0),
        };
    }
    // `T{…}`, `pkg.T{…}`, `[]T{…}`, `T(x)`.
    let type_end = go_type_text_end(s, 0);
    if type_end > 0 {
        let type_end = type_end as usize;
        let b = s.as_bytes();
        let mut j = type_end;
        while matches!(at_byte(b, j), b' ' | b'\t') {
            j += 1;
        }
        if matches!(at_byte(b, j), b'{' | b'(') && go_closing_bracket(s, j) == Some(s.len() - 1) {
            let ty = go_type_text_at(&s[..type_end], file_path, at, index, ctx);
            if at_byte(b, j) == b'{' || go_is_type_literal(&ty.text) {
                return Tri::Value(ty);
            }
            // `T(x)` is a conversion only when `T` is a type.
            match go_named_type_decls(&ty, ctx) {
                Tri::Value(_) => return Tri::Value(ty),
                Tri::Null if re!(r"^[A-Za-z_]\w*\.[A-Za-z_]\w*$").is_match(&ty.text) => return Tri::Null,
                _ => {}
            }
        }
    }
    let mut end = 0usize;
    let links = go_chain_links(s, 0, Some(&mut end), true);
    if end != s.len() {
        return Tri::Undefined;
    }
    go_receiver_chain_type(&links, s.starts_with('('), at, file_path, index, ctx, depth + 1, 0)
}

/// Whether the value a Go declaration gives its name is a call or a conversion: `f(…)`,
/// `pkg.F(…)`, `x.M(…)`, `T(x)`, `a.b().C(…)`, alone or as this name's place among several values
/// (`a, b := f(), g()`). Not a `range` clause, a builtin's `make`/`new`/`append`, an assertion
/// (`go_asserted_local_type` reads those) or a type switch's guard.
fn go_value_is_call(local: &GoDecl, index: &GoScopeIndex) -> bool {
    let Some(GoValueAt { at, k, n }) = local.value else { return false };
    let code = &index.code;
    let end = if local.kind == GoDeclKind::Header { go_header_value_end(code, at) } else { go_statement_end(code, at) };
    let rhs = slice_bytes(code, at, end);
    if re!(r"^\s*range\b").is_match(rhs) {
        return false;
    }
    let values = go_split_top_level(rhs, ",");
    let value = if values.len() == n { values.get(k).map_or(rhs, String::as_str) } else { rhs };
    // A type switch's guard (`switch v := x.(type)`) has each case's type, and an assertion
    // (`v := x.(T)`) is `go_asserted_local_type`'s to read.
    if re!(r"\.\s*\([^()]*(?:\([^()]*\)[^()]*)*\)\s*$").is_match(value) {
        return false;
    }
    re!(r"^\s*[A-Za-z_(][\s\S]*\)\s*$").is_match(value) && !re!(r"^\s*(?:new|make|append)\s*\(").is_match(value)
}

/// The type of the call or conversion a Go local's declaration gives it, read as the indexed and
/// chained calls read it: what the function or method returns (the k-th result of several), or the
/// type converted to. `None` when the local's value is not a call.
pub fn go_call_value_type(local: &GoDecl, file_path: &str, ctx: &Ctx) -> Option<GoTyped> {
    local.value?;
    let index = go_scope_index(file_path, ctx)?;
    go_value_is_call(local, &index).then(|| go_declared_value_type(local, file_path, &index, ctx, 0))
}

/// Whether a Go value of type `t` has no method the project declares: a pointer to a pointer, a
/// type literal, a type from outside the project or a predeclared one, or an alias of such a type.
/// A type parameter or a type the index can't tell is not ruled out.
pub fn go_valued_has_no_project_methods(t: &GoTypeText, ctx: &Ctx) -> bool {
    let text = t.text.strip_prefix('*').unwrap_or(&t.text);
    if text.starts_with('*') || go_is_type_literal(text) {
        return true;
    }
    match go_named_type_decls(&GoTypeText { text: text.to_string(), ..t.clone() }, ctx) {
        Tri::Null => true,
        Tri::Undefined => false,
        Tri::Value(named) => named.nodes.iter().all(|n| matches!(go_alias_target(n, ctx), Tri::Null)),
    }
}

/// The type a local's declaration gives it: its written type, or the type of its initialiser.
fn go_declared_value_type(decl: &GoDecl, file_path: &str, index: &GoScopeIndex, ctx: &Ctx, depth: u32) -> GoTyped {
    if let Some(GoValueAt { at, k, n }) = decl.value {
        let code = &index.code;
        let end = if decl.kind == GoDeclKind::Header { go_header_value_end(code, at) } else { go_statement_end(code, at) };
        let rhs = slice_bytes(code, at, end);
        if let Some(ranged) = re!(r"^\s*range\b").find(rhs) {
            // `for i, v := range xs`: the value is xs's element.
            if k != 1 || n != 2 {
                return Tri::Undefined;
            }
            return go_element_of(&go_expr_type(&rhs[ranged.end()..], decl.at, file_path, Some(index), ctx, depth + 1, 0), ctx, depth + 1);
        }
        let values = go_split_top_level(rhs, ",");
        let value: Option<&str> = if values.len() == n {
            values.get(k).map(String::as_str)
        } else if values.len() == 1 && n == 2 && k == 0 && re!(r"[\])]\s*$").is_match(rhs) {
            Some(rhs)
        } else {
            None
        };
        if let Some(v) = value {
            return go_expr_type(v, decl.at, file_path, Some(index), ctx, depth + 1, 0);
        }
        // `a, b := f()`: result number `k` of the call.
        if values.len() == 1 && n > 1 && re!(r"\)\s*$").is_match(rhs) {
            return go_expr_type(rhs, decl.at, file_path, Some(index), ctx, depth + 1, k);
        }
        return Tri::Undefined;
    }
    let Some(ty) = &decl.ty else { return Tri::Undefined };
    let ty = ty.strip_prefix("...").map_or_else(|| ty.clone(), |rest| format!("[]{rest}"));
    Tri::Value(go_type_text_at(&ty, file_path, decl.at, Some(index), ctx))
}

/// The type of the package-level variable `name` of package `dir`.
fn go_package_var_type_text(name: &str, dir: &str, from_file: &str, imported: bool, ctx: &Ctx, depth: u32) -> GoTyped {
    if depth > GO_VALUE_DEPTH {
        return Tri::Undefined;
    }
    let clause = if imported { None } else { go_package_clause(from_file, ctx) };
    let test_file = |f: &str| f.ends_with("_test.go");
    let vars: Vec<NodeRef> = ctx
        .get_nodes_by_name(name)
        .iter()
        .filter(|n| {
            n.language == Language::Go
                && matches!(n.kind, NodeKind::Variable | NodeKind::Constant)
                && go_package_dir(&n.file_path) == dir
                && if imported { !test_file(&n.file_path) } else { go_package_clause(&n.file_path, ctx) == clause && (test_file(from_file) || !test_file(&n.file_path)) }
        })
        .cloned()
        .collect();
    let mut found: GoTyped = Tri::Undefined;
    for v in &vars {
        let t = go_package_var_spec_type(v, name, ctx, depth + 1);
        match (&found, &t) {
            (_, Tri::Undefined) => return Tri::Undefined,
            (Tri::Undefined, _) => {}
            (Tri::Null, Tri::Null) => {}
            (Tri::Value(a), Tri::Value(b)) if a.text == b.text => {}
            _ => return Tri::Undefined,
        }
        found = t;
    }
    found
}

fn go_package_var_spec_type(v: &NodeRef, name: &str, ctx: &Ctx, depth: u32) -> GoTyped {
    match go_package_value_type(v, ctx) {
        Tri::Null => return Tri::Null,
        Tri::Value((written, file)) => return Tri::Value(go_type_text_at(&written, &file, 0, None, ctx)),
        Tri::Undefined => {}
    }
    let Some(lines) = ctx.get_file_lines(&v.file_path) else { return Tri::Undefined };
    // The declaration's own lines: its initialiser.
    let from = (v.start_line as usize).saturating_sub(1);
    let to = v.end_line.max(v.start_line) as usize;
    if from >= lines.len() {
        return Tri::Undefined;
    }
    let code = blank_go_code(&lines[from..lines.len().min(to)].join("\n"));
    let start = col_byte(&lines[from], v.start_column);
    let head = &code[..go_statement_end(&code, start).min(code.len())];
    let Some(spec) = re!(r"^\s*(?:(?:var|const)\s+)?([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)\s*=(?!=)\s*([\s\S]*)$").captures(head) else { return Tri::Undefined };
    let names: Vec<String> = spec.text(1).unwrap_or("").split(',').map(|x| js_trim(x).to_string()).collect();
    let values = go_split_top_level(spec.text(2).unwrap_or(""), ",");
    match names.iter().position(|n| n == name) {
        Some(k) if values.len() == names.len() => go_expr_type(&values[k], 0, &v.file_path, None, ctx, depth + 1, 0),
        _ => Tri::Undefined,
    }
}

// -- Fields, elements, slices -------------------------------------------------------------------------

/// The type of the field (or embedded type) `name` of a struct type, searched through embedding.
fn go_field_type_of(t: &GoTypeText, name: &str, ctx: &Ctx, depth: u32) -> GoTyped {
    let text = t.text.strip_prefix('*').unwrap_or(&t.text).to_string();
    if text.starts_with('*') || go_is_type_literal(&text) || depth > GO_VALUE_DEPTH {
        return Tri::Undefined;
    }
    let mut level: Vec<GoTypeText> = vec![GoTypeText { text, ..t.clone() }];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut d = 0;
    while d < 4 && !level.is_empty() {
        let mut found: Vec<GoTypeText> = Vec::new();
        let mut next: Vec<GoTypeText> = Vec::new();
        let mut outside = false;
        for ty in &level {
            let (members, file) = match go_struct_of(ty, ctx, depth + 1) {
                Tri::Null => {
                    outside = true;
                    continue;
                }
                Tri::Undefined => continue,
                Tri::Value(s) => s,
            };
            if let Some(own) = members.fields.get(name) {
                found.push(GoTypeText { text: go_normalize_type(own), file: file.clone(), params: Some(Rc::clone(&members.params)) });
            }
            // An embedded type is a field named after itself.
            for e in &members.embedded {
                let norm = go_normalize_type(e);
                let bare = re!(r"^[A-Za-z_]\w*\.").replace_first(&re!(r"\[[\s\S]*\]$").replace_first(norm.strip_prefix('*').unwrap_or(&norm), |_| String::new()), |_| String::new());
                if bare == name {
                    found.push(GoTypeText { text: norm, file: file.clone(), params: Some(Rc::clone(&members.params)) });
                }
            }
            for e in &members.embedded {
                if !seen.insert(format!("{file}\0{e}")) {
                    continue;
                }
                next.push(GoTypeText { text: go_normalize_type(e), file: file.clone(), params: Some(Rc::clone(&members.params)) });
            }
        }
        if found.len() == 1 {
            return Tri::Value(found.remove(0));
        }
        if found.len() > 1 {
            return Tri::Undefined;
        }
        // Nothing found, and part of the search ran into a type from outside the project.
        if outside && next.is_empty() {
            return Tri::Null;
        }
        level = next;
        d += 1;
    }
    Tri::Undefined
}

/// A named type's struct members (through an alias's underlying type), and the file declaring it.
fn go_struct_of(t: &GoTypeText, ctx: &Ctx, depth: u32) -> Tri<(Rc<GoStructMembers>, String)> {
    if depth > GO_VALUE_DEPTH {
        return Tri::Undefined;
    }
    let bare = GoTypeText { text: t.text.strip_prefix('*').unwrap_or(&t.text).to_string(), ..t.clone() };
    let named = match go_named_type_decls(&bare, ctx) {
        Tri::Value(n) => n,
        Tri::Null => return Tri::Null,
        Tri::Undefined => return Tri::Undefined,
    };
    for n in &named.nodes {
        if n.kind == NodeKind::Struct {
            if let Some(members) = go_struct_members(n, ctx) {
                return Tri::Value((members, n.file_path.clone()));
            }
        } else if n.kind == NodeKind::TypeAlias {
            let Some(spec) = go_type_spec(n, ctx) else { continue };
            let via = go_struct_of(&GoTypeText { text: go_normalize_type(&spec.underlying), file: n.file_path.clone(), params: Some(Rc::clone(&spec.params)) }, ctx, depth + 1);
            if !via.is_undefined() {
                return via;
            }
        }
    }
    Tri::Undefined
}

/// The element type of a slice / array / map / pointer-to-array type.
fn go_element_of(t: &GoTyped, ctx: &Ctx, depth: u32) -> GoTyped {
    let Tri::Value(t) = t else { return t.clone() };
    if depth > GO_VALUE_DEPTH {
        return Tri::Value(t.clone());
    }
    let mut s = t.text.as_str();
    if let Some(rest) = s.strip_prefix('*') {
        s = rest;
        if !re!(r"^\[[^\]]").is_match(s) {
            return Tri::Undefined;
        }
    }
    if s.starts_with('[') || s.starts_with("map[") {
        let element = match s.find('[').and_then(|o| go_closing_bracket(s, o)) {
            Some(close) => js_trim(&s[close + 1..]).to_string(),
            None => String::new(),
        };
        return if element.is_empty() { Tri::Undefined } else { Tri::Value(GoTypeText { text: element, ..t.clone() }) };
    }
    if s == "string" {
        return Tri::Null;
    }
    if go_is_type_literal(s) {
        return Tri::Undefined;
    }
    let named = match go_named_type_decls(&GoTypeText { text: s.to_string(), ..t.clone() }, ctx) {
        Tri::Value(n) => n,
        Tri::Null => return Tri::Null,
        Tri::Undefined => return Tri::Undefined,
    };
    for n in &named.nodes {
        let spec = if n.kind == NodeKind::TypeAlias { go_type_spec(n, ctx) } else { None };
        let Some(spec) = spec else { continue };
        let element = go_element_of(&Tri::Value(GoTypeText { text: go_normalize_type(&spec.underlying), file: n.file_path.clone(), params: Some(Rc::clone(&spec.params)) }), ctx, depth + 1);
        if !element.is_undefined() {
            return element;
        }
    }
    Tri::Undefined
}

/// `xs[a:b]` is a slice of the same type; of an array, a slice of its element.
fn go_sliced_type(t: &GoTypeText, ctx: &Ctx, depth: u32) -> GoTyped {
    let s = t.text.strip_prefix('*').unwrap_or(&t.text);
    let array = if re!(r"^\[[^\]]").is_match(s) { Some(GoTypeText { text: s.to_string(), ..t.clone() }) } else { None };
    if array.is_none() && t.text.starts_with('*') {
        return Tri::Undefined;
    }
    let mut underlying = array;
    if underlying.is_none() && s != "string" && !s.starts_with('[') {
        match go_named_type_decls(t, ctx) {
            Tri::Undefined => return Tri::Undefined,
            Tri::Null => {}
            Tri::Value(named) => {
                for n in &named.nodes {
                    let spec = if n.kind == NodeKind::TypeAlias { go_type_spec(n, ctx) } else { None };
                    if let Some(spec) = spec.filter(|sp| re!(r"^\[[^\]]").is_match(&sp.underlying)) {
                        underlying = Some(GoTypeText { text: go_normalize_type(&spec.underlying), file: n.file_path.clone(), params: Some(Rc::clone(&spec.params)) });
                    }
                }
            }
        }
    }
    let Some(underlying) = underlying else { return Tri::Value(t.clone()) };
    match go_element_of(&Tri::Value(underlying), ctx, depth + 1) {
        Tri::Value(e) => Tri::Value(GoTypeText { text: format!("[]{}", e.text), ..e }),
        other => other,
    }
}

// -- Calls through a chain ------------------------------------------------------------------------------

fn call_after_index(links: &[GoChainLink], name: &str) -> Option<usize> {
    let k = links.iter().enumerate().position(|(i, l)| i > 0 && l.kind == GoChainKind::Select && l.text == name && kind_at(links, i + 1) == Some(GoChainKind::Call))?;
    if k > 0 && links[k - 1].kind == GoChainKind::Index {
        Some(k)
    } else {
        None
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `xs[i].Method()`: the method on the indexed element.
pub fn match_go_indexed_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if r.language != Language::Go || !is_calls(r) || !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        return Tri::Undefined;
    }
    let Some(lines) = ctx.get_file_lines(&r.file_path) else { return Tri::Undefined };
    let Some(line) = (r.line as usize).checked_sub(1).and_then(|i| lines.get(i)) else { return Tri::Undefined };
    if !after_col(line, r.column).contains("].") {
        return Tri::Undefined;
    }
    if r.column > 0 && before_col(line, r.column).bytes().next_back().is_some_and(is_word_byte) {
        return Tri::Undefined;
    }
    if starts_with_at(line, &r.reference_name, r.column) && re!(r"^\s*\(").is_match(after_col(line, r.column + r.reference_name.len() as u32)) {
        return Tri::Undefined;
    }
    let from = r.line as usize - 1;
    let joined = lines[from..lines.len().min(r.line as usize + GO_CHAIN_LINES)].join("\n");
    if call_after_index(&go_chain_links(&joined, col_byte(line, r.column), None, false), &r.reference_name).is_none() {
        return Tri::Undefined;
    }
    let Some(index) = go_scope_index(&r.file_path, ctx) else { return Tri::Undefined };
    let at = go_ref_offset(&index, ctx, r);
    let links = go_chain_links(&index.code, at, None, false);
    let Some(k) = call_after_index(&links, &r.reference_name) else { return Tri::Undefined };
    match go_receiver_chain_type(&links[..k], at_byte(index.code.as_bytes(), at) == b'(', at, &r.file_path, Some(&index), ctx, 0, 0) {
        Tri::Value(receiver) => go_method_on_element(&receiver, r, ctx),
        Tri::Null => Tri::Null,
        Tri::Undefined => Tri::Undefined,
    }
}

/// `a.b().c()`: the method on the type the chain's earlier links give.
pub fn match_go_chained_call(r: &UnresolvedRef, ctx: &Ctx) -> Tri<ResolvedRef> {
    if r.language != Language::Go || !is_calls(r) || !re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name) {
        return Tri::Undefined;
    }
    let Some(index) = go_scope_index(&r.file_path, ctx) else { return Tri::Undefined };
    let at = go_ref_offset(&index, ctx, r);
    let code = index.code.as_str();
    let b = code.as_bytes();
    // A name written bare, with a call straight after it, is not a link of a chain.
    if at > 0 && is_word_byte(at_byte(b, at - 1)) {
        return Tri::Undefined;
    }
    let name = r.reference_name.as_str();
    if code.get(at..).is_some_and(|rest| rest.starts_with(name)) && re!(r"^\s*\(").is_match(slice_bytes(code, at + name.len(), at + name.len() + 16)) {
        return Tri::Undefined;
    }
    let links = go_chain_links(code, at, None, true);
    let mut k: Option<usize> = None;
    let mut i = links.len() as i64 - 2;
    while i > 0 && k.is_none() {
        let iu = i as usize;
        if links[iu].kind == GoChainKind::Select && links[iu].text == name && links[iu + 1].kind == GoChainKind::Call {
            k = Some(iu);
        }
        i -= 1;
    }
    let Some(k) = k else { return Tri::Undefined };
    match go_receiver_chain_type(&links[..k], at_byte(b, at) == b'(', at, &r.file_path, Some(&index), ctx, 0, 0) {
        Tri::Value(receiver) => match go_method_on_element(&receiver, r, ctx) {
            Tri::Value(v) => Tri::Value(v),
            _ => Tri::Null,
        },
        _ => Tri::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_is_split_into_its_links() {
        let links = go_chain_links("a.b(1).c[2].(T).d()", 0, None, false);
        let kinds: Vec<GoChainKind> = links.iter().map(|l| l.kind).collect();
        use GoChainKind::*;
        assert_eq!(kinds, vec![Operand, Select, Call, Select, Index, Assert, Select, Call]);
        assert_eq!(links[5].text, "T");
    }

    #[test]
    fn a_composite_literal_is_a_link_only_when_asked_for() {
        let with = go_chain_links("pkg.T{x: 1}.Run()", 0, None, true);
        assert!(with.iter().any(|l| l.kind == GoChainKind::Composite));
        let without = go_chain_links("pkg.T{x: 1}.Run()", 0, None, false);
        assert!(!without.iter().any(|l| l.kind == GoChainKind::Composite));
    }

    #[test]
    fn brackets_close_past_strings_and_comments() {
        assert_eq!(go_closing_bracket("(a, \")\", b) c", 0), Some(10));
        assert_eq!(go_closing_bracket("(a /* ) */ b)", 0), Some(12));
        assert_eq!(go_closing_bracket("(a", 0), None);
    }

    #[test]
    fn type_text_is_normalised() {
        assert_eq!(go_normalize_type("  * pkg . T "), "*pkg.T");
        assert_eq!(go_normalize_type("[] int"), "[]int");
    }
}
