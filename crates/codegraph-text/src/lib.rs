//! Per-language comment stripper for framework route extractors — port of
//! `src/resolution/strip-comments.ts`.
//!
//! Replaces comment characters and string-literal contents that hide
//! routing-shaped text with spaces (NOT removal) so source offsets are
//! preserved: a match offset in the stripped output maps to the same line in
//! the original.
//!
//! The scanners work on UTF-16 code units, exactly as the JavaScript original
//! indexes its strings, so every offset they report (`regex_literals`) is a
//! UTF-16 offset. Callers that need a byte offset convert with
//! [`utf16_to_byte_offset`].

use regex::Regex;
use std::sync::LazyLock;

type Units = Vec<u16>;

fn units(s: &str) -> Units {
    s.encode_utf16().collect()
}

fn from_units(u: &[u16]) -> String {
    String::from_utf16_lossy(u)
}

const fn u(c: char) -> u16 {
    c as u16
}

const NL: u16 = u('\n');

/// Text before a `/` (up to ~32 chars) after which that `/` can open a regex
/// literal rather than divide: expression-start punctuation or a keyword.
pub static REGEX_START_BEFORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:^|[=(:,)!&|?;{{}}\[\]+*%~^<>-]|(?-u:\b)(?:return|throw|case|yield|await|else|do|typeof|void|delete|new|in|of|instanceof))[{}]*$",
        codegraph_types::jscompat::JS_SPACE_CLASS
    ))
    .unwrap()
});

/// Byte offset in `s` of UTF-16 offset `at` (rounded down to a character boundary).
pub fn utf16_to_byte_offset(s: &str, at: usize) -> usize {
    let mut seen = 0;
    for (i, c) in s.char_indices() {
        if seen >= at {
            return i;
        }
        seen += c.len_utf16();
    }
    s.len()
}

