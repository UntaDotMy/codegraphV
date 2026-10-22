//! C++ methods a function-like macro defines, minted where the file invokes the
//! macro — a port of src/extraction/languages/cpp-macro-methods.ts (read its
//! header for the protobuf `PROTOBUF_DEFINE_ACCESSOR` case that motivates it).
//!
//! It reads the file's own `#define`s whose body defines `Class::method(…) … {`
//! at its top level, expands each invocation at namespace scope, and mints a
//! method node per definition, as the walker mints an out-of-line definition. It
//! runs on the original source after the walker, so every file gets the same nodes.
//!
//! The TypeScript indexes UTF-16 units, so this runs on ASCII shadows (see
//! `preparse/shadow.rs`): byte `i` is unit `i`, and the scanners are verbatim ports.
//! A definition whose text holds a non-ASCII placeholder is not minted — it could
//! not be written back as the source spelled it.

use super::brace_scopes;
use crate::ids;
use crate::preparse::jsre::Re;
use crate::preparse::shadow::{has_placeholder, shadow_of};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Edge, EdgeKind, ExtractionResult, Language, Node, NodeKind};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// A function-like macro a file defines (not a variadic one: none defines a method in practice).
struct CppMacro {
    name: String,
    params: Vec<String>,
    /// Its replacement text: continuations spliced, comments blanked.
    body: String,
    /// Where its `#` is.
    at: usize,
}

struct Undef {
    name: String,
    at: usize,
}

/// A method definition at the top level of a macro's expansion.
struct DefinitionHead {
    /// The receiver as written, template arguments dropped (`Descriptor`, `internal::ExtensionSet`).
    receiver: String,
    name: String,
    /// The text before the receiver: specifiers and the return type.
    return_text: String,
    /// Everything up to the body's `{`, whitespace collapsed.
    head: String,
}

struct Invocation {
    name: String,
    at: usize,
    end: usize,
    args: Vec<String>,
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}
fn is_word_char(c: u8) -> bool {
    is_word_start(c) || c.is_ascii_digit()
}
/// JavaScript `\s` on a shadow: the ASCII blanks, and the placeholder for a non-ASCII one.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}
fn starts_at(text: &str, i: usize, pat: &str) -> bool {
    text.get(i..).is_some_and(|rest| rest.starts_with(pat))
}
fn find(text: &str, pat: &str, from: usize) -> Option<usize> {
    text.get(from..).and_then(|rest| rest.find(pat)).map(|k| from + k)
}

