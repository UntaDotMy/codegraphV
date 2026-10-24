//! A per-file scope index of Go source: every `:=`, `var` / `const` and parameter declaration,
//! with the byte range of the block each is visible in.
//!
//! The index reads blanked source (comments and string contents replaced by spaces, offsets
//! unchanged), so a brace or `:=` inside either never counts. Offsets are bytes into that text;
//! a reference's UTF-16 column is converted at the edge (`go_offset`).

use crate::ctx::Ctx;
use crate::name_matcher::util::*;
use crate::types::UnresolvedRef;
use codegraph_jsre::re;
use codegraph_types::jscompat::{js_trim, utf16_len};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;

pub static GO_TYPE_KEYWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| ["chan", "func", "interface", "map", "struct"].into_iter().collect());

static GO_PREDECLARED_CALLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "chan", "func", "interface", "map", "struct", "append", "cap", "clear", "close", "complex", "copy", "delete", "imag", "len", "make", "max",
        "min", "new", "panic", "print", "println", "real", "recover", "any", "bool", "byte", "complex64", "complex128", "error", "float32",
        "float64", "int", "int8", "int16", "int32", "int64", "rune", "string", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr",
    ]
    .into_iter()
    .collect()
});

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GoDeclKind {
    Local,
    Header,
    Case,
    Param,
}

/// The right-hand side a declaration takes its value from: where it starts, which of the `n`
/// names this one is (`k`).
#[derive(Clone, Copy, Debug)]
pub struct GoValueAt {
    pub at: usize,
    pub k: usize,
    pub n: usize,
}

#[derive(Debug)]
pub struct GoDecl {
    pub at: usize,
    pub kind: GoDeclKind,
    /// A parameter's function body (the offset of its `{`).
    pub body: Option<usize>,
    /// The type the declaration writes (`var x T`, `x := T{}`, a parameter's type).
    pub ty: Option<String>,
    /// The package a `x := pkg.Func(…)` / `pkg.Type{…}` value comes from.
    pub from: Option<String>,
    /// The function a `x := f(…)` value is the result of.
    pub callee: Option<String>,
    pub value: Option<GoValueAt>,
}

type Scope = (usize, usize, Rc<GoDecl>);

pub struct GoScopeIndex {
    pub code: String,
    pub line_starts: Vec<usize>,
    events: Vec<usize>,
    open: Vec<isize>,
    close: HashMap<usize, usize>,
    pub decls: HashMap<String, Vec<Rc<GoDecl>>>,
    scopes: RefCell<HashMap<String, Rc<Vec<Scope>>>>,
    pub generic: Cell<Option<bool>>,
}

