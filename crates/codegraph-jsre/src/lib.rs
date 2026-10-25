//! JavaScript `RegExp` semantics on PCRE2.
//!
//! The TypeScript engine's resolution and framework heuristics are written as
//! `RegExp` literals over source text — lookahead, lookbehind, back-references and
//! named groups included — which the `regex` crate does not have and `fancy-regex`
//! runs far too slowly for. PCRE2 (JIT-compiled) has them all with the same
//! backtracking semantics; this crate translates the JavaScript syntax (without
//! the `u` flag's unit/code-point split, see below) to PCRE2 and wraps the match
//! loop in the entry points the ports need:
//!
//! * `exec` / `matchAll` / `replace` / `split` / `test`, with `lastIndex`-style
//!   start positions (lookbehind sees the text before the start);
//! * flags `i` `m` `s` `y` `u` (`g` is a property of the calling API, not the regex).
//!
//! What PCRE2 does differently from V8, and is handled in the translation:
//!
//! * `.` excludes every JavaScript line terminator (`\n` `\r` U+2028 U+2029);
//! * `^` / `$` are start / end of input, or — under `m` — of any JavaScript line;
//! * `\s` is JavaScript's whitespace set; `\d` `\w` `\b` are ASCII;
//! * `{` that does not start a quantifier is a literal; `[]` never matches and
//!   `[^]` matches anything; legacy octal escapes and identity escapes.
//!
//! Offsets are **byte** offsets into the UTF-8 haystack. JavaScript indexes UTF-16
//! units; the two agree on BMP-only text and callers that publish columns convert
//! with [`utf16_len`]. Astral characters match as one unit here, two in V8.

mod pcre;

use pcre::{CaptureLocations, Regex};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

const JIT_STACK_BYTES: usize = 16 << 20;
const JS_SPACE: &str = r"\t\n\x{0B}\x{0C}\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
const LINE_TERMINATORS: &str = r"\n\r\x{2028}\x{2029}";
const NEVER: &str = r"[^\x{0}-\x{10FFFF}]";
const ANY: &str = r"[\x{0}-\x{10FFFF}]";

