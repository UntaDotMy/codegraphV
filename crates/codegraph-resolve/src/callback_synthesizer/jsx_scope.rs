//! Reading what a function's JSX span binds: tags, bare names, destructuring patterns. The text
//! walkers of `src/resolution/callback-synthesizer.ts` (`spanTags`, `destructures`,
//! `bindingElementAt` and their helpers).
//!
//! The walkers index by byte; every character they compare is ASCII, so a byte index agrees with
//! JavaScript's UTF-16 one wherever a comparison could succeed. Whitespace is read with the
//! Unicode `\s` set, decoding multi-byte characters.

use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::is_js_space;
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// How far a bracket walk reads: no pattern or parameter list runs longer.
const PATTERN_REACH: isize = 10_000;
const CONTROL_HEADS: [&str; 5] = ["if", "while", "for", "switch", "with"];

/// A tag's name ends at whitespace, `/` or `>`, or at the `<` of the type arguments a generic
/// component's tag passes.
pub fn jsx_tag_re() -> std::sync::Arc<Re> {
    Re::cached(r"<([A-Z][A-Za-z0-9_]*)(?=[\s/><])", "g")
}

/// `/[\w$]/.test(c)` over one character (`-1`: nothing there).
fn id_char(c: i32) -> bool {
    c >= 0 && ((c as u8).is_ascii_alphanumeric() || c == b'_' as i32 || c == b'$' as i32) && c < 128
}

/// The text the walkers read.
pub struct Code<'a> {
    s: &'a str,
    b: &'a [u8],
}

impl<'a> Code<'a> {
    pub fn new(s: &'a str) -> Self {
        Code { s, b: s.as_bytes() }
    }

    /// `code[i]` as a code unit's value; `-1` out of range, and `0x80` for any non-ASCII byte
    /// (never equal to a character the walkers compare against).
    fn c(&self, i: isize) -> i32 {
        if i < 0 || i as usize >= self.b.len() {
            -1
        } else {
            let b = self.b[i as usize];
            if b < 0x80 {
                b as i32
            } else {
                0x80
            }
        }
    }

    fn len(&self) -> isize {
        self.b.len() as isize
    }

    /// The `\s`-skipping walk: forward (`step` 1) or backward (-1) from `at`.
    fn skip_space(&self, mut at: isize, step: isize) -> isize {
        loop {
            if at < 0 || at >= self.len() {
                return at;
            }
            let byte = self.b[at as usize];
            if byte < 0x80 {
                if is_js_space(byte as char) {
                    at += step;
                    continue;
                }
                return at;
            }
            // A multi-byte character: find where it starts and decode it.
            let mut st = at as usize;
            while st > 0 && !self.s.is_char_boundary(st) {
                st -= 1;
            }
            let ch = self.s[st..].chars().next().unwrap();
            if is_js_space(ch) {
                at = if step > 0 { (st + ch.len_utf8()) as isize } else { st as isize - 1 };
            } else {
                return at;
            }
        }
    }

