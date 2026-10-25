//! Vue template call lexer from `vue-template-calls.ts`. Offsets are UTF-8
//! bytes internally and convert to UTF-16 only when graph positions are emitted.
use codegraph_jsre::re;
use codegraph_types::jscompat::is_js_space;
use std::collections::{HashMap, HashSet};

pub struct TemplateCall {
    pub name: String,
    pub offset: usize,
}
struct Attr<'a> {
    name: &'a str,
    value: Option<&'a str>,
    value_start: usize,
}
struct Tag<'a> {
    name: &'a str,
    attrs: Vec<Attr<'a>>,
    close: usize,
    self_closing: bool,
}
struct Frame<'a> {
    tag: &'a str,
    locals: HashSet<String>,
    pre: bool,
}

/// Keep active binding counts as scopes enter/leave. Rebuilding this union
/// by walking all ancestors for every tag makes deeply nested markup quadratic.
#[derive(Default)]
struct Scope<'a> {
    frames: Vec<Frame<'a>>,
    bindings: HashMap<String, usize>,
    pre_depth: usize,
}

impl<'a> Scope<'a> {
    fn push(&mut self, frame: Frame<'a>) {
        for name in &frame.locals {
            *self.bindings.entry(name.clone()).or_default() += 1;
        }
        self.pre_depth += usize::from(frame.pre);
        self.frames.push(frame);
    }
    fn close(&mut self, name: &str) {
        if let Some(k) = self
            .frames
            .iter()
            .rposition(|f| f.tag == name || f.tag.to_lowercase() == name.to_lowercase())
        {
            for frame in self.frames.drain(k..) {
                self.pre_depth -= usize::from(frame.pre);
                for name in frame.locals {
                    let count = self.bindings.get_mut(&name).expect("active scope binding");
                    *count -= 1;
                    if *count == 0 {
                        self.bindings.remove(&name);
                    }
                }
            }
        }
    }
    fn locals(&self) -> HashSet<String> {
        self.bindings.keys().cloned().collect()
    }
}

fn ws(s: &str, i: usize) -> bool {
    s.get(i..)
        .and_then(|s| s.chars().next())
        .is_some_and(is_js_space)
}
fn skip_ws(s: &str, mut i: usize, end: usize) -> usize {
    while i < end && ws(s, i) {
        i += s[i..].chars().next().unwrap().len_utf8();
    }
    i
}
fn byte(s: &str, i: usize) -> u8 {
    s.as_bytes().get(i).copied().unwrap_or(0)
}
fn ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || matches!(c, b'_' | b'$')
}
fn ident_char(c: u8) -> bool {
    ident_start(c) || c.is_ascii_digit()
}
fn word_end(s: &str, mut i: usize, end: usize) -> usize {
    while i < end && ident_char(byte(s, i)) {
        i += 1;
    }
    i
}
fn identifiers(s: &str) -> impl Iterator<Item = String> + '_ {
    re!(r"[A-Za-z_$][\w$]*", "g")
        .captures_iter(s)
        .map(|c| c.text(0).unwrap().to_owned())
}
fn void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}
fn global(name: &str) -> bool {
    matches!(
        name,
        "Infinity"
            | "undefined"
            | "NaN"
            | "isFinite"
            | "isNaN"
            | "parseFloat"
            | "parseInt"
            | "decodeURI"
            | "decodeURIComponent"
            | "encodeURI"
            | "encodeURIComponent"
            | "Math"
            | "Number"
            | "Date"
            | "Array"
            | "Object"
            | "Boolean"
            | "String"
            | "RegExp"
            | "Map"
            | "Set"
            | "JSON"
            | "Intl"
            | "BigInt"
            | "console"
            | "Error"
            | "Symbol"
    )
}
fn keyword(name: &str) -> bool {
    matches!(
        name,
        "if" | "else"
            | "for"
            | "while"
            | "do"
            | "switch"
            | "case"
            | "default"
            | "break"
            | "continue"
            | "return"
            | "throw"
            | "try"
            | "catch"
            | "finally"
            | "function"
            | "class"
            | "new"
            | "delete"
            | "typeof"
            | "void"
            | "instanceof"
            | "in"
            | "of"
            | "var"
            | "let"
            | "const"
            | "this"
            | "super"
            | "import"
            | "export"
            | "await"
            | "async"
            | "yield"
            | "with"
            | "debugger"
            | "true"
            | "false"
            | "null"
            | "undefined"
    )
}