fn is_go_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn at_byte(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

pub fn go_scope_index(file_path: &str, ctx: &Ctx) -> Option<Rc<GoScopeIndex>> {
    ctx.memo::<Bounded<Option<Rc<GoScopeIndex>>, 64>>().get_or(file_path, false, || {
        ctx.get_file_lines(file_path).map(|lines| Rc::new(read_go_scopes(blank_go_code(&lines.join("\n")))))
    })
}

/// The offset into the index's code of `line` / `column` (a UTF-16 column).
pub fn go_offset(index: &GoScopeIndex, ctx: &Ctx, file_path: &str, line: u32, column: i64) -> usize {
    let col = column.max(0) as u32;
    match (line as usize).checked_sub(1).and_then(|i| index.line_starts.get(i)).copied() {
        Some(start) => start + line_at(ctx, file_path, line).map_or(col as usize, |l| col_byte(&l, col)),
        None => index.code.len() + col as usize,
    }
}

pub fn go_ref_offset(index: &GoScopeIndex, ctx: &Ctx, r: &UnresolvedRef) -> usize {
    go_offset(index, ctx, &r.file_path, r.line, r.column as i64)
}

/// The declaration of `name` visible at `r`'s position.
pub fn go_local_decl(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<Rc<GoDecl>> {
    go_local_decl_at_ref(name, &r.file_path, r.line, r.column, ctx)
}

pub fn go_local_decl_at_ref(name: &str, file_path: &str, line: u32, column: u32, ctx: &Ctx) -> Option<Rc<GoDecl>> {
    let index = go_scope_index(file_path, ctx)?;
    if !index.decls.contains_key(name) {
        return None;
    }
    let at = go_offset(&index, ctx, file_path, line, column as i64);
    go_local_decl_at(&index, name, at)
}

pub fn go_local_decl_at(index: &GoScopeIndex, name: &str, at: usize) -> Option<Rc<GoDecl>> {
    let mut found: Option<Scope> = None;
    for scope in go_scopes_of(index, name).iter() {
        if at > scope.0 && at < scope.1 && found.as_ref().is_none_or(|f| scope.0 > f.0) {
            found = Some(scope.clone());
        }
    }
    found.map(|f| f.2)
}

/// Where the local `name` visible at `r` is declared, and the type its declaration writes.
pub struct GoLocalDeclaration {
    pub line: u32,
    pub column: u32,
    pub ty: Option<String>,
}

pub fn go_local_declaration_at(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<GoLocalDeclaration> {
    let decl = go_local_decl(name, r, ctx)?;
    let index = go_scope_index(&r.file_path, ctx)?;
    let (mut lo, mut hi) = (0usize, index.line_starts.len() - 1);
    while lo < hi {
        let mid = (lo + hi + 1) >> 1;
        if index.line_starts[mid] <= decl.at {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let column = utf16_len(slice_bytes(&index.code, index.line_starts[lo], decl.at)) as u32;
    Some(GoLocalDeclaration { line: lo as u32 + 1, column, ty: decl.ty.clone() })
}

pub fn is_go_locally_bound(r: &UnresolvedRef, ctx: &Ctx) -> bool {
    go_local_decl(&r.reference_name, r, ctx).is_some()
}

fn add(decls: &mut HashMap<String, Vec<Rc<GoDecl>>>, name: &str, decl: GoDecl) {
    if name == "_" {
        return;
    }
    decls.entry(name.to_string()).or_default().push(Rc::new(decl));
}

pub fn read_go_scopes(code: String) -> GoScopeIndex {
    let b = code.as_bytes();
    let mut line_starts = vec![0usize];
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let (mut events, mut open, mut close, mut stack) = (Vec::new(), Vec::new(), HashMap::new(), Vec::<usize>::new());
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'{' => stack.push(i),
            b'}' => {
                if let Some(at) = stack.pop() {
                    close.insert(at, i);
                }
            }
            _ => continue,
        }
        events.push(i);
        open.push(stack.last().map_or(-1, |&x| x as isize));
    }
    let mut decls: HashMap<String, Vec<Rc<GoDecl>>> = HashMap::new();

    // `a, b := …`: every name left of a `:=`, and what its value is.
    let mut search = 0;
    while let Some(rel) = code[search..].find(":=") {
        let i = search + rel;
        search = i + 2;
        let mut names: Vec<(String, usize)> = Vec::new();
        let mut p = i;
        loop {
            while p > 0 && matches!(b[p - 1], b' ' | b'\t') {
                p -= 1;
            }
            let mut s = p;
            while s > 0 && is_go_ident_byte(b[s - 1]) {
                s -= 1;
            }
            if s == p || b[s].is_ascii_digit() || (s > 0 && b[s - 1] == b'.') {
                break;
            }
            names.push((code[s..p].to_string(), s));
            p = s;
            while p > 0 && matches!(b[p - 1], b' ' | b'\t') {
                p -= 1;
            }
            if p == 0 || b[p - 1] != b',' {
                break;
            }
            p -= 1;
        }
        let Some(&(_, first)) = names.last() else { continue };
        let lead = &code[code[..first].rfind('\n').map_or(0, |x| x + 1)..first];
        let kind = if re!(r"^\s*(?:\}\s*else\s+)?(?:if|for|switch)\b").is_match(lead) {
            GoDeclKind::Header
        } else if re!(r"^\s*case\b").is_match(lead) {
            GoDeclKind::Case
        } else {
            GoDeclKind::Local
        };
        let rhs = slice_bytes(&code, i + 2, i + 160);
        let literal = re!(r"^[ \t]*&?[ \t]*((?:[A-Za-z_]\w*\.)?[A-Za-z_]\w*)[ \t]*\{")
            .captures(rhs)
            .and_then(|c| c.text(1).map(String::from))
            .or_else(|| re!(r"^[ \t]*new\([ \t]*((?:[A-Za-z_]\w*\.)?[A-Za-z_]\w*)[ \t]*\)").captures(rhs).and_then(|c| c.text(1).map(String::from)));
        let ty = if names.len() == 1 { literal.filter(|l| !GO_TYPE_KEYWORDS.contains(l.as_str())) } else { None };
        let from = re!(r"^[ \t]*&?[ \t]*([A-Za-z_]\w*)\.[A-Za-z_]\w*[ \t]*[({]").captures(rhs).and_then(|c| c.text(1).map(String::from));
        let call = if from.is_some() {
            None
        } else {
            re!(r"^[ \t]*([A-Za-z_]\w*)[ \t]*(?:\[(?:[^[\]\n]|\[[^[\]\n]*\])*\][ \t]*)?\(").captures(rhs).and_then(|c| c.text(1).map(String::from))
        };
        let callee = call.filter(|c| !GO_PREDECLARED_CALLS.contains(c.as_str()));
        let n = names.len();
        for (r, (name, at)) in names.iter().enumerate() {
            add(
                &mut decls,
                name,
                GoDecl { at: *at, kind, body: None, ty: ty.clone(), from: from.clone(), callee: callee.clone(), value: Some(GoValueAt { at: i + 2, k: n - 1 - r, n }) },
            );
        }
    }

    // `var x T`, `var x = v`, `const ( … )` groups.
    for m in re!(r"(?<![\w.])(?:var|const)\b[ \t]*", "g").captures_iter(&code) {
        let start = m.get(0).map_or(0, |m| m.end());
        if at_byte(b, start) != b'(' {
            add_spec(&code, start, &mut decls);
            continue;
        }
        let end = go_closing(&code, start);
        add_spec(&code, start + 1, &mut decls);
        let mut depth = 0i32;
        for i in start + 1..end {
            match b[i] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b'\n' if depth == 0 => add_spec(&code, i + 1, &mut decls),
                _ => {}
            }
        }
    }

    // Parameters and results of every `func`, declared or literal.
    for m in re!(r"(?<![\w.])func\b", "g").captures_iter(&code) {
        let idx = m.get(0).map_or(0, |m| m.start());
        if re!(r"(?:[\]*)]|\bchan(?:[ \t]*<-)?)[ \t]*$").is_match(slice_bytes(&code, idx.saturating_sub(8), idx)) {
            continue;
        }
        let Some(sig) = go_signature(&code, idx + 4) else { continue };
        for &(from, to) in &sig.groups {
            for (name, at, ty) in go_param_names(&code, from, to) {
                add(&mut decls, &name, GoDecl { at, kind: GoDeclKind::Param, body: Some(sig.body), ty: Some(ty), from: None, callee: None, value: None });
            }
        }
    }
    GoScopeIndex { code, line_starts, events, open, close, decls, scopes: RefCell::new(HashMap::new()), generic: Cell::new(None) }
}

/// One `var` / `const` spec starting at `at`: `a, b T`, `a, b = v`.
fn add_spec(code: &str, at: usize, decls: &mut HashMap<String, Vec<Rc<GoDecl>>>) {
    let Some(rest) = code.get(at..) else { return };
    let Some(s) = re!(r"^[ \t]*([A-Za-z_]\w*(?:[ \t]*,[ \t]*[A-Za-z_]\w*)*)(?=[ \t]*(?:\n|$|=(?!=))|[ \t]+(?:[*[A-Za-z_(]|<-))").captures(rest) else { return };
    let whole = s.get(0).map_or(0, |m| m.end());
    let list = s.text(1).unwrap_or("");
    let ty = re!(r"^[ \t]+([^\s=;][^=;\n]*?)[ \t]*(?:[=;\n]|$)")
        .captures(slice_bytes(code, at + whole, at + whole + 160))
        .and_then(|c| c.text(1).map(String::from));
    let eq = if ty.is_none() { re!(r"^[ \t]*=(?!=)").find(slice_bytes(code, at + whole, at + whole + 40)).map(|m| m.end()) } else { None };
    let parts: Vec<&str> = list.split(',').collect();
    let mut p = at + whole - list.len();
    for (k, part) in parts.iter().enumerate() {
        let name = js_trim(part);
        let value = eq.map(|e| GoValueAt { at: at + whole + e, k, n: parts.len() });
        add(decls, name, GoDecl { at: p + part.find(name).unwrap_or(0), kind: GoDeclKind::Local, body: None, ty: ty.clone(), from: None, callee: None, value });
        p += part.len() + 1;
    }
}

/// The offset of the bracket that closes the one at `at` (the code's length when none does).
pub fn go_closing(code: &str, at: usize) -> usize {
    let b = code.as_bytes();
    let mut depth = 0i32;
    for (i, &c) in b.iter().enumerate().skip(at) {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    code.len()
}

pub struct GoSignature {
    pub groups: Vec<(usize, usize)>,
    pub body: usize,
}

fn skip_blanks(b: &[u8], p: &mut usize) {
    while matches!(at_byte(b, *p), b' ' | b'\t') {
        *p += 1;
    }
}

fn sig_group(code: &str, groups: &mut Vec<(usize, usize)>, p: &mut usize) -> bool {
    let b = code.as_bytes();
    if at_byte(b, *p) != b'(' {
        return false;
    }
    let end = go_closing(code, *p);
    groups.push((*p + 1, end));
    *p = end + 1;
    skip_blanks(b, p);
    true
}

/// A function name (and its type parameters) followed by `(`: positions `p` on that `(`.
fn sig_name(code: &str, p: &mut usize) -> bool {
    let b = code.as_bytes();
    let Some(m) = re!(r"^[A-Za-z_]\w*").find(slice_bytes(code, *p, *p + 128)) else { return false };
    if GO_TYPE_KEYWORDS.contains(m.as_str()) {
        return false;
    }
    let mut q = *p + m.end();
    skip_blanks(b, &mut q);
    if at_byte(b, q) == b'[' {
        q = go_closing(code, q) + 1;
    }
    skip_blanks(b, &mut q);
    if at_byte(b, q) != b'(' {
        return false;
    }
    *p = q;
    true
}

/// The parameter / result groups of a function whose signature starts at `at` (just after
/// `func`), and the offset of its body's `{`.
pub fn go_signature(code: &str, at: usize) -> Option<GoSignature> {
    let b = code.as_bytes();
    let mut groups = Vec::new();
    let mut p = at;
    skip_blanks(b, &mut p);
    if at_byte(b, p) != b'(' {
        if !sig_name(code, &mut p) || !sig_group(code, &mut groups, &mut p) {
            return None;
        }
    } else {
        sig_group(code, &mut groups, &mut p);
        if sig_name(code, &mut p) && !sig_group(code, &mut groups, &mut p) {
            return None;
        }
    }
    if at_byte(b, p) == b'(' {
        sig_group(code, &mut groups, &mut p);
    }
    let mut depth = 0i32;
    while p < b.len() {
        let c = b[p];
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            b'{' => {
                if depth == 0 {
                    if re!(r"(?:struct|interface)\s*$").is_match(slice_bytes(code, p.saturating_sub(12), p)) {
                        p = go_closing(code, p);
                    } else {
                        return Some(GoSignature { groups, body: p });
                    }
                }
            }
            b'\n' | b',' | b';' | b'=' | b'}' if depth == 0 => return None,
            _ => {}
        }
        p += 1;
    }
    None
}

/// The parameter names of one group, with their offsets and written types: `a, b int, c string`.
pub fn go_param_names(code: &str, from: usize, to: usize) -> Vec<(String, usize, String)> {
    let b = code.as_bytes();
    let mut parts: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0i32;
    let mut start = from;
    for i in from..to.min(b.len()) {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                parts.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push((start, to));
    let mut names = Vec::new();
    let mut grouped: Vec<(String, usize)> = Vec::new();
    for (s, e) in parts {
        let text = slice_bytes(code, s, e);
        let Some(m) = re!(r"^\s*([A-Za-z_]\w*)").captures(text) else { continue };
        let name = m.text(1).unwrap_or("");
        if GO_TYPE_KEYWORDS.contains(name) {
            continue;
        }
        let whole = m.get(0).map_or(0, |m| m.end());
        let rest = &text[whole..];
        // `pkg.Type` of an unnamed parameter.
        if re!(r"^\s*\.(?!\.\.)").is_match(rest) {
            continue;
        }
        let at = s + whole - name.len();
        if re!(r"^\s*$").is_match(rest) {
            grouped.push((name.to_string(), at));
            continue;
        }
        let ty = js_trim(rest).to_string();
        for (n, nat) in grouped.drain(..) {
            names.push((n, nat, ty.clone()));
        }
        names.push((name.to_string(), at, ty));
    }
    names
}

/// The innermost `{ … }` block containing `offset`.
pub fn go_block_at(index: &GoScopeIndex, offset: usize) -> Option<(usize, usize)> {
    let at = index.events.partition_point(|&e| e < offset);
    if at == 0 {
        return None;
    }
    let open = index.open[at - 1];
    if open < 0 {
        return None;
    }
    let open = open as usize;
    Some((open, index.close.get(&open).copied().unwrap_or(index.code.len())))
}

/// The end of the statement starting at `at`: a newline or `;` at bracket depth 0.
pub fn go_statement_end(code: &str, at: usize) -> usize {
    let b = code.as_bytes();
    let mut depth = 0i32;
    for (i, &c) in b.iter().enumerate().skip(at) {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth < 0 {
                    return i;
                }
            }
            b'\n' | b';' if depth == 0 => return i,
            _ => {}
        }
    }
    code.len()
}

/// The blocks an `if` / `for` / `switch` header's `:=` is visible in: the header's own block
/// and the `else` blocks that chain on.
fn go_header_blocks(index: &GoScopeIndex, at: usize) -> Vec<(usize, usize)> {
    let code = &index.code;
    let b = code.as_bytes();
    let mut blocks = Vec::new();
    let mut depth = 0i32;
    let mut i = at;
    while i < b.len() {
        match b[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'}' => break,
            b'{' => {
                let end = index.close.get(&i).copied().unwrap_or(code.len());
                if depth != 0 || !re!(r"^[ \t]*(?:\n|$)").is_match(slice_bytes(code, i + 1, i + 80)) {
                    i = end;
                } else {
                    blocks.push((i, end));
                    let Some(chained) = re!(r"^[ \t]*else\b[ \t]*").find(slice_bytes(code, end + 1, end + 40)) else { break };
                    i = end + chained.end();
                    depth = 0;
                }
            }
            _ => {}
        }
        i += 1;
    }
    blocks
}

/// The byte ranges `name`'s declarations are visible in.
fn go_scopes_of(index: &GoScopeIndex, name: &str) -> Rc<Vec<Scope>> {
    if let Some(hit) = index.scopes.borrow().get(name) {
        return Rc::clone(hit);
    }
    let mut scopes: Vec<Scope> = Vec::new();
    for d in index.decls.get(name).map(|v| v.as_slice()).unwrap_or(&[]) {
        if d.kind == GoDeclKind::Param {
            let body = d.body.unwrap_or(0);
            scopes.push((body, index.close.get(&body).copied().unwrap_or(index.code.len()), Rc::clone(d)));
            continue;
        }
        let Some(block) = go_block_at(index, d.at) else { continue };
        if d.kind == GoDeclKind::Header {
            for (start, end) in go_header_blocks(index, d.at) {
                scopes.push((start, end, Rc::clone(d)));
            }
            continue;
        }
        let from = go_statement_end(&index.code, d.at);
        let mut to = block.1;
        // A `case` clause's declarations end where the next clause of the same switch begins.
        if from <= index.code.len() {
            for m in re!(r"\n[ \t]*(?:case\b|default[ \t]*:)", "g").captures_iter(&index.code[from..]) {
                let idx = from + m.get(0).map_or(0, |m| m.start());
                if idx >= to {
                    break;
                }
                if go_block_at(index, idx + 1).map(|b| b.0) == Some(block.0) {
                    to = idx;
                    break;
                }
            }
        }
        if from < to {
            scopes.push((from, to, Rc::clone(d)));
        }
    }
    let scopes = Rc::new(scopes);
    index.scopes.borrow_mut().insert(name.to_string(), Rc::clone(&scopes));
    scopes
}

/// The source with comments and string / rune / raw-string contents replaced by spaces (newlines
/// kept), so offsets are unchanged and a brace or `:=` in either never counts.
pub fn blank_go_code(src: &str) -> String {
    let b = src.as_bytes();
    let mut out: Vec<u8> = b.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in out.iter_mut().take(to.min(b.len())).skip(from) {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'/' => {
                let d = at_byte(b, i + 1);
                if d == b'/' {
                    let end = src[i..].find('\n').map_or(src.len(), |x| i + x);
                    blank(&mut out, i, end);
                    i = end;
                } else if d == b'*' {
                    let end = src[i + 2..].find("*/").map_or(src.len(), |x| i + 2 + x + 2);
                    blank(&mut out, i, end);
                    i = end;
                } else {
                    i += 1;
                }
            }
            b'`' => {
                let end = src[i + 1..].find('`').map_or(src.len(), |x| i + 1 + x);
                blank(&mut out, i + 1, end);
                i = end + 1;
            }
            b'"' | b'\'' => {
                let mut j = i + 1;
                while j < b.len() {
                    let d = b[j];
                    if d == b'\\' {
                        j += 2;
                        while j < b.len() && !src.is_char_boundary(j) {
                            j += 1;
                        }
                        continue;
                    }
                    if d == c || d == b'\n' {
                        break;
                    }
                    j += 1;
                }
                let end = j.min(b.len());
                blank(&mut out, i + 1, end);
                i = end + 1;
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanking_keeps_offsets_and_hides_comments_and_strings() {
        let src = "a := \"x{\" // {\n/* } */ b := `y\n}`\n";
        let blank = blank_go_code(src);
        assert_eq!(blank.len(), src.len());
        assert!(!blank.contains('{') && !blank.contains('}'));
        assert!(blank.contains("a :=") && blank.contains("b :="));
        assert_eq!(blank.matches('\n').count(), src.matches('\n').count());
    }

    #[test]
    fn a_short_declaration_is_visible_to_the_end_of_its_block() {
        let code = "func f() {\n\tx := T{}\n\tx.Run()\n}\nfunc g() {\n\tx.Run()\n}\n".to_string();
        let index = read_go_scopes(blank_go_code(&code));
        let inside = code.find("x.Run").unwrap();
        let decl = go_local_decl_at(&index, "x", inside).expect("declared in f");
        assert_eq!(decl.ty.as_deref(), Some("T"));
        let outside = code.rfind("x.Run").unwrap();
        assert!(go_local_decl_at(&index, "x", outside).is_none());
    }

    #[test]
    fn parameters_are_visible_in_the_function_body_with_their_types() {
        let code = "func (s *Server) Handle(w http.ResponseWriter, a, b int) {\n\tw.Write(nil)\n}\n";
        let index = read_go_scopes(blank_go_code(code));
        let at = code.find("w.Write").unwrap();
        assert_eq!(go_local_decl_at(&index, "w", at).and_then(|d| d.ty.clone()).as_deref(), Some("http.ResponseWriter"));
        assert_eq!(go_local_decl_at(&index, "a", at).and_then(|d| d.ty.clone()).as_deref(), Some("int"));
        assert_eq!(go_local_decl_at(&index, "s", at).and_then(|d| d.ty.clone()).as_deref(), Some("*Server"));
    }
}
