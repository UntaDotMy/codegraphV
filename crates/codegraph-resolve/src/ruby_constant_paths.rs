//! A Ruby constant written with its namespace — `Billing::ChargeCard`, `::Foo` — is the class,
//! module or constant Ruby's lookup finds from where it is written, and a call on one
//! (`Billing::ChargeCard.call`) a method of that class or module (#2503) — port of
//! `src/resolution/ruby-constant-paths.ts`.
//!
//! Both resolve here alone: a path the project doesn't declare is a library's, and a class the
//! path names without the method has it from somewhere the index can't see. Production code never
//! names a class a test declares. A bare constant is left to the name strategies.
//!
//! A call on what `.new` made (`ChargeCard.new(card).charge!`, recorded as
//! `new ChargeCard().charge!`) resolves here too, bare constant or path: an instance method of
//! the class the lookup finds, or nothing.
//!
//! So does a call on an instance variable an instance method makes (`@writer.flush`), when the
//! class's `initialize` gives the variable an instance of one class, `@writer = CacheWriter.new`,
//! and nothing else.

use crate::ctx::Ctx;
use crate::name_matcher::ruby::{ruby_ancestry, ruby_methods_of_kind_first, RubyMethodKind};
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_search::query_utils::is_test_path;
use codegraph_text::strip_comments_for_regex;
use codegraph_types::{EdgeKind, Language, Node, NodeKind, ReferenceKind};
use std::cell::RefCell;
use std::collections::HashMap;

/// The class `initialize` gives an instance variable: untyped (other strategies may run), the
/// project has no such class, or the class it names.
#[derive(Clone)]
enum IvarClass {
    Untyped,
    Missing,
    Found(NodeRef),
}

#[derive(Default)]
struct IvarMemo(RefCell<HashMap<String, IvarClass>>);

/// The class a bare accessor hands back, or `None` once that has been read and it hands none.
#[derive(Default)]
struct AccessorMemo(RefCell<HashMap<String, Option<NodeRef>>>);

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
        // A call on an instance `Foo.new(…)` made, bare or namespaced: an instance method of
        // the class Ruby's lookup finds for `Foo` there, its own or one its ancestry declares.
        // A class the project doesn't declare (rack's `Request.new(env)`) has no method here,
        // and a later name guess must not take a namesake.
        if let Some(m) = re!(r"^new (::)?([A-Z]\w*(?:::[A-Z]\w*)*)\(\)\.([A-Za-z_]\w*[?!=]?)$").captures(&r.reference_name) {
            let (absolute, path, name) = (m.text(1).is_some(), m.text(2).unwrap_or(""), m.text(3).unwrap_or(""));
            let cls = constants_named(path, absolute, r, ctx).into_iter().next().filter(|o| o.kind == NodeKind::Class);
            let method = cls.as_ref().and_then(|o| method_of(&o.qualified_name, name, r, ctx, RubyMethodKind::Instance));
            return match method {
                Some((node, own)) => Tri::Value(ResolvedRef::new(r, node.id.clone(), if own { 0.9 } else { 0.85 }, ResolvedBy::InstanceMethod)),
                None => Tri::Null,
            };
        }
        // A call on an instance variable, typed by the class's `initialize`: an instance
        // method of the class it is given there, or nothing when that class is not the
        // project's. A variable `initialize` doesn't type is left to the other strategies.
        if let Some(m) = re!(r"^@(\w+)\.([A-Za-z_]\w*[?!=]?)$").captures(&r.reference_name) {
            let (var_name, method_name) = (m.text(1).unwrap_or(""), m.text(2).unwrap_or(""));
            return match ivar_class(var_name, r, ctx) {
                IvarClass::Untyped => Tri::Undefined,
                IvarClass::Missing => Tri::Null,
                IvarClass::Found(cls) => match method_of(&cls.qualified_name, method_name, r, ctx, RubyMethodKind::Instance) {
                    Some((node, own)) => Tri::Value(ResolvedRef::new(r, node.id.clone(), if own { 0.85 } else { 0.8 }, ResolvedBy::InstanceMethod)),
                    None => Tri::Null,
                },
            };
        }
        // A call on what a method of the class around it hands back. A name the calling method
        // binds itself, or a method that says nothing of its value, is left to the other strategies.
        if let Some(m) = re!(r"^([a-z_]\w*)\.([A-Za-z_]\w*[?!=]?)$").captures(&r.reference_name) {
            let (recv, method_name) = (m.text(1).unwrap_or(""), m.text(2).unwrap_or(""));
            let cls = accessor_class(recv, r, ctx);
            let method = cls.as_ref().and_then(|c| method_of(&c.qualified_name, method_name, r, ctx, RubyMethodKind::Instance));
            return match method {
                Some((node, own)) => Tri::Value(ResolvedRef::new(r, node.id.clone(), if own { 0.85 } else { 0.8 }, ResolvedBy::InstanceMethod)),
                None => Tri::Undefined,
            };
        }
        let Some(m) = re!(r"^(::)?([A-Z]\w*(?:::[A-Z]\w*)*)\.([A-Za-z_]\w*[?!=]?)$").captures(&r.reference_name) else { return Tri::Undefined };
        let (absolute, path, name) = (m.text(1).is_some(), m.text(2).unwrap_or(""), m.text(3).unwrap_or(""));
        if !absolute && !path.contains("::") {
            return Tri::Undefined;
        }
        let owner = constants_named(path, absolute, r, ctx).into_iter().next();
        let method = owner.filter(|o| is_namespace_kind(o.kind)).and_then(|o| method_of(&o.qualified_name, name, r, ctx, RubyMethodKind::Class));
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
    innermost_namespace(r, ctx).map_or_else(String::new, |n| n.qualified_name.clone())
}

