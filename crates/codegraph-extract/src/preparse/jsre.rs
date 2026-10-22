//! A thin JavaScript-flavoured regex layer for the `preParse` ports.
//!
//! The TypeScript hooks are written against `RegExp` over UTF-16 strings. The
//! ports run on an ASCII *shadow* of the source (see `shadow.rs`), where one byte
//! is one UTF-16 unit, so the patterns carry over nearly verbatim. They need
//! lookahead, lookbehind and back-references, which the `regex` crate does not
//! have and `fancy-regex` runs ~50x slower than V8; PCRE2 (JIT-compiled) has them
//! all, with the same backtracking semantics as `RegExp`.
//!
//! What still differs from JavaScript, and is handled here:
//!
//! * `.` — JavaScript's excludes `\r` as well as `\n`;
//! * `$` — without the `m` flag PCRE2's also matches before a final `\n`
//!   (it becomes `\z`);
//! * line anchors under the `m` flag recognise `\r`, `\n` and `\r\n` (PCRE2's
//!   ANYCRLF); a lone U+2028/U+2029 is not a line break here;
//! * errors (a match or JIT-stack limit) read as "no match", never a panic.
//!
//! `\w` `\s` `\d` `\b` are ASCII in non-UTF mode, as they are in JavaScript.

use pcre2::bytes::{CaptureLocations, Regex, RegexBuilder};
use std::ops::Index;
use std::sync::{Arc, Mutex};

/// Room for the deepest nested-group backtracking a real source reaches.
const JIT_STACK_BYTES: usize = 16 << 20;

/// `pattern` for PCRE2: `.` becomes `[^\n\r]`; `$` becomes `\z` unless `multiline`.
fn translate(pattern: &str, multiline: bool) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut in_class = false;
    let mut prev: Option<char> = None;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                prev = Some('x');
                continue;
            }
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '.' if !in_class => {
                out.push_str(r"[^\n\r]");
                prev = Some('x');
                continue;
            }
            '$' if !in_class && !multiline => {
                out.push_str(r"\z");
                prev = Some('x');
                continue;
            }
            // `*+` `++` `?+` are possessive in PCRE2 and a syntax error in JavaScript.
            '+' if !in_class && matches!(prev, Some('*' | '+' | '?')) => {
                panic!("possessive quantifier in preParse pattern {pattern:?}")
            }
            _ => {}
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

fn build(pattern: &str, multiline: bool) -> Regex {
    let flags = if multiline { "(*ANYCRLF)(?m)" } else { "" };
    let translated = format!("{flags}{}", translate(pattern, multiline));
    RegexBuilder::new()
        .jit_if_available(true)
        .max_jit_stack_size(Some(JIT_STACK_BYTES))
        .build(&translated)
        .unwrap_or_else(|e| panic!("bad preParse pattern {pattern:?}: {e}"))
}

/// A match: byte offsets into the haystack (UTF-16 offsets, on the shadow).
#[derive(Clone, Copy)]
pub(crate) struct Mat<'t> {
    text: &'t str,
    start: usize,
    end: usize,
}

impl<'t> Mat<'t> {
    pub fn start(&self) -> usize {
        self.start
    }
    pub fn end(&self) -> usize {
        self.end
    }
    pub fn as_str(&self) -> &'t str {
        &self.text[self.start..self.end]
    }
}

/// Match buffers, kept for reuse: creating one allocates a JIT stack, which is
/// far costlier than the match itself.
type Pool = Arc<Mutex<Vec<CaptureLocations>>>;

/// The groups of one match. Dropping it returns its buffer to the pool.
pub(crate) struct Caps<'t> {
    text: &'t str,
    locs: Option<CaptureLocations>,
    pool: Pool,
}

impl<'t> Caps<'t> {
    fn locs(&self) -> &CaptureLocations {
        self.locs.as_ref().expect("buffer present until drop")
    }

    pub fn get(&self, i: usize) -> Option<Mat<'t>> {
        self.locs().get(i).map(|(start, end)| Mat { text: self.text, start, end })
    }
}

impl Drop for Caps<'_> {
    fn drop(&mut self) {
        if let Some(locs) = self.locs.take() {
            self.pool.lock().unwrap().push(locs);
        }
    }
}

impl Index<usize> for Caps<'_> {
    type Output = str;
    fn index(&self, i: usize) -> &str {
        let (start, end) = self.locs().get(i).unwrap_or_else(|| panic!("group {i} did not participate"));
        &self.text[start..end]
    }
}

fn run<'t>(re: &Regex, pool: &Pool, s: &'t str, pos: usize) -> Option<Caps<'t>> {
    if pos > s.len() {
        return None;
    }
    let taken = pool.lock().unwrap().pop();
    let mut locs = taken.unwrap_or_else(|| re.capture_locations());
    match re.captures_read_at(&mut locs, s.as_bytes(), pos) {
        Ok(Some(_)) => Some(Caps { text: s, locs: Some(locs), pool: Arc::clone(pool) }),
        _ => {
            pool.lock().unwrap().push(locs);
            None
        }
    }
}

/// A compiled pattern with the JavaScript-flavoured entry points the ports use.
pub(crate) struct Re(Regex, Pool);

