//! Python attribute and factory typing: the class `self.engine`, `Class.attr`, `local.attr` or
//! `make().x` is, read from the annotations and assignments that give the attribute its value —
//! the region of `name-matcher.ts` from `pythonAnnotationEvidence` to `matchPythonFactoryChain`.
//!
//! An answer is a class node, the runtime (a builtin / outside type — nothing in the project), or
//! nothing at all (the evidence disagrees or cannot be read). Every file whose text decided an
//! answer is recorded in `read`, so the edge can say where its type came from.

use super::infer::infer_local_receiver_type;
use super::python::python_derives_from;
use super::python_scope::{is_python_outside_module_name, is_python_outside_module_receiver};
use super::python::python_binds_locally;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_extract::strip_comments::strip_comments_for_regex;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim, utf16_from, utf16_len};
use codegraph_types::{Language, Node, NodeKind, ReferenceKind};
use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;

static PYTHON_RUNTIME_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "list", "dict", "set", "frozenset", "tuple", "str", "bytes", "bytearray", "int", "float", "complex", "bool", "object", "type", "range", "slice",
        "List", "Dict", "Set", "FrozenSet", "Tuple", "Sequence", "Mapping", "MutableMapping", "MutableSequence", "Iterable", "Iterator", "Callable",
        "deque", "defaultdict", "OrderedDict", "Counter", "collections.deque", "collections.defaultdict", "collections.OrderedDict",
        "collections.Counter",
    ]
    .into_iter()
    .collect()
});

// -- Evidence and answers -------------------------------------------------------------------------

/// What one annotation or assignment says an attribute's value is.
#[derive(Clone, Debug)]
pub enum PyEvidence {
    /// A builtin or outside type: nothing in the project.
    Runtime,
    /// A class written by name; `class_level` when a class body (not a method) assigns it.
    Type { ty: String, class_level: bool },
    /// The result of calling a function; `awaited` when written `await f()`.
    Factory { factory: String, awaited: bool },
    /// A conditional expression: both arms.
    Arms(Box<PyEvidence>, Box<PyEvidence>),
}

type PyAttrEvidence = Option<PyEvidence>;

#[derive(Clone, Debug)]
pub enum PyAnswer {
    Runtime,
    Class(NodeRef),
}

/// `Node | 'runtime' | null`.
pub type PyTypeAnswer = Option<PyAnswer>;

