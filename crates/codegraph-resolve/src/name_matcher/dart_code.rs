//! Dart source text the call-chain typer reads: the file with strings and comments blanked, and
//! the bracket scanners that work on it.
//!
//! Offsets: the text has exactly one byte per UTF-16 unit of the source (a non-ASCII unit becomes
//! a blank or an inert byte — Dart identifiers are ASCII), so every index here is the UTF-16
//! index the TypeScript works with, and `text` is pure ASCII (every byte index is a boundary).

use super::util::*;
use crate::ctx::Ctx;
use crate::types::UnresolvedRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::is_js_space;
use codegraph_types::nodepath::posix;
use std::rc::Rc;

pub struct DartCode {
    pub text: String,
    pub line_starts: Vec<usize>,
    pub parts: Vec<String>,
    pub part_of: Option<String>,
}

impl DartCode {
    pub fn bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    /// `code.lineStarts[line - 1]` (1-based line).
    pub fn line_start(&self, line: u32) -> Option<usize> {
        (line as usize).checked_sub(1).and_then(|i| self.line_starts.get(i)).copied()
    }

    /// `code.lineStarts[index]` for an already 0-based index.
    pub fn line_start_at(&self, index: usize) -> Option<usize> {
        self.line_starts.get(index).copied()
    }
}

/// `text[k]` as a byte, `0` out of range (`undefined` in the TypeScript, which matches nothing).
pub fn tb(text: &[u8], k: i64) -> u8 {
    if k < 0 {
        0
    } else {
        text.get(k as usize).copied().unwrap_or(0)
    }
}

/// `/\s/.test(c)` for a byte of the blanked text.
pub fn is_ws(c: u8) -> bool {
    c == b' ' || (9..=13).contains(&c)
}