/// The innermost class or module around `r`, if any.
fn innermost_namespace(r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
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
    inner
}

/// The class `@name` holds where `r` is written, when its class's `initialize` says exactly
/// (see `ivar_class`); `None` for anything else. The parameter-flow pass types an argument by it.
pub fn ivar_class_node(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    match ivar_class(name, r, ctx) {
        IvarClass::Found(n) => Some(n),
        _ => None,
    }
}

/// The class of what `@name` holds where `r` is written, an instance method of a class, read
/// from that class's `initialize`. Undefined for the other strategies when the call is not in
/// an instance method of a class, or `initialize` doesn't assign the variable, or assigns it
/// anything but one `Const.new`.
fn ivar_class(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> IvarClass {
    let Some(caller) = ctx.node_by_id(&r.from_node_id) else { return IvarClass::Untyped };
    if caller.kind != NodeKind::Method || caller.language != Language::Ruby || caller.is_static == Some(true) {
        return IvarClass::Untyped;
    }
    let Some(owner) = innermost_namespace(r, ctx) else { return IvarClass::Untyped };
    if owner.kind != NodeKind::Class {
        return IvarClass::Untyped;
    }
    let key = format!("{}\0{name}", owner.qualified_name);
    let owner_qn = owner.qualified_name.clone();
    let memo = ctx.memo::<IvarMemo>();
    if let Some(hit) = memo.0.borrow().get(&key).cloned() {
        return hit;
    }
    let found = ivar_class_from_initialize(name, &owner_qn, ctx);
    memo.0.borrow_mut().insert(key, found.clone());
    found
}

struct Written {
    path: String,
    absolute: bool,
    file: String,
    line: u32,
}

fn ivar_class_from_initialize(name: &str, owner: &str, ctx: &Ctx) -> IvarClass {
    let mut written: Option<Written> = None;
    for init in ctx.get_nodes_by_qualified_name(&format!("{owner}::initialize")).iter() {
        if init.kind != NodeKind::Method || init.language != Language::Ruby || init.is_static == Some(true) {
            continue;
        }
        let Some(lines) = ctx.get_file_lines(&init.file_path) else { return IvarClass::Untyped };
        let start = (init.start_line as usize).saturating_sub(1);
        let end = (init.end_line as usize).min(lines.len());
        if start >= end {
            return IvarClass::Untyped;
        }
        let body = strip_comments_for_regex(&lines[start..end].join("\n"), "ruby");
        let Some(assigned) = constructed_assignments(&body, name) else { return IvarClass::Untyped };
        for a in assigned {
            if let Some(prev) = &written {
                if prev.path != a.path || prev.absolute != a.absolute {
                    return IvarClass::Untyped;
                }
            } else {
                written = Some(Written { path: a.path, absolute: a.absolute, file: init.file_path.clone(), line: init.start_line.saturating_add(a.line) });
            }
        }
    }
    let Some(written) = written else { return IvarClass::Untyped };
    let at = UnresolvedRef::new("", &written.path, ReferenceKind::Edge(EdgeKind::References), written.line, 0, &written.file, Language::Ruby);
    match constants_named(&written.path, written.absolute, &at, ctx).into_iter().next() {
        Some(cls) if cls.kind == NodeKind::Class => IvarClass::Found(cls),
        _ => IvarClass::Missing,
    }
}

struct Assignment {
    path: String,
    absolute: bool,
    line: u32,
}

/// Each assignment to `@name` in an `initialize` body. `None` when one of them is not a lone
/// `Const.new` at the start of a statement.
fn constructed_assignments(body: &str, name: &str) -> Option<Vec<Assignment>> {
    let pattern = format!(r"(?<![@\w])@{}\s*(?:\|\||&&|\*\*|<<|>>|[-+*/%|&^])?=(?![=~>])", codegraph_jsre::escape_regex(name));
    let re = codegraph_jsre::Re::cached(&pattern, "");
    let mut out = Vec::new();
    for caps in re.captures_iter(body) {
        let whole = caps.get(0)?;
        let at = whole.start();
        let nl = body[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let semi = body[..at].rfind(';').map(|i| i + 1).unwrap_or(0);
        if !body[nl.max(semi)..at].trim().is_empty() {
            return None;
        }
        let (path, absolute) = constructed_value(body, whole.end())?;
        let line = body[..at].bytes().filter(|b| *b == b'\n').count() as u32;
        out.push(Assignment { path, absolute, line });
    }
    Some(out)
}

/// The constant a value constructs when it is a lone `Const.new`, `A::B.new` or `::Foo.new`.
/// Arguments, a block, or a statement modifier may follow. A chain or an operator does not.
fn constructed_value(s: &str, from: usize) -> Option<(String, bool)> {
    let rest = s.get(from..)?;
    let m = re!(r"^\s*(::)?([A-Z]\w*(?:::[A-Z]\w*)*)\s*\.\s*new(?![\w?!])").captures(rest)?;
    let path = m.text(2).unwrap_or("").to_string();
    let absolute = m.text(1).is_some();
    let mut i = from + m.get(0)?.end();
    let bytes = s.as_bytes();
    if bytes.get(i) == Some(&b'(') {
        i = skip_parens(s, i)?;
    }
    let next = bytes.get(i).copied();
    let tail = s.get(i..).unwrap_or("");
    let window = &tail[..tail.len().min(200)];
    let spaced = re!(r#"^[ \t]+(?:$|\n|;|\{|[\w:@$"'[])"#).is_match(window);
    let ok = next.is_none() || matches!(next, Some(b'\n' | b';' | b'{')) || spaced;
    let continued = re!(r"^[ \t]*\n\s*&?\.").is_match(window);
    if ok && !continued { Some((path, absolute)) } else { None }
}

/// The index past the parenthesized group opening at `i`. `None` when it never closes.
fn skip_parens(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'"' | b'\'' => {
                let quote = b[j];
                j += 1;
                while j < b.len() && b[j] != quote {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    if j < b.len() {
                        j += 1;
                    }
                }
                if j < b.len() {
                    j += 1;
                }
            }
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth -= 1;
                j += 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => j += 1,
        }
    }
    None
}

struct LastStatement {
    head: String,
    body: String,
    text: String,
}

/// The class of what the method `name` of the class around `r` hands back. `None` unless that
/// method says so and the calling method doesn't bind the name itself.
fn accessor_class(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let caller = ctx.node_by_id(&r.from_node_id)?;
    if caller.kind != NodeKind::Method || caller.language != Language::Ruby || caller.is_static == Some(true) {
        return None;
    }
    let owner = innermost_namespace(r, ctx)?;
    if owner.kind != NodeKind::Class {
        return None;
    }
    let key = format!("{}\0{name}", owner.qualified_name);
    let memo = ctx.memo::<AccessorMemo>();
    let cached = memo.0.borrow().get(&key).cloned();
    let cls = match cached {
        Some(hit) => hit,
        None => {
            let found = accessor_class_of(name, &owner.qualified_name, r, ctx);
            memo.0.borrow_mut().insert(key, found.clone());
            found
        }
    }?;
    let text = ctx.get_file_lines(&caller.file_path).map(|lines| {
        let start = (caller.start_line as usize).saturating_sub(1);
        let end = (caller.end_line as usize).min(lines.len());
        if start >= lines.len() || start >= end { String::new() } else { lines[start..end].join("\n") }
    }).unwrap_or_default();
    if binds_name(&text, name) { None } else { Some(cls) }
}

/// Whether a method's text binds `name`: a parameter, a block parameter, an assignment, a `rescue =>`.
fn binds_name(text: &str, name: &str) -> bool {
    let name = codegraph_jsre::escape_regex(name);
    let pattern = format!(
        r"(?m)^\s*def\s[^\n]*\b{name}\b|\|[^|\n]*\b{name}\b[^|\n]*\||(?<![.@:\w]){name}\s*(?:\|\||&&|[-+*/%])?=(?![=~>])|(?<![.@:\w]){name}\s*,[^=\n]*=(?![=~>])|=>\s*{name}\b|\bfor\s+{name}\b"
    );
    codegraph_jsre::Re::cached(&pattern, "").is_match(text)
}

/// The class the no-argument instance method `name` of `owner` hands back from its last statement.
fn accessor_class_of(name: &str, owner: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let (accessor, _) = method_of(owner, name, r, ctx, RubyMethodKind::Instance)?;
    if accessor.is_static == Some(true) {
        return None;
    }
    let last = last_statement(&accessor, ctx)?;
    let head_re = format!(r"^\s*def\s+{}\s*(?:\(\s*\))?\s*(?:#.*)?$", codegraph_jsre::escape_regex(name));
    if !codegraph_jsre::Re::cached(&head_re, "").is_match(&last.head) {
        return None;
    }
    let value = re!(r"^@\w+\s*\|\|=\s*").replace_first(&last.text, |_| String::new());
    let at = UnresolvedRef::new("", name, ReferenceKind::Edge(EdgeKind::References), accessor.start_line, 0, &accessor.file_path, Language::Ruby);
    if let Some((path, absolute)) = constructed_value(&value, 0) {
        return constants_named(&path, absolute, &at, ctx).into_iter().find(|n| n.kind == NodeKind::Class);
    }
    let factory = re!(r"^(::)?([A-Z]\w*(?:::[A-Z]\w*)*)\s*\.\s*([a-z_]\w*)\b").captures(&value)?;
    let path = factory.text(2).unwrap_or("");
    let absolute = factory.text(1).is_some();
    let fname = factory.text(3).unwrap_or("");
    let cls = constants_named(path, absolute, &at, ctx).into_iter().find(|n| n.kind == NodeKind::Class)?;
    let built = ctx
        .get_nodes_by_qualified_name(&format!("{}::{fname}", cls.qualified_name))
        .iter()
        .find(|n| n.kind == NodeKind::Method && n.is_static == Some(true))
        .cloned()?;
    let end = last_statement(&built, ctx)?;
    let local_new = if re!(r"^\w+$").is_match(&end.text) {
        let pat = format!(r"(?m)^\s*{}\s*=\s*new\b", codegraph_jsre::escape_regex(&end.text));
        codegraph_jsre::Re::cached(&pat, "").is_match(&end.body)
    } else {
        false
    };
    if re!(r"^new\b").is_match(&end.text) || local_new { Some(cls) } else { None }
}

/// The method's first line, its body with comments blanked, and that body's last statement.
fn last_statement(method: &Node, ctx: &Ctx) -> Option<LastStatement> {
    let lines = ctx.get_file_lines(&method.file_path)?;
    if method.end_line.saturating_sub(method.start_line) < 2 {
        return None;
    }
    let start = method.start_line as usize;
    let end = (method.end_line as usize).saturating_sub(1);
    if start >= end || start >= lines.len() {
        return None;
    }
    let end = end.min(lines.len());
    let body = strip_comments_for_regex(&lines[start..end].join("\n"), "ruby");
    let text = body.split('\n').map(str::trim).filter(|l| !l.is_empty()).next_back()?.to_string();
    let head = lines.get(method.start_line.saturating_sub(1) as usize).cloned().unwrap_or_default();
    Some(LastStatement { head, body, text })
}

/// The method `name` of the class or module `owner`: its own, else the first of its ancestry
/// that declares one. Of a class's `def self.m` and `def m`, which share one qualified name, a
/// call on the class reaches the class method, and a call on an instance (`kind` instance) the
/// instance method. Production code never reaches one a test declares.
fn method_of(owner: &str, name: &str, r: &UnresolvedRef, ctx: &Ctx, kind: RubyMethodKind) -> Option<(NodeRef, bool)> {
    let production = !is_test_path(&r.file_path);
    for qn in ruby_ancestry(owner, ctx, kind == RubyMethodKind::Instance).iter() {
        let methods: Vec<NodeRef> = ctx
            .get_nodes_by_qualified_name(&format!("{qn}::{name}"))
            .iter()
            .filter(|n| n.kind == NodeKind::Method && n.language == Language::Ruby && !(production && is_test_path(&n.file_path)))
            .cloned()
            .collect();
        if !methods.is_empty() {
            return ruby_methods_of_kind_first(ordered(methods, r), Some(kind)).into_iter().next().map(|m| (m, qn == owner));
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
