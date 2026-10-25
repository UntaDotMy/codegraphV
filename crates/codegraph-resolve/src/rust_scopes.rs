//! Rust lexical scopes — port of `src/resolution/rust-scopes.ts`.
//!
//! Functions, impl/trait blocks and the local bindings (parameters, closures, `let`, `if let`,
//! `for`, `match` arms) of a Rust file, read from the source with a bracket matcher. Offsets are
//! UTF-16 indices: the source is turned into a string with one byte per UTF-16 unit, a non-ASCII
//! unit becoming `\x01` (a BMP character), `\x02`/`\x03` (a surrogate pair) or a blank for
//! whitespace — non-ASCII text that survives blanking (identifiers) is not recoverable.

use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::is_js_space;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RustPatternSite {
    pub path: String,
    pub index: Option<usize>,
    pub field: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    Param,
    Closure,
    Let,
    Cond,
    For,
    Arm,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RustBinding {
    pub at: usize,
    pub kind: BindingKind,
    pub from: usize,
    pub to: usize,
    pub ty: Option<String>,
    pub init: Option<String>,
    pub site: Option<RustPatternSite>,
    pub scrutinee: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RustFn {
    pub at: usize,
    pub generics: String,
    pub where_: String,
    pub from: usize,
    pub to: usize,
    pub params_end: usize,
    pub body: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplKind {
    Impl,
    Trait,
}

#[derive(Debug, Clone)]
pub struct RustImplBlock {
    pub at: usize,
    pub kind: ImplKind,
    pub generics: String,
    pub where_: String,
    pub from: usize,
    pub to: usize,
}

pub struct RustScopes {
    pub code: String,
    pub line_starts: Vec<usize>,
    pub fns: Vec<RustFn>,
    pub impls: Vec<RustImplBlock>,
    pub close: HashMap<usize, usize>,
    pub events: Vec<usize>,
    pub open_after: Vec<isize>,
    bindings: RefCell<Option<Rc<HashMap<String, Vec<RustBinding>>>>>,
}

/// One byte per UTF-16 unit (see the module comment).
pub fn rust_units(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else if is_js_space(c) {
            for _ in 0..c.len_utf16() {
                out.push(' ');
            }
        } else if c.len_utf16() == 2 {
            out.push('\u{2}');
            out.push('\u{3}');
        } else {
            out.push('\u{1}');
        }
    }
    out
}

fn at(code: &str, i: usize) -> u8 {
    code.as_bytes().get(i).copied().unwrap_or(0)
}

fn prev(code: &str, i: usize) -> u8 {
    if i == 0 {
        0
    } else {
        at(code, i - 1)
    }
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || matches!(c, 1..=3)
}

fn slice(code: &str, from: usize, to: usize) -> &str {
    let to = to.min(code.len());
    if from >= to {
        ""
    } else {
        &code[from..to]
    }
}

fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c == ' ' || ('\t'..='\r').contains(&c))
}