/// `String.prototype.length`: the UTF-16 length of `s`.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The `RegExp` metacharacters escaped, as the TypeScript `escapeRegex` helpers do:
/// `s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')`.
pub fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(c, '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// --- translation --------------------------------------------------------------

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

struct Ctx {
    unicode: bool,
    /// Number of capturing groups in the whole pattern (decides `\N` meaning).
    groups: usize,
}

/// The escape sequence at `c[i]` (a backslash).
fn escape_atom(c: &[char], i: usize, in_class: bool, cx: &Ctx) -> Result<Atom, String> {
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
        's' => plain(&if in_class { JS_SPACE.to_string() } else { format!("[{JS_SPACE}]") }, true),
        'S' => plain(&if in_class { r"\S".to_string() } else { format!("[^{JS_SPACE}]") }, true),
        'b' if in_class => code_atom(8, next),
        'b' => plain(r"\b", false),
        'B' if in_class => plain("B", false),
        'B' => plain(r"\B", false),
        'n' | 'r' | 't' | 'f' => plain(&format!("\\{e}"), false),
        'v' => code_atom(0x0B, next),
        '0' if !c.get(i + 2).is_some_and(char::is_ascii_digit) => code_atom(0, next),
        '1'..='9' if !in_class => {
            // A back-reference when that many groups exist, else (non-unicode) a legacy octal escape.
            let mut j = i + 1;
            let mut n = 0usize;
            while c.get(j).is_some_and(char::is_ascii_digit) && n * 10 + c[j].to_digit(10).unwrap() as usize <= cx.groups.max(9) {
                n = n * 10 + c[j].to_digit(10).unwrap() as usize;
                j += 1;
                if n > cx.groups {
                    break;
                }
            }
            if n >= 1 && n <= cx.groups {
                Atom { text: format!("(?:\\{n})"), next: j, set: false }
            } else if e <= '7' {
                legacy_octal(c, i)
            } else {
                plain(&e.to_string(), false)
            }
        }
        '0'..='7' => legacy_octal(c, i),
        '8' | '9' => plain(&e.to_string(), false),
        'x' => match hex_val(c, i + 2, 2) {
            Some(v) => code_atom(v, i + 4),
            None => plain("x", false),
        },
        'u' => {
            if c.get(i + 2) == Some(&'{') && cx.unicode {
                let mut j = i + 3;
                let mut v = 0u32;
                while let Some(d) = c.get(j).and_then(|d| d.to_digit(16)) {
                    v = v * 16 + d;
                    j += 1;
                }
                if c.get(j) == Some(&'}') && j > i + 3 {
                    return Ok(code_atom(v, j + 1));
                }
                return Err("Invalid Unicode escape".to_string());
            }
            match hex_val(c, i + 2, 4) {
                Some(hi) if (0xD800..0xDC00).contains(&hi) && c.get(i + 6) == Some(&'\\') && c.get(i + 7) == Some(&'u') => {
                    match hex_val(c, i + 8, 4) {
                        Some(lo) if (0xDC00..0xE000).contains(&lo) => code_atom(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00), i + 12),
                        _ => code_atom(0xFFFD, i + 6),
                    }
                }
                Some(unit) if (0xD800..0xE000).contains(&unit) => code_atom(0xFFFD, i + 6),
                Some(unit) => code_atom(unit, i + 6),
                None => plain("u", false),
            }
        }
        'c' => match c.get(i + 2) {
            Some(l) if l.is_ascii_alphabetic() => code_atom(*l as u32 % 32, i + 3),
            _ => Atom { text: r"\\".to_string(), next: i + 1, set: false },
        },
        'p' | 'P' if cx.unicode && c.get(i + 2) == Some(&'{') => {
            let mut j = i + 3;
            while j < c.len() && c[j] != '}' {
                j += 1;
            }
            if j >= c.len() {
                return Err("Invalid property name".to_string());
            }
            let body: String = c[i + 3..j].iter().collect();
            let body = body.strip_prefix("General_Category=").or_else(|| body.strip_prefix("gc=")).unwrap_or(&body);
            let body = body.strip_prefix("Script=").or_else(|| body.strip_prefix("sc=")).unwrap_or(body);
            let body = body.strip_prefix("Script_Extensions=").or_else(|| body.strip_prefix("scx=")).unwrap_or(body);
            Atom { text: format!("\\{e}{{{body}}}"), next: j + 1, set: true }
        }
        'k' if c.get(i + 2) == Some(&'<') => {
            let mut j = i + 3;
            while j < c.len() && c[j] != '>' {
                j += 1;
            }
            let name: String = c[i + 3..j.min(c.len())].iter().collect();
            Atom { text: format!("\\k<{name}>"), next: (j + 1).min(c.len() + 1), set: false }
        }
        _ if e.is_ascii_alphanumeric() => plain(&e.to_string(), false),
        _ if e.is_ascii() => plain(&format!("\\{e}"), false),
        _ => Atom { text: e.to_string(), next, set: false },
    })
}