static METHOD_MACRO_CANDIDATE: LazyLock<Re> = LazyLock::new(|| {
    Re::multiline(r"^[ \t]*#[ \t]*define[ \t]+[A-Za-z_]\w*\([^)\n]*\)((?:[^\n]*\\\r?\n)*[^\n]*)")
});
static PASTE: LazyLock<Re> = LazyLock::new(|| Re::new(r"\s*##\s*"));
static BLOCK_COMMENT: LazyLock<Re> = LazyLock::new(|| Re::new(r"/\*[\s\S]*?\*/"));
static LINE_COMMENT: LazyLock<Re> = LazyLock::new(|| Re::new(r"//[^\n]*"));
static CONTINUATION: LazyLock<Re> = LazyLock::new(|| Re::new(r"\\\r?\n"));
static DEFINE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^#[ \t]*define[ \t]+([A-Za-z_]\w*)\(([^)]*)\)"));
static UNDEF: LazyLock<Re> = LazyLock::new(|| Re::new(r"^#[ \t]*undef[ \t]+([A-Za-z_]\w*)"));
static WORD: LazyLock<Re> = LazyLock::new(|| Re::new(r"[A-Za-z_]\w*"));
static WHITESPACE_RUN: LazyLock<Re> = LazyLock::new(|| Re::new(r"\s+"));
static OPERATOR: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^operator\s*(\(\s*\)|\[\s*\]|[^\s\w(]+|\w+)\s*\("));
static BODY_QUALIFIER: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^(?:const|volatile|override|final|noexcept|throw|[A-Z][A-Z0-9]*_[A-Z0-9_]*)$"));
static LEADING_ATTRIBUTE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^\[\[[^\]]*\]\]\s*"));
static LEADING_SPECIFIER: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"^(?:inline|static|virtual|constexpr|consteval|explicit|extern|friend|__forceinline)\b\s*")
});
static LEADING_ATTRIBUTE_MACRO: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^[A-Z][A-Z0-9]*_[A-Z0-9_]*(?:\s*\([^()]*\))?\s+"));
static RAW_DELIMITER: LazyLock<Re> = LazyLock::new(|| Re::new(r#"^[^\s()\\"]{0,16}$"#));

fn remove_all(re: &Re, text: &str, with: &'static str) -> String {
    re.replace_all(text, |_| Some(with.to_string()))
}

/// Whether the C++ `source` may define a method with a macro of its own: a cheap
/// test of the raw text, so a file that can't have one skips the pass.
pub fn may_define_cpp_macro_methods(source: &str) -> bool {
    if !source.contains("define") {
        return false;
    }
    let mut pos = 0;
    while let Some(m) = METHOD_MACRO_CANDIDATE.captures_at(source, pos) {
        let whole = m.get(0).unwrap();
        let body = m.get(1).map_or("", |b| b.as_str());
        if body.contains("::") && body.contains('{') {
            return true;
        }
        pos = if whole.end() == whole.start() { whole.end() + 1 } else { whole.end() };
    }
    false
}

/// Add the methods the file's own macros define (see the module comment) to an
/// extraction result. `source` is the original text and `pre_parsed` the same
/// text after the C++ `preParse` (the scopes are read as the parser saw them).
pub(crate) fn append_cpp_macro_methods(result: &mut ExtractionResult, file_path: &str, source: &str, pre_parsed: &str) {
    if !may_define_cpp_macro_methods(source) {
        return;
    }
    let (nodes, edges) = cpp_macro_methods(file_path, &shadow_of(source), &shadow_of(pre_parsed), &result.nodes);
    result.nodes.extend(nodes);
    result.edges.extend(edges);
}

/// The method nodes, and their `contains` edges, for every definition the file's
/// own method-defining macros expand to where the file invokes them at namespace
/// scope. `existing` are the file's extracted nodes: the class a method's
/// receiver names, and the ids already taken.
fn cpp_macro_methods(file_path: &str, source: &str, pre_parsed: &str, existing: &[Node]) -> (Vec<Node>, Vec<Edge>) {
    let mut out_nodes: Vec<Node> = Vec::new();
    let mut out_edges: Vec<Edge> = Vec::new();
    let (macros, undefs) = cpp_macro_directives(source);
    let defining: Vec<&CppMacro> = macros
        .iter()
        .filter(|m| !cpp_definition_heads(&remove_all(&PASTE, &m.body, "")).is_empty())
        .collect();
    if defining.is_empty() {
        return (out_nodes, out_edges);
    }
    // Scopes as the parser saw them (after preParse, offsets unchanged), like the walker's.
    let Some(scopes) = brace_scopes::scan(pre_parsed) else { return (out_nodes, out_edges) };
    let line_starts: Vec<usize> =
        std::iter::once(0).chain(pre_parsed.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1)).collect();
    let position_of = |offset: usize| -> (u32, u32) {
        let row = line_starts.partition_point(|&s| s <= offset).saturating_sub(1);
        (row as u32 + 1, (offset - line_starts[row]) as u32)
    };

    let names: HashSet<String> = defining.iter().map(|m| m.name.clone()).collect();
    let mut taken: HashSet<String> = existing.iter().map(|n| n.id.clone()).collect();
    let file_id = ids::file_node_id(file_path);
    let now = crate::decode::codegraph_now_ms();
    for call in cpp_macro_invocations(source, &names) {
        if !scopes.at_namespace_scope(call.at) {
            continue;
        }
        let Some(macro_) = live_macro(&defining, &undefs, &call.name, call.at) else { continue };
        let Some(expansion) = expand_cpp_macro(macro_, &call.args) else { continue };
        let (start_line, start_column) = position_of(call.at);
        let (end_line, end_column) = position_of(call.end);
        let prefix = scopes.namespaces_at(call.at);
        for def in cpp_definition_heads(&expansion) {
            let receiver_last = def.receiver.rsplit("::").next().unwrap_or("");
            // A constructor's or destructor's.
            if def.name == receiver_last || def.name.starts_with('~') {
                continue;
            }
            let qualified_name = compose_qualified_name(&prefix, &def.receiver, &def.name);
            if [&def.name, &def.head, &def.receiver, &def.return_text, &qualified_name].into_iter().any(|t| has_placeholder(t)) {
                continue;
            }
            let base = ids::node_id(file_path, "method", &def.name, start_line);
            let mut id = base.clone();
            if taken.contains(&id) {
                id = format!("{id}:{start_column}");
            }
            let mut k = 1;
            while taken.contains(&id) {
                id = format!("{base}:{start_column}:{k}");
                k += 1;
            }
            taken.insert(id.clone());
            let mut node = Node::new(
                id.clone(),
                NodeKind::Method,
                def.name.clone(),
                qualified_name,
                file_path,
                Language::parse("cpp").expect("cpp language"),
                start_line,
                end_line,
                start_column,
                end_column,
                now,
            );
            node.signature = Some(def.head.clone());
            node.return_type = super::normalize_cpp_return_type(&cpp_type_text(&def.return_text));
            out_nodes.push(node);
            out_edges.push(Edge::new(file_id.clone(), id.clone(), EdgeKind::Contains));
            // As for an out-of-line definition, the class of its receiver's name in the same file holds it.
            let owner = existing.iter().find(|n| {
                n.name == def.receiver
                    && n.file_path == file_path
                    && matches!(n.kind, NodeKind::Struct | NodeKind::Union | NodeKind::Class | NodeKind::Enum | NodeKind::Trait)
            });
            if let Some(owner) = owner {
                out_edges.push(Edge::new(owner.id.clone(), id, EdgeKind::Contains));
            }
        }
    }
    (out_nodes, out_edges)
}

/// The qualified name of a method written `receiver::name` inside the namespaces
/// `prefix`, as the walker composes an out-of-line definition's: the receiver is
/// anchored at the first namespace it re-spells.
fn compose_qualified_name(prefix: &[String], receiver: &str, name: &str) -> String {
    let base = format!("{receiver}::{name}");
    if prefix.is_empty() {
        return base;
    }
    let first = receiver.split("::").next().unwrap_or("");
    let kept: &[String] = match prefix.iter().position(|p| p == first) {
        Some(anchor) => &prefix[..anchor],
        None => prefix,
    };
    if kept.is_empty() {
        base
    } else {
        format!("{}::{base}", kept.join("::"))
    }
}

/// The type a definition's leading text writes, without its specifiers, template header and attributes.
fn cpp_type_text(return_text: &str) -> String {
    let mut text = js_trim(return_text).to_string();
    loop {
        let before = text.clone();
        if text.starts_with("template") {
            let close = text.find('<').and_then(|open| matching(&text, open, b'<', b'>'));
            let Some(close) = close else { return String::new() };
            text = js_trim(&text[close + 1..]).to_string();
        }
        text = remove_all(&LEADING_ATTRIBUTE, &text, "");
        text = remove_all(&LEADING_SPECIFIER, &text, "");
        // An attribute macro in front (`PROTOBUF_NOINLINE`, `ABSL_MUST_USE_RESULT`).
        text = remove_all(&LEADING_ATTRIBUTE_MACRO, &text, "");
        if text == before {
            return text;
        }
    }
}

/// Where the `close` matching the `open` at `text[from]` is.
fn matching(text: &str, from: usize, open: u8, close: u8) -> Option<usize> {
    let s = text.as_bytes();
    let mut depth = 0i32;
    let mut i = from;
    while i < s.len() {
        let c = s[i];
        if c == b'"' || c == b'\'' {
            i = quoted_end(text, i) - 1;
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Past the string or character literal whose quote is at `i`.
fn quoted_end(text: &str, i: usize) -> usize {
    let s = text.as_bytes();
    let quote = s[i];
    let mut j = i + 1;
    while j < s.len() && s[j] != quote && s[j] != b'\n' {
        j += if s[j] == b'\\' { 2 } else { 1 };
    }
    s.len().min(j + 1)
}

/// Past the blanks at `i`.
fn skip_blank(text: &str, mut i: usize) -> usize {
    let s = text.as_bytes();
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    i
}

/// The end of the identifier at `i`.
fn word_end(s: &[u8], i: usize) -> Option<usize> {
    if i >= s.len() || !is_word_start(s[i]) {
        return None;
    }
    let mut j = i + 1;
    while j < s.len() && is_word_char(s[j]) {
        j += 1;
    }
    Some(j)
}

struct Callable {
    segments: Vec<String>,
    open: usize,
}

/// The qualified name starting at `i` that an argument list follows
/// (`Descriptor::file(`, `internal::ExtensionSet::Has(`, `Box<T>::get(`,
/// `Matrix::operator()(`): its segments and the offset of the `(`. None when no
/// such name starts there, or it has no `::`.
fn qualified_callable_at(text: &str, i: usize) -> Option<Callable> {
    let s = text.as_bytes();
    let mut segments: Vec<String> = Vec::new();
    let mut j = i;
    if starts_at(text, j, "::") {
        j = skip_blank(text, j + 2);
    }
    loop {
        if starts_at(text, j, "operator") && !is_word_char(at(s, j + 8)) {
            let op = OPERATOR.captures(&text[j..])?;
            if segments.is_empty() {
                return None;
            }
            let spelled = remove_all(&WHITESPACE_RUN, op.get(1).unwrap().as_str(), "");
            segments.push(format!("operator{spelled}"));
            return Some(Callable { segments, open: j + op.get(0).unwrap().end() - 1 });
        }
        let tilde = at(s, j) == b'~';
        let start = if tilde { skip_blank(text, j + 1) } else { j };
        let end = word_end(s, start)?;
        let word = &text[start..end];
        segments.push(if tilde { format!("~{word}") } else { word.to_string() });
        let mut k = skip_blank(text, end);
        if at(s, k) == b'<' && !tilde {
            let close = matching(text, k, b'<', b'>')?;
            segments.last_mut().unwrap().push_str(&text[k..=close]);
            k = skip_blank(text, close + 1);
        }
        if starts_at(text, k, "::") {
            if tilde {
                return None;
            }
            j = skip_blank(text, k + 2);
            continue;
        }
        return if at(s, k) == b'(' && segments.len() > 1 { Some(Callable { segments, open: k }) } else { None };
    }
}

/// After a definition's parameter list (from `i`): the offset of its body's `{`,
/// past the qualifiers it may write (`const`, `noexcept(…)`, `override`, `&&`, an
/// attribute, an attribute macro, a trailing return type). None for anything else:
/// a declaration (`;`), `= default`, a constructor's initializer list.
fn body_open_after(text: &str, i: usize) -> Option<usize> {
    let s = text.as_bytes();
    let mut j = skip_blank(text, i);
    loop {
        let c = at(s, j);
        if c == b'{' {
            return Some(j);
        }
        if c == b'&' {
            j = skip_blank(text, j + if at(s, j + 1) == b'&' { 2 } else { 1 });
            continue;
        }
        if starts_at(text, j, "[[") {
            let close = find(text, "]]", j + 2)?;
            j = skip_blank(text, close + 2);
            continue;
        }
        if starts_at(text, j, "->") {
            // A trailing return type runs to the body.
            let mut depth = 0i32;
            for (k, &d) in s.iter().enumerate().skip(j + 2) {
                if d == b'(' || d == b'<' || d == b'[' {
                    depth += 1;
                } else if d == b')' || d == b'>' || d == b']' {
                    depth -= 1;
                } else if d == b'{' && depth <= 0 {
                    return Some(k);
                } else if d == b';' && depth <= 0 {
                    return None;
                }
            }
            return None;
        }
        let end = word_end(s, j)?;
        let word = &text[j..end];
        if !BODY_QUALIFIER.is_match(word) {
            return None;
        }
        j = skip_blank(text, end);
        if at(s, j) == b'(' && !matches!(word, "const" | "volatile" | "override" | "final") {
            let close = matching(text, j, b'(', b')')?;
            j = skip_blank(text, close + 1);
        }
    }
}

/// The method definitions at the top level of a macro's expansion `text`, in order.
fn cpp_definition_heads(text: &str) -> Vec<DefinitionHead> {
    let s = text.as_bytes();
    let mut heads: Vec<DefinitionHead> = Vec::new();
    let mut segment = 0usize; // where the current top-level declaration began
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        if c == b'"' || c == b'\'' {
            i = quoted_end(text, i);
            continue;
        }
        if c == b';' || c == b'}' {
            i += 1;
            segment = i;
            continue;
        }
        if c == b'{' || c == b'(' {
            // A body or group that is no definition read here (a namespace, a class, a statement).
            let Some(close) = matching(text, i, c, if c == b'{' { b'}' } else { b')' }) else { return heads };
            if c == b'{' {
                segment = close + 1;
            }
            i = close + 1;
            continue;
        }
        let callable =
            if is_word_start(c) || (c == b':' && at(s, i + 1) == b':') { qualified_callable_at(text, i) } else { None };
        let Some(callable) = callable else {
            i = word_end(s, i).unwrap_or(i + 1);
            continue;
        };
        let Some(close) = matching(text, callable.open, b'(', b')') else { return heads };
        let Some(open) = body_open_after(text, close + 1) else {
            i = close + 1;
            continue;
        };
        // An explicit specialization's arguments (`Read<int32_t, TYPE_INT32>`) are no part of its name.
        let last = callable.segments.last().unwrap();
        let name = if last.starts_with("operator") { last.clone() } else { super::strip_cpp_template_args(last) };
        let receiver = super::strip_cpp_template_args(&callable.segments[..callable.segments.len() - 1].join("::"));
        if !receiver.is_empty() {
            heads.push(DefinitionHead {
                receiver,
                name,
                return_text: js_trim(&text[segment..i]).to_string(),
                head: js_trim(&remove_all(&WHITESPACE_RUN, &text[segment..open], " ")).to_string(),
            });
        }
        let Some(body_close) = matching(text, open, b'{', b'}') else { return heads };
        i = body_close + 1;
        segment = i;
    }
    heads
}

/// The macro of `name` in effect at `offset`: its last `#define` before it,
/// unless an `#undef` came between.
fn live_macro<'m>(macros: &[&'m CppMacro], undefs: &[Undef], name: &str, offset: usize) -> Option<&'m CppMacro> {
    let mut found: Option<&CppMacro> = None;
    for m in macros {
        if m.name == name && m.at < offset && found.is_none_or(|f| m.at > f.at) {
            found = Some(m);
        }
    }
    let found = found?;
    if undefs.iter().any(|u| u.name == name && u.at > found.at && u.at < offset) {
        None
    } else {
        Some(found)
    }
}