/// Blank string contents while preserving quotes and offsets. Template
/// interpolations are blanked too; callers checking executable expressions must
/// conservatively inspect those expressions in the original source.
/// `regex_literals`, when given, receives the (UTF-16) offsets of the opening
/// and closing `/` of every span read as a regex literal (left as is).
pub fn blank_string_contents(text: &str, mut regex_literals: Option<&mut Vec<usize>>) -> String {
    let t = units(text);
    let mut out = t.clone();
    let n = t.len();
    let mut i = 0;
    while i < n {
        let c = t[i];
        // A quote inside a JS regex is data, not the beginning of a string.
        if c == u('/') {
            let from = i.saturating_sub(32);
            if REGEX_START_BEFORE.is_match(&from_units(&t[from..i])) {
                let mut end = i + 1;
                let mut in_class = false;
                while end < n && t[end] != NL {
                    if t[end] == u('\\') {
                        end += 2;
                        continue;
                    }
                    if t[end] == u('[') {
                        in_class = true;
                    }
                    if t[end] == u(']') {
                        in_class = false;
                    }
                    if t[end] == u('/') && !in_class {
                        break;
                    }
                    end += 1;
                }
                if end < n && t[end] == u('/') {
                    if let Some(r) = regex_literals.as_deref_mut() {
                        r.push(i);
                        r.push(end);
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        if c == u('"') || c == u('\'') || c == u('`') {
            let quote = c;
            i += 1;
            while i < n && t[i] != quote {
                if t[i] == u('\\') && i + 1 < n {
                    out[i] = u(' ');
                    out[i + 1] = u(' ');
                    i += 2;
                    continue;
                }
                if quote != u('`') && t[i] == NL {
                    break;
                }
                if t[i] != NL {
                    out[i] = u(' ');
                }
                i += 1;
            }
            if i < n && t[i] == quote {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    from_units(&out)
}

/// Replace every char in a range with spaces, but keep newlines so line numbers
/// computed downstream remain valid.
fn blank_range(buf: &mut [u16], src: &[u16], start: usize, end: usize) {
    for i in start..end.min(buf.len()) {
        buf[i] = if src[i] == NL { NL } else { u(' ') };
    }
}

fn at(src: &[u16], i: usize) -> u16 {
    src.get(i).copied().unwrap_or(0)
}

fn starts_with_at(src: &[u16], pat: &str, i: usize) -> bool {
    let p: Vec<u16> = pat.encode_utf16().collect();
    src.len() >= i + p.len() && src[i..i + p.len()] == p[..]
}

/// Strip comments (and, per language, docstrings) so route-shaped text inside
/// them cannot match. Languages without a stripper are returned unchanged.
pub fn strip_comments_for_regex(content: &str, lang: &str) -> String {
    match lang {
        "python" => strip_python(content),
        "ruby" => strip_ruby(content),
        "rust" => strip_rust(content),
        "erlang" => strip_erlang(content),
        "php" => strip_php(content),
        "go" => strip_go(content),
        "javascript" | "typescript" | "java" | "csharp" | "swift" | "c" | "cpp" => {
            strip_c_style(content, lang == "javascript" || lang == "typescript")
        }
        _ => content.to_string(),
    }
}

fn skip_line(src: &[u16], mut i: usize) -> usize {
    let n = src.len();
    while i < n && src[i] != NL {
        i += 1;
    }
    i
}

/// Skip a `/* … */` block starting at `i` (which is at the `/`).
fn skip_block_comment(src: &[u16], mut i: usize) -> usize {
    let n = src.len();
    i += 2;
    while i < n && !(src[i] == u('*') && at(src, i + 1) == u('/')) {
        i += 1;
    }
    if i < n {
        i += 2;
    }
    i
}

// ---------- Python ----------

fn strip_python(text: &str) -> String {
    let src = units(text);
    let mut out = src.clone();
    let n = src.len();
    let mut i = 0;
    while i < n {
        let c = src[i];
        let c2 = at(&src, i + 1);
        let c3 = at(&src, i + 2);
        // Triple-quoted string: """...""" or '''...'''
        if (c == u('"') || c == u('\'')) && c2 == c && c3 == c {
            let quote = c;
            let start = i;
            i += 3;
            while i < n {
                if src[i] == u('\\') && i + 1 < n {
                    i += 2;
                    continue;
                }
                if src[i] == quote && at(&src, i + 1) == quote && at(&src, i + 2) == quote {
                    i += 3;
                    break;
                }
                i += 1;
            }
            blank_range(&mut out, &src, start, i);
            continue;
        }
        // Single-line string
        if c == u('"') || c == u('\'') {
            let quote = c;
            i += 1;
            while i < n && src[i] != quote {
                if src[i] == u('\\') && i + 1 < n {
                    i += 2;
                    continue;
                }
                if src[i] == NL {
                    break; // unterminated
                }
                i += 1;
            }
            if i < n && src[i] == quote {
                i += 1;
            }
            continue;
        }
        // Line comment
        if c == u('#') {
            let start = i;
            i = skip_line(&src, i);
            blank_range(&mut out, &src, start, i);
            continue;
        }
        i += 1;
    }
    from_units(&out)
}

// ---------- Ruby ----------

fn strip_ruby(text: &str) -> String {
    let src = units(text);
    let mut out = src.clone();
    let n = src.len();
    let mut i = 0;
    let mut at_line_start = true;
    while i < n {
        let c = src[i];
        // =begin / =end block comments must be at start of line
        if at_line_start && c == u('=') && starts_with_at(&src, "=begin", i) {
            let start = i;
            i += "=begin".len();
            while i < n {
                if src[i] == NL {
                    let mut j = i + 1;
                    while j < n && (src[j] == u(' ') || src[j] == u('\t')) {
                        j += 1;
                    }
                    if starts_with_at(&src, "=end", j) {
                        i = j + "=end".len();
                        while i < n && src[i] != NL {
                            i += 1;
                        }
                        break;
                    }
                }
                i += 1;
            }
            blank_range(&mut out, &src, start, i);
            at_line_start = i > 0 && src[i - 1] == NL;
            continue;
        }
        // String literals
        if c == u('"') || c == u('\'') {
            let quote = c;
            i += 1;
            while i < n && src[i] != quote {
                if src[i] == u('\\') && i + 1 < n {
                    i += 2;
                    continue;
                }
                if src[i] == NL {
                    break;
                }
                i += 1;
            }
            if i < n && src[i] == quote {
                i += 1;
            }
            at_line_start = false;
            continue;
        }
        // Line comment
        if c == u('#') {
            let start = i;
            i = skip_line(&src, i);
            blank_range(&mut out, &src, start, i);
            at_line_start = false;
            continue;
        }
        if c == NL {
            at_line_start = true;
            i += 1;
            continue;
        }
        if c == u(' ') || c == u('\t') {
            i += 1;
            continue;
        }
        at_line_start = false;
        i += 1;
    }
    from_units(&out)
}

// ---------- C-style (JS/TS/Java/C#/Swift/C/C++) ----------

fn strip_c_style(text: &str, allow_single_quote_strings: bool) -> String {
    let src = units(text);
    let mut out = src.clone();
    let n = src.len();
    let mut i = 0;
    while i < n {
        let c = src[i];
        let c2 = at(&src, i + 1);
        if c == u('/') && c2 == u('*') {
            let start = i;
            i = skip_block_comment(&src, i);
            blank_range(&mut out, &src, start, i);
            continue;
        }
        if c == u('/') && c2 == u('/') {
            let start = i;
            i = skip_line(&src, i);
            blank_range(&mut out, &src, start, i);
            continue;
        }
        if c == u('"') || (allow_single_quote_strings && c == u('\'')) || c == u('`') {
            let quote = c;
            i += 1;
            while i < n && src[i] != quote {
                if src[i] == u('\\') && i + 1 < n {
                    i += 2;
                    continue;
                }
                // Template literal can span lines; regular strings break on newline.
                if quote != u('`') && src[i] == NL {
                    break;
                }
                i += 1;
            }
            if i < n && src[i] == quote {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    from_units(&out)
}

// ---------- PHP ----------

fn strip_php(text: &str) -> String {
    let src = units(text);
    let mut out = src.clone();
    let n = src.len();
    let mut i = 0;
    while i < n {
        let c = src[i];
        let c2 = at(&src, i + 1);
        if c == u('/') && c2 == u('*') {
            let start = i;
            i = skip_block_comment(&src, i);
            blank_range(&mut out, &src, start, i);
            continue;
        }
        // `//` and `#` line comments
        if (c == u('/') && c2 == u('/')) || c == u('#') {
            let start = i;
            i = skip_line(&src, i);
            blank_range(&mut out, &src, start, i);
            continue;
        }
        if c == u('"') || c == u('\'') || c == u('`') {
            let quote = c;
            i += 1;
            while i < n && src[i] != quote {
                if src[i] == u('\\') && i + 1 < n {
                    i += 2;
                    continue;
                }
                if src[i] == NL {
                    break;
                }
                i += 1;
            }
            if i < n && src[i] == quote {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    from_units(&out)
}

// ---------- Go ----------

fn strip_go(text: &str) -> String {
    let src = units(text);
    let mut out = src.clone();
    let n = src.len();
    let mut i = 0;
    while i < n {
        let c = src[i];
        let c2 = at(&src, i + 1);
        if c == u('/') && c2 == u('*') {
            let start = i;
            i = skip_block_comment(&src, i);
            blank_range(&mut out, &src, start, i);
            continue;
        }
        if c == u('/') && c2 == u('/') {
            let start = i;
            i = skip_line(&src, i);
            blank_range(&mut out, &src, start, i);
            continue;
        }
        // Raw string with backticks (no escapes, can span lines)
        if c == u('`') {
            i += 1;
            while i < n && src[i] != u('`') {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        // Interpreted string and rune literal
        if c == u('"') || c == u('\'') {
            let quote = c;
            i += 1;
            while i < n && src[i] != quote {
                if src[i] == u('\\') && i + 1 < n {
                    i += 2;
                    continue;
                }
                if src[i] == NL {
                    break;
                }
                i += 1;
            }
            if i < n && src[i] == quote {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    from_units(&out)
}

// ---------- Rust ----------

fn strip_rust(text: &str) -> String {
    let src = units(text);
    let mut out = src.clone();
    let n = src.len();
    let mut i = 0;
    while i < n {
        let c = src[i];
        let c2 = at(&src, i + 1);
        // Nested block comment
        if c == u('/') && c2 == u('*') {
            let start = i;
            i += 2;
            let mut depth = 1;
            while i < n && depth > 0 {
                if src[i] == u('/') && at(&src, i + 1) == u('*') {
                    depth += 1;
                    i += 2;
                } else if src[i] == u('*') && at(&src, i + 1) == u('/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            blank_range(&mut out, &src, start, i);
            continue;
        }
        if c == u('/') && c2 == u('/') {
            let start = i;
            i = skip_line(&src, i);
            blank_range(&mut out, &src, start, i);
            continue;
        }
        if c == u('"') {
            i += 1;
            while i < n && src[i] != u('"') {
                if src[i] == u('\\') && i + 1 < n {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            if i < n && src[i] == u('"') {
                i += 1;
            }
            continue;
        }
        // Char literal — could also be a lifetime; neither holds routing text.
        if c == u('\'') {
            i += 1;
            while i < n && src[i] != u('\'') {
                if src[i] == u('\\') && i + 1 < n {
                    i += 2;
                    continue;
                }
                if src[i] == NL {
                    break;
                }
                i += 1;
            }
            if i < n && src[i] == u('\'') {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    from_units(&out)
}

// ---------- Erlang ----------

/// Erlang: `%` starts a line comment unless it sits inside a `"string"`, a
/// `'quoted atom'`, or is the character literal `$%`. Strings and quoted atoms
/// are left intact; only the comment text is blanked.
fn strip_erlang(text: &str) -> String {
    let src = units(text);
    let mut out = src.clone();
    let n = src.len();
    let mut i = 0;
    while i < n {
        let c = src[i];
        if c == u('"') || c == u('\'') {
            let quote = c;
            i += 1;
            while i < n && src[i] != quote {
                if src[i] == u('\\') && i + 1 < n {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        // Character literal: `$x`, `$\n`, `$%` — the next char (or escape) is data.
        if c == u('$') {
            i += 1;
            if i < n && src[i] == u('\\') {
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == u('%') {
            let end = skip_line(&src, i);
            blank_range(&mut out, &src, i, end);
            i = end;
            continue;
        }
        i += 1;
    }
    from_units(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_docstrings_and_comments_blank() {
        let s = strip_comments_for_regex("x = 1  # path('/fake/', V)\nreal = 2\n\"\"\"path('/doc/', V)\"\"\"\nk = '#keep'", "python");
        // The comment `# path('/fake/', V)` is 19 chars, the docstring 22; neither keeps a character.
        let expected = format!("x = 1  {}\nreal = 2\n{}\nk = '#keep'", " ".repeat(19), " ".repeat(22));
        assert_eq!(s, expected);
        assert_eq!(s.len(), "x = 1  # path('/fake/', V)\nreal = 2\n\"\"\"path('/doc/', V)\"\"\"\nk = '#keep'".len());
    }

    #[test]
    fn c_style_keeps_strings_and_blanks_comments() {
        let s = strip_comments_for_regex("a('/x'); // app.get('/y')\n/* app.get('/z') */ b", "javascript");
        // `// app.get('/y')` is 16 chars and `/* app.get('/z') */` is 19.
        let expected = format!("a('/x'); {}\n{} b", " ".repeat(16), " ".repeat(19));
        assert_eq!(s, expected);
        // Java has no single-quote strings, so `'` is left alone.
        let j = strip_comments_for_regex("char c = '/'; // x", "java");
        assert_eq!(j, format!("char c = '/'; {}", " ".repeat(4)));
    }

    #[test]
    fn rust_nested_block_comments() {
        let s = strip_comments_for_regex("a /* x /* y */ z */ b", "rust");
        assert_eq!(s, "a                   b");
    }

    #[test]
    fn ruby_begin_end_blocks() {
        let s = strip_comments_for_regex("a\n=begin\nroute\n=end\nb # c", "ruby");
        assert_eq!(s, "a\n      \n     \n    \nb    ");
    }

    #[test]
    fn erlang_percent_comments_but_not_in_strings() {
        let s = strip_comments_for_regex("a(\"100%\"). % gone\n$%", "erlang");
        assert_eq!(s, "a(\"100%\").       \n$%");
    }

    #[test]
    fn offsets_are_utf16() {
        // An astral char is two code units, blanked to two spaces like JS would.
        let s = strip_comments_for_regex("/* 😀 */x", "javascript");
        // `/* 😀 */` is 8 UTF-16 units, so 8 spaces replace it.
        assert_eq!(s, format!("{}x", " ".repeat(8)));
        assert!(s.ends_with('x'));
    }

    #[test]
    fn blank_string_contents_skips_regex_literals() {
        let mut lits = Vec::new();
        let s = blank_string_contents("a = /'x/; b = 'y'", Some(&mut lits));
        assert_eq!(s, "a = /'x/; b = ' '");
        assert_eq!(lits, vec![4, 7]);
    }
}
