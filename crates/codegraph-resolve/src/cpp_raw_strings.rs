//! C++ comments and raw string literals as column ranges — port of
//! `src/resolution/cpp-raw-strings.ts`.
//!
//! A raw string (`R"x( … )x"`) can span lines and hold anything, so a line-by-line scan of
//! code cannot see it. Block comments are marked the same way. Columns are UTF-16 units, as
//! `startColumn` is.

use crate::ctx::Ctx;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Per line (1-based): the `[start, end)` column ranges that are not code.
pub type NonCodeLines = HashMap<usize, Vec<(usize, usize)>>;

#[derive(Default)]
struct RawStringMemo(RefCell<HashMap<String, Rc<NonCodeLines>>>);

fn is_word_unit(c: Option<u16>) -> bool {
    matches!(c, Some(c) if c < 128 && ((c as u8).is_ascii_alphanumeric() || c == b'_' as u16 || c == b'$' as u16))
}

fn u(c: char) -> u16 {
    c as u16
}

fn index_of(text: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(text.len()));
    }
    if from >= text.len() {
        return None;
    }
    text[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The comment and raw-string ranges of `file`, per line.
pub fn cpp_non_code_lines(file: &str, ctx: &Ctx) -> Rc<NonCodeLines> {
    let memo = ctx.memo::<RawStringMemo>();
    if let Some(hit) = memo.0.borrow().get(file) {
        return Rc::clone(hit);
    }
    let mut lines: NonCodeLines = HashMap::new();
    if let Some(source) = ctx.read_file(file) {
        if source.contains("R\"") || source.contains("/*") {
            scan(&units(&source), &mut lines);
        }
    }
    let lines = Rc::new(lines);
    memo.0.borrow_mut().insert(file.to_string(), Rc::clone(&lines));
    lines
}

fn scan(text: &[u16], lines: &mut NonCodeLines) {
    let n = text.len();
    let mut line = 1usize;
    let mut column = 0usize;
    let mut i = 0usize;
    let nl = u('\n');
    let cr = u('\r');

    let mark = |lines: &mut NonCodeLines, from: usize, from_line: usize, from_column: usize, to: usize| {
        let mut l = from_line;
        let mut start = from_column;
        let mut c = from_column;
        for k in from..to {
            if text[k] == nl {
                if c > start {
                    lines.entry(l).or_default().push((start, c));
                }
                l += 1;
                start = 0;
                c = 0;
            } else if text[k] != cr {
                c += 1;
            }
        }
        if c > start {
            lines.entry(l).or_default().push((start, c));
        }
    };

    macro_rules! advance {
        ($to:expr) => {{
            let to: usize = $to;
            while i < to {
                if text[i] == nl {
                    line += 1;
                    column = 0;
                } else if text[i] != cr {
                    column += 1;
                }
                i += 1;
            }
        }};
    }

    while i < n {
        let c = text[i];
        let next = text.get(i + 1).copied();
        if c == u('/') && next == Some(u('/')) {
            let end = index_of(text, &[nl], i);
            advance!(end.unwrap_or(n));
        } else if c == u('/') && next == Some(u('*')) {
            let end = index_of(text, &units("*/"), i + 2);
            let to = end.map_or(n, |e| e + 2);
            mark(lines, i, line, column, to);
            advance!(to);
        } else if c == u('"') {
            // `R"delim( … )delim"`, prefixed by nothing, `u8`, `u`, `U` or `L`.
            let from = i.saturating_sub(3);
            let before = &text[from..i];
            let prefix_len = {
                let s = String::from_utf16_lossy(before);
                crate::cpp_raw_strings::raw_prefix_len(&s)
            };
            let open = match prefix_len {
                Some(len) if !is_word_unit(i.checked_sub(len + 1).and_then(|k| text.get(k).copied())) => {
                    index_of(text, &[u('(')], i + 1)
                }
                _ => None,
            };
            let delimiter: Option<&[u16]> = match open {
                Some(o) if o > i => Some(&text[i + 1..o]),
                _ => None,
            };
            let valid = delimiter.is_some_and(|d| {
                d.len() <= 16 && d.iter().all(|&c| c != u('(') && c != u(')') && c != u('\\') && !is_js_space_unit(c))
            });
            if valid {
                let d = delimiter.unwrap();
                let mut closing: Vec<u16> = vec![u(')')];
                closing.extend_from_slice(d);
                closing.push(u('"'));
                let close = index_of(text, &closing, open.unwrap() + 1);
                let end = close.map_or(n, |c| c + d.len() + 2);
                mark(lines, i, line, column, end);
                advance!(end);
            } else {
                let mut k = i + 1;
                while k < n && text[k] != u('"') && text[k] != nl {
                    k += if text[k] == u('\\') { 2 } else { 1 };
                }
                advance!(n.min(if text.get(k) == Some(&u('"')) { k + 1 } else { k }));
            }
        } else if c == u('\'') && !is_word_unit(i.checked_sub(1).and_then(|k| text.get(k).copied())) {
            let mut k = i + 1;
            while k < n && text[k] != u('\'') && text[k] != nl {
                k += if text[k] == u('\\') { 2 } else { 1 };
            }
            advance!(n.min(if text.get(k) == Some(&u('\'')) { k + 1 } else { k }));
        } else {
            advance!(i + 1);
        }
    }
}

fn is_js_space_unit(c: u16) -> bool {
    char::from_u32(c as u32).is_some_and(codegraph_types::jscompat::is_js_space)
}

/// `/(?:u8|[uUL])?R$/` on the three units before a quote: the length of the raw-string prefix.
pub(crate) fn raw_prefix_len(before: &str) -> Option<usize> {
    let m = codegraph_jsre::re!(r"(?:u8|[uUL])?R$").find(before)?;
    Some(m.as_str().encode_utf16().count())
}

/// `text` (line number `line`) with string/char literals and the comment ranges blanked.
pub fn cpp_literals_blanked(text: &str, line: usize, raw_lines: &NonCodeLines) -> String {
    let raw = raw_lines.get(&line);
    let mut out: Vec<u16> = units(text);
    if let Some(raw) = raw {
        for &(start, end) in raw {
            let mut k = start;
            while k < end && k < out.len() {
                out[k] = u(' ');
                k += 1;
            }
        }
    }
    if !text.contains('"') && !text.contains('\'') {
        return String::from_utf16_lossy(&out);
    }
    let src = units(text);
    let in_raw = |k: usize| raw.is_some_and(|r| r.iter().any(|&(start, end)| k >= start && k < end));
    let mut k = 0usize;
    while k < src.len() {
        let c = src[k];
        if in_raw(k) {
            k += 1;
            continue;
        }
        if c == u('/') && src.get(k + 1) == Some(&u('/')) {
            break;
        }
        if c == u('/') && src.get(k + 1) == Some(&u('*')) {
            match index_of(&src, &units("*/"), k + 2) {
                None => break,
                Some(close) => {
                    k = close + 2;
                    continue;
                }
            }
        }
        if c != u('"') && !(c == u('\'') && !is_word_unit(k.checked_sub(1).and_then(|p| src.get(p).copied()))) {
            k += 1;
            continue;
        }
        let mut end = k + 1;
        while end < src.len() && src[end] != c {
            end += if src[end] == u('\\') { 2 } else { 1 };
        }
        let upto = end.min(src.len());
        for b in k + 1..upto {
            out[b] = u(' ');
        }
        k = end + 1;
    }
    String::from_utf16_lossy(&out)
}