/// A macro's expansion for the arguments `args` (as written), or None when they don't fit its parameters.
fn expand_cpp_macro(macro_: &CppMacro, args: &[String]) -> Option<String> {
    let values: Vec<String> = if macro_.params.is_empty() && args.len() == 1 && js_trim(&args[0]).is_empty() {
        Vec::new()
    } else {
        args.iter().map(|a| js_trim(a).to_string()).collect()
    };
    if values.len() != macro_.params.len() {
        return None;
    }
    let value_of: HashMap<&str, &str> =
        macro_.params.iter().map(String::as_str).zip(values.iter().map(String::as_str)).collect();
    let replaced = WORD.replace_all(&macro_.body, |c| value_of.get(c.get(0).unwrap().as_str()).map(|v| v.to_string()));
    // `a ## b`: one token.
    Some(remove_all(&PASTE, &replaced, ""))
}

/// The function-like `#define`s of a C++ source and its `#undef`s, in order, read
/// past comments and literals: a `#define` in a comment or a raw string defines nothing.
fn cpp_macro_directives(source: &str) -> (Vec<CppMacro>, Vec<Undef>) {
    let mut macros: Vec<CppMacro> = Vec::new();
    let mut undefs: Vec<Undef> = Vec::new();
    for (at, directive) in cpp_code_scan(source, None).directives {
        let spliced = remove_all(&BLOCK_COMMENT, &CONTINUATION.replace_all(&directive, |_| Some(" ".to_string())), " ");
        let text = without_line_comment(&spliced);
        if let Some(define) = DEFINE.captures(text) {
            let params: Vec<String> =
                define.get(2).unwrap().as_str().split(',').map(|p| js_trim(p).to_string()).filter(|p| !p.is_empty()).collect();
            if !params.iter().any(|p| p.ends_with("...")) {
                macros.push(CppMacro {
                    name: define.get(1).unwrap().as_str().to_string(),
                    params,
                    body: text[define.get(0).unwrap().end()..].to_string(),
                    at,
                });
            }
            continue;
        }
        let undef_name = UNDEF.captures(text).map(|undef| undef.get(1).unwrap().as_str().to_string());
        if let Some(name) = undef_name {
            undefs.push(Undef { name, at });
        }
    }
    (macros, undefs)
}