pub fn template_calls(source: &str) -> Result<Vec<TemplateCall>, String> {
    let mut masked = source.as_bytes().to_vec();
    for pattern in [
        re!(r"<(script|style)(\s[^>]*)?>[\s\S]*?</\1\s*>", "g"),
        re!(r"<!--[\s\S]*?-->", "g"),
    ] {
        // The second scan sees the first scan's mask, as String.replace does.
        let s = std::str::from_utf8(&masked).expect("mask preserves UTF-8 boundaries");
        let spans: Vec<_> = pattern
            .captures_iter(s)
            .map(|c| {
                let m = c.get(0).unwrap();
                m.start()..m.end()
            })
            .collect();
        for span in spans {
            for b in &mut masked[span] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
    }
    let s = std::str::from_utf8(&masked).expect("masked source is valid UTF-8");
    let Some((start, end)) = root_template(s) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    scan_markup(s, start, end, &mut out)?;
    out.sort_by_key(|c| c.offset);
    Ok(out)
}

fn root_template(s: &str) -> Option<(usize, usize)> {
    let mut start = None;
    let mut depth = 0usize;
    for c in re!(
        r#"<(/?)template(?=[\s/>])((?:[^>"']|"[^"]*"|'[^']*')*)>"#,
        "g"
    )
    .captures_iter(s)
    {
        let closing = c.text(1) == Some("/");
        let attrs = c.text(2).unwrap_or("");
        let self_closing = attrs.trim_end_matches(is_js_space).ends_with('/');
        if let Some(open) = start {
            if closing {
                depth -= 1;
                if depth == 0 {
                    return Some((open, c.get(0).unwrap().start()));
                }
            } else if !self_closing {
                depth += 1;
            }
        } else {
            if closing || self_closing {
                continue;
            }
            if re!(r#"\blang\s*=\s*["']?([\w-]+)"#)
                .captures(attrs)
                .is_some_and(|c| c.text(1).unwrap().to_lowercase() != "html")
            {
                return None;
            }
            start = Some(c.get(0).unwrap().end());
            depth = 1;
        }
    }
    start.map(|start| (start, s.len()))
}
fn directive_kind(name: &str) -> &'static str {
    if name == "v-for" {
        "for"
    } else if name.starts_with('#')
        || name == "v-slot"
        || name.starts_with("v-slot:")
        || matches!(name, "slot-scope" | "scope")
    {
        "slot"
    } else if name.starts_with('@') || name == "v-on" || name.starts_with("v-on:") {
        "on"
    } else if name.starts_with([':', '.']) || name.starts_with("v-") {
        "expr"
    } else {
        ""
    }
}

fn open_tag(s: &str, at: usize, end: usize) -> Tag<'_> {
    let mut p = at + 1;
    let name_start = p;
    while p < end && !ws(s, p) && !matches!(byte(s, p), b'/' | b'>') {
        p += 1;
    }
    let name = &s[name_start..p];
    let mut attrs = Vec::new();
    while p < end {
        p = skip_ws(s, p, end);
        if p >= end {
            break;
        }
        if byte(s, p) == b'>' {
            return Tag {
                name,
                attrs,
                close: p + 1,
                self_closing: false,
            };
        }
        if byte(s, p) == b'/' && byte(s, p + 1) == b'>' {
            return Tag {
                name,
                attrs,
                close: p + 2,
                self_closing: true,
            };
        }
        let attr_start = p;
        while p < end && !ws(s, p) && !matches!(byte(s, p), b'"' | b'\'' | b'>' | b'/' | b'=') {
            p += 1;
        }
        if p == attr_start {
            p += 1;
            continue;
        }
        let attr_name = &s[attr_start..p];
        let mut q = skip_ws(s, p, end);
        if byte(s, q) != b'=' {
            attrs.push(Attr {
                name: attr_name,
                value: None,
                value_start: 0,
            });
            continue;
        }
        q = skip_ws(s, q + 1, end);
        let quote = byte(s, q);
        if matches!(quote, b'\'' | b'"') {
            let value_start = (q + 1).min(end);
            let value_end = s[value_start..end]
                .find(quote as char)
                .map(|n| value_start + n)
                .unwrap_or(end);
            attrs.push(Attr {
                name: attr_name,
                value: Some(&s[value_start..value_end]),
                value_start,
            });
            p = value_end + 1;
        } else {
            let value_start = q;
            while q < end && !ws(s, q) && byte(s, q) != b'>' {
                q += 1;
            }
            attrs.push(Attr {
                name: attr_name,
                value: Some(&s[value_start..q]),
                value_start,
            });
            p = q;
        }
    }
    Tag {
        name,
        attrs,
        close: end,
        self_closing: false,
    }
}