#[derive(Clone, Debug)]
pub enum AttrAnswer {
    Absent,
    Answer(PyTypeAnswer),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PyKind {
    Class,
    Function,
}

impl PyKind {
    fn node_kind(self) -> NodeKind {
        match self {
            PyKind::Class => NodeKind::Class,
            PyKind::Function => NodeKind::Function,
        }
    }
}

pub struct AttrStatements {
    mentioned: bool,
    evidence: Vec<PyEvidence>,
}

memo_map!(PyLinesMemo, String, Option<Rc<Vec<String>>>);
memo_map!(PyAttrMemo, String, Rc<(AttrAnswer, Vec<String>)>);
memo_map!(PyFactoryMemo, String, Rc<(PyTypeAnswer, Vec<String>)>);
memo_map!(PyStatementsMemo, String, Rc<AttrStatements>);
memo_map!(PyBasesMemo, String, Rc<Vec<Option<NodeRef>>>);
memo_map!(PyAttrCallMemo, String, Rc<(Tri<ResolvedRef>, Vec<String>)>);
memo_map!(PyFileClassesMemo, String, Rc<Vec<NodeRef>>);

fn python_lines(ctx: &Ctx, file_path: &str) -> Option<Rc<Vec<String>>> {
    memoized::<PyLinesMemo>(ctx, file_path.to_string(), || {
        ctx.read_file(file_path).map(|source| Rc::new(strip_comments_for_regex(&source, "python").split('\n').map(String::from).collect()))
    })
}

fn indent_of(l: &str) -> usize {
    utf16_len(l) - utf16_len(l.trim_start_matches(is_js_space))
}

fn line_text(lines: &[String], index: usize) -> &str {
    lines.get(index).map_or("", String::as_str)
}

fn is_upper_first(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn last_segment(s: &str) -> &str {
    s.rsplit('.').next().unwrap_or("")
}

// -- Annotations and values ------------------------------------------------------------------------

fn unquote(s: &str) -> String {
    js_trim(&re!(r#"^['"]|['"]$"#, "g").replace_all_str(js_trim(s), "")).to_string()
}

/// An annotation's evidence: `Optional[Engine]`, `Engine | None`, `"Engine"` → the class written.
fn python_annotation_evidence(raw: &str) -> PyAttrEvidence {
    let mut t = unquote(raw);
    for _ in 0..4 {
        if t.is_empty() {
            break;
        }
        let inner = re!(r"^(?:typing\.)?Optional\s*\[\s*(.+)\s*\]$")
            .captures(&t)
            .or_else(|| re!(r"^(?:typing\.)?Union\s*\[\s*([^,\]]+?)\s*,\s*None\s*\]$").captures(&t))
            .or_else(|| re!(r"^(.+?)\s*\|\s*None$").captures(&t))
            .or_else(|| re!(r"^None\s*\|\s*(.+)$").captures(&t))
            .and_then(|c| c.text(1).map(String::from));
        match inner {
            Some(inner) => t = unquote(&inner),
            None => break,
        }
    }
    let head = re!(r"^typing\.").replace_first(js_trim(t.split('[').next().unwrap_or("")), |_| String::new());
    if PYTHON_RUNTIME_TYPES.contains(head.as_str()) {
        return Some(PyEvidence::Runtime);
    }
    if t.contains('[') || !re!(r"^[A-Za-z_][\w.]*$").is_match(&t) || head == "Any" {
        return None;
    }
    Some(PyEvidence::Type { ty: t, class_level: false })
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Open {
    No,
    Continues,
    Broken,
}

fn run_at(chars: &[char], at: usize, c: char, n: usize) -> bool {
    at + n <= chars.len() && chars[at..at + n].iter().all(|x| *x == c)
}

/// The shape of an expression at bracket depth zero — strings collapse to `S`, bracketed groups
/// to their brackets — and whether it runs on to the next line.
fn python_expr_scan(raw: &str) -> (Option<String>, Open) {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut depth = 0i32;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            let triple = run_at(&chars, i, c, 3);
            let close = if triple { 3 } else { 1 };
            let mut j = i + close;
            while j < chars.len() && !run_at(&chars, j, c, close) {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            if j >= chars.len() {
                return (None, if triple { Open::Continues } else { Open::Broken });
            }
            if depth == 0 {
                out = re!(r"[rbfuRBFU]{1,2}$").replace_first(&out, |_| String::new());
                out.push('S');
            }
            i = j + close;
            continue;
        } else if matches!(c, '(' | '[' | '{') {
            if depth == 0 {
                out.push(c);
            }
            depth += 1;
        } else if matches!(c, ')' | ']' | '}') {
            depth -= 1;
            if depth < 0 {
                return (None, Open::Broken);
            }
            if depth == 0 {
                out.push(c);
            }
        } else if depth == 0 {
            out.push(c);
        }
        i += 1;
    }
    let shape = js_trim(&re!(r"\s+", "g").replace_all_str(&out, " ")).to_string();
    if depth > 0 || shape.ends_with('\\') {
        return (None, Open::Continues);
    }
    (Some(shape), Open::No)
}

fn python_expr_shape(raw: &str) -> Option<String> {
    python_expr_scan(raw).0
}

/// `a if cond else b` → `(a, b)`, read at bracket depth zero.
fn python_conditional_arms(raw: &str) -> Option<(String, String)> {
    let chars: Vec<char> = raw.chars().collect();
    let mut depth = 0i32;
    let mut if_at: Option<usize> = None;
    let word_then_space = |i: usize, word: &str| {
        let w: Vec<char> = word.chars().collect();
        chars.len() > i + w.len() && chars[i..i + w.len()] == w[..] && is_js_space(chars[i + w.len()])
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            let close = if run_at(&chars, i, c, 3) { 3 } else { 1 };
            let mut j = i + close;
            while j < chars.len() && !run_at(&chars, j, c, close) {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            i = j + close;
            continue;
        } else if matches!(c, '(' | '[' | '{') {
            depth += 1;
        } else if matches!(c, ')' | ']' | '}') {
            depth -= 1;
        } else if depth == 0 && i > 0 && is_js_space(chars[i - 1]) {
            if if_at.is_none() && word_then_space(i, "if") {
                if_at = Some(i);
            } else if let Some(at) = if_at {
                if word_then_space(i, "else") {
                    let head: String = chars[..at].iter().collect();
                    let tail: String = chars[(i + 4).min(chars.len())..].iter().collect();
                    return Some((js_trim(&head).to_string(), js_trim(&tail).to_string()));
                }
            }
        }
        i += 1;
    }
    None
}

/// An assigned value's evidence: a literal is the runtime, `Engine()` the class, `make()` a factory
/// call, a parameter name its annotation.
fn python_value_evidence(raw: &str, signature: Option<&str>) -> PyAttrEvidence {
    let mut v = js_trim(raw).to_string();
    if let Some((a, b)) = python_conditional_arms(&v) {
        let (a, b) = (python_value_evidence(&a, signature), python_value_evidence(&b, signature));
        return match (a, b) {
            (Some(a), Some(b)) => Some(PyEvidence::Arms(Box::new(a), Box::new(b))),
            _ => None,
        };
    }
    let awaited = re!(r"^await\s").is_match(&v);
    if awaited {
        v = re!(r"^await\s+").replace_first(&v, |_| String::new());
    }
    let shape = python_expr_shape(&v)?;
    if re!(r"^(?:S(?: S)*|-?\d[\w.]*|True|False|\[\]|\{\}|\(\))$").is_match(&shape) {
        return if awaited { None } else { Some(PyEvidence::Runtime) };
    }
    if let Some(ctor) = re!(r"^([A-Za-z_][\w.]*) ?\(\)$").captures(&shape) {
        let callee = ctor.text(1).unwrap_or("");
        if PYTHON_RUNTIME_TYPES.contains(callee) {
            return if awaited { None } else { Some(PyEvidence::Runtime) };
        }
        if is_upper_first(last_segment(callee)) {
            return if awaited { None } else { Some(PyEvidence::Type { ty: callee.to_string(), class_level: false }) };
        }
        return Some(PyEvidence::Factory { factory: callee.to_string(), awaited });
    }
    if let Some(signature) = signature.filter(|_| !awaited && re!(r"^[A-Za-z_]\w*$").is_match(&v)) {
        let param = Re::cached(&format!(r"[(,]\s*\*{{0,2}}{v}\s*:\s*([^,)=]+)"), "").captures(signature);
        if let Some(param) = param {
            return python_annotation_evidence(param.text(1).unwrap_or(""));
        }
    }
    None
}

/// A value that runs over several lines (an open bracket, a trailing backslash), joined; the line
/// it ends on.
fn python_joined_value(lines: &[String], ln: usize, first: &str, last_line: usize) -> (String, usize) {
    if python_expr_scan(first).1 != Open::Continues {
        return (first.to_string(), ln);
    }
    let mut text = first.to_string();
    let mut end = ln;
    while end < last_line && end - ln < 30 {
        text = format!("{} {}", re!(r"\\\s*$").replace_first(&text, |_| String::new()), js_trim(line_text(lines, end)));
        end += 1;
        match python_expr_scan(&text).1 {
            Open::No => return (text, end),
            Open::Broken => break,
            Open::Continues => {}
        }
    }
    (first.to_string(), ln)
}

/// The callee of a value that is a call (or of each arm of a conditional).
fn python_value_callees(value: &str) -> Vec<String> {
    let value = js_trim(value);
    if let Some((a, b)) = python_conditional_arms(value) {
        let mut out = python_value_callees(&a);
        out.extend(python_value_callees(&b));
        return out;
    }
    let stripped = re!(r"^await\s+").replace_first(value, |_| String::new());
    python_expr_shape(&stripped)
        .and_then(|shape| re!(r"^([A-Za-z_][\w.]*) ?\(\)$").captures(&shape).and_then(|c| c.text(1).map(String::from)))
        .into_iter()
        .collect()
}

/// A line with its string contents blanked (one space per UTF-16 unit), quotes kept.
fn python_blank_strings(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = chars.clone();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '"' && c != '\'' {
            i += 1;
            continue;
        }
        let q = if run_at(&chars, i, c, 3) { 3 } else { 1 };
        let mut j = i + q;
        while j < chars.len() && !run_at(&chars, j, c, q) {
            j += if chars[j] == '\\' { 2 } else { 1 };
        }
        for slot in out.iter_mut().take(j.min(chars.len())).skip(i + q) {
            *slot = ' ';
        }
        i = (j + q).min(chars.len());
    }
    let mut blanked = String::with_capacity(line.len());
    for (k, c) in out.iter().enumerate() {
        if *c == ' ' && chars[k] != ' ' {
            for _ in 0..chars[k].len_utf16() {
                blanked.push(' ');
            }
        } else {
            blanked.push(*c);
        }
    }
    blanked
}

/// The assignment targets of a statement, `a = b = value` → `a`, `b`.
fn python_assign_targets(statement: &str) -> Vec<String> {
    let stmt = re!(r"^\s*(?:(?:el)?if|while|else|try|finally|except|with|for)\b[^:]*:(?!=)").replace_first(statement, |_| String::new());
    let chars: Vec<char> = stmt.chars().collect();
    let mut targets = Vec::new();
    let mut depth = 0i32;
    let mut from = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, '(' | '[' | '{') {
            depth += 1;
        } else if matches!(c, ')' | ']' | '}') {
            depth = (depth - 1).max(0);
        } else if c == '=' && depth == 0 {
            if chars.get(i + 1) == Some(&'=') {
                i += 2;
                continue;
            }
            let prev = if i > 0 { chars[i - 1] } else { '\0' };
            if prev == '!' || prev == ':' {
                i += 1;
                continue;
            }
            if (prev == '<' || prev == '>') && (i < 2 || chars[i - 2] != prev) {
                i += 1;
                continue;
            }
            let seg: String = chars[from..i].iter().collect();
            if re!(r"\blambda\b").is_match(&seg) {
                break;
            }
            let seg = re!(r"(?:\*\*|\/\/|>>|<<|[-+*/%&|^@])$").replace_first(&seg, |_| String::new());
            targets.push(js_trim(seg.split(':').next().unwrap_or("")).to_string());
            from = i + 1;
        }
        i += 1;
    }
    targets
}

struct DefStatement {
    line: usize,
    statements: Vec<String>,
    nested: Option<String>,
}

/// A `def` header and the top-level statements of its body up to line `upto`.
fn python_def_statements(lines: &[String], def_line: usize, upto: usize) -> (String, Vec<DefStatement>) {
    let mut header = String::new();
    let mut ln = def_line;
    while ln <= upto && ln <= lines.len() {
        header.push(' ');
        header.push_str(js_trim(line_text(lines, ln - 1)));
        if re!(r"\)\s*(?:->[^:]*)?:\s*$").is_match(&header) {
            break;
        }
        ln += 1;
    }
    let mut body = Vec::new();
    let mut depth = 0i32;
    let mut continued = false;
    let mut nested_indent: i64 = -1;
    let mut k = ln + 1;
    while k < upto {
        let raw = line_text(lines, k - 1);
        k += 1;
        let line = k - 1;
        if js_trim(raw).is_empty() {
            continue;
        }
        let blanked = python_blank_strings(raw);
        let inside = depth > 0 || continued;
        for c in blanked.chars() {
            if matches!(c, '(' | '[' | '{') {
                depth += 1;
            } else if matches!(c, ')' | ']' | '}') {
                depth = (depth - 1).max(0);
            }
        }
        continued = re!(r"\\\s*$").is_match(&blanked);
        if inside {
            continue;
        }
        if nested_indent >= 0 {
            if indent_of(raw) as i64 > nested_indent {
                continue;
            }
            nested_indent = -1;
        }
        let text = js_trim(&blanked);
        if let Some(nested) = re!(r"^(?:async\s+)?(?:def|class)\s+(\w+)").captures(text) {
            nested_indent = indent_of(raw) as i64;
            body.push(DefStatement { line, statements: Vec::new(), nested: nested.text(1).map(String::from) });
            continue;
        }
        body.push(DefStatement {
            line,
            statements: text.split(';').map(|t| js_trim(t).to_string()).filter(|t| !t.is_empty()).collect(),
            nested: None,
        });
    }
    let _ = body.iter().map(|b| b.line).count();
    (header, body)
}

/// Whether `target` (the left side of an assignment) binds `name` itself, not an attribute or
/// element of it.
fn python_target_binds(target: &str, name: &str) -> bool {
    let n = escape_regex(name);
    for m in Re::cached(&format!(r"(?<![.\w]){n}\b"), "g").captures_iter(target) {
        let Some(whole) = m.get(0) else { continue };
        let index = whole.start();
        let after = target[index + name.len()..].trim_start_matches(is_js_space);
        if after.starts_with('.') || after.starts_with('[') || after.starts_with('(') {
            continue;
        }
        let mut stack: Vec<bool> = Vec::new();
        for (i, c) in target.char_indices() {
            if i >= index {
                break;
            }
            match c {
                '[' => stack.push(re!(r"[\w)\]]\s*$").is_match(&target[..i])),
                ']' | ')' => {
                    stack.pop();
                }
                '(' => stack.push(false),
                _ => {}
            }
        }
        if !stack.iter().any(|b| *b) {
            return true;
        }
    }
    false
}

/// Whether `name` is bound inside the function starting at `def_line`, before line `upto`: a
/// parameter (when `params`), an assignment, loop target, `with … as`, walrus or nested definition.
fn python_bound_in_def(lines: &[String], def_line: usize, upto: usize, name: &str, params: bool) -> bool {
    let n = escape_regex(name);
    let (header, body) = python_def_statements(lines, def_line, upto);
    if params {
        let list = re!(r"\((.*)\)").captures(&header).and_then(|c| c.text(1).map(String::from)).unwrap_or_default();
        let bound = list.split(',').any(|p| {
            let p = re!(r"^\*{1,2}").replace_first(js_trim(p), |_| String::new());
            js_trim(re!(r"[:=]").split(&p).first().copied().unwrap_or("")) == name
        });
        if bound {
            return true;
        }
    }
    let loop_re = Re::cached(&format!(r"^(?:async\s+)?for\s+[\w\s,()\[\]]*(?<![.\w]){n}\b[\w\s,()\[\]]*\bin\b"), "");
    let as_re = Re::cached(&format!(r"\bas\s+\(?\s*{n}\b"), "");
    let walrus = Re::cached(&format!(r"(?<![.\w]){n}\s*:="), "");
    for item in &body {
        if item.nested.as_deref() == Some(name) {
            return true;
        }
        for st in &item.statements {
            if python_assign_targets(st).iter().any(|t| python_target_binds(t, name)) {
                return true;
            }
            if loop_re.is_match(st) || walrus.is_match(st) {
                return true;
            }
            if !re!(r"^(?:from|import)\b").is_match(st) && as_re.is_match(st) {
                return true;
            }
        }
    }
    false
}

// -- What a class says about an attribute ----------------------------------------------------------------

fn python_attr_statements(owner: &Node, attr: &str, ctx: &Ctx) -> Rc<AttrStatements> {
    memoized::<PyStatementsMemo>(ctx, format!("{}\0{attr}", owner.id), || Rc::new(python_attr_statements_uncached(owner, attr, ctx)))
}

fn python_attr_statements_uncached(owner: &Node, attr: &str, ctx: &Ctx) -> AttrStatements {
    let Some(lines) = python_lines(ctx, &owner.file_path) else { return AttrStatements { mentioned: false, evidence: Vec::new() } };
    let nested: Vec<NodeRef> = ctx
        .get_nodes_in_file(&owner.file_path)
        .iter()
        .filter(|n| n.kind == NodeKind::Class && n.id != owner.id && n.start_line > owner.start_line && n.end_line <= owner.end_line)
        .cloned()
        .collect();
    let a = escape_regex(attr);
    let class_level = Re::cached(&format!(r"^{a}\s*(?::\s*([^=]+?))?\s*(?:=\s*(.+?))?\s*$"), "");
    let via_self = Re::cached(&format!(r"\b(?:self|cls)\.{a}\s*(?::\s*([^=]+?))?\s*(?:=(?!=)\s*(.+?))?\s*$"), "");
    let mut body_indent: i64 = -1;
    let mut signature: Option<String> = None;
    let mut def_start: usize = 0;
    let mut nested_def_indent: i64 = -1;
    let mut mentioned = false;
    let mut evidence: Vec<PyEvidence> = Vec::new();
    let end_line = owner.end_line as usize;
    let mut next = owner.start_line as usize + 1;
    while next <= end_line {
        let ln = next;
        next += 1;
        let line = line_text(&lines, ln - 1);
        if js_trim(line).is_empty() {
            continue;
        }
        if nested.iter().any(|n| ln as u32 >= n.start_line && ln as u32 <= n.end_line) {
            continue;
        }
        if body_indent < 0 {
            body_indent = indent_of(line) as i64;
        }
        let text = js_trim(line);
        let line_indent = indent_of(line) as i64;
        if re!(r"^(?:async\s+)?def\s").is_match(text) && line_indent == body_indent {
            def_start = ln;
            nested_def_indent = -1;
            let mut sig = text.to_string();
            let mut k = ln;
            while k < end_line && !re!(r"\)\s*(?:->[^:]*)?:\s*$").is_match(&sig) {
                sig.push(' ');
                sig.push_str(js_trim(line_text(&lines, k)));
                k += 1;
            }
            signature = Some(sig);
            continue;
        }
        if nested_def_indent >= 0 && line_indent <= nested_def_indent {
            nested_def_indent = -1;
        }
        if nested_def_indent < 0 && re!(r"^(?:async\s+)?def\s").is_match(text) && line_indent > body_indent {
            nested_def_indent = line_indent;
            continue;
        }
        let m = if line_indent == body_indent { class_level.captures(text) } else { via_self.captures(text) };
        let Some(m) = m else { continue };
        let annotation = m.text(1).filter(|s| !s.is_empty());
        let assigned = m.text(2).filter(|s| !s.is_empty());
        if annotation.is_none() && assigned.is_none() {
            continue;
        }
        mentioned = true;
        if nested_def_indent >= 0 && line_indent > nested_def_indent {
            continue;
        }
        let in_def = line_indent != body_indent && def_start > 0;
        let at = ln;
        let mut value = assigned.map(String::from);
        if annotation.is_none() {
            if let Some(first) = value.clone() {
                let (joined, end) = python_joined_value(&lines, ln, &first, end_line);
                value = Some(joined);
                next = end + 1;
            }
        }
        if annotation.is_none() && in_def {
            let v = js_trim(value.as_deref().unwrap_or("")).to_string();
            let reads: Vec<String> = python_value_callees(&v).iter().map(|c| c.split('.').next().unwrap_or("").to_string()).collect();
            if reads.iter().any(|c| python_bound_in_def(&lines, def_start, at, c, true)) {
                continue;
            }
            if re!(r"^[A-Za-z_]\w*$").is_match(&v) && python_bound_in_def(&lines, def_start, at, &v, false) {
                continue;
            }
        }
        let ev = match annotation {
            Some(a) => python_annotation_evidence(a),
            None => python_value_evidence(value.as_deref().unwrap_or(""), if in_def { signature.as_deref() } else { None }),
        };
        if let Some(ev) = ev {
            evidence.push(match ev {
                PyEvidence::Type { ty, .. } if annotation.is_none() && !in_def => PyEvidence::Type { ty, class_level: true },
                other => other,
            });
        }
    }
    AttrStatements { mentioned, evidence }
}

// -- Classes by name ------------------------------------------------------------------------------------

fn python_module_class(module_path: &str, class_name: &str, from_file: &str, ctx: &Ctx, kind: PyKind) -> Option<NodeRef> {
    let dots = module_path.chars().take_while(|c| *c == '.').count();
    let mut rel = module_path[dots..].replace('.', "/");
    if dots > 0 {
        let normalized = from_file.replace('\\', "/");
        let mut dir: Vec<&str> = normalized.split('/').collect();
        dir.pop();
        for _ in 1..dots {
            if dir.is_empty() {
                return None;
            }
            dir.pop();
        }
        let mut parts: Vec<String> = dir.iter().map(|s| s.to_string()).collect();
        if !rel.is_empty() {
            parts.push(rel);
        }
        rel = parts.join("/");
    }
    if rel.is_empty() {
        return None;
    }
    let wanted = [format!("{rel}.py"), format!("{rel}/__init__.py"), format!("{rel}.pyi")];
    let hits: Vec<NodeRef> = ctx
        .get_nodes_by_name(class_name)
        .iter()
        .filter(|n| {
            if n.kind != kind.node_kind() || n.language != Language::Python {
                return false;
            }
            if kind == PyKind::Function && n.qualified_name != n.name {
                return false;
            }
            let fp = n.file_path.replace('\\', "/");
            wanted.iter().any(|w| fp == *w || (dots == 0 && fp.ends_with(&format!("/{w}"))))
        })
        .cloned()
        .collect();
    if hits.len() == 1 {
        Some(hits[0].clone())
    } else {
        None
    }
}

/// The class (or module-level function) a possibly dotted name denotes in `from_file`: through the
/// file's imports, or declared in the file itself.
fn python_class_named(type_name: &str, from_file: &str, ctx: &Ctx, kind: PyKind) -> Option<NodeRef> {
    let segs: Vec<&str> = type_name.split('.').collect();
    let class_name = segs[segs.len() - 1];
    let imports = ctx.get_import_mappings(from_file, Language::Python);
    let mut candidates: Vec<Option<NodeRef>> = Vec::new();
    for imp in imports.iter() {
        if imp.is_namespace && segs.len() > 1 && type_name.starts_with(&format!("{}.", imp.source)) {
            let tail = &type_name[imp.source.len() + 1..];
            let parts: Vec<&str> = tail.split('.').collect();
            let mut module = vec![imp.source.as_str()];
            module.extend(&parts[..parts.len() - 1]);
            candidates.push(python_module_class(&module.join("."), class_name, from_file, ctx, kind));
        } else if imp.local_name == segs[0] {
            if segs.len() == 1 {
                candidates.push(if imp.is_namespace { None } else { python_module_class(&imp.source, &imp.exported_name, from_file, ctx, kind) });
            } else {
                let base = if imp.is_namespace {
                    imp.source.clone()
                } else if imp.source.ends_with('.') {
                    format!("{}{}", imp.source, imp.exported_name)
                } else {
                    format!("{}.{}", imp.source, imp.exported_name)
                };
                let mut module = vec![base.as_str()];
                module.extend(&segs[1..segs.len() - 1]);
                candidates.push(python_module_class(&module.join("."), class_name, from_file, ctx, kind));
            }
        }
    }
    if !candidates.is_empty() {
        let ids: HashSet<&str> = candidates.iter().map(|c| c.as_ref().map_or("", |n| n.id.as_str())).collect();
        return if ids.len() == 1 { candidates[0].clone() } else { None };
    }
    if segs.len() > 1 {
        return None;
    }
    let local: Vec<NodeRef> = ctx
        .get_nodes_by_name(type_name)
        .iter()
        .filter(|n| n.kind == kind.node_kind() && n.language == Language::Python && n.file_path == from_file && (kind == PyKind::Class || n.qualified_name == n.name))
        .cloned()
        .collect();
    if local.len() == 1 {
        Some(local[0].clone())
    } else {
        None
    }
}

fn python_class_base_names(cls: &Node, ctx: &Ctx) -> Vec<String> {
    let Some(lines) = python_lines(ctx, &cls.file_path) else { return Vec::new() };
    let from = (cls.start_line as usize).saturating_sub(1).min(lines.len());
    let to = (cls.start_line as usize + 4).min(lines.len());
    let header = lines[from..to.max(from)].join(" ");
    let bases = Re::cached(&format!(r"\bclass\s+{}\s*\(([^)]*)\)", escape_regex(&cls.name)), "").captures(&header).and_then(|c| c.text(1).map(String::from));
    bases
        .unwrap_or_default()
        .split(',')
        .map(|b| js_trim(b).to_string())
        .filter(|b| re!(r"^[A-Za-z_][\w.]*$").is_match(b) && b != "object")
        .collect()
}

fn python_class_bases(cls: &Node, ctx: &Ctx) -> Rc<Vec<Option<NodeRef>>> {
    memoized::<PyBasesMemo>(ctx, cls.id.clone(), || {
        Rc::new(python_class_base_names(cls, ctx).iter().map(|b| python_class_named(b, &cls.file_path, ctx, PyKind::Class)).collect())
    })
}

/// Whether every base of `cls`, transitively, is a project class or a module from outside the
/// project — so a method it lacks is truly absent.
fn python_hierarchy_known(cls: &Node, ctx: &Ctx, seen: &mut HashSet<String>) -> bool {
    if seen.contains(&cls.id) || seen.len() >= 5 {
        return true;
    }
    seen.insert(cls.id.clone());
    let bases = python_class_bases(cls, ctx);
    for (i, name) in python_class_base_names(cls, ctx).iter().enumerate() {
        let known = match bases.get(i).and_then(|b| b.as_ref()) {
            Some(base) => python_hierarchy_known(base, ctx, seen),
            None => is_python_outside_module_name(name.split('.').next().unwrap_or(""), &cls.file_path, ctx),
        };
        if !known {
            return false;
        }
    }
    true
}

fn python_find_method(cls: &Node, method_name: &str, ctx: &Ctx, read: &mut BTreeSet<String>, seen: &mut HashSet<String>) -> Option<(NodeRef, bool)> {
    if seen.contains(&cls.id) || seen.len() >= 5 {
        return None;
    }
    seen.insert(cls.id.clone());
    let own_qualified = format!("{}::{method_name}", cls.name);
    let nested_qualified = format!("::{own_qualified}");
    let own = ctx
        .get_nodes_by_name(method_name)
        .iter()
        .find(|m| {
            m.kind == NodeKind::Method
                && m.language == Language::Python
                && m.file_path == cls.file_path
                && (m.qualified_name == own_qualified || m.qualified_name.ends_with(&nested_qualified))
        })
        .cloned();
    if let Some(own) = own {
        return Some((own, false));
    }
    read.insert(cls.file_path.clone());
    for base in python_class_bases(cls, ctx).iter() {
        if let Some(base) = base {
            if let Some((method, _)) = python_find_method(base, method_name, ctx, read, seen) {
                return Some((method, true));
            }
        }
    }
    None
}

fn python_method_on_class(cls: &Node, method_name: &str, r: &UnresolvedRef, ctx: &Ctx, read: &mut BTreeSet<String>) -> Option<ResolvedRef> {
    let (method, inherited) = python_find_method(cls, method_name, ctx, read, &mut HashSet::new())?;
    Some(ResolvedRef::new(r, method.id.clone(), if inherited { 0.8 } else { 0.85 }, ResolvedBy::InstanceMethod))
}

/// The one subclass method of that name, when the receiver's class is a base the hierarchy narrows.
fn python_subclass_method(cls: &Node, method_name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let descendants: Vec<NodeRef> = ctx
        .get_nodes_by_name(method_name)
        .iter()
        .filter(|m| {
            if m.kind != NodeKind::Method || m.language != Language::Python {
                return false;
            }
            let parent = ctx
                .get_nodes_in_file(&m.file_path)
                .iter()
                .find(|c| c.kind == NodeKind::Class && m.qualified_name == format!("{}::{method_name}", c.qualified_name))
                .cloned();
            parent.is_some_and(|p| python_derives_from(&p, cls, r, ctx, &mut HashSet::new()))
        })
        .cloned()
        .collect();
    if descendants.len() == 1 {
        Some(ResolvedRef::new(r, descendants[0].id.clone(), 0.8, ResolvedBy::InstanceMethod))
    } else {
        None
    }
}

fn answer_key(a: &PyTypeAnswer) -> String {
    match a {
        None => "?".to_string(),
        Some(PyAnswer::Runtime) => String::new(),
        Some(PyAnswer::Class(n)) => n.id.clone(),
    }
}

/// All the answers agree on one class (or on the runtime).
fn python_agree(answers: &[PyTypeAnswer]) -> PyTypeAnswer {
    if answers.is_empty() || answers.iter().any(|a| a.is_none()) {
        return None;
    }
    let keys: HashSet<String> = answers.iter().map(answer_key).collect();
    if keys.len() == 1 {
        answers[0].clone()
    } else {
        None
    }
}

fn python_evidence_class(ev: &PyEvidence, from_file: &str, ctx: &Ctx, read: &mut BTreeSet<String>, depth: u32, cls_is: &Option<NodeRef>) -> PyTypeAnswer {
    match ev {
        PyEvidence::Runtime => Some(PyAnswer::Runtime),
        PyEvidence::Arms(a, b) => python_agree(&[
            python_evidence_class(a, from_file, ctx, read, depth, cls_is),
            python_evidence_class(b, from_file, ctx, read, depth, cls_is),
        ]),
        PyEvidence::Type { ty, class_level } => python_class_named(ty, from_file, ctx, PyKind::Class).map(PyAnswer::Class).or_else(|| {
            if !*class_level && is_python_outside_module_name(ty.split('.').next().unwrap_or(""), from_file, ctx) {
                Some(PyAnswer::Runtime)
            } else {
                None
            }
        }),
        PyEvidence::Factory { factory, awaited } => {
            if factory == "cls" {
                return if *awaited { None } else { cls_is.clone().map(PyAnswer::Class) };
            }
            python_factory_type(factory, *awaited, from_file, ctx, read, depth)
        }
    }
}

/// The class a call of `callee` returns, from its return annotation or its `return` statements.
fn python_factory_type(callee: &str, awaited: bool, from_file: &str, ctx: &Ctx, read: &mut BTreeSet<String>, depth: u32) -> PyTypeAnswer {
    if depth > 0 {
        return python_factory_type_uncached(callee, awaited, from_file, ctx, read, depth);
    }
    let hit = memoized::<PyFactoryMemo>(ctx, format!("{from_file}\0{callee}\0{}", awaited as u8), || {
        let mut own = BTreeSet::new();
        let answer = python_factory_type_uncached(callee, awaited, from_file, ctx, &mut own, 0);
        Rc::new((answer, own.into_iter().collect()))
    });
    read.extend(hit.1.iter().cloned());
    hit.0.clone()
}

fn python_factory_type_uncached(callee: &str, awaited: bool, from_file: &str, ctx: &Ctx, read: &mut BTreeSet<String>, depth: u32) -> PyTypeAnswer {
    if depth > 3 {
        return None;
    }
    let segs: Vec<&str> = callee.split('.').collect();
    let fname = segs[segs.len() - 1];
    let prefix = segs[..segs.len() - 1].join(".");
    let mut receiver: Option<NodeRef> = None;
    let func: Option<NodeRef>;
    if !prefix.is_empty() && segs.len() >= 2 && is_upper_first(segs[segs.len() - 2]) {
        receiver = python_class_named(&prefix, from_file, ctx, PyKind::Class);
        func = receiver.as_ref().and_then(|r| python_find_method(r, fname, ctx, read, &mut HashSet::new()).map(|(m, _)| m));
    } else {
        func = python_class_named(callee, from_file, ctx, PyKind::Function);
    }
    let func = func?;
    read.insert(func.file_path.clone());
    let lines = python_lines(ctx, &func.file_path)?;
    let def_line = js_trim(line_text(&lines, (func.start_line as usize).saturating_sub(1)));
    let is_async = re!(r"^async\s+def\b").is_match(def_line);
    if is_async != awaited {
        return None;
    }
    if let Some(returns) = re!(r"->\s*(.+)$").captures(func.signature.as_deref().unwrap_or("")) {
        return match python_annotation_evidence(returns.text(1).unwrap_or("")) {
            Some(PyEvidence::Runtime) => Some(PyAnswer::Runtime),
            Some(PyEvidence::Type { ty, .. }) => {
                if re!(r"^(?:typing(?:_extensions)?\.)?Self$").is_match(&ty) {
                    receiver.map(PyAnswer::Class)
                } else {
                    python_class_named(&ty, &func.file_path, ctx, PyKind::Class).map(PyAnswer::Class)
                }
            }
            _ => None,
        };
    }
    let nested: Vec<NodeRef> = ctx
        .get_nodes_in_file(&func.file_path)
        .iter()
        .filter(|n| {
            n.id != func.id
                && n.start_line > func.start_line
                && n.end_line <= func.end_line
                && matches!(n.kind, NodeKind::Function | NodeKind::Method | NodeKind::Class)
        })
        .cloned()
        .collect();
    let mut decorated = String::new();
    let mut dl = func.start_line as i64 - 2;
    while dl >= 0 && re!(r"^\s*@").is_match(line_text(&lines, dl as usize)) {
        decorated.push_str(line_text(&lines, dl as usize));
        dl -= 1;
    }
    let is_classmethod = re!(r"@classmethod\b").is_match(&decorated);
    let class_is = if is_classmethod { receiver.clone() } else { None };
    let end_line = func.end_line as usize;
    let mut answers: Vec<PyTypeAnswer> = Vec::new();
    let mut next = func.start_line as usize + 1;
    while next <= end_line {
        let ln = next;
        next += 1;
        if nested.iter().any(|n| ln as u32 >= n.start_line && ln as u32 <= n.end_line) {
            continue;
        }
        let text = js_trim(line_text(&lines, ln - 1));
        let code = python_blank_strings(text);
        if re!(r"^yield\b|[=(\s]yield\b").is_match(&code) {
            return None;
        }
        let Some(ret) = re!(r"(?:^|[:;]\s*)return\b\s*").find(&code) else { continue };
        let at = ln;
        let offset = utf16_len(&code[..ret.end()]);
        let (joined, end) = python_joined_value(&lines, ln, utf16_from(text, offset), end_line);
        next = end + 1;
        let expr = js_trim(&joined);
        if expr.is_empty() || expr == "None" {
            continue;
        }
        let Some(ev) = python_value_evidence(expr, None) else { return None };
        if python_value_callees(expr).iter().any(|c| c != "cls" && python_bound_in_def(&lines, func.start_line as usize, at, c.split('.').next().unwrap_or(""), true)) {
            return None;
        }
        answers.push(python_evidence_class(&ev, &func.file_path, ctx, read, depth + 1, &class_is));
    }
    python_agree(&answers)
}

/// The class a `self.attr` / `Class.attr` holds, from every statement of the class (and its
/// bases) that annotates or assigns it.
fn python_attr_class(owner: &Node, attr: &str, ctx: &Ctx, read: &mut BTreeSet<String>) -> AttrAnswer {
    let hit = memoized::<PyAttrMemo>(ctx, format!("{}\0{attr}", owner.id), || {
        let mut own = BTreeSet::new();
        let answer = python_attr_class_walk(owner, attr, ctx, &mut own, &mut HashSet::new());
        Rc::new((answer, own.into_iter().collect()))
    });
    read.extend(hit.1.iter().cloned());
    hit.0.clone()
}

fn python_attr_class_walk(owner: &Node, attr: &str, ctx: &Ctx, read: &mut BTreeSet<String>, seen: &mut HashSet<String>) -> AttrAnswer {
    if seen.contains(&owner.id) || seen.len() >= 5 {
        return AttrAnswer::Absent;
    }
    seen.insert(owner.id.clone());
    read.insert(owner.file_path.clone());
    let statements = python_attr_statements(owner, attr, ctx);
    if statements.mentioned {
        let mut answers: Vec<PyTypeAnswer> = Vec::new();
        for ev in &statements.evidence {
            let made = python_evidence_class(ev, &owner.file_path, ctx, read, 0, &None);
            if made.is_some() || matches!(ev, PyEvidence::Runtime) || !matches!(ev, PyEvidence::Factory { .. }) {
                answers.push(made);
            }
        }
        let keys: HashSet<String> = answers.iter().map(answer_key).collect();
        if keys.len() != 1 {
            return AttrAnswer::Answer(None);
        }
        return AttrAnswer::Answer(answers[0].clone());
    }
    let mut from_bases: Vec<PyTypeAnswer> = Vec::new();
    for base in python_class_bases(owner, ctx).iter() {
        let Some(base) = base else { continue };
        if let AttrAnswer::Answer(got) = python_attr_class_walk(base, attr, ctx, read, seen) {
            from_bases.push(got);
        }
    }
    if !from_bases.is_empty() {
        return AttrAnswer::Answer(python_agree(&from_bases));
    }
    if python_has_outside_base(owner, ctx) {
        AttrAnswer::Answer(Some(PyAnswer::Runtime))
    } else {
        AttrAnswer::Absent
    }
}

/// A base from outside the project that may define attributes of its own (not an ABC, an enum or an
/// exception).
fn python_has_outside_base(cls: &Node, ctx: &Ctx) -> bool {
    python_class_base_names(cls, ctx)
        .iter()
        .filter(|b| !re!(r"^(?:ABC|Protocol|NamedTuple|TypedDict|Enum|IntEnum|StrEnum|Flag|IntFlag|\w*(?:Error|Exception|Warning))$").is_match(last_segment(b)))
        .any(|b| is_python_outside_module_name(b.split('.').next().unwrap_or(""), &cls.file_path, ctx))
}

fn python_file_classes(file_path: &str, ctx: &Ctx) -> Rc<Vec<NodeRef>> {
    memoized::<PyFileClassesMemo>(ctx, file_path.to_string(), || {
        Rc::new(ctx.get_nodes_in_file(file_path).iter().filter(|n| n.kind == NodeKind::Class).cloned().collect())
    })
}

fn python_attr_call_on(owner: &Node, attr: &str, method_name: &str, r: &UnresolvedRef, ctx: &Ctx, read: &mut BTreeSet<String>) -> Tri<ResolvedRef> {
    let hit = memoized::<PyAttrCallMemo>(ctx, format!("{}\0{attr}\0{method_name}", owner.id), || {
        let mut own = BTreeSet::new();
        let cls = python_attr_class(owner, attr, ctx, &mut own);
        let answer = match cls {
            AttrAnswer::Answer(Some(PyAnswer::Runtime)) => Tri::Null,
            AttrAnswer::Absent | AttrAnswer::Answer(None) => Tri::Undefined,
            AttrAnswer::Answer(Some(PyAnswer::Class(cls))) => {
                match python_method_on_class(&cls, method_name, r, ctx, &mut own).or_else(|| python_subclass_method(&cls, method_name, r, ctx)) {
                    Some(m) => Tri::Value(m),
                    None => {
                        if python_hierarchy_known(&cls, ctx, &mut HashSet::new()) {
                            Tri::Null
                        } else {
                            Tri::Undefined
                        }
                    }
                }
            }
        };
        Rc::new((answer, own.into_iter().collect()))
    });
    read.extend(hit.1.iter().cloned());
    match &hit.0 {
        Tri::Value(m) => {
            let mut m = m.clone();
            m.original = r.clone();
            Tri::Value(m)
        }
        other => other.clone(),
    }
}

fn with_type_from(mut hit: ResolvedRef, read: &BTreeSet<String>) -> ResolvedRef {
    if !read.is_empty() {
        let mut metadata = codegraph_types::Metadata::new();
        metadata.insert("typeFrom".to_string(), serde_json::Value::Array(read.iter().map(|f| serde_json::Value::String(f.clone())).collect()));
        hit.metadata = Some(metadata);
    }
    hit
}

/// `self.engine.start()`, `Config.engine.start()`, `local.engine.start()`: the method on the class
/// the attribute holds.
pub fn match_python_attr_call(receiver: &str, method_name: &str, r: &UnresolvedRef, ctx: &Ctx, read: &mut BTreeSet<String>) -> Tri<ResolvedRef> {
    let segs: Vec<&str> = receiver.split('.').collect();
    let root = segs[0];
    if is_python_outside_module_receiver(root, r, ctx)
        || (ctx.get_import_mappings(&r.file_path, Language::Python).iter().any(|m| m.is_namespace && m.local_name == root)
            && !python_binds_locally(root, r, ctx, false))
    {
        return Tri::Null;
    }
    if segs.len() != 2 {
        return Tri::Undefined;
    }
    let (head, attr) = (segs[0], segs[1]);
    let mut owner: Option<NodeRef> = None;
    if head == "self" || head == "cls" {
        for n in python_file_classes(&r.file_path, ctx).iter() {
            if n.start_line > r.line || n.end_line < r.line {
                continue;
            }
            if owner.as_ref().is_none_or(|o| n.start_line > o.start_line) {
                owner = Some(n.clone());
            }
        }
    } else if is_upper_first(head) {
        owner = python_class_named(head, &r.file_path, ctx, PyKind::Class);
    } else if let Some(head_type) = infer_local_receiver_type(head, r, ctx, None) {
        owner = python_class_named(&head_type, &r.file_path, ctx, PyKind::Class);
    }
    let Some(owner) = owner else { return Tri::Undefined };
    match python_attr_call_on(&owner, attr, method_name, r, ctx, read) {
        Tri::Value(hit) => {
            read.remove(&r.file_path);
            Tri::Value(with_type_from(hit, read))
        }
        other => other,
    }
}

/// `make().method()` / `Factory.create().method()`: the method on the class the factory returns.
pub fn match_python_factory_chain(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let m = re!(r"^([A-Za-z_][\w.]*)\(\)\.(\w+)$").captures(&r.reference_name)?;
    let (callee, method_name) = (m.text(1)?, m.text(2)?);
    let lines = python_lines(ctx, &r.file_path);
    let mut func: Option<NodeRef> = None;
    for n in ctx.get_nodes_in_file(&r.file_path).iter() {
        if !matches!(n.kind, NodeKind::Function | NodeKind::Method) || n.start_line > r.line || n.end_line < r.line {
            continue;
        }
        if func.as_ref().is_none_or(|f| n.start_line > f.start_line) {
            func = Some(n.clone());
        }
    }
    if let (Some(lines), Some(func)) = (&lines, &func) {
        if python_bound_in_def(lines, func.start_line as usize, r.line as usize, callee.split('.').next().unwrap_or(""), true) {
            return None;
        }
    }
    let mut read: BTreeSet<String> = BTreeSet::new();
    let made: PyTypeAnswer = if is_upper_first(last_segment(callee)) {
        python_class_named(callee, &r.file_path, ctx, PyKind::Class).map(PyAnswer::Class)
    } else {
        python_factory_type(callee, false, &r.file_path, ctx, &mut read, 0)
    };
    let Some(PyAnswer::Class(made)) = made else { return None };
    let hit = python_method_on_class(&made, method_name, r, ctx, &mut read)?;
    read.remove(&r.file_path);
    Some(with_type_from(hit, &read))
}

/// Whether a Python attribute reference (`self.engine.start`, `make().start`) resolves through
/// the attribute machinery.
pub fn python_attr_ref_resolves(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if r.language != Language::Python {
        return false;
    }
    if r.reference_name.contains("().") {
        return match_python_factory_chain(r, ctx).is_some();
    }
    let Some(dot) = r.reference_name.rfind('.').filter(|d| *d > 0) else { return false };
    let receiver = &r.reference_name[..dot];
    if !receiver.contains('.') {
        return false;
    }
    match_python_attr_call(receiver, &r.reference_name[dot + 1..], r, ctx, &mut BTreeSet::new()).value().is_some()
}

/// `from pkg import name` binds a module only: the import reference whose name is the `from`
/// module of its own statement.
pub fn is_python_from_import_module(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if r.language != Language::Python || r.reference_kind != ReferenceKind::Edge(codegraph_types::EdgeKind::Imports) {
        return false;
    }
    let lines = lines_of(ctx, &r.file_path);
    let Some(line) = (r.line as usize).checked_sub(1).and_then(|i| lines.get(i)) else { return false };
    if !starts_with_at(line, "from", r.column) {
        return false;
    }
    let mut text = after_col(line, r.column + 4).to_string();
    let mut next = r.line as usize;
    while re!(r"\\\s*$").is_match(&text) && next < r.line as usize + 3 {
        text = format!("{}{}", re!(r"\\\s*$").replace_first(&text, |_| " ".to_string()), lines.get(next).map_or("", String::as_str));
        next += 1;
    }
    let name = text.trim_start_matches(is_js_space);
    name.len() < text.len() && name.starts_with(r.reference_name.as_str()) && re!(r"^\s+import\b").is_match(&name[r.reference_name.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_annotation_names_its_class_through_optional_and_union() {
        for written in ["Engine", "Optional[Engine]", "\"Engine\"", "Engine | None", "typing.Optional[Engine]", "Union[Engine, None]"] {
            match python_annotation_evidence(written) {
                Some(PyEvidence::Type { ty, .. }) => assert_eq!(ty, "Engine", "{written}"),
                other => panic!("{written}: {other:?}"),
            }
        }
        assert!(matches!(python_annotation_evidence("dict[str, int]"), Some(PyEvidence::Runtime)));
        assert!(python_annotation_evidence("Foo[int]").is_none());
        assert!(matches!(python_annotation_evidence("List[Engine]"), Some(PyEvidence::Runtime)));
    }

    #[test]
    fn a_value_is_a_literal_a_constructor_or_a_factory_call() {
        assert!(matches!(python_value_evidence("[]", None), Some(PyEvidence::Runtime)));
        assert!(matches!(python_value_evidence("'text'", None), Some(PyEvidence::Runtime)));
        assert!(matches!(python_value_evidence("Engine()", None), Some(PyEvidence::Type { .. })));
        assert!(matches!(python_value_evidence("make_engine()", None), Some(PyEvidence::Factory { awaited: false, .. })));
        assert!(matches!(python_value_evidence("await make_engine()", None), Some(PyEvidence::Factory { awaited: true, .. })));
        assert!(matches!(python_value_evidence("Engine(1, 2)", None), Some(PyEvidence::Type { .. })));
        assert!(python_value_evidence("engine", None).is_none());
    }

    #[test]
    fn a_conditional_value_has_two_arms() {
        assert_eq!(python_conditional_arms("Engine() if fast else Slow()"), Some(("Engine()".to_string(), "Slow()".to_string())));
        assert_eq!(python_conditional_arms("foo(a if b else c)"), None);
    }

    #[test]
    fn assignment_targets_skip_comparisons_and_compound_operators() {
        assert_eq!(python_assign_targets("a = b = c()"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(python_assign_targets("a == b"), Vec::<String>::new());
        assert_eq!(python_assign_targets("a += 1"), vec!["a".to_string()]);
    }

    #[test]
    fn a_target_binds_the_name_not_its_attribute() {
        assert!(python_target_binds("engine", "engine"));
        assert!(python_target_binds("a, engine", "engine"));
        assert!(!python_target_binds("engine.x", "engine"));
        assert!(!python_target_binds("table[engine]", "engine"));
    }
}