/// Blank comments, string contents and char-literal contents, keeping offsets and newlines.
pub fn blank_rust_code(src: &str) -> String {
    let n = src.len();
    let bytes = src.as_bytes();
    let mut parts: Vec<String> = Vec::new();
    let mut last = 0usize;
    let blank = |from: usize, to: usize, parts: &mut Vec<String>, last: &mut usize| {
        if to <= from {
            return;
        }
        parts.push(slice(src, *last, from).to_string());
        let chunk = &src[from..to];
        if chunk.contains('\n') {
            parts.push(chunk.chars().map(|c| if c == '\n' { '\n' } else { ' ' }).collect());
        } else {
            parts.push(" ".repeat(to - from));
        }
        *last = to;
    };
    let mut i = 0usize;
    while i < n {
        let c = bytes[i];
        if c != b'/' && c != b'"' && c != b'\'' {
            i += 1;
            continue;
        }
        let d = at(src, i + 1);
        if c == b'/' {
            if d == b'/' {
                let end = src[i..].find('\n').map(|p| p + i);
                let to = end.unwrap_or(n);
                blank(i, to, &mut parts, &mut last);
                i = to;
                continue;
            } else if d == b'*' {
                let mut depth = 1;
                let mut j = i + 2;
                while j < n && depth > 0 {
                    if bytes[j] == b'/' && at(src, j + 1) == b'*' {
                        depth += 1;
                        j += 2;
                    } else if bytes[j] == b'*' && at(src, j + 1) == b'/' {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                blank(i, j, &mut parts, &mut last);
                i = j;
                continue;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            let mut k = i as isize - 1;
            while k >= 0 && bytes[k as usize] == b'#' {
                k -= 1;
            }
            let mut p = k - 1;
            if p >= 0 && (bytes[p as usize] == b'b' || bytes[p as usize] == b'c') {
                p -= 1;
            }
            if k >= 0 && bytes[k as usize] == b'r' && (p < 0 || !is_ident_char(bytes[p as usize])) {
                let close = format!("\"{}", "#".repeat((i as isize - 1 - k) as usize));
                let end = src[(i + 1).min(n)..].find(&close).map(|p| p + i + 1);
                let to = end.unwrap_or(n);
                blank(i + 1, to, &mut parts, &mut last);
                i = n.min(to + close.len());
                continue;
            }
            let mut j = i + 1;
            while j < n && bytes[j] != b'"' {
                j += if bytes[j] == b'\\' { 2 } else { 1 };
            }
            blank(i + 1, j.min(n), &mut parts, &mut last);
            i = n.min(j + 1);
            continue;
        }
        // `'`: a char literal, or a lifetime (left alone).
        if d == b'\\' {
            let end = src[(i + 3).min(n)..].find('\'').map(|p| p + i + 3);
            if let Some(end) = end.filter(|&e| e > 0 && e - i <= 12) {
                blank(i + 1, end, &mut parts, &mut last);
                i = end + 1;
                continue;
            }
        } else if at(src, i + 2) == b'\'' && d != 0 && d != b'\n' {
            blank(i + 1, i + 2, &mut parts, &mut last);
            i += 3;
            continue;
        } else if d == 2 && at(src, i + 3) == b'\'' {
            blank(i + 1, i + 3, &mut parts, &mut last);
            i += 4;
            continue;
        }
        i += 1;
    }
    parts.push(slice(src, last, n).to_string());
    parts.concat()
}

fn pair_brackets(code: &str) -> HashMap<usize, usize> {
    let mut close = HashMap::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, c) in code.bytes().enumerate() {
        match c {
            b'(' | b'[' | b'{' => stack.push(i),
            b')' | b']' | b'}' => {
                let want = match c {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                if let Some(&top) = stack.last() {
                    if at(code, top) == want {
                        close.insert(stack.pop().unwrap(), i);
                    }
                }
            }
            _ => {}
        }
    }
    close
}

fn skip_space(code: &str, mut i: usize) -> usize {
    loop {
        match at(code, i) {
            32 | 10 | 13 | 9 | 12 | 11 => i += 1,
            _ => return i,
        }
    }
}

fn ident_at(code: &str, i: usize) -> Option<&str> {
    let c = at(code, i);
    if !(c.is_ascii_alphabetic() || c == b'_') {
        return None;
    }
    let mut j = i + 1;
    while j < code.len() && (code.as_bytes()[j].is_ascii_alphanumeric() || code.as_bytes()[j] == b'_') {
        j += 1;
    }
    Some(&code[i..j])
}

fn angle_close(code: &str, from: usize, limit: usize) -> Option<usize> {
    let mut depth = 0i32;
    for i in from..limit.min(code.len()) {
        let c = at(code, i);
        if c == b'<' {
            depth += 1;
        } else if c == b'>' && prev(code, i) != b'-' && prev(code, i) != b'=' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        } else if c == b';' || c == b'{' || c == b'}' {
            return None;
        }
    }
    None
}

fn walk_top(code: &str, from: usize, to: usize, close: &HashMap<usize, usize>, angles: bool, stop: &dyn Fn(usize) -> bool) -> usize {
    let mut depth = 0i32;
    let mut i = from;
    while i < to {
        let c = at(code, i);
        if depth == 0 && stop(i) {
            return i;
        }
        if c == b'(' || c == b'[' || c == b'{' {
            match close.get(&i) {
                Some(&end) if end < to => {
                    i = end + 1;
                    continue;
                }
                _ => return to,
            }
        }
        if angles && c == b'<' {
            depth += 1;
        } else if angles && c == b'>' && prev(code, i) != b'-' && prev(code, i) != b'=' && depth > 0 {
            depth -= 1;
        }
        i += 1;
    }
    to
}

fn split_top(code: &str, from: usize, to: usize, close: &HashMap<usize, usize>, angles: bool) -> Vec<(usize, usize)> {
    let mut parts = Vec::new();
    let mut start = from;
    loop {
        let comma = walk_top(code, start, to, close, angles, &|i| at(code, i) == b',');
        if comma >= to {
            break;
        }
        parts.push((start, comma));
        start = comma + 1;
    }
    if !trim(slice(code, start, to)).is_empty() {
        parts.push((start, to));
    }
    parts
}

fn top_colon(code: &str, from: usize, to: usize, close: &HashMap<usize, usize>) -> Option<usize> {
    let p = walk_top(code, from, to, close, true, &|i| at(code, i) == b':' && at(code, i + 1) != b':' && prev(code, i) != b':');
    if p < to {
        Some(p)
    } else {
        None
    }
}

fn word_at(code: &str, i: usize, word: &str) -> bool {
    code[i.min(code.len())..].starts_with(word) && !is_ident_char(prev(code, i)) && !is_ident_char(at(code, i + word.len()))
}

fn is_pattern_word(id: &str) -> bool {
    matches!(id, "ref" | "mut" | "box" | "true" | "false" | "self" | "Self" | "crate" | "super" | "_")
}

fn is_binding_name(id: &str) -> bool {
    re!(r"^[a-z_][A-Za-z0-9_]*$").is_match(id) && !is_pattern_word(id)
}

fn read_path(code: &str, at_: usize) -> (Vec<String>, usize) {
    let mut segments: Vec<String> = Vec::new();
    let mut p = at_;
    loop {
        let Some(id) = ident_at(code, p) else { break };
        segments.push(id.to_string());
        p += id.len();
        let mut q = skip_space(code, p);
        if !code[q.min(code.len())..].starts_with("::") {
            break;
        }
        q = skip_space(code, q + 2);
        if at(code, q) == b'<' {
            let Some(end) = angle_close(code, q, code.len()) else { break };
            p = end + 1;
            q = skip_space(code, p);
            if !code[q.min(code.len())..].starts_with("::") {
                break;
            }
            q = skip_space(code, q + 2);
        }
        p = q;
    }
    (segments, p)
}

type Emit<'a> = &'a mut dyn FnMut(&str, usize, Option<RustPatternSite>);

fn walk_pattern(code: &str, from: usize, to: usize, close: &HashMap<usize, usize>, site: Option<RustPatternSite>, emit: Emit) {
    let mut p = from;
    while p < to {
        let c = at(code, p);
        if c == b'(' || c == b'[' {
            let end = close.get(&p).copied().unwrap_or(to).min(to);
            for (a, b) in split_top(code, p + 1, end, close, false) {
                walk_pattern(code, a, b, close, None, emit);
            }
            p = end + 1;
            continue;
        }
        if !(c.is_ascii_alphabetic() || c == b'_') || is_ident_char(prev(code, p)) {
            p += 1;
            continue;
        }
        let (segments, path_end) = read_path(code, p);
        if segments.is_empty() {
            p += 1;
            continue;
        }
        let after = skip_space(code, path_end);
        let written = segments.join("::");
        if at(code, after) == b'(' {
            let end = close.get(&after).copied().unwrap_or(to).min(to);
            let mut index = 0;
            for (a, b) in split_top(code, after + 1, end, close, false) {
                walk_pattern(code, a, b, close, Some(RustPatternSite { path: written.clone(), index: Some(index), field: None }), emit);
                index += 1;
            }
            p = end + 1;
            continue;
        }
        if at(code, after) == b'{' {
            let end = close.get(&after).copied().unwrap_or(to).min(to);
            for (a, b) in split_top(code, after + 1, end, close, false) {
                if let Some(colon) = top_colon(code, a, b, close) {
                    let field = trim(slice(code, a, colon));
                    if re!(r"^\w+$").is_match(field) {
                        walk_pattern(code, colon + 1, b, close, Some(RustPatternSite { path: written.clone(), index: None, field: Some(field.to_string()) }), emit);
                    }
                    continue;
                }
                let text = slice(code, a, b);
                if let Some(short) = re!(r"^\s*(?:ref\s+)?(?:mut\s+)?([A-Za-z_]\w*)\s*$").captures(text) {
                    let name = short.text(1).unwrap();
                    if is_binding_name(name) {
                        emit(name, a + text.rfind(name).unwrap_or(0), Some(RustPatternSite { path: written.clone(), index: None, field: Some(name.to_string()) }));
                    }
                }
            }
            p = end + 1;
            continue;
        }
        if segments.len() == 1 && is_binding_name(&segments[0]) {
            emit(&segments[0], p, site.clone());
        }
        p = if at(code, after) == b'@' { after + 1 } else { path_end };
    }
}

fn block_at(events: &[usize], open: &[isize], close: &HashMap<usize, usize>, code_length: usize, offset: usize) -> Option<(usize, usize)> {
    // The last event strictly before `offset`.
    let idx = events.partition_point(|&e| e < offset);
    if idx == 0 {
        return None;
    }
    let o = open[idx - 1];
    if o < 0 {
        None
    } else {
        Some((o as usize, close.get(&(o as usize)).copied().unwrap_or(code_length)))
    }
}

fn opens_closure(code: &str, i: usize) -> bool {
    let bytes = code.as_bytes();
    let mut k = i as isize - 1;
    while k >= 0 && matches!(bytes[k as usize], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        k -= 1;
    }
    if k < 0 {
        return true;
    }
    let c = bytes[k as usize];
    let ku = k as usize;
    if c == b':' {
        return prev(code, ku) != b':';
    }
    if b"([{,=;".contains(&c) {
        return true;
    }
    if c == b'>' {
        return prev(code, ku) == b'=';
    }
    let from = ku.saturating_sub(8);
    let word = re!(r"([A-Za-z_]\w*)$").captures(slice(code, from, ku + 1)).and_then(|c| c.text(1).map(str::to_string));
    matches!(word.as_deref(), Some("move" | "return" | "async" | "static"))
}

fn expression_end(code: &str, from: usize, close: &HashMap<usize, usize>) -> usize {
    let mut i = from;
    while i < code.len() {
        let c = at(code, i);
        if c == b'(' || c == b'[' || c == b'{' {
            match close.get(&i) {
                None => return code.len(),
                Some(&end) => {
                    i = end + 1;
                    continue;
                }
            }
        }
        if matches!(c, b',' | b';' | b')' | b']' | b'}') {
            return i;
        }
        i += 1;
    }
    code.len()
}

/// Read the scopes of Rust `source`.
pub fn read_rust_scopes(source: &str) -> RustScopes {
    let code = blank_rust_code(&rust_units(source));
    let n = code.len();
    let mut line_starts = vec![0usize];
    for (i, c) in code.bytes().enumerate() {
        if c == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let close = pair_brackets(&code);
    let mut fns: Vec<RustFn> = Vec::new();
    for m in re!(r"\bfn\b", "g").captures_iter(&code) {
        let idx = m.get(0).unwrap().start();
        let mut p = skip_space(&code, idx + 2);
        let Some(name) = ident_at(&code, p) else { continue };
        p = skip_space(&code, p + name.len());
        let mut generics = String::new();
        if at(&code, p) == b'<' {
            let Some(end) = angle_close(&code, p, n) else { continue };
            generics = slice(&code, p + 1, end).to_string();
            p = skip_space(&code, end + 1);
        }
        if at(&code, p) != b'(' {
            continue;
        }
        let Some(&params_end) = close.get(&p) else { continue };
        let mut body: isize = -1;
        let mut where_: isize = -1;
        let mut q = params_end + 1;
        while q < n {
            let c = at(&code, q);
            if c == b'{' {
                body = q as isize;
                break;
            }
            if c == b';' || c == b'}' {
                break;
            }
            if c == b'(' || c == b'[' {
                q = close.get(&q).copied().unwrap_or(n) + 1;
                continue;
            }
            if where_ < 0 && c == b'w' && word_at(&code, q, "where") {
                where_ = q as isize + 5;
            }
            q += 1;
        }
        if body < 0 {
            continue;
        }
        let body = body as usize;
        fns.push(RustFn {
            at: idx,
            generics,
            where_: if where_ < 0 { String::new() } else { slice(&code, where_ as usize, body).to_string() },
            from: p,
            to: close.get(&body).copied().unwrap_or(n),
            params_end,
            body,
        });
    }
    let mut impls: Vec<RustImplBlock> = Vec::new();
    for m in re!(r"\b(impl|trait)\b", "g").captures_iter(&code) {
        let idx = m.get(0).unwrap().start();
        let kind = if m.text(1) == Some("impl") { ImplKind::Impl } else { ImplKind::Trait };
        let kind_len = if kind == ImplKind::Impl { 4 } else { 5 };
        if kind == ImplKind::Impl {
            let mut k = idx as isize - 1;
            while k >= 0 && matches!(at(&code, k as usize), b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                k -= 1;
            }
            if k >= 0 {
                let ku = k as usize;
                if !b"{};]".contains(&at(&code, ku)) && !re!(r"\b(?:unsafe|default)$").is_match(slice(&code, ku.saturating_sub(7), ku + 1)) {
                    continue;
                }
            }
        }
        let mut p = skip_space(&code, idx + kind_len);
        if kind == ImplKind::Trait {
            let Some(name) = ident_at(&code, p) else { continue };
            p = skip_space(&code, p + name.len());
        }
        let mut generics = String::new();
        if at(&code, p) == b'<' {
            let Some(end) = angle_close(&code, p, n) else { continue };
            generics = slice(&code, p + 1, end).to_string();
            p = end + 1;
        }
        let mut where_: isize = -1;
        let mut open: isize = -1;
        let mut q = p;
        while q < n {
            let c = at(&code, q);
            if c == b'{' {
                open = q as isize;
                break;
            }
            if c == b';' || c == b'}' {
                break;
            }
            if c == b'(' || c == b'[' {
                q = close.get(&q).copied().unwrap_or(n) + 1;
                continue;
            }
            if where_ < 0 && c == b'w' && word_at(&code, q, "where") {
                where_ = q as isize + 5;
            }
            q += 1;
        }
        if open < 0 {
            continue;
        }
        let open = open as usize;
        impls.push(RustImplBlock {
            at: idx,
            kind,
            generics,
            where_: if where_ < 0 { String::new() } else { slice(&code, where_ as usize, open).to_string() },
            from: open,
            to: close.get(&open).copied().unwrap_or(n),
        });
    }
    let mut events: Vec<usize> = Vec::new();
    let mut open_after: Vec<isize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, c) in code.bytes().enumerate() {
        if c == b'{' {
            stack.push(i);
        } else if c == b'}' {
            stack.pop();
        } else {
            continue;
        }
        events.push(i);
        open_after.push(stack.last().map_or(-1, |&s| s as isize));
    }
    RustScopes { code, line_starts, fns, impls, close, events, open_after, bindings: RefCell::new(None) }
}

/// The innermost `{…}` block around `offset`, as `(open, close)`.
pub fn rust_inner_block_at(scopes: &RustScopes, offset: usize) -> Option<(usize, usize)> {
    block_at(&scopes.events, &scopes.open_after, &scopes.close, scopes.code.len(), offset)
}

type Bindings = HashMap<String, Vec<RustBinding>>;

fn add(bindings: &mut Bindings, name: &str, binding: RustBinding) {
    bindings.entry(name.to_string()).or_default().push(binding);
}

#[allow(clippy::too_many_arguments)]
fn bind_pattern(
    code: &str,
    close: &HashMap<usize, usize>,
    bindings: &mut Bindings,
    from: usize,
    to: usize,
    kind: BindingKind,
    scope_from: usize,
    scope_to: usize,
    scrutinee: Option<&str>,
) {
    let emit_fn = |bindings: &mut Bindings, name: &str, at_: usize, site: Option<RustPatternSite>| {
        add(bindings, name, RustBinding { at: at_, kind, from: scope_from, to: scope_to, ty: None, init: None, site, scrutinee: scrutinee.map(str::to_string) });
    };
    let open = skip_space(code, from);
    let end = if at(code, open) == b'(' { close.get(&open).copied() } else { None };
    if let Some(end) = end.filter(|&e| e < to && skip_space(code, e + 1) >= to) {
        let mut index = 0;
        for (a, b) in split_top(code, open + 1, end, close, false) {
            let text = slice(code, a, b);
            let simple = re!(r"^\s*(?:ref\s+)?(?:mut\s+)?([A-Za-z_]\w*)\s*$").captures(text);
            match simple.as_ref().and_then(|s| s.text(1)).filter(|n| is_binding_name(n)) {
                Some(name) => emit_fn(bindings, name, a + text.rfind(name).unwrap_or(0), Some(RustPatternSite { path: String::new(), index: Some(index), field: None })),
                None => {
                    let mut emit = |name: &str, at_: usize, site: Option<RustPatternSite>| emit_fn(bindings, name, at_, site);
                    walk_pattern(code, a, b, close, None, &mut emit);
                }
            }
            index += 1;
        }
        return;
    }
    let mut emit = |name: &str, at_: usize, site: Option<RustPatternSite>| emit_fn(bindings, name, at_, site);
    walk_pattern(code, from, to, close, None, &mut emit);
}

#[allow(clippy::too_many_arguments)]
fn param(code: &str, close: &HashMap<usize, usize>, bindings: &mut Bindings, from: usize, to: usize, kind: BindingKind, scope_from: usize, scope_to: usize) {
    let colon = top_colon(code, from, to, close);
    let pattern = slice(code, from, colon.unwrap_or(to));
    if let Some(simple) = re!(r"^\s*(?:mut\s+)?([A-Za-z_]\w*)\s*$").captures(pattern) {
        let name = simple.text(1).unwrap();
        if is_binding_name(name) {
            add(
                bindings,
                name,
                RustBinding {
                    at: from + pattern.rfind(name).unwrap_or(0),
                    kind,
                    from: scope_from,
                    to: scope_to,
                    ty: colon.map(|c| trim(slice(code, c + 1, to)).to_string()),
                    init: None,
                    site: None,
                    scrutinee: None,
                },
            );
        }
        return;
    }
    bind_pattern(code, close, bindings, from, colon.unwrap_or(to), kind, scope_from, scope_to, None);
}

fn read_rust_bindings(scopes: &RustScopes) -> Bindings {
    let code = scopes.code.as_str();
    let close = &scopes.close;
    let n = code.len();
    let mut bindings: Bindings = HashMap::new();
    for f in &scopes.fns {
        for (a, b) in split_top(code, f.from + 1, f.params_end, close, true) {
            param(code, close, &mut bindings, a, b, BindingKind::Param, f.body, f.to);
        }
    }
    // Closures: `|a, b: T| body`.
    let mut i = code.find('|');
    while let Some(start) = i {
        let mut cur = start;
        'one: {
            if at(code, cur + 1) == b'|' {
                cur += 1;
                break 'one;
            }
            if at(code, cur + 1) == b'=' || !opens_closure(code, cur) {
                break 'one;
            }
            let mut end: isize = -1;
            let mut depth = 0i32;
            let mut j = cur + 1;
            while j < n && j < cur + 2000 {
                let c = at(code, j);
                if c == b'(' || c == b'[' || c == b'{' {
                    match close.get(&j) {
                        None => break,
                        Some(&e) => {
                            j = e + 1;
                            continue;
                        }
                    }
                }
                if c == b'<' {
                    depth += 1;
                } else if c == b'>' && prev(code, j) != b'-' && prev(code, j) != b'=' {
                    depth -= 1;
                } else if c == b'|' && depth <= 0 {
                    end = j as isize;
                    break;
                } else if matches!(c, b';' | b')' | b']' | b'}') {
                    break;
                }
                j += 1;
            }
            if end < 0 {
                break 'one;
            }
            let end = end as usize;
            let b = skip_space(code, end + 1);
            let body_end = if code[b.min(n)..].starts_with("->") {
                match code[b..].find('{') {
                    None => n,
                    Some(p) => close.get(&(b + p)).copied().unwrap_or(n),
                }
            } else if at(code, b) == b'{' {
                close.get(&b).copied().unwrap_or(n)
            } else {
                expression_end(code, b, close)
            };
            for (s, e) in split_top(code, cur + 1, end, close, true) {
                param(code, close, &mut bindings, s, e, BindingKind::Closure, end + 1, body_end);
            }
            cur = end;
        }
        i = code[(cur + 1).min(n)..].find('|').map(|p| p + cur + 1);
    }
    // `let` statements and `if let` / `while let` conditions.
    for m in re!(r"\blet\b", "g").captures_iter(code) {
        let at_ = m.get(0).unwrap().start();
        let cond = re!(r"(?:\bif|\bwhile|&&)\s*$").is_match(slice(code, at_.saturating_sub(16), at_));
        let pat_from = at_ + 3;
        let pat_end = walk_top(code, pat_from, n, close, false, &|i| {
            let c = at(code, i);
            if c == b';' || c == b'}' {
                return true;
            }
            if c == b':' {
                return at(code, i + 1) != b':' && prev(code, i) != b':';
            }
            c == b'=' && at(code, i + 1) != b'=' && at(code, i + 1) != b'>' && !b"=!<>".contains(&prev(code, i))
        });
        if pat_end >= n {
            continue;
        }
        let mut ty: Option<String> = None;
        let mut eq: isize = if at(code, pat_end) == b'=' { pat_end as isize } else { -1 };
        if at(code, pat_end) == b':' {
            let type_end = walk_top(code, pat_end + 1, n, close, true, &|i| {
                let c = at(code, i);
                c == b';' || c == b'}' || (c == b'=' && at(code, i + 1) != b'=' && at(code, i + 1) != b'>' && !b"=!<>".contains(&prev(code, i)))
            });
            if type_end >= n {
                continue;
            }
            ty = Some(trim(slice(code, pat_end + 1, type_end)).to_string());
            if at(code, type_end) == b'=' {
                eq = type_end as isize;
            }
        }
        let pattern_text = slice(code, pat_from, pat_end);
        let simple = re!(r"^\s*(?:ref\s+)?(?:mut\s+)?([A-Za-z_]\w*)\s*$").captures(pattern_text).and_then(|c| c.text(1).map(str::to_string));
        if cond {
            if eq < 0 {
                continue;
            }
            let eq = eq as usize;
            let value_end = walk_top(code, eq + 1, n, close, false, &|i| at(code, i) == b'{' || at(code, i) == b';' || (at(code, i) == b'&' && at(code, i + 1) == b'&'));
            let scrutinee = trim(slice(code, eq + 1, value_end)).to_string();
            let open = if at(code, value_end) == b'{' {
                value_end
            } else {
                walk_top(code, value_end, n, close, false, &|i| at(code, i) == b'{' || at(code, i) == b';')
            };
            if at(code, open) != b'{' {
                continue;
            }
            bind_pattern(code, close, &mut bindings, pat_from, pat_end, BindingKind::Cond, value_end, close.get(&open).copied().unwrap_or(n), Some(&scrutinee));
            continue;
        }
        let init_end;
        let stmt_end;
        if eq >= 0 {
            let eq = eq as usize;
            let stop = walk_top(code, eq + 1, n, close, false, &|i| {
                let c = at(code, i);
                c == b';'
                    || c == b'}'
                    || (c == b'e' && word_at(code, i, "else") && !re!(r"\}\s*$").is_match(slice(code, i.saturating_sub(64), i)))
            });
            init_end = stop;
            if at(code, stop) == b'e' {
                let open = skip_space(code, stop + 4);
                let else_end = if at(code, open) == b'{' { close.get(&open).copied().unwrap_or(n) } else { n };
                let semi = skip_space(code, else_end + 1);
                stmt_end = if at(code, semi) == b';' { semi } else { else_end };
            } else {
                stmt_end = stop;
            }
        } else {
            init_end = n;
            stmt_end = walk_top(code, pat_end, n, close, false, &|i| at(code, i) == b';' || at(code, i) == b'}');
        }
        let Some(block) = block_at(&scopes.events, &scopes.open_after, close, n, at_) else { continue };
        let scope_from = stmt_end + 1;
        let scope_to = block.1;
        let init: Option<String> = if eq >= 0 { Some(trim(slice(code, eq as usize + 1, init_end)).to_string()) } else { None };
        if let Some(name) = simple {
            if is_binding_name(&name) {
                add(
                    &mut bindings,
                    &name,
                    RustBinding {
                        at: pat_from + pattern_text.rfind(name.as_str()).unwrap_or(0),
                        kind: BindingKind::Let,
                        from: scope_from,
                        to: scope_to,
                        init: if ty.is_none() { init.clone() } else { None },
                        ty: ty.clone(),
                        site: None,
                        scrutinee: None,
                    },
                );
            }
            continue;
        }
        bind_pattern(code, close, &mut bindings, pat_from, pat_end, BindingKind::Let, scope_from, scope_to, init.as_deref());
    }
    // `for PATTERN in EXPR {`.
    for m in re!(r"\bfor\b", "g").captures_iter(code) {
        let p = skip_space(code, m.get(0).unwrap().start() + 3);
        if at(code, p) == b'<' {
            continue;
        }
        let in_at = walk_top(code, p, n, close, false, &|i| {
            let c = at(code, i);
            c == b'{' || c == b';' || c == b'=' || (c == b'i' && word_at(code, i, "in"))
        });
        if at(code, in_at) != b'i' {
            continue;
        }
        let open = walk_top(code, in_at + 2, n, close, false, &|i| at(code, i) == b'{' || at(code, i) == b';');
        if at(code, open) != b'{' {
            continue;
        }
        let scrut = trim(slice(code, in_at + 2, open)).to_string();
        bind_pattern(code, close, &mut bindings, p, in_at, BindingKind::For, open, close.get(&open).copied().unwrap_or(n), Some(&scrut));
    }
    // `match EXPR { PATTERN => … }`.
    for m in re!(r"\bmatch\b", "g").captures_iter(code) {
        let from = m.get(0).unwrap().start() + 5;
        let open = walk_top(code, from, n, close, false, &|i| matches!(at(code, i), b'{' | b';' | b'}'));
        if at(code, open) != b'{' {
            continue;
        }
        let scrutinee = trim(slice(code, from, open)).to_string();
        if scrutinee.is_empty() {
            continue;
        }
        let end = close.get(&open).copied().unwrap_or(n);
        let mut arm_start = open + 1;
        let mut p = open + 1;
        while p < end {
            let c = at(code, p);
            if c == b'(' || c == b'[' || c == b'{' {
                p = close.get(&p).copied().unwrap_or(end) + 1;
                continue;
            }
            if c != b'=' || at(code, p + 1) != b'>' {
                p += 1;
                continue;
            }
            let guard = walk_top(code, arm_start, p, close, false, &|i| at(code, i) == b'i' && word_at(code, i, "if"));
            let mut q = skip_space(code, p + 2);
            let arm_end;
            if at(code, q) == b'{' {
                arm_end = close.get(&q).copied().unwrap_or(end);
                q = skip_space(code, arm_end + 1);
                if at(code, q) == b',' {
                    q += 1;
                } else {
                    q = arm_end + 1;
                }
            } else {
                arm_end = walk_top(code, q, end, close, false, &|i| at(code, i) == b',');
                q = arm_end + 1;
            }
            bind_pattern(code, close, &mut bindings, arm_start, guard, BindingKind::Arm, guard, arm_end, Some(&scrutinee));
            arm_start = q;
            p = q;
        }
    }
    bindings
}

/// The innermost impl/trait block whose body contains `offset`.
pub fn rust_impl_at(scopes: &RustScopes, offset: usize) -> Option<&RustImplBlock> {
    let mut best: Option<&RustImplBlock> = None;
    for b in &scopes.impls {
        if offset > b.from && offset < b.to && best.is_none_or(|x| b.from > x.from) {
            best = Some(b);
        }
    }
    best
}

/// Split `text` at top-level `sep`, trimming each part and dropping empty ones.
pub fn split_rust_top_level(text: &str, sep: char) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        let c = bytes[i];
        if matches!(c, b'(' | b'[' | b'{' | b'<') {
            depth += 1;
        } else if matches!(c, b')' | b']' | b'}') || (c == b'>' && prev(text, i) != b'-' && prev(text, i) != b'=') {
            depth -= 1;
        } else if c as char == sep && depth == 0 {
            parts.push(text[start..i].to_string());
            start = i + 1;
        }
    }
    parts.push(text[start..].to_string());
    parts.into_iter().map(|p| trim(&p).to_string()).filter(|p| !p.is_empty()).collect()
}