fn legacy_octal(c: &[char], i: usize) -> Atom {
    let e = c[i + 1];
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

/// The class starting at `c[i]` (`[`); returns the index after its `]`.
fn class_to_pcre(c: &[char], i: usize, out: &mut String, cx: &Ctx) -> Result<usize, String> {
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
                let atom = escape_atom(c, j, true, cx)?;
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
            // PCRE2 reads these inside a class where JavaScript takes them literally.
            '&' | '~' | '|' | '#' | ' ' | '^' => {
                out.push('\\');
                out.push(c[j]);
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

fn count_groups(c: &[char]) -> usize {
    let mut n = 0;
    let mut i = 0;
    let mut in_class = false;
    while i < c.len() {
        match c[i] {
            '\\' => i += 1,
            '[' => in_class = true,
            ']' => in_class = false,
            '(' if !in_class => {
                if c.get(i + 1) != Some(&'?') || (c.get(i + 2) == Some(&'<') && !matches!(c.get(i + 3), Some('=' | '!'))) {
                    n += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
}

/// A regex source written for JavaScript as PCRE2 source with the same meaning.
pub fn js_to_pcre(src: &str, multiline: bool, dot_all: bool, unicode: bool) -> Result<String, String> {
    let c: Vec<char> = src.chars().collect();
    let n = c.len();
    let cx = Ctx { unicode, groups: count_groups(&c) };
    let mut out = String::with_capacity(src.len() + 24);
    let mut i = 0;
    let mut prev_quantifiable = false;
    while i < n {
        match c[i] {
            '\\' => {
                let atom = escape_atom(&c, i, false, &cx)?;
                out.push_str(&atom.text);
                i = atom.next;
                prev_quantifiable = !matches!(c.get(i.wrapping_sub(1)), None) && !matches!(atom.text.as_str(), r"\b" | r"\B");
            }
            '[' => {
                i = class_to_pcre(&c, i, &mut out, &cx)?;
                prev_quantifiable = true;
            }
            '.' => {
                out.push_str(if dot_all { ANY } else { r"[^\n\r\x{2028}\x{2029}]" });
                i += 1;
                prev_quantifiable = true;
            }
            '^' => {
                out.push_str(&if multiline { format!(r"(?:\A|(?<=[{LINE_TERMINATORS}]))") } else { r"\A".to_string() });
                i += 1;
                prev_quantifiable = false;
            }
            '$' => {
                out.push_str(&if multiline { format!(r"(?=[{LINE_TERMINATORS}]|\z)") } else { r"\z".to_string() });
                i += 1;
                prev_quantifiable = false;
            }
            '{' => match quantifier_len(&c, i) {
                Some(len) if prev_quantifiable || !unicode => {
                    out.extend(&c[i..i + len]);
                    i += len;
                }
                _ => {
                    out.push_str(r"\{");
                    i += 1;
                    prev_quantifiable = true;
                }
            },
            '}' => {
                out.push_str(r"\}");
                i += 1;
                prev_quantifiable = true;
            }
            '(' => {
                // `(?<name>` is the same in PCRE2; lookarounds and `(?:` pass through.
                out.push('(');
                i += 1;
                prev_quantifiable = false;
            }
            ')' => {
                out.push(')');
                i += 1;
                prev_quantifiable = true;
            }
            '*' | '+' | '?' => {
                out.push(c[i]);
                i += 1;
                prev_quantifiable = false;
            }
            '|' => {
                out.push('|');
                i += 1;
                prev_quantifiable = false;
            }
            ch => {
                out.push(ch);
                i += 1;
                prev_quantifiable = true;
            }
        }
    }
    Ok(out)
}

// --- the regex ------------------------------------------------------------------

type Pool = Arc<Mutex<Vec<CaptureLocations>>>;

/// A compiled `RegExp`.
pub struct Re {
    inner: Regex,
    pool: Pool,
    names: Arc<[(String, usize)]>,
    group_count: usize,
    source: String,
}

impl std::fmt::Debug for Re {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "/{}/", self.source)
    }
}

impl Re {
    /// Compile `/source/flags`. Panics on an invalid pattern — a literal in the
    /// source is a bug if it does not compile, and a regex built from data goes
    /// through [`Re::try_new`].
    pub fn new(source: &str, flags: &str) -> Re {
        Re::try_new(source, flags).unwrap_or_else(|e| panic!("bad regex /{source}/{flags}: {e}"))
    }

    pub fn try_new(source: &str, flags: &str) -> Result<Re, String> {
        let multiline = flags.contains('m');
        let pcre = js_to_pcre(source, multiline, flags.contains('s'), flags.contains('u'))?;
        let sticky = flags.contains('y');
        let pattern = if sticky { format!(r"\G(?:{pcre})") } else { pcre };
        let inner = Regex::new(&pattern, flags.contains('i'))?;
        let mut names = Vec::new();
        for (idx, name) in inner.capture_names().iter().enumerate() {
            if let Some(name) = name {
                names.push((name.clone(), idx));
            }
        }
        let group_count = inner.captures_len();
        Ok(Re { inner, pool: Pool::default(), names: names.into(), group_count, source: source.to_string() })
    }

    /// A shared compiled regex for a pattern built at run time.
    pub fn cached(source: &str, flags: &str) -> Arc<Re> {
        static CACHE: LazyLock<Mutex<HashMap<(String, String), Arc<Re>>>> = LazyLock::new(Default::default);
        let key = (source.to_string(), flags.to_string());
        if let Some(hit) = CACHE.lock().unwrap().get(&key) {
            return Arc::clone(hit);
        }
        let re = Arc::new(Re::new(source, flags));
        let mut cache = CACHE.lock().unwrap();
        if cache.len() > 20_000 {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&re));
        re
    }

    /// Like [`Re::cached`] for data-derived patterns that may be invalid.
    pub fn try_cached(source: &str, flags: &str) -> Option<Arc<Re>> {
        static CACHE: LazyLock<Mutex<HashMap<(String, String), Option<Arc<Re>>>>> = LazyLock::new(Default::default);
        let key = (source.to_string(), flags.to_string());
        if let Some(hit) = CACHE.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let re = Re::try_new(source, flags).ok().map(Arc::new);
        let mut cache = CACHE.lock().unwrap();
        if cache.len() > 20_000 {
            cache.clear();
        }
        cache.insert(key, re.clone());
        re
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// `regex.test(s)` (a non-global regex: always from the start).
    pub fn is_match(&self, s: &str) -> bool {
        self.captures_at(s, 0).is_some()
    }

    /// `regex.exec(s)` with `lastIndex = 0`.
    pub fn captures<'t>(&self, s: &'t str) -> Option<Caps<'t>> {
        self.captures_at(s, 0)
    }

    /// `regex.exec(s)` with `lastIndex = pos`: the first match starting at or after
    /// `pos` (exactly at `pos` for a sticky regex). Lookbehind sees the text before it.
    pub fn captures_at<'t>(&self, s: &'t str, pos: usize) -> Option<Caps<'t>> {
        if pos > s.len() {
            return None;
        }
        let taken = self.pool.lock().unwrap().pop();
        let mut locs = taken.unwrap_or_else(|| self.inner.capture_locations(JIT_STACK_BYTES));
        match self.inner.captures_read_at(&mut locs, s, pos) {
            true => Some(Caps { text: s, locs: Some(locs), pool: Arc::clone(&self.pool), names: Arc::clone(&self.names), len: self.group_count }),
            _ => {
                self.pool.lock().unwrap().push(locs);
                None
            }
        }
    }

    /// `s.match(re)` for a non-global regex.
    pub fn find<'t>(&self, s: &'t str) -> Option<Match<'t>> {
        self.captures(s).and_then(|c| c.get(0))
    }

    /// `s.matchAll(globalRe)`: every non-overlapping match, JavaScript's empty-match advance included.
    pub fn captures_iter<'r, 't>(&'r self, s: &'t str) -> CapturesIter<'r, 't> {
        CapturesIter { re: self, text: s, pos: 0, done: false }
    }

    /// `s.replace(globalRe, fn)`. The replacer may return `None` to keep the match as written.
    pub fn replace_all(&self, s: &str, mut f: impl FnMut(&Caps) -> Option<String>) -> String {
        let mut out = String::with_capacity(s.len());
        let mut last = 0;
        for caps in self.captures_iter(s) {
            let m = caps.get(0).unwrap();
            out.push_str(&s[last..m.start()]);
            match f(&caps) {
                Some(rep) => out.push_str(&rep),
                None => out.push_str(m.as_str()),
            }
            last = m.end();
        }
        out.push_str(&s[last..]);
        out
    }

    /// `s.replace(re, fn)` for a non-global regex: only the first match.
    pub fn replace_first(&self, s: &str, f: impl FnOnce(&Caps) -> String) -> String {
        match self.captures(s) {
            Some(caps) => {
                let m = caps.get(0).unwrap();
                format!("{}{}{}", &s[..m.start()], f(&caps), &s[m.end()..])
            }
            None => s.to_string(),
        }
    }

    /// `s.replace(globalRe, replacement)` with a literal replacement (no `$` expansion).
    pub fn replace_all_str(&self, s: &str, replacement: &str) -> String {
        self.replace_all(s, |_| Some(replacement.to_string()))
    }

    /// `s.split(re)`: capture groups are spliced into the result, as JavaScript does.
    pub fn split<'t>(&self, s: &'t str) -> Vec<&'t str> {
        // Captured groups are rarely used with split in the ported code; they are
        // dropped here and `split_with_captures` is the faithful variant.
        let mut out = Vec::new();
        let mut last = 0;
        let mut pos = 0;
        if s.is_empty() {
            return if self.is_match(s) { Vec::new() } else { vec![s] };
        }
        while pos < s.len() {
            let Some(caps) = self.captures_at(s, pos) else { break };
            let m = caps.get(0).unwrap();
            if m.end() == m.start() && m.start() >= s.len() {
                break;
            }
            if m.end() == last && m.start() == m.end() {
                pos = next_char(s, pos);
                continue;
            }
            if m.start() == m.end() && m.start() == last {
                pos = next_char(s, pos);
                continue;
            }
            out.push(&s[last..m.start()]);
            last = m.end();
            pos = if m.end() == m.start() { next_char(s, m.end()) } else { m.end() };
        }
        out.push(&s[last..]);
        out
    }

    /// Number of capture groups, group 0 included.
    pub fn groups(&self) -> usize {
        self.group_count
    }
}

