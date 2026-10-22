//! Lexical masks shared by the C / C++ blanking passes (languages/c-cpp.ts):
//! comment and literal masking, raw-string masking, span blanking, directive
//! restoration. All operate on the ASCII shadow, so a byte offset is a UTF-16
//! offset.

use super::jsre::Re;
use regex::Regex as PlainRe;
use std::cell::RefCell;
use std::sync::LazyLock;

pub(crate) fn spaces(n: usize) -> String {
    " ".repeat(n)
}

pub(crate) fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// JavaScript's `/\s/` on the shadow (its Unicode spaces are `\x0B`).
pub(crate) fn is_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ')
}

/// `String.prototype.trim` on the shadow.
pub(crate) fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii() && is_space(c as u8))
}

pub(crate) fn from_bytes(bytes: Vec<u8>) -> String {
    // The shadow is ASCII, and every edit writes ASCII.
    String::from_utf8(bytes).expect("shadow text stays ASCII")
}

/// Replace every byte of `bytes[from..to]` that is not a line break with a space.
pub(crate) fn blank_range(bytes: &mut [u8], from: usize, to: usize) {
    let to = to.min(bytes.len());
    for b in &mut bytes[from..to] {
        if *b != b'\n' && *b != b'\r' {
            *b = b' ';
        }
    }
}

// ---------------------------------------------------------------------------
// Comments and literals
// ---------------------------------------------------------------------------

/// Comments and string/char literals. The char-literal alternative's
/// `(?<!\w)` lookbehind is applied by the caller (the `regex` crate has none).
static COMMENT_OR_LITERAL: LazyLock<PlainRe> = LazyLock::new(|| {
    PlainRe::new(
        r#"//(?:\\\r?\n|[^\r\n])*|/\*[\s\S]*?(?:\*/|$)|"(?:\\[\s\S]|[^"\\])*(?:"|$)|(?:u8|[LuU])?'(?:\\[\s\S]|[^'\\])*(?:'|$)"#,
    )
    .unwrap()
});

/// The `[start, end)` spans of comments and literals, as the JS scan finds them.
pub(crate) fn comment_or_literal_spans(src: &str) -> Vec<(usize, usize)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= src.len() {
        let Some(m) = COMMENT_OR_LITERAL.find_at(src, pos) else { break };
        let first = b[m.start()];
        if first != b'/' && first != b'"' && m.start() > 0 && is_word(b[m.start() - 1]) {
            // `(?<!\w)` fails: no alternative matches here, resume one on.
            pos = m.start() + 1;
            continue;
        }
        out.push((m.start(), m.end()));
        pos = m.end();
    }
    out
}

/// The source with comments and string/char literals turned to spaces, line endings kept.
pub(crate) fn mask_cpp_comments_and_literals(source: &str) -> String {
    let mut bytes = source.as_bytes().to_vec();
    for (s, e) in comment_or_literal_spans(source) {
        blank_range(&mut bytes, s, e);
    }
    from_bytes(bytes)
}

static DIRECTIVE_LINES: LazyLock<PlainRe> = LazyLock::new(|| PlainRe::new(r"(?m)^[ \t]*#(?:\\\r?\n|[^\r\n])*").unwrap());

thread_local! {
    /// The last source `mask_cpp_code` masked, and its mask (see `blank_cpp_spans`).
    static MASK_MEMO: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
}

/// The code the macro scans match on, at the source's offsets: comments are
/// spaces, a literal keeps its place as `\x01`s, and preprocessor directives
/// (with their `\` continuation lines) are spaces.
pub(crate) fn mask_cpp_code(source: &str) -> String {
    if let Some(code) = MASK_MEMO.with(|m| m.borrow().as_ref().filter(|(s, _)| s == source).map(|(_, c)| c.clone())) {
        return code;
    }
    let mut bytes = source.as_bytes().to_vec();
    for (s, e) in comment_or_literal_spans(source) {
        let fill = if bytes[s] == b'/' { b' ' } else { 1u8 };
        for b in &mut bytes[s..e] {
            if *b != b'\n' && *b != b'\r' {
                *b = fill;
            }
        }
    }
    let masked = from_bytes(bytes);
    let mut bytes = masked.as_bytes().to_vec();
    for m in DIRECTIVE_LINES.find_iter(&masked) {
        blank_range(&mut bytes, m.start(), m.end());
    }
    let code = from_bytes(bytes);
    MASK_MEMO.with(|m| *m.borrow_mut() = Some((source.to_string(), code.clone())));
    code
}