/// The invocations of the macros `names` in code (not in a directive, comment or
/// literal), with their arguments.
fn cpp_macro_invocations(source: &str, names: &HashSet<String>) -> Vec<Invocation> {
    let s = source.as_bytes();
    let n = s.len();
    let mut calls: Vec<Invocation> = Vec::new();
    for (name, word_at, word_end) in cpp_code_scan(source, Some(names)).words {
        let open = skip_blank_and_comments(source, word_end);
        if at(s, open) != b'(' {
            continue;
        }
        let mut args: Vec<String> = Vec::new();
        let mut depth = 0i32;
        let mut from = open + 1;
        let mut close: Option<usize> = None;
        let mut i = open;
        while i < n {
            let c = s[i];
            if c == b'"' || c == b'\'' {
                i = quoted_end(source, i) - 1;
            } else if c == b'/' && at(s, i + 1) == b'*' {
                i = find(source, "*/", i + 2).map_or(n, |end| end + 1);
            } else if c == b'/' && at(s, i + 1) == b'/' {
                i = find(source, "\n", i).map_or(n, |end| end - 1);
            } else if c == b'(' {
                depth += 1;
            } else if c == b')' {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            } else if c == b',' && depth == 1 {
                args.push(source[from..i].to_string());
                from = i + 1;
            }
            i += 1;
        }
        let Some(close) = close else { continue };
        args.push(source[from..close].to_string());
        calls.push(Invocation {
            name,
            at: word_at,
            end: close + 1,
            args: args.iter().map(|a| remove_all(&LINE_COMMENT, &remove_all(&BLOCK_COMMENT, a, " "), " ")).collect(),
        });
    }
    calls
}