fn next_char(s: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// One group's span.
#[derive(Clone, Copy, Debug)]
pub struct Match<'t> {
    text: &'t str,
    start: usize,
    end: usize,
}

impl<'t> Match<'t> {
    pub fn start(&self) -> usize {
        self.start
    }
    pub fn end(&self) -> usize {
        self.end
    }
    pub fn as_str(&self) -> &'t str {
        &self.text[self.start..self.end]
    }
    /// The UTF-16 `index` of this match, as `RegExpExecArray.index` reports it.
    pub fn utf16_start(&self) -> usize {
        utf16_len(&self.text[..self.start])
    }
}

/// The groups of one match. Dropping it returns its buffer to the regex's pool.
pub struct Caps<'t> {
    text: &'t str,
    locs: Option<CaptureLocations>,
    pool: Pool,
    names: Arc<[(String, usize)]>,
    len: usize,
}

impl<'t> Caps<'t> {
    fn locs(&self) -> &CaptureLocations {
        self.locs.as_ref().expect("buffer present until drop")
    }

    /// Group `i` (0 = the whole match), if it participated.
    pub fn get(&self, i: usize) -> Option<Match<'t>> {
        if i >= self.len {
            return None;
        }
        self.locs().get(i).map(|(start, end)| Match { text: self.text, start, end })
    }

    /// Group `i` as text; `""` is a participating empty group, `None` one that did not match.
    pub fn text(&self, i: usize) -> Option<&'t str> {
        self.get(i).map(|m| m.as_str())
    }

    /// A named group's text.
    pub fn name(&self, name: &str) -> Option<&'t str> {
        let idx = self.names.iter().find(|(n, _)| n == name)?.1;
        self.text(idx)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Caps<'_> {
    fn drop(&mut self) {
        if let Some(locs) = self.locs.take() {
            self.pool.lock().unwrap().push(locs);
        }
    }
}