/// Replace each `[start, end)` span of `source` with spaces, keeping line
/// endings. Given the mask the spans were found on, blank it the same way and
/// keep it for the next pass.
pub(crate) fn blank_cpp_spans(source: &str, spans: &[(usize, usize)], code: Option<&str>) -> String {
    if spans.is_empty() {
        return source.to_string();
    }
    let blank = |text: &str| -> String {
        let mut bytes = text.as_bytes().to_vec();
        for &(s, e) in spans {
            blank_range(&mut bytes, s, e);
        }
        from_bytes(bytes)
    };
    let out = blank(source);
    if let Some(code) = code {
        let blanked_code = blank(code);
        MASK_MEMO.with(|m| *m.borrow_mut() = Some((out.clone(), blanked_code)));
    }
    out
}

pub(crate) fn skip_cpp_space(code: &str, mut i: usize) -> usize {
    let b = code.as_bytes();
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    i
}

/// The index just past the template argument list opening at `code[open]`, or
/// `None` when it doesn't close before a `;`, `{` or `}` — then it was a comparison.
pub(crate) fn skip_cpp_angle_args(code: &str, open: usize) -> Option<usize> {
    let b = code.as_bytes();
    let mut angles = 0i32;
    let mut parens = 0i32;
    let limit = b.len().min(open + 4000);
    for i in open..limit {
        let c = b[i];
        if c == b'(' {
            parens += 1;
        } else if c == b')' {
            parens -= 1;
            if parens < 0 {
                return None;
            }
        } else if c == b';' || c == b'{' || c == b'}' {
            return None;
        } else if parens == 0 && c == b'<' {
            angles += 1;
        } else if parens == 0 && c == b'>' && b.get(i.wrapping_sub(1)).copied() != Some(b'-') {
            angles -= 1;
            if angles == 0 {
                return Some(i + 1);
            }
        }
    }
    None
}

static RUN_TOKEN: LazyLock<PlainRe> =
    LazyLock::new(|| PlainRe::new(r"[A-Z][A-Z0-9_]*(?:\s*\((?:[^()]|\((?:[^()]|\([^()]*\))*\))*\))?").unwrap());

/// Add a span for each macro of `run` (found at `start`), leaving what lies between them.
pub(crate) fn push_cpp_run_tokens(spans: &mut Vec<(usize, usize)>, run: &str, start: usize) {
    for t in RUN_TOKEN.find_iter(run) {
        spans.push((start + t.start(), start + t.end()));
    }
}

// ---------------------------------------------------------------------------
// Raw strings
// ---------------------------------------------------------------------------

static RAW_SCAN: LazyLock<PlainRe> = LazyLock::new(|| {
    PlainRe::new(
        r#"//[^\r\n]*|/\*[\s\S]*?(?:\*/|$)|\b(?:u8|[LuU])?R"([^ \t\x0B\x0C\r\n()\\]{0,16})\(|"(?:\\[\s\S]|[^"\\])*(?:"|$)|(?:u8|[LuU])?'(?:\\[\s\S]|[^'\\])*(?:'|$)"#,
    )
    .unwrap()
});

/// A source with its C++ raw string literals masked (`maskCppRawStrings`).
pub(crate) struct RawMask {
    /// The masked text, to run the passes on (the original when nothing was masked).
    pub source: String,
    original: String,
    spans: Vec<(usize, usize)>,
}

impl RawMask {
    /// Put the raw literals' bytes back wherever the passes left their mask intact.
    pub fn restore(&self, blanked: &str) -> String {
        if self.spans.is_empty() {
            return blanked.to_string();
        }
        // A length-changing rewrite cannot be restored at the original offsets.
        if blanked == self.source || blanked.len() != self.original.len() {
            return self.original.clone();
        }
        let mut bytes = blanked.as_bytes().to_vec();
        let orig = self.original.as_bytes();
        for &(s, e) in &self.spans {
            for i in s..e {
                if bytes[i] == 0 {
                    bytes[i] = orig[i];
                }
            }
        }
        from_bytes(bytes)
    }
}