    /// The `(`, `[` or `{` around `from`, past balanced brackets; -1 when none is near.
    fn enclosing_opener(&self, from: isize) -> isize {
        let mut depth = 0;
        let stop = (from - PATTERN_REACH).max(0);
        let mut i = from;
        while i >= stop {
            let c = self.c(i);
            if c == b')' as i32 || c == b']' as i32 || c == b'}' as i32 {
                depth += 1;
            } else if c == b'(' as i32 || c == b'[' as i32 || c == b'{' as i32 {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            i -= 1;
        }
        -1
    }

    /// The bracket that closes the one at `open`; -1 when none is near.
    fn matching_closer(&self, open: isize) -> isize {
        let mut depth = 0;
        let stop = self.len().min(open + PATTERN_REACH);
        let mut i = open;
        while i < stop {
            let c = self.c(i);
            if c == b'(' as i32 || c == b'[' as i32 || c == b'{' as i32 {
                depth += 1;
            } else if c == b')' as i32 || c == b']' as i32 || c == b'}' as i32 {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            i += 1;
        }
        -1
    }

    /// The identifier that ends at `end`.
    fn word_ending_at(&self, end: isize) -> &'a str {
        let mut start = end;
        while start > 0 && id_char(self.c(start - 1)) {
            start -= 1;
        }
        let from = start.max(0) as usize;
        let to = ((end + 1).max(0) as usize).min(self.b.len());
        if from >= to {
            return "";
        }
        &self.s[from..to]
    }

    /// `PARAMETERS_END`: after a parameter list, a return type on its line, then `=>` or a body.
    fn parameters_end_at(&self, at: isize) -> bool {
        if at < 0 || at > self.len() {
            return false;
        }
        let rest = &self.s[at as usize..];
        re!(r"^\s*(?::[^=;{}()\n]*)?(?:=>|\{)").is_match(rest)
    }

    /// `IMPORTED_VALUE`: after a declaration's pattern, its type, then a `require(…)` or
    /// `import(…)` value.
    fn imported_value_at(&self, at: isize) -> bool {
        if at < 0 || at > self.len() {
            return false;
        }
        let rest = &self.s[at as usize..];
        re!(r"^\s*(?::[^=;]*)?=\s*(?:await\s+)?(?:require|import)\s*\(").is_match(rest)
    }

    /// Where the `index`th line of the code, counted from 0, ends.
    pub fn line_end(&self, index: i64) -> isize {
        let mut at: isize = -1;
        let mut i = 0;
        while i <= index {
            match self.s[(at + 1) as usize..].find('\n') {
                Some(p) => at = at + 1 + p as isize,
                None => return self.len(),
            }
            i += 1;
        }
        at
    }

    /// Whether `src`, before `end`, binds `name` in a destructuring pattern (`destructures`).
    pub fn destructures(&self, name: &str, end: isize) -> bool {
        let mut from = 0usize;
        while let Some(p) = self.s.get(from..).and_then(|t| t.find(name)) {
            let at = from + p;
            if at as isize >= end {
                break;
            }
            if self.binding_element_at(at as isize, name.len() as isize) {
                return true;
            }
            from = at + name.len().max(1);
        }
        false
    }

    /// The patterns the code destructures at its top, as [opener, closer] pairs.
    pub fn top_level_patterns(&self) -> Vec<(isize, isize)> {
        let mut patterns = Vec::new();
        let mut depth = 0;
        let mut at = 0usize;
        let decl = re!(r"\b(?:const|let|var)\s*[{[]", "g");
        for m in decl.captures_iter(self.s) {
            let whole = m.get(0).unwrap();
            while at < whole.start() {
                if self.b[at] == b'{' {
                    depth += 1;
                } else if self.b[at] == b'}' {
                    depth -= 1;
                }
                at += 1;
            }
            if depth != 0 {
                continue;
            }
            let open = whole.end() as isize - 1;
            let close = self.matching_closer(open);
            if close > open {
                patterns.push((open, close));
            }
        }
        patterns
    }

    /// Whether one of `patterns` binds `name` (`bindingElementAt`).
    pub fn patterns_bind(&self, patterns: &[(isize, isize)], name: &str) -> bool {
        for &(open, close) in patterns {
            let mut from = open.max(0) as usize;
            while let Some(p) = self.s.get(from..).and_then(|t| t.find(name)) {
                let at = from + p;
                if at as isize >= close {
                    break;
                }
                if self.binding_element_at(at as isize, name.len() as isize) {
                    return true;
                }
                from = at + name.len().max(1);
            }
        }
        false
    }

    /// Whether the name at `at` is an element of a pattern that binds it.
    fn binding_element_at(&self, at: isize, length: isize) -> bool {
        if id_char(self.c(at - 1)) || id_char(self.c(at + length)) {
            return false;
        }
        let next = self.skip_space(at + length, 1);
        let after = self.c(next);
        if after != b',' as i32
            && after != b'}' as i32
            && after != b']' as i32
            && (after != b'=' as i32 || self.c(next + 1) == b'=' as i32 || self.c(next + 1) == b'>' as i32)
        {
            return false;
        }
        let mut prev = self.skip_space(at - 1, -1);
        // A rest element; any other `.` reads a member.
        if self.c(prev) == b'.' as i32 {
            if self.c(prev - 1) != b'.' as i32 || self.c(prev - 2) != b'.' as i32 {
                return false;
            }
            prev = self.skip_space(prev - 3, -1);
        }
        let mut opener = prev;
        if self.c(prev) == b',' as i32 {
            opener = self.enclosing_opener(prev - 1);
        } else if self.c(prev) == b':' as i32 {
            // A rename's target: its key opens an element of an object pattern.
            if !self.key_opens_element(prev) {
                return false;
            }
            opener = self.enclosing_opener(prev - 1);
            if self.c(opener) != b'{' as i32 {
                return false;
            }
        }
        (self.c(opener) == b'{' as i32 || self.c(opener) == b'[' as i32) && self.binds_pattern(opener, 0)
    }

    /// Whether the `{` or `[` at `opener` opens a pattern that binds its names: a declaration's,
    /// unless it declares an import, or a parameter's.
    fn binds_pattern(&self, opener: isize, depth: u32) -> bool {
        if depth > 8 {
            return false;
        }
        let p = self.skip_space(opener - 1, -1);
        let c = self.c(p);
        if c == b'(' as i32 {
            return self.opens_parameters(p);
        }
        if c == b'[' as i32 {
            return self.binds_pattern(p, depth + 1);
        }
        if c == b',' as i32 {
            let q = self.enclosing_opener(p - 1);
            if self.c(q) == b'(' as i32 {
                return self.opens_parameters(q);
            }
            return self.c(q) == b'[' as i32 && self.binds_pattern(q, depth + 1);
        }
        if c == b':' as i32 {
            // `{ key: { … } }` nests a pattern; `props: {` and `}: {` annotate a type.
            if !self.key_opens_element(p) {
                return false;
            }
            let q = self.enclosing_opener(p - 1);
            return self.c(q) == b'{' as i32 && self.binds_pattern(q, depth + 1);
        }
        if !id_char(c) {
            return false;
        }
        let word = self.word_ending_at(p);
        if word != "const" && word != "let" && word != "var" {
            return false;
        }
        let close = self.matching_closer(opener);
        if close < 0 {
            return false;
        }
        !self.imported_value_at(close + 1)
    }

    /// Whether the `(` at `paren` opens a parameter list: an arrow or a body follows it, and no
    /// control-flow keyword precedes it.
    fn opens_parameters(&self, paren: isize) -> bool {
        let p = self.skip_space(paren - 1, -1);
        if id_char(self.c(p)) && CONTROL_HEADS.contains(&self.word_ending_at(p)) {
            return false;
        }
        let close = self.matching_closer(paren);
        if close < 0 {
            return false;
        }
        self.parameters_end_at(close + 1)
    }

    /// Whether a property key ends at the `:` at `colon` and opens its element.
    fn key_opens_element(&self, colon: isize) -> bool {
        let mut k = self.skip_space(colon - 1, -1);
        let c = self.c(k);
        if c == b']' as i32 {
            k = self.enclosing_opener(k - 1) - 1;
        } else if c == b'"' as i32 || c == b'\'' as i32 {
            // `code.lastIndexOf(c, k - 1)`: a negative start reads as 0.
            let from = (k - 1).max(0) as usize;
            let found = self.s.get(..(from + 1).min(self.b.len())).and_then(|t| t.rfind(c as u8 as char));
            k = found.map_or(-1, |p| p as isize) - 1;
        } else if id_char(c) {
            while k >= 0 && id_char(self.c(k)) {
                k -= 1;
            }
        } else {
            return false;
        }
        let b = self.c(self.skip_space(k, -1));
        b == b'{' as i32 || b == b',' as i32
    }
}

/// What a function's span holds: each tag's name and the line of its first tag (0 while only type
/// argument or parameter lists name it), the names it also writes outside a tag, its code, and
/// whether that code destructures a name by a line.
pub struct SpanTags {
    pub tags: IndexMap<String, u32>,
    pub bare: HashSet<String>,
    pub src: String,
    pub destructured: RefCell<HashMap<String, bool>>,
}

/// The char before byte `at`, if any.
fn char_before(src: &str, at: usize) -> Option<char> {
    src[..at].chars().next_back()
}

/// Whether the `<Name` at `at` opens a JSX tag. A type argument list follows its type's name
/// directly and a tag never does, short of a `return` written up against it. A generic arrow
/// function's type parameters read like a tag but constrain theirs.
fn opens_tag(src: &str, at: usize, name: &str) -> bool {
    let end = at + 1 + name.len();
    // `src.slice(end, end + 40)` — 40 UTF-16 units.
    let mut window = String::new();
    let mut units = 0;
    for ch in src[end.min(src.len())..].chars() {
        units += ch.len_utf16();
        if units > 40 {
            break;
        }
        window.push(ch);
    }
    if re!(r"^\s+extends\s").is_match(&window) {
        return false;
    }
    let before = char_before(src, at);
    if !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return true;
    }
    // `/\breturn$/.test(src.slice(Math.max(0, at - 7), at))`
    let mut tail = String::new();
    let mut units = 0;
    for ch in src[..at].chars().rev() {
        units += ch.len_utf16();
        if units > 7 {
            break;
        }
        tail.insert(0, ch);
    }
    re!(r"\breturn$").is_match(&tail)
}