impl Re {
    pub fn new(pattern: &str) -> Re {
        Re(build(pattern, false), Pool::default())
    }

    /// The pattern with the `m` flag: `^` / `$` match at line breaks.
    pub fn multiline(pattern: &str) -> Re {
        Re(build(pattern, true), Pool::default())
    }

    pub fn is_match(&self, s: &str) -> bool {
        self.0.is_match(s.as_bytes()).unwrap_or(false)
    }

    /// `exec` from `pos`: the first match starting at or after it. Lookbehind
    /// sees the text before `pos`, as with a global `RegExp`'s `lastIndex`.
    pub fn captures_at<'t>(&self, s: &'t str, pos: usize) -> Option<Caps<'t>> {
        run(&self.0, &self.1, s, pos)
    }

    pub fn captures<'t>(&self, s: &'t str) -> Option<Caps<'t>> {
        self.captures_at(s, 0)
    }

    /// `String.prototype.replace` with a global pattern and a function replacer.
    /// The replacer may return `None` to keep the match as written.
    pub fn replace_all(&self, s: &str, mut f: impl FnMut(&Caps) -> Option<String>) -> String {
        let mut out = String::with_capacity(s.len());
        let mut last = 0;
        let mut pos = 0;
        while let Some(caps) = self.captures_at(s, pos) {
            let m = caps.get(0).unwrap();
            out.push_str(&s[last..m.start()]);
            match f(&caps) {
                Some(rep) => out.push_str(&rep),
                None => out.push_str(m.as_str()),
            }
            last = m.end();
            pos = if m.end() == m.start() { m.end() + 1 } else { m.end() };
            if pos > s.len() {
                break;
            }
        }
        if last <= s.len() {
            out.push_str(&s[last..]);
        }
        out
    }
}

/// A sticky (`y`) pattern: it must match exactly at the position given.
pub(crate) struct Sticky(Regex, Pool);

impl Sticky {
    pub fn new(pattern: &str) -> Sticky {
        Sticky(build(&format!(r"\G(?:{pattern})"), false), Pool::default())
    }

    /// The match starting at `pos`, if any (offsets are relative to `s`).
    pub fn at<'t>(&self, s: &'t str, pos: usize) -> Option<(usize, usize, Caps<'t>)> {
        let caps = run(&self.0, &self.1, s, pos)?;
        let end = caps.get(0)?.end();
        Some((pos, end, caps))
    }

    pub fn matches_at(&self, s: &str, pos: usize) -> Option<usize> {
        self.at(s, pos).map(|(_, end, _)| end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_stops_at_carriage_return() {
        assert_eq!(Re::new(r"//.*$").replace_all("a // b\r", |_| Some(String::new())), "a // b\r");
        assert_eq!(Re::new(r"//.*$").replace_all("a // b", |_| Some(String::new())), "a ");
        assert!(Re::new(r"[.]x").is_match(".x"));
        assert!(Re::new(r"a\.b").is_match("a.b"));
        assert!(!Re::new(r"a\.b").is_match("axb"));
    }

    #[test]
    fn dollar_is_end_of_input_unless_multiline() {
        assert!(!Re::new(r"a$").is_match("a\n"));
        assert!(Re::new(r"a$").is_match("a"));
        assert!(Re::multiline(r"a$").is_match("a\nb"));
        assert!(Re::multiline(r"a$").is_match("a\r\nb"));
        assert!(Re::multiline(r"^b").is_match("a\nb"));
    }

    #[test]
    fn shorthands_are_ascii() {
        let re = Re::new(r#"\bfoo\b[^"\\]*\s\d\w"#);
        assert!(re.is_match("a foo xyz 1_"));
        assert!(!re.is_match("afoo xyz 1_"));
        assert!(Re::new(r"[\w$]+\s*=").is_match("$x_1 ="));
        assert!(Re::new(r"[^\]]+\]").is_match("ab]"));
        assert!(!Re::new(r"^[^\n]+$").is_match("a\nb"));
        assert!(Re::new(r"a\Wb").is_match("a-b"));
        assert!(Re::new(r"a\sb").is_match("a\x0Bb"));
    }

    #[test]
    fn lookaround_and_backrefs() {
        let re = Re::new(r"(?<![\w$])[A-Z]+(?=\()");
        assert!(re.is_match("x FOO("));
        assert!(!re.is_match("xFOO("));
        assert!(Re::new(r"(a)\1").is_match("aa"));
    }

    #[test]
    fn lookbehind_sees_text_before_the_start() {
        let re = Re::new(r"(?<![\w$])ab");
        assert!(re.captures_at("xab", 1).is_none());
        assert!(re.captures_at("x ab", 2).is_some());
    }

    #[test]
    fn sticky_matches_only_in_place() {
        let st = Sticky::new(r"[a-z]+(?=;)");
        assert_eq!(st.matches_at("12abc;", 2), Some(5));
        assert_eq!(st.matches_at("12abc;", 1), None);
    }

    #[test]
    fn global_replace_follows_js_empty_match_rules() {
        assert_eq!(Re::new(r"x*").replace_all("abc", |_| Some("-".into())), "-a-b-c-");
    }

    #[test]
    #[should_panic(expected = "possessive")]
    fn possessive_quantifiers_are_refused() {
        Re::new(r"a*+");
    }
}
