//! Port of the `ignore` package (node-ignore 7.0.5) — the gitignore matcher the
//! scanner, the watcher and the project config share.
//!
//! The scan decides which files the index holds, so this is a port of the
//! *package*, quirks included, not a gitignore implementation. Each pattern is
//! pushed through the package's ordered replacer pipeline into the source of a
//! JavaScript regular expression, which is compiled lazily on first use — a
//! pattern that cannot compile fails at match time, not at `add`, exactly as in
//! JavaScript (`readGitignorePatterns` in the orchestrator relies on that).
//!
//! The replacers are hand-coded over `char`s because the originals lean on
//! JavaScript regex behaviour (lazy groups, `\s`, `.` that stops at `\r`) that
//! must be reproduced to the character; the regexes they *produce* are
//! translated to PCRE2 by [`js_to_pcre`] (UTF mode, JIT) with JavaScript's
//! non-unicode semantics: legacy octal escapes, identity escapes, literal `{`,
//! a `[` that is an ordinary class member, `$` that means end of input.

use codegraph_types::jscompat::is_js_space;
use pcre2::bytes::{Regex, RegexBuilder};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

/// Why a path could not be tested (node-ignore throws these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoreError {
    /// `path must not be empty`.
    Empty,
    /// `path should be a path.relative()d string` — absolute, `./x`, `../x`, `.`, `..`.
    NotRelative(String),
    /// A rule's regular expression does not compile.
    Regex(String),
}

impl fmt::Display for IgnoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IgnoreError::Empty => write!(f, "path must not be empty"),
            IgnoreError::NotRelative(p) => write!(f, "path should be a `path.relative()`d string, but got \"{p}\""),
            IgnoreError::Regex(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for IgnoreError {}

impl From<IgnoreError> for String {
    fn from(e: IgnoreError) -> String {
        e.to_string()
    }
}

/// `{ ignored, unignored }` of a single test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TestResult {
    pub ignored: bool,
    pub unignored: bool,
}

// --- the replacer pipeline ---------------------------------------------------

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `/^﻿/` → ''.
fn strip_bom(s: &str) -> String {
    s.strip_prefix('\u{FEFF}').unwrap_or(s).to_string()
}

/// `/((?:\\\\)*?)(\\?\s+)$/` → `m1 + (m2 starts with a backslash ? ' ' : '')`.
fn trailing_space(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut ws_start = n;
    while ws_start > 0 && is_js_space(c[ws_start - 1]) {
        ws_start -= 1;
    }
    for i in 0..n {
        let mut k = 0usize;
        loop {
            let pos = i + 2 * k;
            if pos < n {
                let hit = if c[pos] == '\\' {
                    (pos + 1 < n && pos + 1 >= ws_start).then_some(true)
                } else {
                    (pos >= ws_start).then_some(false)
                };
                if let Some(escaped_space) = hit {
                    let mut out: String = c[..i + 2 * k].iter().collect();
                    if escaped_space {
                        out.push(' ');
                    }
                    return out;
                }
            }
            if pos + 1 < n && c[pos] == '\\' && c[pos + 1] == '\\' {
                k += 1;
            } else {
                break;
            }
        }
    }
    s.to_string()
}