/// Generics and `where` bounds on type parameter `name`, across `items`; `None` when no item
/// declares it.
pub fn rust_bounds_of(name: &str, items: &[(&str, &str)]) -> Option<Vec<String>> {
    let mut declared = false;
    let mut bounds: Vec<String> = Vec::new();
    let take = |text: &str, bounds: &mut Vec<String>| {
        for b in split_rust_top_level(text, '+') {
            let bound = trim(&re!(r"^for\s*<[^>]*>\s*").replace_first(&b, |_| String::new())).to_string();
            if bound.is_empty() || bound.starts_with('?') || bound.starts_with('\'') {
                continue;
            }
            bounds.push(bound);
        }
    };
    for (generics, _) in items {
        for part in split_rust_top_level(generics, ',') {
            let Some(m) = re!(r"^([A-Za-z_]\w*)\s*(?::([\s\S]*?))?(?:=[\s\S]*)?$").captures(&part) else { continue };
            if m.text(1) != Some(name) {
                continue;
            }
            declared = true;
            if let Some(b) = m.text(2).filter(|s| !s.is_empty()) {
                take(b, &mut bounds);
            }
        }
    }
    if !declared {
        return None;
    }
    for (_, where_) in items {
        for part in split_rust_top_level(where_, ',') {
            if let Some(m) = re!(r"^([A-Za-z_]\w*)\s*:([\s\S]*)$").captures(&part) {
                if m.text(1) == Some(name) {
                    take(m.text(2).unwrap_or(""), &mut bounds);
                }
            }
        }
    }
    Some(bounds)
}

