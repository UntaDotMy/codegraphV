//! A JavaScript or JSX file's code, for a scan that reads JSX tags off it with a pattern — port of
//! `src/resolution/jsx-code.ts`.
//!
//! Comments, and the contents of string, template and regex literals and of JSX attribute strings,
//! are blanked. Every character keeps its offset and every line break its place, so a match in the
//! result sits at the same line and column as in the source. `None` when the file does not read as
//! one program that way; a scan should then read the source as it is.
//!
//! The lexer works on UTF-16 code units, as the TypeScript original does, so offsets agree.

use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Code,
    Template,
    Tag,
    Children,
}

struct Frame {
    mode: Mode,
    /// Code: braces opened in this frame and not closed yet.
    braces: i32,
    /// Tag and Children: the element's name, which its closing tag repeats.
    name: Vec<u16>,
}

/// Where an element opened among code: what to undo to read its `<` again.
struct Root {
    at: usize,
    depth: usize,
    blanks: usize,
}

/// Keywords an expression follows: a `<` after one opens an element, a `/` a regex.
const BEFORE_EXPRESSION: [&str; 15] = [
    "return", "typeof", "instanceof", "in", "of", "new", "delete", "void", "throw", "case", "do", "else", "yield",
    "await", "default",
];

/// How many elements may fail to read before the file is read as it is.
const MAX_REREADS: usize = 64;

fn is_space(c: i32) -> bool {
    matches!(c, 32 | 10 | 13 | 9 | 11 | 12 | 0xa0 | 0xfeff)
}
fn is_id_start(c: i32) -> bool {
    (97..=122).contains(&c) || (65..=90).contains(&c) || c == 36 || c == 95 || (c > 127 && !is_space(c))
}
fn is_id_part(c: i32) -> bool {
    is_id_start(c) || (48..=57).contains(&c)
}
/// A JSX name's characters: an identifier's, and the `-`, `.` and `:` between its parts.
fn is_name_part(c: i32) -> bool {
    is_id_part(c) || c == 45 || c == 46 || c == 58
}

struct Src<'a>(&'a [u16]);

impl Src<'_> {
    /// `src.charCodeAt(k)`; `-1` (no code unit) out of range.
    fn at(&self, k: usize) -> i32 {
        self.0.get(k).map_or(-1, |c| *c as i32)
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn skip_space(&self, mut k: usize) -> usize {
        while k < self.len() && is_space(self.at(k)) {
            k += 1;
        }
        k
    }
    fn name_end(&self, mut k: usize) -> usize {
        while k < self.len() && is_name_part(self.at(k)) {
            k += 1;
        }
        k
    }
    fn line_end(&self, mut k: usize) -> usize {
        while k < self.len() && self.at(k) != 10 && self.at(k) != 13 {
            k += 1;
        }
        k
    }
    /// The `/` closing the regex literal opened at `start`, or -1 when it doesn't close on its line.
    fn regex_end(&self, start: usize) -> isize {
        let mut in_class = false;
        let mut k = start + 1;
        while k < self.len() {
            let c = self.at(k);
            if c == 10 || c == 13 {
                return -1;
            }
            if c == 92 {
                k += 1;
                if self.at(k) == 10 || self.at(k) == 13 {
                    return -1;
                }
            } else if c == 91 {
                in_class = true;
            } else if c == 93 {
                in_class = false;
            } else if c == 47 && !in_class {
                return k as isize;
            }
            k += 1;
        }
        -1
    }
    /// The quote closing the string opened at `start`, or -1 when the string doesn't close.
    fn string_end(&self, start: usize) -> isize {
        let quote = self.at(start);
        let mut k = start + 1;
        while k < self.len() {
            let c = self.at(k);
            if c == quote {
                return k as isize;
            }
            if c == 92 {
                k += if self.at(k + 1) == 13 && self.at(k + 2) == 10 { 2 } else { 1 };
            } else if c == 10 || c == 13 {
                return -1;
            }
            k += 1;
        }
        -1
    }
    fn index_of(&self, needle: &[u16], from: usize) -> Option<usize> {
        if needle.is_empty() || from + needle.len() > self.len() {
            return None;
        }
        (from..=self.len() - needle.len()).find(|&i| &self.0[i..i + needle.len()] == needle)
    }
    fn starts_with(&self, at: usize, word: &str) -> bool {
        word.encode_utf16().enumerate().all(|(j, u)| self.at(at + j) == u as i32)
    }
}