/// `/(\\+?)\s/g` → `m1.slice(0, len - len % 2) + ' '`.
fn escaped_space(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        if c[i] == '\\' {
            let mut j = i;
            while j < n && c[j] == '\\' {
                j += 1;
            }
            let len = j - i;
            if j < n && is_js_space(c[j]) {
                out.extend(std::iter::repeat('\\').take(len - len % 2));
                out.push(' ');
                i = j + 1;
            } else {
                out.extend(std::iter::repeat('\\').take(len));
                i = j;
            }
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// `/[\\$.|*+(){^]/g` → a backslash before each.
fn escape_meta(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        if matches!(ch, '\\' | '$' | '.' | '|' | '*' | '+' | '(' | ')' | '{' | '^') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// `/^\//` → '^'.
fn leading_slash(s: &str) -> String {
    match s.strip_prefix('/') {
        Some(rest) => format!("^{rest}"),
        None => s.to_string(),
    }
}

/// `/^\^*\\\*\\\*\\\//` → `^(?:.*\/)?`.
fn leading_globstar(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut p = 0;
    while p < c.len() && c[p] == '^' {
        p += 1;
    }
    const SEQ: [char; 6] = ['\\', '*', '\\', '*', '\\', '/'];
    if c.len() >= p + 6 && c[p..p + 6] == SEQ {
        let rest: String = c[p + 6..].iter().collect();
        return format!("^(?:.*\\/)?{rest}");
    }
    s.to_string()
}

/// `/^(?=[^^])/` — the start anchor, chosen by whether the *original* pattern
/// has a slash that is not its last character.
fn starting(s: &str, body: &str) -> String {
    match s.chars().next() {
        None | Some('^') => s.to_string(),
        Some(_) => {
            let inner_slash = body.char_indices().any(|(i, ch)| ch == '/' && i + 1 < body.len());
            let prefix = if inner_slash { "^" } else { "(?:^|\\/)" };
            format!("{prefix}{s}")
        }
    }
}

/// `/\\\/\\\*\\\*(?=\\\/|$)/g` — a `/**` segment: zero or more directories in
/// the middle of a pattern, everything below at the end.
fn two_globstars(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    const NEEDLE: [char; 6] = ['\\', '/', '\\', '*', '\\', '*'];
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        if i + 6 <= n && c[i..i + 6] == NEEDLE && (i + 6 == n || (c[i + 6] == '\\' && i + 7 < n && c[i + 7] == '/')) {
            out.push_str(if i + 6 < n { "(?:\\/[^\\/]+)*" } else { "\\/.+" });
            i += 6;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// `\*` pairs from `p`, then a lookahead for one more non-terminator character.
/// Backtracks the repetition exactly as the regex would.
fn star_pairs_then_char(c: &[char], p: usize) -> Option<usize> {
    let n = c.len();
    let mut k = 0;
    while p + 2 * k + 1 < n && c[p + 2 * k] == '\\' && c[p + 2 * k + 1] == '*' {
        k += 1;
    }
    while k >= 1 {
        let end = p + 2 * k;
        if end < n && !is_line_terminator(c[end]) {
            return Some(end);
        }
        k -= 1;
    }
    None
}

/// `/(^|[^\\]+)(\\\*)+(?=.+)/g` → `p1 + '[^\/]*'` (a run of `\*` collapses to one).
fn intermediate_wildcards(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'scan: while i < n {
        // `^` alternative, then the run of non-backslashes
        let mut hit: Option<(usize, usize)> = None; // (end of p1, end of match)
        if i == 0 {
            if let Some(end) = star_pairs_then_char(&c, 0) {
                hit = Some((0, end));
            }
        }
        if hit.is_none() {
            let mut j = i;
            while j < n && c[j] != '\\' {
                j += 1;
            }
            if j > i {
                if let Some(end) = star_pairs_then_char(&c, j) {
                    hit = Some((j, end));
                }
            }
        }
        if let Some((p1_end, end)) = hit {
            out.extend(&c[i..p1_end]);
            out.push_str("[^\\/]*");
            i = end;
            continue 'scan;
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// `/\\\\\\(?=[$.|*+(){^])/g` → one backslash.
fn unescape_triple(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        if i + 3 < n && c[i] == '\\' && c[i + 1] == '\\' && c[i + 2] == '\\' && matches!(c[i + 3], '$' | '.' | '|' | '*' | '+' | '(' | ')' | '{' | '^') {
            out.push('\\');
            i += 3;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// `/\\\\/g` → one backslash.
fn unescape_double(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        if i + 1 < n && c[i] == '\\' && c[i + 1] == '\\' {
            out.push('\\');
            i += 2;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// `/([0-z])-([0-z])/g` → '' for an out-of-order pair.
fn sanitize_range(range: &[char]) -> String {
    let in_range = |ch: char| ('0'..='z').contains(&ch);
    let n = range.len();
    let mut out = String::new();
    let mut i = 0;
    while i < n {
        if i + 2 < n && in_range(range[i]) && range[i + 1] == '-' && in_range(range[i + 2]) {
            if range[i] <= range[i + 2] {
                out.extend(&range[i..i + 3]);
            }
            i += 3;
        } else {
            out.push(range[i]);
            i += 1;
        }
    }
    out
}

/// `/(\\)?\[([^\]/]*?)(\\*)($|\])/g` — the range notation.
fn ranges(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        let (lead, bracket) = if c[i] == '\\' && i + 1 < n && c[i + 1] == '[' {
            (true, i + 1)
        } else if c[i] == '[' {
            (false, i)
        } else {
            out.push(c[i]);
            i += 1;
            continue;
        };
        let rs = bracket + 1;
        let mut p = rs;
        // (range end, end-escape end, close char or none)
        let found = loop {
            let mut q = p;
            while q < n && c[q] == '\\' {
                q += 1;
            }
            if q == n {
                break Some((p, q, None));
            }
            if c[q] == ']' {
                break Some((p, q, Some(']')));
            }
            if p < n && c[p] != ']' && c[p] != '/' {
                p += 1;
            } else {
                break None;
            }
        };
        let Some((range_end, esc_end, close)) = found else {
            out.push(c[i]);
            i += 1;
            continue;
        };
        let range = &c[rs..range_end];
        let esc_len = esc_end - range_end;
        let close_str = close.map(String::from).unwrap_or_default();
        if lead {
            out.push_str("\\[");
            out.extend(range);
            out.extend(std::iter::repeat('\\').take(esc_len - esc_len % 2));
            out.push_str(&close_str);
        } else if close == Some(']') {
            if esc_len % 2 == 0 {
                out.push('[');
                out.push_str(&sanitize_range(range));
                out.extend(std::iter::repeat('\\').take(esc_len));
                out.push(']');
            } else {
                out.push_str("[]");
            }
        } else {
            out.push_str("[]");
        }
        i = esc_end + usize::from(close.is_some());
    }
    out
}

/// `/(?:[^*])$/` — the end anchor.
fn ending(s: &str) -> String {
    match s.chars().next_back() {
        None | Some('*') => s.to_string(),
        Some('/') => format!("{s}$"),
        Some(_) => format!("{s}(?=$|\\/$)"),
    }
}

/// `makeRegexPrefix`: the regex source for a (negation-stripped) pattern.
pub(crate) fn make_regex_prefix(body: &str) -> String {
    let s = strip_bom(body);
    let s = trailing_space(&s);
    let s = escaped_space(&s);
    let s = escape_meta(&s);
    let s = s.replace('?', "[^/]");
    let s = leading_slash(&s);
    let s = s.replace('/', "\\/");
    let s = leading_globstar(&s);
    let s = starting(&s, body);
    let s = two_globstars(&s);
    let s = intermediate_wildcards(&s);
    let s = unescape_triple(&s);
    let s = unescape_double(&s);
    let s = ranges(&s);
    ending(&s)
}

/// `IgnoreRule._make(MODE_IGNORE)`: the trailing-wildcard rewrite.
pub(crate) fn finish_regex_source(prefix: &str) -> String {
    let c: Vec<char> = prefix.chars().collect();
    let n = c.len();
    if n >= 2 && c[n - 2] == '\\' && c[n - 1] == '*' {
        let t = n - 2;
        let start_slash = t >= 2 && c[t - 2] == '\\' && c[t - 1] == '/';
        let head: String = c[..if start_slash { t - 2 } else { t }].iter().collect();
        let body = if start_slash { "\\/[^/]+" } else { "[^/]*" };
        return format!("{head}{body}(?=$|\\/$)");
    }
    prefix.to_string()
}

// --- JavaScript regex source → PCRE2 -----------------------------------------

const PCRE_JS_SPACE: &str = r"\t\n\x{0B}\x{0C}\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
const NEVER: &str = r"[^\x{0}-\x{10FFFF}]";
const ANY: &str = r"[\x{0}-\x{10FFFF}]";

// JavaScript (without the `u` flag) matches UTF-16 *units*: `?` consumes half of
// an emoji, `[😀]` is two members. PCRE2 matches code points, so both the rules'
// regexes and the paths they test go through the same mapping — every surrogate
// unit becomes one code point of plane 15 (Supplementary Private Use Area-A),
// which no BMP character can collide with.
const SURROGATE_STAND_IN: u32 = 0xF0000;

/// One UTF-16 unit as a PCRE escape.
fn unit_escape(unit: u32) -> String {
    let code = if (0xD800..=0xDFFF).contains(&unit) { SURROGATE_STAND_IN + (unit - 0xD800) } else { unit };
    format!("\\x{{{code:X}}}")
}

/// A character as the escapes of its UTF-16 units.
fn char_units(ch: char) -> String {
    let mut buf = [0u16; 2];
    ch.encode_utf16(&mut buf).iter().map(|&u| unit_escape(u32::from(u))).collect()
}

/// The path with every astral character split into two stand-in units.
fn to_units(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().all(|c| (c as u32) < 0x10000) {
        return s.into();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        if (ch as u32) < 0x10000 {
            out.push(ch);
        } else {
            let mut buf = [0u16; 2];
            for &u in ch.encode_utf16(&mut buf).iter() {
                out.push(char::from_u32(SURROGATE_STAND_IN + (u32::from(u) - 0xD800)).expect("plane 15"));
            }
        }
    }
    out.into()
}

struct Atom {
    text: String,
    next: usize,
    /// `\d` `\w` `\s` and friends: a set, never a range endpoint.
    set: bool,
}

fn hex_val(c: &[char], from: usize, count: usize) -> Option<u32> {
    if from + count > c.len() {
        return None;
    }
    let mut v = 0u32;
    for ch in &c[from..from + count] {
        v = v * 16 + ch.to_digit(16)?;
    }
    Some(v)
}

fn code_atom(code: u32, next: usize) -> Atom {
    Atom { text: format!("\\x{{{code:X}}}"), next, set: false }
}

/// The escape sequence at `c[i]` (a backslash), JavaScript non-unicode semantics.
fn escape_atom(c: &[char], i: usize, in_class: bool) -> Result<Atom, String> {
    let Some(&e) = c.get(i + 1) else {
        return Err("\\ at end of pattern".to_string());
    };
    let next = i + 2;
    let plain = |text: &str, set: bool| Atom { text: text.to_string(), next, set };
    Ok(match e {
        'd' => plain(if in_class { "0-9" } else { r"\d" }, true),
        'D' => plain(r"\D", true),
        'w' => plain(if in_class { "A-Za-z0-9_" } else { r"\w" }, true),
        'W' => plain(r"\W", true),
        's' => plain(&if in_class { PCRE_JS_SPACE.to_string() } else { format!("[{PCRE_JS_SPACE}]") }, true),
        'S' => plain(&if in_class { r"\S".to_string() } else { format!("[^{PCRE_JS_SPACE}]") }, true),
        'b' if in_class => code_atom(8, next),
        'b' => plain(r"\b", false),
        'B' if in_class => plain("B", false),
        'B' => plain(r"\B", false),
        'n' | 'r' | 't' | 'f' => plain(&format!("\\{e}"), false),
        'v' => code_atom(0x0B, next),
        '0'..='7' => {
            // LegacyOctalEscapeSequence (no capture group can own `\1`..`\7` here).
            let max_len = if e <= '3' { 3 } else { 2 };
            let mut value = 0u32;
            let mut len = 0;
            while len < max_len {
                match c.get(i + 1 + len).and_then(|d| d.to_digit(8)) {
                    Some(d) => {
                        value = value * 8 + d;
                        len += 1;
                    }
                    None => break,
                }
            }
            code_atom(value, i + 1 + len)
        }
        '8' | '9' => plain(&e.to_string(), false),
        'x' => match hex_val(c, i + 2, 2) {
            Some(v) => code_atom(v, i + 4),
            None => plain("x", false),
        },
        'u' => match hex_val(c, i + 2, 4) {
            Some(unit) => Atom { text: unit_escape(unit), next: i + 6, set: false },
            None => plain("u", false),
        },
        'c' => match c.get(i + 2) {
            Some(l) if l.is_ascii_alphabetic() => code_atom(*l as u32 % 32, i + 3),
            // `\c` not followed by a letter: a literal backslash, then `c` as itself.
            _ => Atom { text: r"\\".to_string(), next: i + 1, set: false },
        },
        _ if e.is_ascii_alphanumeric() => plain(&e.to_string(), false),
        _ if e.is_ascii() => plain(&format!("\\{e}"), false),
        _ => Atom { text: char_units(e), next, set: false },
    })
}

/// The class starting at `c[i]` (`[`); returns the index after its `]`.
fn class_to_pcre(c: &[char], i: usize, out: &mut String) -> Result<usize, String> {
    let n = c.len();
    let mut j = i + 1;
    let negated = c.get(j) == Some(&'^');
    if negated {
        j += 1;
    }
    if c.get(j) == Some(&']') {
        out.push_str(if negated { ANY } else { NEVER });
        return Ok(j + 1);
    }
    out.push('[');
    if negated {
        out.push('^');
    }
    let mut prev_set = false;
    loop {
        if j >= n {
            return Err("Unterminated character class".to_string());
        }
        match c[j] {
            ']' => {
                out.push(']');
                return Ok(j + 1);
            }
            '\\' => {
                let atom = escape_atom(c, j, true)?;
                out.push_str(&atom.text);
                prev_set = atom.set;
                j = atom.next;
            }
            '[' => {
                out.push_str(r"\[");
                prev_set = false;
                j += 1;
            }
            '-' => {
                let next_set = c.get(j + 1) == Some(&'\\') && matches!(c.get(j + 2), Some('d' | 'D' | 'w' | 'W' | 's' | 'S'));
                out.push_str(if prev_set || next_set { r"\-" } else { "-" });
                prev_set = false;
                j += 1;
            }
            ch if !ch.is_ascii() => {
                out.push_str(&char_units(ch));
                prev_set = false;
                j += 1;
            }
            ch => {
                out.push(ch);
                prev_set = false;
                j += 1;
            }
        }
    }
}

/// `{n}` `{n,}` `{n,m}` at `c[i]`: its length, if it is a quantifier.
fn quantifier_len(c: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let digits = |j: &mut usize| {
        let s = *j;
        while c.get(*j).is_some_and(char::is_ascii_digit) {
            *j += 1;
        }
        *j > s
    };
    if !digits(&mut j) {
        return None;
    }
    if c.get(j) == Some(&',') {
        j += 1;
        digits(&mut j);
    }
    (c.get(j) == Some(&'}')).then_some(j + 1 - i)
}

/// A regex source written for JavaScript (no flags other than `i`, handled by
/// the builder) as PCRE2 source with the same meaning.
pub(crate) fn js_to_pcre(src: &str) -> Result<String, String> {
    let c: Vec<char> = src.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(src.len() + 16);
    let mut i = 0;
    while i < n {
        match c[i] {
            '\\' => {
                let atom = escape_atom(&c, i, false)?;
                out.push_str(&atom.text);
                i = atom.next;
            }
            '[' => i = class_to_pcre(&c, i, &mut out)?,
            '.' => {
                out.push_str(r"[^\n\r\x{2028}\x{2029}]");
                i += 1;
            }
            '$' => {
                out.push_str(r"\z");
                i += 1;
            }
            '{' => match quantifier_len(&c, i) {
                Some(len) => {
                    out.extend(&c[i..i + len]);
                    i += len;
                }
                None => {
                    out.push_str(r"\{");
                    i += 1;
                }
            },
            '}' => {
                out.push_str(r"\}");
                i += 1;
            }
            ch if !ch.is_ascii() => {
                out.push_str(&char_units(ch));
                i += 1;
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    Ok(out)
}

type Compiled = Result<Arc<Regex>, String>;

/// Compiled rule regexes, shared across matchers: the built-in default rules are
/// rebuilt for every repo, embedded repo and watcher scope, and compiling them
/// again each time would dominate small scans.
fn compile(source: &str, ignore_case: bool) -> Compiled {
    static CACHE: OnceLock<Mutex<HashMap<(String, bool), Compiled>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(&(source.to_string(), ignore_case)) {
        return hit.clone();
    }
    let built: Compiled = js_to_pcre(source).and_then(|pcre| {
        RegexBuilder::new()
            .utf(true)
            .caseless(ignore_case)
            .jit_if_available(true)
            .build(&pcre)
            .map(Arc::new)
            .map_err(|e| format!("Invalid regular expression: /{source}/: {e}"))
    });
    cache.lock().unwrap().insert((source.to_string(), ignore_case), built.clone());
    built
}

// --- rules and the matcher ---------------------------------------------------

struct Rule {
    negative: bool,
    regex_prefix: String,
    ignore_case: bool,
    regex: OnceLock<Compiled>,
}

impl Rule {
    fn new(pattern: &str, ignore_case: bool) -> Rule {
        let (negative, rest) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        let body = if let Some(r) = rest.strip_prefix("\\!") {
            format!("!{r}")
        } else {
            rest.to_string()
        };
        let body = if let Some(r) = body.strip_prefix("\\#") { format!("#{r}") } else { body };
        Rule { negative, regex_prefix: make_regex_prefix(&body), ignore_case, regex: OnceLock::new() }
    }

    fn matches(&self, path: &str) -> Result<bool, IgnoreError> {
        let compiled = self.regex.get_or_init(|| compile(&finish_regex_source(&self.regex_prefix), self.ignore_case));
        match compiled {
            Ok(re) => Ok(re.is_match(to_units(path).as_bytes()).unwrap_or(false)),
            Err(msg) => Err(IgnoreError::Regex(msg.clone())),
        }
    }
}

/// `checkPattern`: a usable gitignore line.
fn check_pattern(pattern: &str) -> bool {
    if pattern.is_empty() || pattern.chars().all(is_js_space) || pattern.starts_with('#') {
        return false;
    }
    // REGEX_INVALID_TRAILING_BACKSLASH = /(?:[^\\]|^)\\$/
    let mut rev = pattern.chars().rev();
    if rev.next() == Some('\\') && rev.next() != Some('\\') {
        return false;
    }
    true
}

/// `splitPattern`: `.split(/\r?\n/g).filter(Boolean)`.
fn split_pattern(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).filter(|l| !l.is_empty())
}

/// `checkPath.convert` on Windows: back slashes become slashes unless the path
/// is a `\\?\` path or holds a character Windows forbids in names.
fn convert_path(path: &str) -> std::borrow::Cow<'_, str> {
    if !cfg!(windows) {
        return path.into();
    }
    let forbidden = path.chars().any(|c| matches!(c, '"' | '<' | '>' | '|') || (c as u32) < 0x20);
    if path.starts_with("\\\\?\\") || forbidden {
        path.into()
    } else {
        path.replace('\\', "/").into()
    }
}

/// REGEX_TEST_INVALID_PATH = /^\.{0,2}\/|^\.{1,2}$/ (plus a drive letter on Windows).
fn is_not_relative(path: &str) -> bool {
    let b = path.as_bytes();
    // ^\.{0,2}\/
    let leading_slash = (0..=2).any(|k| b.get(k) == Some(&b'/') && b[..k].iter().all(|&x| x == b'.'));
    if leading_slash || path == "." || path == ".." {
        return true;
    }
    cfg!(windows) && b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/'
}

type Cache = Mutex<HashMap<String, TestResult>>;

/// A matcher. `ignores` follows node-ignore: a path is ignored when its own
/// rules say so or any parent directory is.
pub struct Ignore {
    ignore_case: bool,
    rules: Vec<Rule>,
    ignore_cache: Cache,
    test_cache: Cache,
    /// `excludeBuildOutputUnderSrc`: when set, `ignores` also drops build output
    /// written below a `src/` folder. The inner value is the project's own root
    /// rules, which can re-include a folder or file by name.
    src_build: Option<Option<Box<Ignore>>>,
}

impl Default for Ignore {
    fn default() -> Self {
        Ignore::new()
    }
}

impl Ignore {
    pub fn new() -> Ignore {
        Ignore { ignore_case: true, rules: Vec::new(), ignore_cache: Cache::default(), test_cache: Cache::default(), src_build: None }
    }

    /// Add gitignore text (one pattern per line, `\r\n` or `\n`).
    pub fn add(&mut self, text: &str) -> &mut Self {
        self.add_lines(split_pattern(text))
    }

    /// Add patterns that are already one per entry (not split further).
    pub fn add_patterns<S: AsRef<str>>(&mut self, patterns: &[S]) -> &mut Self {
        self.add_lines(patterns.iter().map(AsRef::as_ref))
    }

    fn add_lines<'a>(&mut self, lines: impl Iterator<Item = &'a str>) -> &mut Self {
        let mut added = false;
        for line in lines {
            if check_pattern(line) {
                self.rules.push(Rule::new(line, self.ignore_case));
                added = true;
            }
        }
        if added {
            self.ignore_cache.lock().unwrap().clear();
            self.test_cache.lock().unwrap().clear();
        }
        self
    }

    /// Apply the build-output-under-`src/` exclusion on top of the patterns.
    pub fn with_src_build_exclusion(mut self, project_rules: Option<Ignore>) -> Ignore {
        self.src_build = Some(project_rules.map(Box::new));
        self
    }

    /// `rules.test` without the parent walk.
    fn test_rules(&self, path: &str, check_unignored: bool) -> Result<TestResult, IgnoreError> {
        let mut ignored = false;
        let mut unignored = false;
        for rule in &self.rules {
            let negative = rule.negative;
            if (unignored == negative && ignored != unignored) || (negative && !ignored && !unignored && !check_unignored) {
                continue;
            }
            if !rule.matches(path)? {
                continue;
            }
            ignored = !negative;
            unignored = negative;
        }
        Ok(TestResult { ignored, unignored })
    }

    fn walk(&self, path: &str, cache: &Cache, check_unignored: bool, slices: Option<Vec<&str>>) -> Result<TestResult, IgnoreError> {
        if let Some(hit) = cache.lock().unwrap().get(path) {
            return Ok(*hit);
        }
        let mut slices = slices.unwrap_or_else(|| path.split('/').filter(|s| !s.is_empty()).collect());
        slices.pop();
        let result = if slices.is_empty() {
            self.test_rules(path, check_unignored)?
        } else {
            let parent_path = format!("{}/", slices.join("/"));
            let parent = self.walk(&parent_path, cache, check_unignored, Some(slices))?;
            // It is not possible to re-include a file if a parent directory of it is excluded.
            if parent.ignored {
                parent
            } else {
                self.test_rules(path, check_unignored)?
            }
        };
        cache.lock().unwrap().insert(path.to_string(), result);
        Ok(result)
    }

    fn checked(&self, original: &str, cache: &Cache, check_unignored: bool) -> Result<TestResult, IgnoreError> {
        let path = convert_path(original);
        if path.is_empty() {
            return Err(IgnoreError::Empty);
        }
        if is_not_relative(&path) {
            return Err(IgnoreError::NotRelative(original.to_string()));
        }
        self.walk(&path, cache, check_unignored, None)
    }

    /// node-ignore's `test`: the result including whether a rule un-ignored the path.
    pub fn test(&self, path: &str) -> Result<TestResult, IgnoreError> {
        self.checked(path, &self.test_cache, true)
    }

    /// Whether `path` is ignored. A trailing slash marks a directory.
    pub fn ignores(&self, path: &str) -> Result<bool, IgnoreError> {
        let by_patterns = self.checked(path, &self.ignore_cache, false)?.ignored;
        if by_patterns {
            return Ok(true);
        }
        let Some(project_rules) = &self.src_build else {
            return Ok(false);
        };
        let Some(folder) = build_output_folder_under_src(path) else {
            return Ok(false);
        };
        let Some(project) = project_rules else {
            return Ok(true);
        };
        // The file, then each folder above it up to the `build` folder.
        let bytes = path.as_bytes();
        let mut end = path.len();
        while end >= folder.len() {
            if project.test(&path[..end])?.unignored {
                return Ok(false);
            }
            // rel.lastIndexOf('/', end - 2) + 1
            end = match end.checked_sub(1).and_then(|limit| bytes[..limit].iter().rposition(|&b| b == b'/')) {
                Some(slash) => slash + 1,
                None => 0,
            };
        }
        Ok(true)
    }
}

// --- build output below src/ --------------------------------------------------

fn ends_with_ci(name: &str, suffixes: &[&str]) -> bool {
    let lower = name.to_ascii_lowercase();
    suffixes.iter().any(|s| lower.ends_with(s))
}

/// `HAND_WRITTEN_SOURCE`: `/\.(?:[mc]?ts|tsx|ets|jsx|vue|svelte|astro|go|cs|rs)$/i`.
fn is_hand_written_source(name: &str) -> bool {
    ends_with_ci(name, &[".ts", ".mts", ".cts", ".tsx", ".ets", ".jsx", ".vue", ".svelte", ".astro", ".go", ".cs", ".rs"])
}

/// `DECLARATION_FILE`: `/\.d\.(?:[mc]?ts|ets)$/i`.
fn is_declaration_file(name: &str) -> bool {
    ends_with_ci(name, &[".d.ts", ".d.mts", ".d.cts", ".d.ets"])
}

fn jvm_build_packages() -> &'static Ignore {
    static JVM: OnceLock<Ignore> = OnceLock::new();
    JVM.get_or_init(|| {
        let mut ig = Ignore::new();
        ig.add_patterns(&["**/src/*/java/**/build/", "**/src/*/kotlin/**/build/", "**/src/*/scala/**/build/"]);
        ig
    })
}

/// The `build` folder below a `src/` folder that file `rel` sits in, with a
/// trailing slash (`src/build/` for `src/build/legacy/page.tsx`), or none. A JVM
/// package named `build` (#1642) does not count.
fn src_build_folder_of(rel: &str) -> Option<String> {
    let lower = rel.to_ascii_lowercase();
    let has_build = lower.starts_with("build/") || lower.contains("/build/");
    if !has_build {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    let mut in_src = false;
    for i in 0..parts.len().saturating_sub(1) {
        let part = parts[i].to_lowercase();
        if part == "src" {
            in_src = true;
        } else if part == "build" && in_src {
            let folder = format!("{}/", parts[..=i].join("/"));
            return if jvm_build_packages().ignores(&folder).unwrap_or(false) { None } else { Some(folder) };
        }
    }
    None
}

/// When `rel` is a file in a `build` folder below `src/` that a build step could
/// have written, that folder; otherwise none.
pub(crate) fn build_output_folder_under_src(rel: &str) -> Option<String> {
    let name = &rel[rel.rfind('/').map_or(0, |i| i + 1)..];
    if !name.contains('.') {
        return None;
    }
    if is_hand_written_source(name) && !is_declaration_file(name) {
        return None;
    }
    src_build_folder_of(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(lines: &[&str]) -> Ignore {
        let mut ig = Ignore::new();
        ig.add_patterns(lines);
        ig
    }

    #[test]
    fn directory_rules_and_negation() {
        let m = ig(&["dist/", "*.log", "!keep.log", "/root.txt"]);
        assert!(m.ignores("dist/").unwrap());
        assert!(m.ignores("dist/a.js").unwrap());
        assert!(!m.ignores("dist").unwrap());
        assert!(m.ignores("a/b.log").unwrap());
        assert!(!m.ignores("keep.log").unwrap());
        assert!(m.ignores("root.txt").unwrap());
        assert!(!m.ignores("sub/root.txt").unwrap());
    }

    #[test]
    fn parent_directory_exclusion_cannot_be_undone() {
        let m = ig(&["vendor/", "!vendor/keep.txt"]);
        assert!(m.ignores("vendor/keep.txt").unwrap());
        let m = ig(&["vendor/*", "!vendor/keep.txt"]);
        assert!(!m.ignores("vendor/keep.txt").unwrap());
        assert!(m.ignores("vendor/other.txt").unwrap());
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(ig(&["Build/"]).ignores("build/x.o").unwrap());
    }

    #[test]
    fn invalid_paths_are_refused() {
        let m = ig(&["a"]);
        assert_eq!(m.ignores(""), Err(IgnoreError::Empty));
        assert!(matches!(m.ignores("./a"), Err(IgnoreError::NotRelative(_))));
        assert!(matches!(m.ignores("../a"), Err(IgnoreError::NotRelative(_))));
        assert!(matches!(m.ignores("/a"), Err(IgnoreError::NotRelative(_))));
        assert!(matches!(m.ignores("."), Err(IgnoreError::NotRelative(_))));
        assert!(m.ignores(".a").is_ok());
    }

    #[test]
    fn blank_comment_and_trailing_backslash_lines_are_skipped() {
        let mut m = Ignore::new();
        m.add("# comment\n   \n\\#hash\nfoo \nbar\\ \nbaz\\\n");
        assert!(m.ignores("#hash").unwrap());
        assert!(m.ignores("foo").unwrap());
        assert!(m.ignores("bar ").unwrap());
        assert!(!m.ignores("baz").unwrap());
    }

    #[test]
    fn a_bad_range_fails_at_match_time() {
        let mut m = Ignore::new();
        m.add("[é-a]x");
        assert!(matches!(m.ignores("zx"), Err(IgnoreError::Regex(_))));
        // an out-of-order ASCII range is sanitized away instead
        let mut m = Ignore::new();
        m.add("[z-a]x");
        assert!(m.ignores("zx").is_ok());
    }

    #[test]
    fn build_output_under_src() {
        assert_eq!(build_output_folder_under_src("src/build/legacy/page.html").as_deref(), Some("src/build/"));
        assert_eq!(build_output_folder_under_src("src/build/legacy/page.tsx"), None);
        assert_eq!(build_output_folder_under_src("src/build/types.d.ts").as_deref(), Some("src/build/"));
        assert_eq!(build_output_folder_under_src("src/main/java/com/build/Foo.java"), None);
        assert_eq!(build_output_folder_under_src("build/out.js"), None);
    }
}