/// `/[\w$]/.test(c)`.
pub fn is_wordish(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// `text.slice(from, to)` for byte offsets, clamped.
pub fn sl(text: &str, from: i64, to: i64) -> &str {
    let len = text.len() as i64;
    let a = from.clamp(0, len) as usize;
    let b = to.clamp(0, len) as usize;
    if a >= b {
        ""
    } else {
        &text[a..b]
    }
}

memo_map!(DartCodeMemo, String, Option<Rc<DartCode>>);

/// The file's blanked text, line starts and `part` / `part of` links; none when it cannot be read.
pub fn dart_code_of(file_path: &str, ctx: &Ctx) -> Option<Rc<DartCode>> {
    let memo = ctx.memo::<Bounded<Option<Rc<DartCode>>, 64>>();
    memo.get_or(file_path, false, || {
        let source = ctx.read_file(file_path)?;
        let text = blank_dart_code(&source);
        let mut line_starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        let dir = posix::dirname(file_path);
        let sibling = |uri: &str| posix::normalize(&posix::join(&[&dir, uri]));
        let parts: Vec<String> = Re::cached(r#"^\s*part\s+['"]([^'"]+)['"]\s*;"#, "gm")
            .captures_iter(&source)
            .map(|m| sibling(m.text(1).unwrap_or("")))
            .collect();
        let part_of = Re::cached(r#"^\s*part\s+of\s+['"]([^'"]+)['"]\s*;"#, "m")
            .captures(&source)
            .and_then(|m| m.text(1).filter(|s| !s.is_empty()).map(|s| sibling(s)));
        Some(Rc::new(DartCode { text, line_starts, parts, part_of }))
    })
}

struct Literal {
    quote: u8,
    triple: bool,
    raw: bool,
}

/// The source with strings, comments and string-interpolation delimiters blanked (newlines kept).
pub fn blank_dart_code(source: &str) -> String {
    // One byte per UTF-16 unit.
    let mut out: Vec<u8> = Vec::with_capacity(source.len());
    for c in source.chars() {
        if c.is_ascii() {
            out.push(c as u8);
        } else {
            let fill = if is_js_space(c) { b' ' } else { 1u8 };
            for _ in 0..c.len_utf16() {
                out.push(fill);
            }
        }
    }
    let src: Vec<u8> = out.clone();
    let n = src.len();
    let at = |k: usize| -> u8 { src.get(k).copied().unwrap_or(0) };
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        let mut k = from;
        while k < to && k < n {
            if out[k] != b'\n' && out[k] != b'\r' {
                out[k] = b' ';
            }
            k += 1;
        }
    };
    let starts_with = |at_: usize, c: u8, count: usize| -> bool { (0..count).all(|j| at_ + j < n && src[at_ + j] == c) };

    let mut outer: Vec<(Literal, i64)> = Vec::new();
    let mut literal: Option<Literal> = None;
    let mut depth: i64 = 0;
    let mut i = 0usize;
    while i < n {
        let c = src[i];
        if let Some(lit) = &literal {
            if !lit.raw && c == b'\\' {
                blank(&mut out, i, i + 2);
                i += 2;
            } else if !lit.raw && c == b'$' && at(i + 1) == b'{' {
                blank(&mut out, i, i + 2);
                outer.push((literal.take().unwrap(), depth));
                depth = 0;
                i += 2;
            } else if starts_with(i, lit.quote, if lit.triple { 3 } else { 1 }) {
                i += if lit.triple { 3 } else { 1 };
                literal = None;
            } else if !lit.triple && c == b'\n' {
                literal = None;
                i += 1;
            } else {
                blank(&mut out, i, i + 1);
                i += 1;
            }
            continue;
        }
        if c == b'/' && at(i + 1) == b'/' {
            let stop = src[i..].iter().position(|b| *b == b'\n').map_or(n, |p| i + p);
            blank(&mut out, i, stop);
            i = stop;
        } else if c == b'/' && at(i + 1) == b'*' {
            let mut nest = 1;
            let mut j = i + 2;
            while j < n && nest > 0 {
                if src[j] == b'/' && at(j + 1) == b'*' {
                    nest += 1;
                    j += 2;
                } else if src[j] == b'*' && at(j + 1) == b'/' {
                    nest -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if c == b'\'' || c == b'"' {
            let triple = starts_with(i, c, 3);
            let prev = if i >= 1 { src[i - 1] } else { 0 };
            let prev2 = if i >= 2 { src[i - 2] } else { 0 };
            literal = Some(Literal { quote: c, triple, raw: prev == b'r' && !is_wordish(prev2) });
            i += if triple { 3 } else { 1 };
        } else {
            if !outer.is_empty() && c == b'{' {
                depth += 1;
            } else if !outer.is_empty() && c == b'}' {
                if depth == 0 {
                    blank(&mut out, i, i + 1);
                    let (lit, d) = outer.pop().unwrap();
                    literal = Some(lit);
                    depth = d;
                    i += 1;
                    continue;
                }
                depth -= 1;
            }
            i += 1;
        }
    }
    String::from_utf8(out).expect("blanked Dart text is ASCII")
}

/// The offset in the file where the reference's name starts on its line, or -1.
pub fn dart_name_start(r: &UnresolvedRef, name: &str, code: &DartCode) -> i64 {
    let Some(from) = code.line_start(r.line) else { return -1 };
    let to = code.line_starts.get(r.line as usize).copied().unwrap_or(code.text.len());
    let line = &code.text[from..to];
    let col = r.column as usize;
    let col_in = |c: usize| -> bool { line.get(c..).is_some_and(|rest| rest.starts_with(name)) };
    let start = if col_in(col) {
        Some(col)
    } else if col >= name.len() && col_in(col - name.len()) {
        Some(col - name.len())
    } else {
        line.find(name)
    };
    start.map_or(-1, |s| (from + s) as i64)
}

/// The opening bracket matching the closing one at `at`, scanning backwards (`-1` when none).
pub fn dart_opening(text: &[u8], at: i64, open: u8, close: u8, limit: i64) -> i64 {
    let mut depth = 0;
    let mut k = at;
    while k >= 0 && at - k < limit {
        let c = tb(text, k);
        if c == close {
            depth += 1;
        } else if c == open {
            depth -= 1;
            if depth == 0 {
                return k;
            }
        } else if open == b'<' && matches!(c, b';' | b'{' | b'}' | b'=') {
            return -1;
        }
        k -= 1;
    }
    -1
}

/// The simple name of a written type: `a.B<C>?` → `B`.
pub fn dart_simple_type_name(text: &str) -> Option<String> {
    let no_args = re!(r"<[\s\S]*$").replace_first(text, |_| String::new());
    let name = no_args.replace('?', "");
    let last = codegraph_types::jscompat::js_trim(name.rsplit('.').next().unwrap_or(""));
    re!(r"^[A-Za-z_$][\w$]*$").is_match(last).then(|| last.to_string())
}

/// The `>` closing the type-argument list opened at `open`, when a call, member access or
/// nullable follows it; `-1` otherwise.
pub fn dart_type_arguments_end(text: &[u8], open: usize) -> i64 {
    let mut depth = 0;
    let mut k = open;
    while k < text.len() && k - open < 400 {
        let ch = text[k];
        if ch == b'<' {
            depth += 1;
        } else if ch == b'>' {
            depth -= 1;
            if depth == 0 {
                // /^\s*[(.>?]/ over the next three bytes
                let tail = &text[(k + 1).min(text.len())..(k + 4).min(text.len())];
                let first = tail.iter().copied().find(|c| !is_ws(*c));
                return if matches!(first, Some(b'(' | b'.' | b'>' | b'?')) { k as i64 } else { -1 };
            }
        } else if matches!(ch, b';' | b'{' | b'}' | b'=') {
            return -1;
        }
        k += 1;
    }
    -1
}

/// The `(` opening the group (call arguments, parameter list) that contains `at`, or -1.
pub fn dart_group_open(text: &[u8], at: usize) -> i64 {
    let mut parens = 0;
    let mut braces = 0;
    let mut k = at as i64 - 1;
    while k >= 0 && at as i64 - k < 4000 {
        let ch = tb(text, k);
        if ch == b'}' {
            braces += 1;
        } else if ch == b'{' {
            if braces == 0 {
                return -1;
            }
            braces -= 1;
        } else if braces > 0 {
            k -= 1;
            continue;
        } else if ch == b')' {
            parens += 1;
        } else if ch == b'(' {
            if parens == 0 {
                return k;
            }
            parens -= 1;
        } else if ch == b';' {
            return -1;
        }
        k -= 1;
    }
    -1
}

/// The `)` closing the group opened at `open`, or -1.
pub fn dart_group_close(text: &[u8], open: usize) -> i64 {
    let mut depth = 0;
    let mut k = open;
    while k < text.len() && k - open < 8000 {
        if text[k] == b'(' {
            depth += 1;
        } else if text[k] == b')' {
            depth -= 1;
            if depth == 0 {
                return k as i64;
            }
        }
        k += 1;
    }
    -1
}

/// A memo of the positions a name is used at, per file; emptied when it grows past 4096 entries.
#[derive(Default)]
pub struct NameUses(std::cell::RefCell<std::collections::HashMap<String, Rc<Vec<usize>>>>);

/// Every position `name` appears at as a whole, unqualified word of the file's code.
pub fn dart_name_uses(code: &DartCode, file_path: &str, name: &str, ctx: &Ctx) -> Rc<Vec<usize>> {
    let memo = ctx.memo::<NameUses>();
    let key = format!("{file_path}|{name}");
    if let Some(hit) = memo.0.borrow().get(&key) {
        return Rc::clone(hit);
    }
    let uses: Vec<usize> = Re::cached(&format!(r"(?<![\w$.]){}(?![\w$])", escape_regex(name)), "g")
        .captures_iter(&code.text)
        .filter_map(|m| m.get(0).map(|g| g.start()))
        .collect();
    let uses = Rc::new(uses);
    let mut map = memo.0.borrow_mut();
    if map.len() >= 4096 {
        map.clear();
    }
    map.insert(key, Rc::clone(&uses));
    uses
}
