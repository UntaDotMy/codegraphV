//! C++ type aliases — port of `src/resolution/cpp-type-aliases.ts`.
//!
//! A `using Alias = ns::Type;` / `typedef ns::Type Alias;` names the type it aliases: a
//! constructor call or a member access written through the alias belongs to the type, found
//! by C++ name lookup (innermost scope outward, base classes included), never by the alias
//! name alone.

use crate::ctx::Ctx;
use crate::types::{NodeRef, Tri, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_from, utf16_slice};
use codegraph_types::{Node, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

fn is_declaring_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Union
            | NodeKind::Enum
            | NodeKind::Interface
            | NodeKind::Namespace
            | NodeKind::Module
    )
}

fn is_class_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union)
}

const MAX_HOPS: usize = 4;
const MAX_BASE_DEPTH: usize = 4;
const HEAD_LINES: usize = 12;
const MAX_TEMPLATE_HEADER: usize = 1000;

/// A type an alias resolves to: `target` is the aliased type's written segments, `rest` the
/// segments written after the alias in the reference, `scope` the scope the target is looked up
/// from.
#[derive(Debug, Clone, PartialEq)]
pub struct CppAliasedType {
    pub target: Vec<String>,
    pub rest: Vec<String>,
    pub scope: String,
    pub pointer: bool,
    pub class_template: bool,
}

#[derive(Debug, Clone)]
struct TypeName {
    names: Vec<String>,
    templated: Vec<bool>,
    global: bool,
}

/// `text` with every `<…>` template-argument list removed.
pub fn strip_cpp_template_arguments(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        if c == '<' {
            depth += 1;
        } else if c == '>' {
            depth = depth.saturating_sub(1);
        } else if depth == 0 {
            out.push(c);
        }
    }
    out
}

fn cpp_type_name(raw: &str) -> Option<TypeName> {
    let mut parts: Vec<String> = Vec::new();
    let mut templated: Vec<bool> = Vec::new();
    let mut depth = 0usize;
    let mut parens = 0usize;
    let mut part = String::new();
    let mut args = false;
    let text = re!(r"\b(?:const|volatile|mutable|typename|template|class|struct|union|enum)\b", "g").replace_all_str(raw, " ");
    let text = re!(r"[&*]+", "g").replace_all_str(&text, " ");
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if depth > 0 && (c == '(' || c == ')') {
            parens = if c == '(' { parens + 1 } else { parens.saturating_sub(1) };
        } else if parens > 0 {
        } else if c == '<' {
            depth += 1;
            args = true;
        } else if c == '>' {
            depth = depth.saturating_sub(1);
        } else if depth > 0 {
        } else if c == ':' && chars.get(i + 1) == Some(&':') {
            parts.push(js_trim(&part).to_string());
            templated.push(args);
            part.clear();
            args = false;
            i += 1;
        } else {
            part.push(c);
        }
        i += 1;
    }
    parts.push(js_trim(&part).to_string());
    templated.push(args);
    let global = parts[0].is_empty();
    if global {
        parts.remove(0);
        templated.remove(0);
    }
    if parts.is_empty() || !parts.iter().all(|p| re!(r"^[A-Za-z_]\w*$").is_match(p)) {
        return None;
    }
    Some(TypeName { names: parts, templated, global })
}

fn scopes_for(name: &TypeName, scopes: &[String]) -> Vec<String> {
    if name.global {
        vec![String::new()]
    } else {
        scopes.to_vec()
    }
}

/// Does the written type end in a pointer (`T*`, not `T<U*>`)?
pub fn is_cpp_pointer_type(raw: &str) -> bool {
    strip_cpp_template_arguments(raw).contains('*')
}

/// The `::`-separated segments of a written type, or `None` when it is not a plain name.
pub fn cpp_type_segments(raw: &str) -> Option<Vec<String>> {
    cpp_type_name(raw).map(|n| n.names)
}

/// The top-level template arguments of the first `<…>` in `raw`.
pub fn cpp_template_arguments(raw: &str) -> Option<Vec<String>> {
    let open = raw.find('<')?;
    let mut depth = 0i32;
    for (i, b) in raw.bytes().enumerate().skip(open) {
        if b == b'<' {
            depth += 1;
        } else if b == b'>' {
            depth -= 1;
            if depth == 0 {
                return Some(split_top_level(&raw[open + 1..i]).iter().map(|a| js_trim(a).to_string()).collect());
            }
        }
    }
    None
}