/// `src` with the ranges (start and end pairs, in order) turned to spaces, line breaks kept.
fn blank_ranges(src: &[u16], blanks: &[usize]) -> Vec<u16> {
    if blanks.is_empty() {
        return src.to_vec();
    }
    let mut out: Vec<u16> = Vec::with_capacity(src.len());
    let mut from = 0;
    for pair in blanks.chunks(2) {
        let (start, end) = (pair[0], pair[1]);
        if start < from {
            continue;
        }
        out.extend_from_slice(&src[from..start]);
        let text = &src[start..end];
        if text.iter().any(|c| *c == 10 || *c == 13) {
            out.extend(text.iter().map(|c| if *c == 10 || *c == 13 { *c } else { 32 }));
        } else {
            out.extend(std::iter::repeat_n(32u16, end - start));
        }
        from = end;
    }
    out.extend_from_slice(&src[from..]);
    out
}

/// A file's code with its comments and string contents blanked, or `None` when it doesn't read as
/// one program.
pub fn jsx_code(src: &str) -> Option<String> {
    let units: Vec<u16> = src.encode_utf16().collect();
    let out = jsx_code_units(&units)?;
    Some(String::from_utf16_lossy(&out))
}

/// {@link jsx_code} over UTF-16 code units.
pub fn jsx_code_units(units: &[u16]) -> Option<Vec<u16>> {
    JsxLexer::new(units).read()
}

struct JsxLexer<'a> {
    src: Src<'a>,
    n: usize,
    /// Ranges to blank, as start and end pairs, in source order.
    blanks: Vec<usize>,
    stack: Vec<Frame>,
    /// The elements opened among code that are still open, innermost last.
    roots: Vec<Root>,
    /// `<`s that failed to read as an element.
    operators: BTreeSet<usize>,
    i: usize,
    /// Whether the last token ends an expression: a `<` then compares, a `/` divides.
    expr_end: bool,
    /// Whether the last token is a `.`, so a keyword is a property's name.
    after_dot: bool,
    rereads: usize,
}

impl<'a> JsxLexer<'a> {
    fn new(units: &'a [u16]) -> Self {
        JsxLexer {
            src: Src(units),
            n: units.len(),
            blanks: Vec::new(),
            stack: vec![Frame { mode: Mode::Code, braces: 0, name: Vec::new() }],
            roots: Vec::new(),
            operators: BTreeSet::new(),
            i: 0,
            expr_end: false,
            after_dot: false,
            rereads: 0,
        }
    }

    fn read(mut self) -> Option<Vec<u16>> {
        let budget = 4 * self.n + 65536;
        let mut steps = 0usize;
        loop {
            while self.i < self.n {
                steps += 1;
                if steps > budget {
                    return None;
                }
                let mode = self.stack.last().unwrap().mode;
                let c = self.src.at(self.i);
                let ok = match mode {
                    Mode::Code => self.code(c),
                    Mode::Template => self.template(c),
                    Mode::Tag => self.tag(c),
                    Mode::Children => self.children(c),
                };
                if !ok {
                    return None;
                }
            }
            if self.stack.len() == 1 && self.stack[0].braces == 0 {
                break;
            }
            if !self.reread() {
                return None;
            }
        }
        Some(blank_ranges(self.src.0, &self.blanks))
    }