/// Whether `src` writes `name` other than in a tag (`<Name`, `</Name`): a parent binds a name
/// only where it writes it bare.
fn written_bare(src: &str, name: &str) -> bool {
    let mut from = 0usize;
    while let Some(p) = src.get(from..).and_then(|t| t.find(name)) {
        let at = from + p;
        from = at + name.len().max(1);
        let before = char_before(src, at);
        let after = src[at + name.len()..].chars().next();
        let is_id = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
        if is_id(before) || is_id(after) {
            continue;
        }
        if before == Some('<') {
            continue;
        }
        if before == Some('/') && char_before(src, at - 1) == Some('<') {
            continue;
        }
        return true;
    }
    false
}

/// The tags `src`, a function's lines from `start_line`, holds.
pub fn span_tags(src: String, start_line: u32) -> SpanTags {
    let mut names = SpanTags { tags: IndexMap::new(), bare: HashSet::new(), src, destructured: RefCell::new(HashMap::new()) };
    if names.src.contains("</") || names.src.contains("/>") {
        let mut line = start_line;
        let mut counted = 0usize;
        let src = names.src.clone();
        for m in jsx_tag_re().captures_iter(&src) {
            let at = m.get(0).unwrap().start();
            let name = m.text(1).unwrap_or("");
            // Newlines from `counted` up to this match.
            let mut from = counted;
            while let Some(p) = src[from..].find('\n') {
                let nl = from + p;
                if nl >= at {
                    break;
                }
                line += 1;
                from = nl + 1;
            }
            counted = at;
            if names.tags.get(name).copied().unwrap_or(0) == 0 {
                names.tags.insert(name.to_string(), if opens_tag(&src, at, name) { line } else { 0 });
            }
        }
        for (name, first) in &names.tags {
            if *first != 0 && written_bare(&src, name) {
                names.bare.insert(name.clone());
            }
        }
    }
    names
}

impl SpanTags {
    /// Whether a span's code destructures `name` through its `index`th line, once per span, line
    /// and name.
    pub fn destructured_by(&self, name: &str, index: i64) -> bool {
        let key = format!("{index}\0{name}");
        if let Some(b) = self.destructured.borrow().get(&key) {
            return *b;
        }
        let code = Code::new(&self.src);
        let binds = if index < 0 { false } else { code.destructures(name, code.line_end(index)) };
        self.destructured.borrow_mut().insert(key, binds);
        binds
    }
}

/// Whether the lines a file or top-level value owns declare `name` (`const Card = vi.fn()` in a
/// test callback).
pub fn own_lines_bind(name: &str, src: &str) -> bool {
    let n = escape_regex(name);
    Re::cached(&format!(r"\b(?:const|let|var)\s+{n}\b(?!\s*[,\]}}])"), "").is_match(src)
}