impl std::ops::Index<usize> for Caps<'_> {
    type Output = str;
    fn index(&self, i: usize) -> &str {
        let (start, end) = self.locs().get(i).unwrap_or_else(|| panic!("group {i} did not participate"));
        &self.text[start..end]
    }
}

pub struct CapturesIter<'r, 't> {
    re: &'r Re,
    text: &'t str,
    pos: usize,
    done: bool,
}

impl<'r, 't> Iterator for CapturesIter<'r, 't> {
    type Item = Caps<'t>;
    fn next(&mut self) -> Option<Caps<'t>> {
        if self.done || self.pos > self.text.len() {
            return None;
        }
        let caps = self.re.captures_at(self.text, self.pos)?;
        let m = caps.get(0).unwrap();
        self.pos = if m.end() == m.start() {
            if m.end() >= self.text.len() {
                self.done = true;
                m.end()
            } else {
                next_char(self.text, m.end())
            }
        } else {
            m.end()
        };
        Some(caps)
    }
}

/// `static` regexes: `re!(r"\bfoo\b")` / `re!(r"\bfoo\b", "i")`.
#[macro_export]
macro_rules! re {
    ($src:expr) => {{
        static R: ::std::sync::LazyLock<$crate::Re> = ::std::sync::LazyLock::new(|| $crate::Re::new($src, ""));
        &*R
    }};
    ($src:expr, $flags:expr) => {{
        static R: ::std::sync::LazyLock<$crate::Re> = ::std::sync::LazyLock::new(|| $crate::Re::new($src, $flags));
        &*R
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_vec(re: &Re, s: &str) -> Vec<Vec<Option<String>>> {
        re.captures_iter(s).map(|c| (0..c.len()).map(|i| c.text(i).map(String::from)).collect()).collect()
    }

    #[test]
    fn dot_excludes_line_terminators() {
        assert!(!Re::new("a.b", "").is_match("a\nb"));
        assert!(!Re::new("a.b", "").is_match("a\rb"));
        assert!(!Re::new("a.b", "").is_match("a\u{2028}b"));
        assert!(Re::new("a.b", "s").is_match("a\nb"));
        assert!(Re::new("a.b", "").is_match("a\u{85}b"));
    }

    #[test]
    fn anchors() {
        assert!(!Re::new("a$", "").is_match("a\n"));
        assert!(Re::new("a$", "m").is_match("a\nb"));
        assert!(Re::new("a$", "m").is_match("a\r\nb"));
        assert!(Re::new("^b", "m").is_match("a\r\nb"));
        assert!(!Re::new("^b", "").is_match("a\nb"));
        // JS: after a lone \r in \r\n, ^ matches too
        assert_eq!(Re::new("^", "mg").captures_iter("a\r\nb").count(), 3);
    }

    #[test]
    fn shorthand_sets() {
        assert!(Re::new(r"a\sb", "").is_match("a\u{A0}b"));
        assert!(Re::new(r"a\sb", "").is_match("a\u{FEFF}b"));
        assert!(Re::new(r"[\s\d]+", "").is_match("1 2"));
        assert!(!Re::new(r"\w", "").is_match("é"));
        assert!(Re::new(r"[^\s]+", "").is_match("x"));
        assert!(Re::new(r"[\w$]+\s*=", "").is_match("$x_1 ="));
    }

    #[test]
    fn braces_and_classes() {
        assert!(Re::new(r"a{", "").is_match("a{"));
        assert!(Re::new(r"a{2}", "").is_match("aa"));
        assert!(Re::new(r"x{,3}", "").is_match("x{,3}"));
        assert!(Re::new(r"[]a", "").captures("a").is_none());
        assert!(Re::new(r"[^]", "").is_match("\n"));
        assert!(Re::new(r"[a-z&&b]", "").is_match("&"));
        assert!(Re::new(r"[\]]", "").is_match("]"));
        assert!(Re::new(r"[[]", "").is_match("["));
        assert!(Re::new(r"[\-a]", "").is_match("-"));
        assert!(Re::new(r"\/", "").is_match("/"));
    }

    #[test]
    fn lookaround_backrefs_names() {
        let re = Re::new(r"(?<![\w$])[A-Z]+(?=\()", "");
        assert!(re.is_match("x FOO("));
        assert!(!re.is_match("xFOO("));
        assert!(Re::new(r"(a)\1", "").is_match("aa"));
        assert!(Re::new(r"(?<q>['\x22]).*?\k<q>", "").is_match("'abc'"));
        let re = Re::new(r"(?<n>\w+)=(\d+)", "");
        let c = re.captures("x=12").unwrap();
        assert_eq!(c.name("n"), Some("x"));
        assert_eq!(c.text(2), Some("12"));
    }

    #[test]
    fn octal_vs_backreference() {
        // no group 1: `\1` is a legacy octal escape (U+0001)
        assert!(Re::new(r"a\1", "").is_match("a\u{1}"));
        assert!(Re::new(r"(a)(?:b)\1", "").is_match("aba"));
    }

    #[test]
    fn global_iteration_and_empty_matches() {
        let re = Re::new(r"x*", "g");
        let all = caps_vec(&re, "abc");
        assert_eq!(all.len(), 4);
        assert_eq!(re.replace_all("abc", |_| Some("-".into())), "-a-b-c-");
        assert_eq!(Re::new(r"\d+", "g").replace_all("a1b22", |c| Some(format!("<{}>", &c[0]))), "a<1>b<22>");
        assert_eq!(caps_vec(&Re::new("é", "g"), "éé").len(), 2);
    }

    #[test]
    fn sticky_and_last_index() {
        let st = Re::new(r"[a-z]+(?=;)", "y");
        assert!(st.captures_at("12abc;", 2).is_some());
        assert!(st.captures_at("12abc;", 1).is_none());
        let re = Re::new(r"(?<![\w$])ab", "");
        assert!(re.captures_at("xab", 1).is_none());
        assert!(re.captures_at("x ab", 2).is_some());
    }

    #[test]
    fn caseless() {
        assert!(Re::new(r"abc", "i").is_match("ABC"));
        assert!(Re::new(r"[a-c]+", "i").is_match("B"));
    }

    #[test]
    fn split_like_js() {
        assert_eq!(Re::new(r"\r?\n", "").split("a\r\nb\nc"), vec!["a", "b", "c"]);
        assert_eq!(Re::new(r",", "").split(""), vec![""]);
        assert_eq!(Re::new(r"", "").split("abc"), vec!["a", "b", "c"]);
        assert_eq!(Re::new(r"::|->|\.", "").split("a::b.c"), vec!["a", "b", "c"]);
        assert_eq!(Re::new(r"\.", "").split(".a."), vec!["", "a", ""]);
    }

    #[test]
    fn unicode_flag_escapes() {
        assert!(Re::new(r"\u{1F600}", "u").is_match("😀"));
        assert!(Re::new(r"\p{L}+", "u").is_match("é"));
        assert!(Re::new(r"é", "").is_match("é"));
    }

    #[test]
    fn escape_regex_matches_the_ts_helper() {
        assert_eq!(escape_regex("a.b*c(d)[e]\\$^|+?{}"), r"a\.b\*c\(d\)\[e\]\\\$\^\|\+\?\{\}");
        assert!(Re::new(&escape_regex("$x"), "").is_match("a$x"));
    }
}

#[cfg(test)]
mod astral_tests {
    use super::*;
    #[test]
    fn empty_match_before_astral() {
        let re = Re::new(r"^[\p{L}\p{Nd}_]*", "u");
        let c = re.captures("😀 emoji");
        assert!(c.is_some(), "no match");
        assert!(Re::new(r"^x*", "").captures("😀 emoji").is_some());
        assert!(Re::new(r"(?<![\p{L}\p{N}_])", "u").captures("😀 emoji").is_some());
    }
}

#[cfg(test)]
mod idprop_tests {
    use super::*;
    #[test]
    fn id_start_and_continue() {
        let r = Re::new(r"^[$_\p{ID_Start}][$‌‍\p{ID_Continue}]*$", "u");
        assert!(r.is_match("fooBar1"));
        assert!(r.is_match("$x"));
        assert!(r.is_match("é"));
        assert!(!r.is_match("1a"));
    }
}