    fn blank(&mut self, from: usize, to: usize) {
        if to > from {
            self.blanks.push(from);
            self.blanks.push(to.min(self.n));
        }
    }

    fn code_frame(&mut self) {
        self.stack.push(Frame { mode: Mode::Code, braces: 0, name: Vec::new() });
        self.expr_end = false;
        self.after_dot = false;
    }

    /// Undo back to the innermost element still open among code and read its `<` again as an
    /// operator. False when there is none, or too many have failed.
    fn reread(&mut self) -> bool {
        let Some(root) = self.roots.pop() else { return false };
        self.rereads += 1;
        if self.rereads > MAX_REREADS {
            return false;
        }
        self.stack.truncate(root.depth);
        self.blanks.truncate(root.blanks);
        self.operators.retain(|k| *k <= root.at);
        self.operators.insert(root.at);
        self.i = root.at;
        self.expr_end = false;
        self.after_dot = false;
        true
    }

    /// Open the element whose `<` is at `lt`. False when it cannot be one.
    fn open_element(&mut self, lt: usize) -> bool {
        if self.src.at(lt + 1) == 62 {
            self.stack.push(Frame { mode: Mode::Children, braces: 0, name: Vec::new() });
            self.i = lt + 2;
            return true;
        }
        let mut k = self.src.name_end(lt + 1);
        let name: Vec<u16> = self.src.0[lt + 1..k].to_vec();
        // `<Entry extends BaseEntity>(…) =>`: a generic arrow function's type parameters.
        let next = self.src.skip_space(k);
        if next > k && self.src.starts_with(next, "extends") && is_space(self.src.at(next + 7)) {
            return false;
        }
        if self.src.at(k) == 60 {
            // `<Select<Option> …>`: the element's type arguments.
            let mut depth = 0i32;
            let limit = self.n.min(k + 1000);
            while k < limit {
                let c = self.src.at(k);
                if c == 60 {
                    depth += 1;
                } else if c == 62 {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                k += 1;
            }
            if depth != 0 {
                return false;
            }
            k += 1;
        }
        self.stack.push(Frame { mode: Mode::Tag, braces: 0, name });
        self.i = k;
        true
    }

    /// Close the innermost element. Closing an element opened among code ends an expression.
    fn close_element(&mut self) {
        self.stack.pop();
        if self.stack.last().unwrap().mode == Mode::Code {
            self.roots.pop();
            self.expr_end = true;
            self.after_dot = false;
        }
    }

    /// One step among code. False when the file is given up.
    fn code(&mut self, c: i32) -> bool {
        let i = self.i;
        if is_space(c) {
            self.i = i + 1;
            return true;
        }
        if c == 47 {
            let d = self.src.at(i + 1);
            if d == 47 {
                let end = self.src.line_end(i);
                self.blank(i, end);
                self.i = end;
                return true;
            }
            if d == 42 {
                let star_slash: [u16; 2] = [b'*' as u16, b'/' as u16];
                let Some(end) = self.src.index_of(&star_slash, i + 2) else { return self.reread() };
                self.blank(i, end + 2);
                self.i = end + 2;
                return true;
            }
            if self.expr_end {
                // `/>` among code: an element read as code.
                if d == 62 {
                    return self.reread();
                }
                self.i = i + 1;
                self.expr_end = false;
                self.after_dot = false;
                return true;
            }
            let end = self.src.regex_end(i);
            if end == -1 {
                self.i = i + 1;
                return true;
            }
            self.blank(i + 1, end as usize);
            let mut k = end as usize + 1;
            while k < self.n && is_id_part(self.src.at(k)) {
                k += 1;
            }
            self.i = k;
            self.expr_end = true;
            self.after_dot = false;
            return true;
        }
        if c == 34 || c == 39 {
            let end = self.src.string_end(i);
            if end == -1 {
                return self.reread();
            }
            self.blank(i + 1, end as usize);
            self.i = end as usize + 1;
            self.expr_end = true;
            self.after_dot = false;
            return true;
        }
        if c == 96 {
            self.stack.push(Frame { mode: Mode::Template, braces: 0, name: Vec::new() });
            self.i = i + 1;
            return true;
        }
        if c == 60 {
            let d = self.src.at(i + 1);
            if !self.expr_end && (d == 62 || is_id_start(d)) && !self.operators.contains(&i) {
                self.roots.push(Root { at: i, depth: self.stack.len(), blanks: self.blanks.len() });
                return self.open_element(i) || self.reread();
            }
            // `</` among code: a closing tag read as code.
            if d == 47 {
                return self.reread();
            }
            self.i = i + if d == 60 || d == 61 { 2 } else { 1 };
            self.expr_end = false;
            self.after_dot = false;
            return true;
        }
        if c == 123 {
            self.stack.last_mut().unwrap().braces += 1;
            self.i = i + 1;
            self.expr_end = false;
            self.after_dot = false;
            return true;
        }
        if c == 125 {
            if self.stack.last().unwrap().braces > 0 {
                self.stack.last_mut().unwrap().braces -= 1;
                self.i = i + 1;
                self.expr_end = false;
                self.after_dot = false;
                return true;
            }
            if self.stack.len() == 1 {
                return self.reread();
            }
            // The end of a JSX `{…}` or of a template's `${…}`.
            self.stack.pop();
            self.i = i + 1;
            return true;
        }
        if is_id_start(c) {
            let mut end = i + 1;
            while end < self.n && is_id_part(self.src.at(end)) {
                end += 1;
            }
            let word = String::from_utf16_lossy(&self.src.0[i..end]);
            self.expr_end = self.after_dot || !BEFORE_EXPRESSION.contains(&word.as_str());
            self.after_dot = false;
            self.i = end;
            return true;
        }
        let d = self.src.at(i + 1);
        if (48..=57).contains(&c) || (c == 46 && (48..=57).contains(&d)) {
            let mut end = i + 1;
            while end < self.n && (is_id_part(self.src.at(end)) || self.src.at(end) == 46) {
                end += 1;
            }
            self.i = end;
            self.expr_end = true;
            self.after_dot = false;
            return true;
        }
        if c == 46 {
            if d == 46 && self.src.at(i + 2) == 46 {
                self.i = i + 3;
                self.expr_end = false;
                self.after_dot = false;
            } else {
                self.i = i + 1;
                self.expr_end = false;
                self.after_dot = true;
            }
            return true;
        }
        if c == 41 || c == 93 {
            self.i = i + 1;
            self.expr_end = true;
            self.after_dot = false;
            return true;
        }
        // `++` and `--` leave the expression where it was, and so does TypeScript's `x!`.
        if (c == 43 || c == 45) && d == c {
            self.i = i + 2;
            return true;
        }
        if c == 33 && self.expr_end && d != 61 {
            self.i = i + 1;
            return true;
        }
        if c == 35 && i == 0 && d == 33 {
            self.i = self.src.line_end(0);
            return true;
        }
        self.i = i + 1;
        self.expr_end = false;
        self.after_dot = false;
        true
    }

    /// One step in a template's text.
    fn template(&mut self, c: i32) -> bool {
        let i = self.i;
        if c == 96 {
            self.stack.pop();
            self.i = i + 1;
            self.expr_end = true;
            self.after_dot = false;
            return true;
        }
        if c == 92 {
            self.blank(i, i + 2);
            self.i = i + 2;
            return true;
        }
        if c == 36 && self.src.at(i + 1) == 123 {
            self.code_frame();
            self.i = i + 2;
            return true;
        }
        let mut end = i + 1;
        while end < self.n {
            let e = self.src.at(end);
            if e == 96 || e == 92 || e == 36 {
                break;
            }
            end += 1;
        }
        self.blank(i, end);
        self.i = end;
        true
    }

    /// One step in an element's opening tag. False when the file is given up.
    fn tag(&mut self, c: i32) -> bool {
        let i = self.i;
        if is_space(c) {
            self.i = i + 1;
            return true;
        }
        if c == 62 {
            self.stack.last_mut().unwrap().mode = Mode::Children;
            self.i = i + 1;
            return true;
        }
        if c == 47 {
            let d = self.src.at(i + 1);
            if d == 62 {
                self.i = i + 2;
                self.close_element();
                return true;
            }
            if d == 47 {
                let end = self.src.line_end(i);
                self.blank(i, end);
                self.i = end;
                return true;
            }
            if d == 42 {
                let star_slash: [u16; 2] = [b'*' as u16, b'/' as u16];
                if let Some(end) = self.src.index_of(&star_slash, i + 2) {
                    self.blank(i, end + 2);
                    self.i = end + 2;
                    return true;
                }
            }
            return self.reread();
        }
        if c == 123 {
            self.code_frame();
            self.i = i + 1;
            return true;
        }
        if is_id_start(c) {
            let end = self.src.name_end(i + 1);
            let eq = self.src.skip_space(end);
            if self.src.at(eq) != 61 {
                self.i = end;
                return true;
            }
            let value = self.src.skip_space(eq + 1);
            let q = self.src.at(value);
            if q == 34 || q == 39 {
                // JSX strings have no escapes and may span lines.
                let quote = [q as u16];
                let Some(close) = self.src.index_of(&quote, value + 1) else { return self.reread() };
                self.blank(value + 1, close);
                self.i = close + 1;
                return true;
            }
            if q == 123 {
                self.code_frame();
                self.i = value + 1;
                return true;
            }
            let next = self.src.at(value + 1);
            if q == 60 && (next == 62 || is_id_start(next)) && self.open_element(value) {
                return true;
            }
        }
        self.reread()
    }

    /// One step among an element's children. False when the file is given up.
    fn children(&mut self, c: i32) -> bool {
        let i = self.i;
        if c == 123 {
            self.code_frame();
            self.i = i + 1;
            return true;
        }
        if c == 60 {
            let d = self.src.at(i + 1);
            if d == 47 {
                let start = self.src.skip_space(i + 2);
                let end = self.src.name_end(start);
                let gt = self.src.skip_space(end);
                if self.src.at(gt) != 62 || self.src.0[start..end] != self.stack.last().unwrap().name[..] {
                    return self.reread();
                }
                self.i = gt + 1;
                self.close_element();
                return true;
            }
            if (d == 62 || is_id_start(d)) && self.open_element(i) {
                return true;
            }
            return self.reread();
        }
        // JSX text can't hold a `>` or a `}`: code read as text.
        if c == 62 || c == 125 {
            return self.reread();
        }
        let mut end = i + 1;
        while end < self.n {
            let e = self.src.at(end);
            if e == 123 || e == 60 || e == 62 || e == 125 {
                break;
            }
            end += 1;
        }
        self.i = end;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_comment_and_string() {
        let src = "const a = \"<Foo/>\"; // <Bar/>\nconst b = <Baz x=\"<Q/>\" />;";
        let out = jsx_code(src).unwrap();
        assert_eq!(out.len(), src.len());
        assert!(!out.contains("<Foo"));
        assert!(!out.contains("<Bar"));
        assert!(out.contains("<Baz"));
        assert!(!out.contains("<Q/>"));
    }

    #[test]
    fn jsx_text_apostrophe() {
        let src = "const a = <p>Don't have an account? <Link to=\"/s\">x</Link></p>;";
        let out = jsx_code(src).unwrap();
        assert!(out.contains("<Link"));
    }

    #[test]
    fn unbalanced_gives_none() {
        assert!(jsx_code("const a = {").is_none());
    }
}