/// Hide C++ raw literals from the offset-preserving preParse scans (#1505).
pub(crate) fn mask_cpp_raw_strings(source: &str) -> RawMask {
    let unchanged = || RawMask { source: source.to_string(), original: source.to_string(), spans: Vec::new() };
    if !source.contains("R\"") {
        return unchanged();
    }
    let b = source.as_bytes();
    let mut spans = Vec::new();
    let mut masked = String::with_capacity(source.len());
    let mut last = 0;
    let mut pos = 0;
    while pos <= source.len() {
        let Some(caps) = RAW_SCAN.captures_at(source, pos) else { break };
        let m = caps.get(0).unwrap();
        let Some(delim) = caps.get(1) else {
            // A comment or ordinary literal; a char literal still needs `(?<!\w)`.
            let first = b[m.start()];
            if first != b'/' && first != b'"' && m.start() > 0 && is_word(b[m.start() - 1]) {
                pos = m.start() + 1;
            } else {
                pos = m.end();
            }
            continue;
        };
        let closer = format!("){}\"", delim.as_str());
        let end = match source[m.end()..].find(&closer) {
            Some(i) => m.end() + i + closer.len(),
            None => source.len(), // an unterminated raw literal owns the rest of the file
        };
        spans.push((m.start(), end));
        masked.push_str(&source[last..m.start()]);
        for &c in &b[m.start()..end] {
            masked.push(if c == b'\r' || c == b'\n' { c as char } else { '\0' });
        }
        last = end;
        pos = end;
    }
    if spans.is_empty() {
        return unchanged();
    }
    masked.push_str(&source[last..]);
    RawMask { source: masked, original: source.to_string(), spans }
}

// ---------------------------------------------------------------------------
// Directive lines
// ---------------------------------------------------------------------------

static DIRECTIVE_START: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*#"));
static ENDS_WITH_BACKSLASH: LazyLock<Re> = LazyLock::new(|| Re::new(r"\\\s*$"));

/// Copy preprocessor-directive lines (and their `\` continuations) back from
/// `original` after the blanking passes ran, so a blank never breaks a directive.
pub(crate) fn restore_directive_lines(original: &str, blanked: &str) -> String {
    if blanked == original || !original.contains('#') {
        return blanked.to_string();
    }
    let o: Vec<&str> = original.split('\n').collect();
    let mut b: Vec<&str> = blanked.split('\n').collect();
    let mut changed = false;
    let mut continuation = false;
    for i in 0..o.len().min(b.len()) {
        let line = o[i];
        let is_directive = continuation || DIRECTIVE_START.is_match(line);
        if is_directive && b[i] != line {
            b[i] = line;
            changed = true;
        }
        continuation = is_directive && ENDS_WITH_BACKSLASH.is_match(line.strip_suffix('\r').unwrap_or(line));
    }
    if changed {
        b.join("\n")
    } else {
        blanked.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_comments_and_literals_but_keeps_digit_separators() {
        let s = "a /* x */ b \"s\" 1'000 // c\nd";
        let masked = mask_cpp_comments_and_literals(s);
        assert_eq!(masked.len(), s.len());
        assert!(masked.starts_with("a         b     1'000"));
        assert!(masked.ends_with("\nd"));
        let code = mask_cpp_code("#define X 1\\\n 2\nint /*c*/ y = \"s\";");
        assert_eq!(code, "            \n  \nint       y = \u{1}\u{1}\u{1};");
    }

    #[test]
    fn raw_strings_are_masked_and_restored() {
        let src = "x = R\"d(FOO(1)\n)d\"; FOO(2)";
        let raw = mask_cpp_raw_strings(src);
        assert!(raw.source.contains('\0'));
        assert!(!raw.source.contains("FOO(1)"));
        assert_eq!(raw.restore(&raw.source), src);
        let blanked = raw.source.replace("FOO(2)", "      ");
        assert_eq!(raw.restore(&blanked), "x = R\"d(FOO(1)\n)d\";       ");
    }

    #[test]
    fn directive_lines_come_back() {
        let o = "#define A B\\\n  C\nint x;";
        let b = "           \n     \nint y;";
        assert_eq!(restore_directive_lines(o, b), "#define A B\\\n  C\nint y;");
    }

    #[test]
    fn angle_args_close_or_bail() {
        assert_eq!(skip_cpp_angle_args("<a<b>> x", 0), Some(6));
        assert_eq!(skip_cpp_angle_args("< a; >", 0), None);
    }
}