/// The binding of `name` in scope at `offset`, and whether another binding shares its scope.
pub fn rust_binding_at(scopes: &RustScopes, name: &str, offset: usize) -> Option<(RustBinding, bool)> {
    let all = {
        let mut slot = scopes.bindings.borrow_mut();
        if slot.is_none() {
            *slot = Some(Rc::new(read_rust_bindings(scopes)));
        }
        Rc::clone(slot.as_ref().unwrap())
    };
    let list = all.get(name)?;
    let mut best: Option<&RustBinding> = None;
    for b in list {
        if offset >= b.from && offset < b.to && best.is_none_or(|x| b.at > x.at) {
            best = Some(b);
        }
    }
    let best = best?;
    let ambiguous = list.iter().any(|b| {
        !std::ptr::eq(b, best)
            && b.from == best.from
            && b.to == best.to
            && b.kind == best.kind
            && matches!(b.kind, BindingKind::Arm | BindingKind::Cond | BindingKind::Let)
    });
    Some((best.clone(), ambiguous))
}

/// The innermost fn whose signature-through-body range contains `offset`.
pub fn rust_fn_at(scopes: &RustScopes, offset: usize) -> Option<&RustFn> {
    let mut best: Option<&RustFn> = None;
    for f in &scopes.fns {
        if offset >= f.from && offset <= f.to && best.is_none_or(|x| f.from > x.from) {
            best = Some(f);
        }
    }
    best
}

#[allow(dead_code)]
fn _keep(_: &Re) {}