fn scan_markup(
    s: &str,
    start: usize,
    end: usize,
    out: &mut Vec<TemplateCall>,
) -> Result<(), String> {
    let mut scope = Scope::default();
    let mut next_mustache = 0;
    let mut i = start;
    while i < end {
        if next_mustache < i {
            next_mustache = s[i..end].find("{{").map(|n| i + n).unwrap_or(end);
        }
        let next_tag = s[i..end].find('<').map(|n| i + n).unwrap_or(end);
        if next_mustache < next_tag {
            let expr_start = next_mustache + 2;
            let close = s[expr_start..]
                .find("}}")
                .map(|n| expr_start + n)
                .filter(|n| *n <= end)
                .unwrap_or(end);
            if scope.pre_depth == 0 {
                scan_expression(s, expr_start, close, scope.locals(), out)?;
            }
            i = close + 2;
            continue;
        }
        if next_tag >= end {
            break;
        }
        if let Some(c) = re!(r"^</([A-Za-z][^\s/>]*)\s*>").captures(&s[next_tag..]) {
            let name = c.text(1).unwrap();
            scope.close(name);
            i = next_tag + c.get(0).unwrap().end();
            continue;
        }
        if !byte(s, next_tag + 1).is_ascii_alphabetic() {
            i = next_tag + 1;
            continue;
        }
        let tag = open_tag(s, next_tag, end);
        i = tag.close;
        let mut own = HashSet::new();
        let mut pre = false;
        let mut for_source = None;
        for a in &tag.attrs {
            if a.name == "v-pre" {
                pre = true;
            }
            let Some(value) = a.value else {
                continue;
            };
            match directive_kind(a.name) {
                "for" => {
                    if let Some(c) = re!(r"([\s\S]*?)\s+(?:in|of)\s+(\S[\s\S]*)").captures(value) {
                        own.extend(identifiers(c.text(1).unwrap()));
                        for_source = Some((
                            a.value_start + value.len() - c.text(2).unwrap().len(),
                            a.value_start + value.len(),
                        ));
                    }
                }
                "slot" => own.extend(identifiers(value)),
                _ => {}
            }
        }
        if !pre && scope.pre_depth == 0 {
            let mut locals = scope.locals();
            locals.extend(own.iter().cloned());
            for a in &tag.attrs {
                let Some(value) = a.value else {
                    continue;
                };
                match directive_kind(a.name) {
                    "expr" | "on" => scan_expression(
                        s,
                        a.value_start,
                        a.value_start + value.len(),
                        locals.clone(),
                        out,
                    )?,
                    "for" => {
                        if let Some((start, end)) = for_source {
                            scan_expression(s, start, end, locals.clone(), out)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !tag.self_closing && !void_element(&tag.name.to_lowercase()) {
            scope.push(Frame {
                tag: tag.name,
                locals: own,
                pre,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum Prev {
    Start,
    Operand,
    Dot,
    New,
    Decl,
}
struct Paren {
    open: usize,
    call: Option<usize>,
    params: bool,
}

fn scan_expression(
    s: &str,
    start: usize,
    end: usize,
    locals: HashSet<String>,
    out: &mut Vec<TemplateCall>,
) -> Result<(), String> {
    let mut tasks = vec![(start, end, locals)];
    let mut work = 0usize;
    while let Some((start, end, outer)) = tasks.pop() {
        work = work.saturating_add(end.saturating_sub(start));
        if work > 8 * 1024 * 1024 {
            return Err("defer: Vue template nesting exceeds parse budget".into());
        }
        let mut params = HashSet::new();
        let mut found: Vec<(TemplateCall, bool)> = Vec::new();
        let mut parens: Vec<Paren> = Vec::new();
        let mut last_group = None;
        let mut last_ident = String::new();
        let mut function_pending = false;
        let mut prev = Prev::Start;
        let mut i = start;
        while i < end {
            let c = byte(s, i);
            if ws(s, i) {
                i = skip_ws(s, i, end);
                continue;
            }
            if matches!(c, b'\'' | b'"') {
                i = skip_string(s, i, end);
                prev = Prev::Operand;
                continue;
            }
            if c == b'`' {
                let (next, substitutions) = template_literal(s, i, end);
                for (a, b) in substitutions {
                    let mut bound = outer.clone();
                    bound.extend(params.iter().cloned());
                    tasks.push((a, b, bound));
                }
                i = next;
                prev = Prev::Operand;
                continue;
            }
            if c == b'/' {
                if byte(s, i + 1) == b'/' {
                    i = s[i..end].find('\n').map(|n| i + n).unwrap_or(end);
                    continue;
                }
                if byte(s, i + 1) == b'*' {
                    i = s[i + 2..]
                        .find("*/")
                        .map(|n| i + n + 4)
                        .filter(|n| *n - 2 <= end)
                        .unwrap_or(end);
                    continue;
                }
                if prev != Prev::Operand {
                    i = skip_regex(s, i, end);
                    prev = Prev::Operand;
                    continue;
                }
                i += 1;
                prev = Prev::Start;
                continue;
            }
            if c.is_ascii_digit() || (c == b'.' && byte(s, i + 1).is_ascii_digit()) {
                i += 1;
                while i < end
                    && (byte(s, i).is_ascii_alphanumeric() || matches!(byte(s, i), b'_' | b'.'))
                {
                    i += 1;
                }
                prev = Prev::Operand;
                continue;
            }
            if ident_start(c) {
                let w_end = word_end(s, i, end);
                let word = &s[i..w_end];
                if prev == Prev::Dot {
                    i = w_end;
                    prev = Prev::Operand;
                    last_ident.clear();
                    continue;
                }
                if keyword(word) {
                    i = w_end;
                    last_ident.clear();
                    prev = if word == "new" {
                        Prev::New
                    } else if matches!(word, "function" | "class") {
                        if word == "function" {
                            function_pending = true;
                        }
                        Prev::Decl
                    } else if matches!(
                        word,
                        "this" | "super" | "true" | "false" | "null" | "undefined"
                    ) {
                        Prev::Operand
                    } else {
                        Prev::Start
                    };
                    continue;
                }
                if prev == Prev::Decl {
                    i = w_end;
                    prev = Prev::Operand;
                    continue;
                }
                let mut segments = vec![word];
                let mut p = w_end;
                loop {
                    let mut q = skip_ws(s, p, end);
                    if byte(s, q) == b'!' && byte(s, q + 1) == b'.' {
                        q += 1;
                    }
                    let r = if byte(s, q) == b'.' && byte(s, q + 1) != b'.' {
                        skip_ws(s, q + 1, end)
                    } else if byte(s, q) == b'?'
                        && byte(s, q + 1) == b'.'
                        && !byte(s, q + 2).is_ascii_digit()
                    {
                        skip_ws(s, q + 2, end)
                    } else {
                        break;
                    };
                    if !ident_start(byte(s, r)) {
                        break;
                    }
                    let seg_end = word_end(s, r, end);
                    segments.push(&s[r..seg_end]);
                    p = seg_end;
                }
                let mut q = skip_ws(s, p, end);
                if byte(s, q) == b'?'
                    && byte(s, q + 1) == b'.'
                    && byte(s, skip_ws(s, q + 2, end)) == b'('
                {
                    q = skip_ws(s, q + 2, end);
                }
                if byte(s, q) == b'(' {
                    let mut call = None;
                    if prev != Prev::New
                        && !word.starts_with('$')
                        && !global(word)
                        && !params.contains(word)
                        && !outer.contains(word)
                    {
                        call = Some(found.len());
                        found.push((
                            TemplateCall {
                                name: segments.join("."),
                                offset: i,
                            },
                            false,
                        ));
                    }
                    parens.push(Paren {
                        open: q,
                        call,
                        params: false,
                    });
                    i = q + 1;
                    prev = Prev::Start;
                    last_ident.clear();
                    continue;
                }
                i = p;
                prev = Prev::Operand;
                last_ident = if segments.len() == 1 {
                    word.to_owned()
                } else {
                    String::new()
                };
                continue;
            }
            match c {
                b'(' => {
                    parens.push(Paren {
                        open: i,
                        call: None,
                        params: function_pending,
                    });
                    function_pending = false;
                    i += 1;
                    prev = Prev::Start;
                }
                b')' => {
                    if let Some(paren) = parens.pop() {
                        last_group = Some((paren.open, i));
                        if paren.params {
                            params.extend(identifiers(&s[paren.open + 1..i]));
                        }
                        if let Some(call) = paren.call {
                            if byte(s, skip_ws(s, i + 1, end)) == b'{' {
                                found[call].1 = true;
                            }
                        }
                    }
                    i += 1;
                    prev = Prev::Operand;
                    last_ident.clear();
                }
                b'=' if byte(s, i + 1) == b'>' => {
                    if !last_ident.is_empty() {
                        params.insert(last_ident.clone());
                    } else if let Some((open, close)) = last_group {
                        if close == previous_non_ws(s, i, start) {
                            params.extend(identifiers(&s[open + 1..close]));
                        }
                    }
                    i += 2;
                    prev = Prev::Start;
                    last_ident.clear();
                }
                b'.' => {
                    if byte(s, i + 1) == b'.' && byte(s, i + 2) == b'.' {
                        i += 3;
                        prev = Prev::Start;
                    } else {
                        i += 1;
                        prev = Prev::Dot;
                    }
                    last_ident.clear();
                }
                b'?' if byte(s, i + 1) == b'.' && !byte(s, i + 2).is_ascii_digit() => {
                    i += 2;
                    prev = Prev::Dot;
                    last_ident.clear();
                }
                _ => {
                    i += 1;
                    prev = if matches!(c, b']' | b'}') {
                        Prev::Operand
                    } else {
                        Prev::Start
                    };
                    last_ident.clear();
                }
            }
        }
        out.extend(
            found
                .into_iter()
                .filter(|(_, dropped)| !dropped)
                .map(|(call, _)| call),
        );
    }
    Ok(())
}

fn previous_non_ws(s: &str, i: usize, floor: usize) -> usize {
    s[floor..i]
        .char_indices()
        .rev()
        .find(|(_, c)| !is_js_space(*c))
        .map(|(p, _)| floor + p)
        .unwrap_or(floor)
}
fn skip_string(s: &str, i: usize, end: usize) -> usize {
    let quote = byte(s, i);
    let mut k = i + 1;
    while k < end {
        if byte(s, k) == b'\\' {
            k += 2;
        } else if byte(s, k) == quote {
            return k + 1;
        } else {
            k += 1;
        }
    }
    end
}

enum BraceFrame {
    Brace(usize),
    Template,
}
fn matching_brace(s: &str, i: usize, end: usize) -> usize {
    let mut frames = vec![BraceFrame::Brace(0)];
    let mut k = i;
    while k < end {
        match frames.last_mut().unwrap() {
            BraceFrame::Template => {
                if byte(s, k) == b'\\' {
                    k += 2;
                } else if byte(s, k) == b'`' {
                    frames.pop();
                    k += 1;
                } else if byte(s, k) == b'$' && byte(s, k + 1) == b'{' {
                    frames.push(BraceFrame::Brace(0));
                    k += 2;
                } else {
                    k += 1;
                }
            }
            BraceFrame::Brace(depth) => match byte(s, k) {
                b'\'' | b'"' => k = skip_string(s, k, end),
                b'`' => {
                    frames.push(BraceFrame::Template);
                    k += 1;
                }
                b'{' => {
                    *depth += 1;
                    k += 1;
                }
                b'}' if *depth == 0 => {
                    frames.pop();
                    if frames.is_empty() {
                        return k;
                    }
                    k += 1;
                }
                b'}' => {
                    *depth -= 1;
                    k += 1;
                }
                _ => k += 1,
            },
        }
    }
    end
}
fn template_literal(s: &str, i: usize, end: usize) -> (usize, Vec<(usize, usize)>) {
    let mut substitutions = Vec::new();
    let mut k = i + 1;
    while k < end {
        if byte(s, k) == b'\\' {
            k += 2;
        } else if byte(s, k) == b'`' {
            return (k + 1, substitutions);
        } else if byte(s, k) == b'$' && byte(s, k + 1) == b'{' {
            let close = matching_brace(s, k + 2, end);
            substitutions.push((k + 2, close));
            k = close + 1;
        } else {
            k += 1;
        }
    }
    (end, substitutions)
}
fn skip_regex(s: &str, i: usize, end: usize) -> usize {
    let mut k = i + 1;
    let mut in_class = false;
    while k < end {
        let c = byte(s, k);
        if c == b'\\' {
            k += 2;
            continue;
        }
        if c == b'\n' {
            return i + 1;
        }
        if c == b'[' {
            in_class = true;
        } else if c == b']' {
            in_class = false;
        } else if c == b'/' && !in_class {
            k += 1;
            while k < end && byte(s, k).is_ascii_alphabetic() {
                k += 1;
            }
            return k;
        }
        k += 1;
    }
    i + 1
}