/// A spliced directive without the `//` comment that ends it, one in a string literal aside.
fn without_line_comment(text: &str) -> &str {
    let s = text.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'"' || c == b'\'' {
            i = quoted_end(text, i) - 1;
        } else if c == b'/' && at(s, i + 1) == b'/' {
            return &text[..i];
        }
        i += 1;
    }
    text
}

/// Past blanks and comments from `i`.
fn skip_blank_and_comments(source: &str, mut i: usize) -> usize {
    loop {
        i = skip_blank(source, i);
        if starts_at(source, i, "//") {
            i = find(source, "\n", i).unwrap_or(source.len());
        } else if starts_at(source, i, "/*") {
            i = find(source, "*/", i + 2).map_or(source.len(), |end| end + 2);
        } else {
            return i;
        }
    }
}

fn is_raw_string_prefix(word: &str) -> bool {
    matches!(word, "R" | "uR" | "UR" | "LR" | "u8R")
}

struct CodeScan {
    /// Each directive's `#` offset and its text, `\` continuations included.
    directives: Vec<(usize, String)>,
    /// Each of the wanted identifiers: the word and where it starts and ends.
    words: Vec<(String, usize, usize)>,
}

/// One pass over C/C++ source, skipping comments and string, character and raw
/// string literals: the preprocessor directives (each with its `\` continuations),
/// and, when `names` is given, the code's identifiers among them.
fn cpp_code_scan(source: &str, names: Option<&HashSet<String>>) -> CodeScan {
    let s = source.as_bytes();
    let n = s.len();
    let mut directives: Vec<(usize, String)> = Vec::new();
    let mut words: Vec<(String, usize, usize)> = Vec::new();
    let mut line_start = true;
    let mut last_word_start = 0usize;
    let mut last_word_end: Option<usize> = None;
    let mut i = 0usize;
    while i < n {
        let c = s[i];
        if c == 10 {
            line_start = true;
            i += 1;
            continue;
        }
        if matches!(c, 32 | 9 | 13 | 11 | 12) {
            i += 1;
            continue;
        }
        if c == b'/' && at(s, i + 1) == b'/' {
            // A `\` at the end continues a line comment.
            let mut end = i;
            loop {
                match find(source, "\n", end) {
                    None => {
                        end = n;
                        break;
                    }
                    Some(e) => end = e,
                }
                let mut k = end as isize - 1;
                if k >= 0 && s[k as usize] == 13 {
                    k -= 1;
                }
                if k < 0 || s[k as usize] != 92 {
                    break;
                }
                end += 1;
            }
            i = end;
            continue;
        }
        if c == b'/' && at(s, i + 1) == b'*' {
            i = find(source, "*/", i + 2).map_or(n, |end| end + 2);
            continue;
        }
        if c == b'#' && line_start {
            let mut j = i + 1;
            while j < n {
                let d = s[j];
                if d == b'/' && at(s, j + 1) == b'*' {
                    j = find(source, "*/", j + 2).map_or(n - 1, |end| end + 1);
                } else if d == 10 {
                    let mut k = j as isize - 1;
                    if k >= 0 && s[k as usize] == 13 {
                        k -= 1;
                    }
                    if k < 0 || s[k as usize] != 92 {
                        break;
                    }
                }
                j += 1;
            }
            directives.push((i, source[i..j.min(n)].to_string()));
            i = j;
            continue;
        }
        line_start = false;
        if c == b'"' {
            if last_word_end == Some(i) && is_raw_string_prefix(&source[last_word_start..i]) {
                if let Some(paren) = find(source, "(", i + 1) {
                    let delimiter = &source[i + 1..paren];
                    if RAW_DELIMITER.is_match(delimiter) {
                        let close = format!("){delimiter}\"");
                        i = find(source, &close, paren + 1).map_or(n, |end| end + delimiter.len() + 2);
                        continue;
                    }
                }
            }
            i = quoted_end(source, i);
            continue;
        }
        if c == b'\'' {
            // A digit separator (`1'000`) is no character literal.
            if i > 0 && is_word_char(s[i - 1]) && last_word_end != Some(i) {
                i += 1;
                continue;
            }
            i = quoted_end(source, i);
            continue;
        }
        if is_word_char(c) {
            let mut j = i + 1;
            while j < n && is_word_char(s[j]) {
                j += 1;
            }
            if is_word_start(c) {
                last_word_start = i;
                last_word_end = Some(j);
                if names.is_some_and(|names| names.contains(&source[i..j])) {
                    words.push((source[i..j].to_string(), i, j));
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    CodeScan { directives, words }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROTOBUF_SHAPE: &str = r#"
#define PROTOBUF_DEFINE_ACCESSOR(CLASS, FIELD, TYPE) \
  inline TYPE CLASS::FIELD() const { return FIELD##_; }

namespace google {
namespace protobuf {
class FieldDescriptor { public: int number() const; };
PROTOBUF_DEFINE_ACCESSOR(FieldDescriptor, number, int)
}  // namespace protobuf
}  // namespace google
"#;

    fn mint(source: &str) -> (Vec<Node>, Vec<Edge>) {
        cpp_macro_methods("a.h", source, source, &[])
    }

    #[test]
    fn a_method_defining_macro_mints_a_method_where_it_is_invoked() {
        assert!(may_define_cpp_macro_methods(PROTOBUF_SHAPE));
        let (nodes, edges) = mint(PROTOBUF_SHAPE);
        assert_eq!(nodes.len(), 1);
        let n = &nodes[0];
        assert_eq!(n.name, "number");
        assert_eq!(n.qualified_name, "google::protobuf::FieldDescriptor::number");
        assert_eq!(n.signature.as_deref(), Some("inline int FieldDescriptor::number() const"));
        assert_eq!(n.return_type.as_deref(), None);
        assert_eq!(n.kind, NodeKind::Method);
        assert_eq!((n.start_line, n.end_line), (8, 8));
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].source, "file:a.h");
    }

    #[test]
    fn constructors_undefs_and_class_scope_invocations_mint_nothing() {
        let ctor = "#define C(CLASS) CLASS::CLASS() {}\nC(Foo)\n";
        assert!(mint(ctor).0.is_empty());
        let undef = "#define M(C, F) void C::F() {}\n#undef M\nM(A, b)\n";
        assert!(mint(undef).0.is_empty());
        let in_class = "#define M(C, F) void C::F() {}\nclass K {\nM(A, b)\n};\n";
        assert!(mint(in_class).0.is_empty());
        let plain = "#define MAX(a, b) ((a) > (b) ? (a) : (b))\nint x = MAX(1, 2);\n";
        assert!(!may_define_cpp_macro_methods(plain));
    }

    #[test]
    fn colliding_ids_get_the_column() {
        let src = "#define M(C, F) void C::F() {}\nM(A, f) M(A, f)\n";
        let (nodes, _) = mint(src);
        assert_eq!(nodes.len(), 2);
        assert_ne!(nodes[0].id, nodes[1].id);
        assert!(nodes[1].id.ends_with(":8"), "{}", nodes[1].id);
    }
}