fn is_c_family(n: &Node) -> bool {
    matches!(n.language, codegraph_types::Language::Cpp | codegraph_types::Language::C)
}

fn scopes_within(qualified: &str) -> Vec<String> {
    let parts: Vec<&str> = if qualified.is_empty() { Vec::new() } else { qualified.split("::").collect() };
    (0..=parts.len()).rev().map(|i| parts[..i].join("::")).collect()
}

fn parent_scope(qualified: &str) -> String {
    match qualified.rfind("::") {
        None => String::new(),
        Some(cut) => qualified[..cut].to_string(),
    }
}

fn caller_scope(caller: Option<&NodeRef>) -> String {
    match caller {
        None => String::new(),
        Some(c) if c.kind == NodeKind::File => String::new(),
        Some(c) if is_declaring_kind(c.kind) => c.qualified_name.clone(),
        Some(c) => parent_scope(&c.qualified_name),
    }
}

fn lines_of(file_path: &str, ctx: &Ctx) -> Option<Rc<Vec<String>>> {
    ctx.get_file_lines(file_path)
}

/// The text of `node`'s declaration — its own span for a `typedef`/`using`, else the whole lines.
fn declaration_text(node: &Node, ctx: &Ctx) -> Option<String> {
    let lines = lines_of(&node.file_path, ctx)?;
    let from = (node.start_line as usize).saturating_sub(1);
    let to = (node.end_line as usize).max(from);
    let mut span: Vec<String> = lines.iter().skip(from).take(to - from).cloned().collect();
    if span.is_empty() {
        return None;
    }
    let whole = span.join("\n");
    if span.len() == 1 {
        span[0] = utf16_slice(&span[0], node.start_column as usize, node.end_column as usize).to_string();
    } else {
        span[0] = utf16_from(&span[0], node.start_column as usize).to_string();
        let last = span.len() - 1;
        span[last] = utf16_slice(&span[last], 0, node.end_column as usize).to_string();
    }
    let cut = span.join("\n");
    Some(if re!(r"\b(?:typedef|using)\b").is_match(&cut) { cut } else { whole })
}

fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        match b {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' | b')' | b']' | b'}' => depth = (depth - 1).max(0),
            b',' if depth == 0 => {
                parts.push(text[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(text[start..].to_string());
    parts
}

/// The type `name` is declared an alias of in `text` (`using Name = T;` / `typedef T Name;`).
fn aliased_type_in(text: &str, name: &str) -> Option<String> {
    let escaped = escape_regex(name);
    let using = Re::cached(&format!(r"\busing\s+{escaped}\s*(?:\[\[[^\]]*\]\]\s*)?=\s*([^;]+)"), "");
    if let Some(m) = using.captures(text) {
        return Some(js_trim(m.text(1).unwrap_or("")).to_string());
    }
    let typedef = re!(r"\btypedef\b([^;]+)").captures(text)?;
    let first = split_top_level(typedef.text(1).unwrap_or("")).into_iter().next().unwrap_or_default();
    let declared = Re::cached(&format!(r"^([\s\S]*?)\b{escaped}\s*$"), "").captures(&first)?;
    let t = js_trim(declared.text(1).unwrap_or(""));
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Is the alias written under a `template<…>` header (an alias template)?
fn is_alias_template(alias: &Node, ctx: &Ctx) -> bool {
    let Some(lines) = lines_of(&alias.file_path, ctx) else { return false };
    let from = (alias.start_line as usize).saturating_sub(4);
    let to = (alias.start_line as usize).saturating_sub(1);
    let mut before: Vec<String> = lines.iter().take(to).skip(from).cloned().collect();
    before.push(utf16_slice(lines.get(alias.start_line as usize - 1).map(String::as_str).unwrap_or(""), 0, alias.start_column as usize).to_string());
    re!(r"\btemplate\s*<[^;{}]*>\s*$").is_match(&before.join(" "))
}

#[derive(Default)]
struct LocalAliasLines(RefCell<HashMap<String, Rc<Vec<usize>>>>);

fn local_alias_lines(caller: &Node, lines: &[String], ctx: &Ctx) -> Rc<Vec<usize>> {
    let memo = ctx.memo::<LocalAliasLines>();
    if let Some(found) = memo.0.borrow().get(&caller.id) {
        return Rc::clone(found);
    }
    let mut found = Vec::new();
    let last = (caller.end_line as usize).min(lines.len());
    let mut i = last as i64 - 1;
    while i >= caller.start_line as i64 - 1 && i >= 0 {
        if re!(r"\b(?:typedef\b|using\s+\w+\s*=)").is_match(&lines[i as usize]) {
            found.push(i as usize);
        }
        i -= 1;
    }
    let found = Rc::new(found);
    memo.0.borrow_mut().insert(caller.id.clone(), Rc::clone(&found));
    found
}

/// Does the function around `r` declare, before `r`, a local alias named like the first
/// segment of `written`?
pub fn cpp_declares_local_alias(written: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(name) = cpp_type_name(written) else { return false };
    if written.trim_start().starts_with("::") {
        return false;
    }
    !local_aliased_type(&name.names[0], ctx.node_by_id(&r.from_node_id).as_ref(), r, ctx).is_undefined()
}

/// The type a local alias named `name` in the calling function names: `Undefined` when there is
/// none, `Null` when it is one that names no plain type.
fn local_aliased_type(name: &str, caller: Option<&NodeRef>, r: &UnresolvedRef, ctx: &Ctx) -> Tri<String> {
    let Some(caller) = caller else { return Tri::Undefined };
    if !matches!(caller.kind, NodeKind::Function | NodeKind::Method) || caller.file_path != r.file_path {
        return Tri::Undefined;
    }
    let Some(lines) = lines_of(&r.file_path, ctx) else { return Tri::Undefined };
    let escaped = escape_regex(name);
    let declares = Re::cached(&format!(r"\b(?:using\s+{escaped}\s*=|typedef\b[^;]*\b{escaped}\s*[;,])"), "");
    for &i in local_alias_lines(caller, &lines, ctx).iter() {
        if i as i64 > r.line as i64 - 1 {
            continue;
        }
        let line = &lines[i];
        if line.contains(name) && declares.is_match(line) {
            return aliased_type_in(line, name).into();
        }
    }
    Tri::Undefined
}

#[derive(Debug, Clone)]
enum Declared {
    Alias(NodeRef),
    Cls(String),
    Other,
    Ambiguous(Vec<NodeRef>),
}

pub type Visible<'a> = Option<&'a dyn Fn(&Node) -> bool>;

fn type_declarations(name: &str, ctx: &Ctx, visible: Visible) -> Vec<NodeRef> {
    ctx.get_nodes_by_name(name)
        .iter()
        .filter(|n| {
            is_c_family(n)
                && (n.kind == NodeKind::TypeAlias || is_declaring_kind(n.kind))
                && visible.is_none_or(|v| v(n))
        })
        .cloned()
        .collect()
}

fn declared_in(scope: &str, name: &str, named: &[NodeRef], templated: bool, r: &UnresolvedRef, ctx: &Ctx) -> Option<Declared> {
    let qualified = if scope.is_empty() { name.to_string() } else { format!("{scope}::{name}") };
    let mut here: Vec<NodeRef> = named.iter().filter(|n| n.qualified_name == qualified).cloned().collect();
    if !templated {
        here.retain(|n| n.kind != NodeKind::TypeAlias || !is_alias_template(n, ctx));
    }
    if here.is_empty() {
        return None;
    }
    if here.iter().any(|n| is_class_kind(n.kind)) {
        return Some(Declared::Cls(qualified));
    }
    if here.iter().any(|n| n.kind != NodeKind::TypeAlias) {
        return Some(Declared::Other);
    }
    match pick_alias(&here, r, ctx) {
        Some(alias) => Some(Declared::Alias(alias)),
        None => Some(Declared::Ambiguous(here)),
    }
}

/// The one alias `r` means among several same-named ones, or `None` when it cannot tell.
fn pick_alias(aliases: &[NodeRef], r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    if aliases.len() == 1 {
        return Some(Rc::clone(&aliases[0]));
    }
    let same_file: Vec<&NodeRef> = aliases.iter().filter(|a| a.file_path == r.file_path).collect();
    if same_file.len() == 1 {
        return Some(Rc::clone(same_file[0]));
    }
    let stem = |p: &str| re!(r"\.[^./\\]+$").replace_first(p, |_| String::new());
    let header: Vec<&NodeRef> = aliases.iter().filter(|a| stem(&a.file_path) == stem(&r.file_path)).collect();
    if header.len() == 1 {
        return Some(Rc::clone(header[0]));
    }
    let mut named: HashSet<Option<String>> = HashSet::new();
    for a in aliases {
        let text = declaration_text(a, ctx);
        let ty = text.and_then(|t| aliased_type_in(&t, &a.name));
        named.insert(ty.map(|t| cpp_type_segments(&t).map(|s| s.join("::")).unwrap_or(t)));
    }
    if named.len() == 1 && !named.contains(&None) {
        Some(Rc::clone(&aliases[0]))
    } else {
        None
    }
}

#[derive(Default)]
struct Bases(RefCell<HashMap<String, Vec<String>>>);
#[derive(Default)]
struct TemplateParameters(RefCell<HashMap<String, Rc<HashSet<String>>>>);

fn collect_template_header(lines: &[String], start_line: u32, start_column: u32, names: &mut HashSet<String>) {
    let start_line = start_line as usize;
    let from = start_line.saturating_sub(9);
    let mut before: Vec<String> = lines.iter().take(start_line.saturating_sub(1)).skip(from).cloned().collect();
    before.push(utf16_slice(lines.get(start_line.wrapping_sub(1)).map(String::as_str).unwrap_or(""), 0, start_column as usize).to_string());
    let before = before.join("\n");
    let Some(at) = before.rfind("template") else { return };
    let Some(header) = re!(
        r"^template\s*<([\s\S]*)>(?:\s*(?:\[\[[^\]]*\]\]|(?:static|inline|constexpr|consteval|virtual|explicit|friend|extern)\b|[A-Z][A-Z0-9]*_[A-Z0-9_]*\b(?:\s*\([^()]*\))?))*\s*$"
    )
    .captures(&before[at..]) else {
        return;
    };
    for item in split_top_level(header.text(1).unwrap_or("")) {
        add_template_parameter(&item, names);
    }
}

fn add_template_parameter(item: &str, names: &mut HashSet<String>) {
    let stripped = strip_cpp_template_arguments(item);
    let declared = stripped.split('=').next().unwrap_or("");
    let found = re!(r"([A-Za-z_]\w*)\s*$").captures(declared).and_then(|m| m.text(1).map(String::from));
    if let Some(name) = found {
        if name != "typename" && name != "class" {
            names.insert(name);
        }
    }
}

/// Index (in UTF-16 units) of the `>` closing the template header opened at `open`.
fn template_header_end(text: &[u16], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut parens = 0i32;
    let mut i = open;
    while i < text.len() && i < open + MAX_TEMPLATE_HEADER {
        let c = text[i];
        if c == b'(' as u16 || c == b'[' as u16 {
            parens += 1;
        } else if c == b')' as u16 || c == b']' as u16 {
            parens = (parens - 1).max(0);
        } else if parens > 0 {
        } else if c == b'<' as u16 {
            depth += 1;
        } else if c == b'>' as u16 {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        } else if c == b';' as u16 || c == b'{' as u16 || c == b'}' as u16 {
            return None;
        }
        i += 1;
    }
    None
}

fn typedef_declarators(text: &[u16], from: usize) -> String {
    let mut depth = 0i32;
    let mut start = from;
    let mut i = from;
    while i < text.len() && i < from + 4 * MAX_TEMPLATE_HEADER {
        let c = text[i];
        if c == b'{' as u16 {
            depth += 1;
        } else if c == b'}' as u16 {
            depth -= 1;
            if depth < 0 {
                return String::new();
            }
            if depth == 0 {
                start = i + 1;
            }
        } else if c == b';' as u16 && depth == 0 {
            return String::from_utf16_lossy(&text[start..i]);
        }
        i += 1;
    }
    String::new()
}

/// The type names `text` declares: template parameters, `using` aliases and `typedef`s.
pub fn cpp_type_names_declared(text: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let units: Vec<u16> = text.encode_utf16().collect();
    for m in re!(r"\btemplate\s*<", "g").captures_iter(text) {
        let whole = m.get(0).unwrap();
        let open = whole.utf16_start() + whole.as_str().encode_utf16().count() - 1;
        if let Some(close) = template_header_end(&units, open) {
            if close > open {
                for item in split_top_level(&String::from_utf16_lossy(&units[open + 1..close])) {
                    add_template_parameter(&item, &mut names);
                }
            }
        }
    }
    for m in re!(r"\busing\s+([A-Za-z_]\w*)\s*(?:\[\[[^\]]*\]\]\s*)?=(?!=)", "g").captures_iter(text) {
        names.insert(m.text(1).unwrap_or("").to_string());
    }
    for m in re!(r"\btypedef\b", "g").captures_iter(text) {
        let whole = m.get(0).unwrap();
        let from = whole.utf16_start() + whole.as_str().encode_utf16().count();
        for declarator in split_top_level(&typedef_declarators(&units, from)) {
            let name = re!(r"\(\s*(?:[A-Za-z_]\w*\s*::\s*)*[*&^]+\s*(?:const\s+)?([A-Za-z_]\w*)\s*\)")
                .captures(&declarator)
                .and_then(|c| c.text(1).map(String::from))
                .or_else(|| {
                    let trimmed = re!(r"\([^()]*\)\s*(?:const\s*)?$").replace_first(&declarator, |_| String::new());
                    re!(r"([A-Za-z_]\w*)\s*(?:\[[^\]]*\]\s*)*$").captures(&trimmed).and_then(|c| c.text(1).map(String::from))
                });
            if let Some(name) = name {
                names.insert(name);
            }
        }
    }
    names
}

fn template_parameters_around(node: &Node, ctx: &Ctx) -> Rc<HashSet<String>> {
    let memo = ctx.memo::<TemplateParameters>();
    if let Some(hit) = memo.0.borrow().get(&node.id) {
        return Rc::clone(hit);
    }
    let mut names = HashSet::new();
    if let Some(lines) = lines_of(&node.file_path, ctx) {
        collect_template_header(&lines, node.start_line, node.start_column, &mut names);
        let mut scope = parent_scope(&node.qualified_name);
        while !scope.is_empty() {
            for cls in ctx.get_nodes_by_qualified_name(&scope).iter() {
                if !is_c_family(cls) || !is_class_kind(cls.kind) || cls.file_path != node.file_path {
                    continue;
                }
                if node.start_line < cls.start_line || node.start_line > cls.end_line {
                    continue;
                }
                collect_template_header(&lines, cls.start_line, cls.start_column, &mut names);
            }
            scope = parent_scope(&scope);
        }
    }
    let names = Rc::new(names);
    memo.0.borrow_mut().insert(node.id.clone(), Rc::clone(&names));
    names
}

/// The base classes of the class named `cls`, as qualified class names.
fn base_classes_of(cls: &str, ctx: &Ctx) -> Vec<String> {
    let memo = ctx.memo::<Bases>();
    if let Some(hit) = memo.0.borrow().get(cls) {
        return hit.clone();
    }
    // Guard a cycle: a class under construction has no bases yet.
    memo.0.borrow_mut().insert(cls.to_string(), Vec::new());
    let mut bases: Vec<String> = Vec::new();
    for decl in ctx.get_nodes_by_qualified_name(cls).iter() {
        if !is_c_family(decl) || !is_class_kind(decl.kind) {
            continue;
        }
        let Some(lines) = lines_of(&decl.file_path, ctx) else { continue };
        let from = (decl.start_line as usize).saturating_sub(1);
        let mut head: Vec<String> = lines.iter().skip(from).take(HEAD_LINES).cloned().collect();
        if head.is_empty() {
            continue;
        }
        head[0] = utf16_from(&head[0], decl.start_column as usize).to_string();
        let text = head.join("\n");
        let Some(brace) = text.find('{') else { continue };
        if text[..brace].contains(';') {
            continue;
        }
        let Some(colon) = re!(r"(?<!:):(?!:)").find(&text[..brace]).map(|m| m.start()) else { continue };
        for part in split_top_level(&text[colon + 1..brace]) {
            let cleaned = re!(r"\b(?:public|protected|private|virtual)\b", "g").replace_all_str(&part, " ");
            let Some(name) = cpp_type_name(&cleaned) else { continue };
            let base = class_named(&name.names, &scopes_for(&name, &scopes_within(&parent_scope(cls))), ctx);
            if let Some(base) = base {
                if base != cls && !bases.contains(&base) {
                    bases.push(base);
                }
            }
        }
    }
    memo.0.borrow_mut().insert(cls.to_string(), bases.clone());
    bases
}

fn class_reached(names: &[String], scope: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<String> {
    if let Some(spelled) = class_named(names, &scopes_within(scope), ctx) {
        return Some(spelled);
    }
    let tn = TypeName { names: names.to_vec(), templated: vec![false; names.len()], global: false };
    match lookup_segments(&tn, &scopes_within(scope), r, ctx, None).0 {
        Some(Declared::Cls(c)) => Some(c),
        _ => None,
    }
}

fn class_named(names: &[String], scopes: &[String], ctx: &Ctx) -> Option<String> {
    let spelled = names.join("::");
    for scope in scopes {
        let qualified = if scope.is_empty() { spelled.clone() } else { format!("{scope}::{spelled}") };
        if ctx.get_nodes_by_qualified_name(&qualified).iter().any(|n| is_c_family(n) && is_class_kind(n.kind)) {
            return Some(qualified);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn member_of(
    cls: &str,
    name: &str,
    named: &[NodeRef],
    templated: bool,
    r: &UnresolvedRef,
    ctx: &Ctx,
    depth: usize,
    seen: &mut HashSet<String>,
) -> Option<Declared> {
    let own = declared_in(cls, name, named, templated, r, ctx);
    if own.is_some() || depth >= MAX_BASE_DEPTH {
        return own;
    }
    seen.insert(cls.to_string());
    for base in base_classes_of(cls, ctx) {
        if seen.contains(&base) {
            continue;
        }
        let inherited = member_of(&base, name, named, templated, r, ctx, depth + 1, seen);
        if inherited.is_some() {
            return inherited;
        }
    }
    None
}

fn lookup_unqualified(
    name: &str,
    templated: bool,
    scopes: &[String],
    r: &UnresolvedRef,
    ctx: &Ctx,
    visible: Visible,
) -> Option<Declared> {
    let named = type_declarations(name, ctx, visible);
    if named.is_empty() {
        return None;
    }
    for scope in scopes {
        let found = if scope.is_empty() {
            declared_in("", name, &named, templated, r, ctx)
        } else {
            member_of(scope, name, &named, templated, r, ctx, 0, &mut HashSet::new())
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// The type `raw_type` names once its alias chain is followed, as written from the calling
/// scope: `Undefined` when it names no alias, `Null` when the chain cannot be followed safely.
pub fn resolve_cpp_aliased_type(raw_type: &str, r: &UnresolvedRef, ctx: &Ctx) -> Tri<CppAliasedType> {
    let Some(name) = cpp_type_name(raw_type) else { return Tri::Undefined };
    let caller = ctx.node_by_id(&r.from_node_id);
    let scope = caller_scope(caller.as_ref());
    let local = if name.global { Tri::Undefined } else { local_aliased_type(&name.names[0], caller.as_ref(), r, ctx) };
    let resolved = if !local.is_undefined() {
        let parameters = caller.as_ref().map(|c| template_parameters_around(c, ctx));
        expand(local.value().as_deref(), &name.names[1..], &name.templated[1..], &scope, r, ctx, 1, parameters.as_deref()).into()
    } else {
        follow(&name, &scope, r, ctx, 0)
    };
    match resolved {
        Tri::Value(v) if is_cpp_pointer_type(raw_type) => Tri::Value(CppAliasedType { pointer: true, ..v }),
        other => other,
    }
}

/// `Type::rest…` of an aliased type, as one qualified spelling.
pub fn cpp_aliased_type_name(resolved: &CppAliasedType) -> String {
    let mut parts = vec![resolved.target.last().cloned().unwrap_or_default()];
    parts.extend(resolved.rest.iter().cloned());
    parts.join("::")
}

/// The class an aliased type names, when it is one.
pub fn cpp_aliased_class(resolved: &CppAliasedType, ctx: &Ctx, r: Option<&UnresolvedRef>) -> Option<String> {
    let names: Vec<String> = resolved.target.iter().chain(resolved.rest.iter()).cloned().collect();
    match r {
        Some(r) => class_reached(&names, &resolved.scope, r, ctx),
        None => class_named(&names, &scopes_within(&resolved.scope), ctx),
    }
}

/// What `cpp_class_named` finds: a class, or the alias it stopped at.
#[derive(Debug, Clone)]
pub enum CppNamedClass {
    Cls(String),
    Alias(NodeRef),
}

/// The class `written` names from `scopes`: `Undefined` when nothing of that name is declared,
/// `Null` when something is but it is no class.
pub fn cpp_class_named(written: &str, scopes: &[String], r: &UnresolvedRef, ctx: &Ctx, visible: Visible) -> Tri<CppNamedClass> {
    let Some(name) = cpp_type_name(written) else { return Tri::Null };
    let (found, next) = lookup_segments(&name, scopes, r, ctx, visible);
    let Some(found) = found else { return Tri::Undefined };
    match found {
        Declared::Other => Tri::Null,
        Declared::Cls(c) => Tri::Value(CppNamedClass::Cls(c)),
        Declared::Ambiguous(mut nodes) => {
            nodes.sort_by(|a, b| {
                a.file_path.encode_utf16().cmp(b.file_path.encode_utf16()).then(a.start_line.cmp(&b.start_line))
            });
            if next == name.names.len() {
                Tri::Value(CppNamedClass::Alias(Rc::clone(&nodes[0])))
            } else {
                Tri::Null
            }
        }
        Declared::Alias(alias) => {
            let text = declaration_text(&alias, ctx);
            let aliased = expand(
                text.and_then(|t| aliased_type_in(&t, &alias.name)).as_deref(),
                &name.names[next..],
                &name.templated[next..],
                &parent_scope(&alias.qualified_name),
                r,
                ctx,
                1,
                Some(&template_parameters_around(&alias, ctx)),
            );
            if let Some(aliased) = aliased {
                if !aliased.pointer {
                    let all: Vec<String> = aliased.target.iter().chain(aliased.rest.iter()).cloned().collect();
                    if let Some(cls) = class_reached(&all, &aliased.scope, r, ctx) {
                        return Tri::Value(CppNamedClass::Cls(cls));
                    }
                }
            }
            if next == name.names.len() {
                Tri::Value(CppNamedClass::Alias(alias))
            } else {
                Tri::Null
            }
        }
    }
}

/// The text of the type an alias written as `written` stands for, with the alias node.
pub fn cpp_aliased_type_written(
    written: &str,
    scopes: &[String],
    r: &UnresolvedRef,
    ctx: &Ctx,
) -> Tri<(String, NodeRef)> {
    let Some(name) = cpp_type_name(written) else { return Tri::Undefined };
    let (found, next) = lookup_segments(&name, &scopes_for(&name, scopes), r, ctx, None);
    let alias = match found {
        None | Some(Declared::Cls(_)) | Some(Declared::Other) => return Tri::Undefined,
        Some(Declared::Ambiguous(_)) => return Tri::Null,
        Some(Declared::Alias(alias)) => alias,
    };
    if next != name.names.len() || is_alias_template(&alias, ctx) {
        return Tri::Null;
    }
    let text = declaration_text(&alias, ctx);
    let aliased = text.and_then(|t| aliased_type_in(&t, &alias.name));
    match aliased {
        Some(a) if !re!(r"\b(?:typename|decltype)\b").is_match(&a) => Tri::Value((a, alias)),
        _ => Tri::Null,
    }
}

pub fn cpp_scopes_within(qualified: &str) -> Vec<String> {
    scopes_within(qualified)
}

pub fn cpp_parent_scope(qualified: &str) -> String {
    parent_scope(qualified)
}

pub fn cpp_template_parameters(node: &Node, ctx: &Ctx) -> Rc<HashSet<String>> {
    template_parameters_around(node, ctx)
}

fn lookup_segments(
    name: &TypeName,
    scopes: &[String],
    r: &UnresolvedRef,
    ctx: &Ctx,
    visible: Visible,
) -> (Option<Declared>, usize) {
    let mut found = lookup_unqualified(&name.names[0], name.templated[0], scopes, r, ctx, visible);
    let mut next = 1;
    if found.is_none() && name.names.len() > 1 {
        let mut i = name.names.len();
        while i >= 2 && found.is_none() {
            let spelled = &name.names[..i];
            let last = &spelled[spelled.len() - 1];
            let named = type_declarations(last, ctx, visible);
            let qualifier = spelled[..spelled.len() - 1].join("::");
            for s in scopes {
                let scope = if s.is_empty() { qualifier.clone() } else { format!("{s}::{qualifier}") };
                found = declared_in(&scope, last, &named, name.templated[i - 1], r, ctx);
                if found.is_some() {
                    break;
                }
            }
            next = i;
            i -= 1;
        }
    }
    while let Some(Declared::Cls(cls)) = &found {
        if next >= name.names.len() {
            break;
        }
        let cls = cls.clone();
        found = member_of(
            &cls,
            &name.names[next],
            &type_declarations(&name.names[next], ctx, visible),
            name.templated[next],
            r,
            ctx,
            0,
            &mut HashSet::new(),
        );
        next += 1;
    }
    (found, next)
}

fn follow(name: &TypeName, scope: &str, r: &UnresolvedRef, ctx: &Ctx, hops: usize) -> Tri<CppAliasedType> {
    let (found, next) = lookup_segments(name, &scopes_for(name, &scopes_within(scope)), r, ctx, None);
    let Some(Declared::Alias(alias)) = found else { return Tri::Undefined };
    if hops >= MAX_HOPS {
        return Tri::Null;
    }
    let text = declaration_text(&alias, ctx);
    expand(
        text.and_then(|t| aliased_type_in(&t, &alias.name)).as_deref(),
        &name.names[next..],
        &name.templated[next..],
        &parent_scope(&alias.qualified_name),
        r,
        ctx,
        hops + 1,
        Some(&template_parameters_around(&alias, ctx)),
    )
    .into()
}

#[allow(clippy::too_many_arguments)]
fn expand(
    written: Option<&str>,
    rest: &[String],
    rest_templated: &[bool],
    scope: &str,
    r: &UnresolvedRef,
    ctx: &Ctx,
    hops: usize,
    parameters: Option<&HashSet<String>>,
) -> Option<CppAliasedType> {
    let written = written.filter(|w| !w.is_empty())?;
    if re!(r"\btypename\b").is_match(written) {
        return None;
    }
    let target = cpp_type_name(written)?;
    if parameters.is_some_and(|p| p.contains(&target.names[0])) {
        return None;
    }
    let pointer = is_cpp_pointer_type(written);
    let names: Vec<String> = target.names.iter().chain(rest.iter()).cloned().collect();
    let templated: Vec<bool> = target.templated.iter().chain(rest_templated.iter()).copied().collect();
    let further = follow(&TypeName { names: names.clone(), templated, global: target.global }, scope, r, ctx, hops);
    match further {
        Tri::Null => return None,
        Tri::Value(f) => {
            let pointer = f.pointer || pointer;
            return Some(CppAliasedType { pointer, ..f });
        }
        Tri::Undefined => {}
    }
    let from = if target.global { String::new() } else { scope.to_string() };
    let cls = class_named(&names, &scopes_within(&from), ctx);
    let class_template = cls.as_ref().is_some_and(|cls| {
        ctx.get_nodes_by_qualified_name(cls)
            .iter()
            .any(|n| is_c_family(n) && is_class_kind(n.kind) && !template_parameters_around(n, ctx).is_empty())
    });
    Some(CppAliasedType { target: target.names, rest: rest.to_vec(), scope: from, pointer, class_template })
}
