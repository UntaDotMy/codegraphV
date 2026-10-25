//! `preParse` passes that both the C and the C++ pipelines use (languages/c-cpp.ts):
//! declaration-markup macro calls, a macro alone on its line, Foundation enum
//! macros, a leading attribute macro, CUDA constructs.

use super::cmask::*;
use super::jsre::{Re, Sticky};
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Declaration-markup macro calls (UPROPERTY(...), REDIS_NO_SANITIZE("x"), ...)
// ---------------------------------------------------------------------------

static MARKUP_GUARD: LazyLock<Re> = LazyLock::new(|| Re::multiline(r"^[ \t]*[A-Z][A-Z0-9_]{2,}\s*\("));
static MARKUP_HEAD: LazyLock<Re> = LazyLock::new(|| Re::multiline(r"^([ \t]*)([A-Z][A-Z0-9_]{2,})(\s*)\("));

/// The index just past the `)` closing the `(` at `open`, skipping string and char
/// literals; `None` when the parens never balance.
pub(crate) fn balanced_close(src: &str, open: usize) -> Option<usize> {
    let b = src.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

/// Blank annotation-style macro invocations that decorate a declaration but
/// carry no terminating semicolon. With `keep_c_return_types`, a column-0 macro
/// that wraps the return type of the function declared after it stays.
pub(crate) fn blank_cpp_annotation_macro_calls(source: &str, keep_c_return_types: bool) -> String {
    if !MARKUP_GUARD.is_match(source) {
        return source.to_string();
    }
    let raw = mask_cpp_raw_strings(source);
    let source = raw.source.as_str();
    let mut chars = source.as_bytes().to_vec();
    let b = source.as_bytes();
    let mut pos = 0;
    while let Some(caps) = MARKUP_HEAD.captures_at(source, pos) {
        let m = caps.get(0).unwrap();
        pos = m.end();
        let macro_start = m.start() + caps.get(1).unwrap().as_str().len();
        let open = m.end() - 1;
        let Some(end) = balanced_close(source, open) else { continue };
        let mut j = end;
        while j < b.len() && is_space(b[j]) {
            j += 1;
        }
        // Only markup is followed by the declaration it decorates.
        match b.get(j) {
            Some(c) if c.is_ascii_alphabetic() || *c == b'_' || *c == b'~' || *c == b'#' => {}
            _ => continue,
        }
        if keep_c_return_types && macro_start == m.start() && is_c_return_type_macro(source, m.start(), open, end, j) {
            continue;
        }
        blank_range(&mut chars, macro_start, end);
        pos = end;
    }
    raw.restore(&from_bytes(chars))
}

static C_RETURN_TYPE_ARG: LazyLock<Re> = LazyLock::new(|| Re::new(r"^\s*[A-Za-z_][\w\s*]*$"));
static C_DECLARATOR_HEAD: LazyLock<Sticky> = LazyLock::new(|| Sticky::new(r"[A-Za-z_]\w*\s*\("));
static C_DECLARATOR_TAIL: LazyLock<Sticky> = LazyLock::new(|| {
    Sticky::new(
        r"\s*(?:(?:__attribute__\s*\(\((?:[^()]|\([^()]*\))*\)\)|[A-Z][A-Z0-9_]*(?![\w$])(?:\s*\((?:[^()]|\([^()]*\))*\))?)\s*)*[{;]",
    )
});

/// Whether the column-0 macro call starting at `line_start` is the return type of
/// the function declared right after it, not markup.
fn is_c_return_type_macro(source: &str, line_start: usize, open: usize, end: usize, next: usize) -> bool {
    if !C_RETURN_TYPE_ARG.is_match(&source[open + 1..end - 1]) {
        return false;
    }
    let Some(head_end) = C_DECLARATOR_HEAD.matches_at(source, next) else { return false };
    let b = source.as_bytes();
    let mut close = None;
    let mut depth = 0i32;
    let mut i = head_end - 1;
    while i < b.len() && close.is_none() {
        match b[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                }
            }
            b';' | b'{' | b'}' => return false,
            _ => {}
        }
        i += 1;
    }
    let Some(close) = close else { return false };
    C_DECLARATOR_TAIL.matches_at(source, close + 1).is_some() && !opens_c_declaration(source, line_start)
}

static LINE_COMMENT: LazyLock<Re> = LazyLock::new(|| Re::new(r"//.*$"));
static ENDS_WORD_OR_STAR: LazyLock<Re> = LazyLock::new(|| Re::new(r"[\w*]$"));

/// Whether the line right above `line_start` leaves a declaration open: its code
/// ends in a type's last token.
fn opens_c_declaration(source: &str, line_start: usize) -> bool {
    if line_start == 0 {
        return false;
    }
    let b = source.as_bytes();
    // `source.lastIndexOf('\n', lineStart - 2)`: a negative start clamps to 0.
    let from_idx = line_start.saturating_sub(2);
    let from = b[..=from_idx.min(b.len() - 1)].iter().rposition(|c| *c == b'\n').map_or(0, |i| i + 1);
    let slice = &source[from..(line_start - 1).max(from)];
    let line = LINE_COMMENT.replace_all(slice, |_| Some(String::new()));
    let line = trim(&line);
    !line.is_empty() && !line.starts_with('#') && ENDS_WORD_OR_STAR.is_match(line)
}

// ---------------------------------------------------------------------------
// A macro alone on its line
// ---------------------------------------------------------------------------

static LONE_GUARD: LazyLock<Re> = LazyLock::new(|| Re::multiline(r"^[ \t]*[A-Z][A-Z0-9_]{3,}[ \t]*\r?$"));
static LONE_MACRO_LINE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^[ \t]*([A-Z][A-Z0-9_]{3,})[ \t]*(?://[^\n\r]*|/\*[^\n\r]*\*/[ \t]*)?\r?$"));
static COMMENT_START: LazyLock<Re> = LazyLock::new(|| Re::new(r"^(?://|/\*|\*)"));
static COMMENTS_IN_LINE: LazyLock<Re> = LazyLock::new(|| Re::new(r"/\*.*?\*/|//.*$"));
static TEMPLATE_HEAD: LazyLock<Re> = LazyLock::new(|| Re::new(r"^template\s*<"));
static CONTINUATION_END: LazyLock<Re> = LazyLock::new(|| Re::new(r"[=+\-*/%&|^<>?:,(\\]$"));

/// How a language's code reads around a macro alone on its line.
pub(crate) struct LoneMacroRules {
    /// What the code after the macro may start with.
    pub next_code: fn(u8) -> bool,
    /// Whether a macro that is a body's only statement, between `{` and `}`, stays.
    pub keeps_sole_statement: bool,
}

pub(crate) const C_LONE_MACROS: LoneMacroRules = LoneMacroRules {
    next_code: |c| c.is_ascii_alphabetic() || matches!(c, b'_' | b'#' | b'{' | b'}' | b'~'),
    keeps_sole_statement: false,
};

/// Objective-C's (OBJC_LONE_MACROS): the code after the macro may also be a directive like the
/// `@interface` below `NS_ASSUME_NONNULL_BEGIN`, and a macro that is a body's only statement stays.
pub(crate) const OBJC_LONE_MACROS: LoneMacroRules = LoneMacroRules {
    next_code: |c| c.is_ascii_alphabetic() || matches!(c, b'_' | b'#' | b'{' | b'}' | b'~' | b'@'),
    keeps_sole_statement: true,
};

/// The objc extractor's preParse: Foundation enum macros rewritten as the enums they declare,
/// then lone macro lines blanked (offsets kept).
pub(crate) fn pre_parse_objc(source: &str) -> String {
    blank_lone_macro_lines(&rewrite_foundation_enum_macros(source), &OBJC_LONE_MACROS)
}

fn line_content(l: &str) -> &str {
    trim(l.strip_suffix('\r').unwrap_or(l))
}

/// The first character of code at or after line `from`, past blank lines and comments.
fn first_code_char_after(lines: &[String], from: usize) -> Option<u8> {
    let mut in_block = false;
    for line in lines.iter().skip(from) {
        let mut text: String = trim(line).to_string();
        loop {
            if in_block {
                match text.find("*/") {
                    None => {
                        text.clear();
                        break;
                    }
                    Some(end) => {
                        text = trim(&text[end + 2..]).to_string();
                        in_block = false;
                    }
                }
            }
            if text.starts_with("//") {
                text.clear();
            } else if text.starts_with("/*") {
                in_block = true;
                text = text[2..].to_string();
                continue;
            }
            break;
        }
        if !text.is_empty() {
            return Some(text.as_bytes()[0]);
        }
    }
    None
}

pub(crate) fn blank_lone_macro_lines(source: &str, rules: &LoneMacroRules) -> String {
    if !LONE_GUARD.is_match(source) {
        return source.to_string();
    }
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    let mut changed = false;
    for i in 0..lines.len() {
        let line = lines[i].clone();
        let Some(m) = LONE_MACRO_LINE.captures(&line) else { continue };
        let name = m.get(1).unwrap().as_str();
        // The underscore requirement rides the macro convention (FMT_BEGIN_NAMESPACE, Q_OBJECT).
        if !name.contains('_') {
            continue;
        }
        let mut prev = i as isize - 1;
        while prev >= 0 && line_content(&lines[prev as usize]).is_empty() {
            prev -= 1;
        }
        let prev_text = if prev >= 0 { line_content(&lines[prev as usize]).to_string() } else { String::new() };
        let prev_code = if COMMENT_START.is_match(&prev_text) {
            String::new()
        } else {
            trim(&COMMENTS_IN_LINE.replace_all(&prev_text, |_| Some(String::new()))).to_string()
        };
        let directive_ends = prev_code.starts_with('#') && !prev_code.ends_with('\\');
        if !prev_code.is_empty()
            && !directive_ends
            && !TEMPLATE_HEAD.is_match(&prev_code)
            && CONTINUATION_END.is_match(&prev_code)
        {
            continue;
        }
        let first = first_code_char_after(&lines, i + 1);
        if let Some(f) = first {
            if !(rules.next_code)(f) {
                continue;
            }
        }
        if rules.keeps_sole_statement && prev_code.ends_with('{') && first == Some(b'}') {
            continue;
        }
        let start = line.find(name).unwrap();
        lines[i] = format!("{}{}{}", &line[..start], spaces(name.len()), &line[start + name.len()..]);
        changed = true;
    }
    if changed {
        lines.join("\n")
    } else {
        source.to_string()
    }
}

// ---------------------------------------------------------------------------
// Foundation enum macros (NS_ENUM, CF_OPTIONS, ...)
// ---------------------------------------------------------------------------

static FOUNDATION_GUARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b(?:NS|CF)_[A-Z_]*(?:ENUM|OPTIONS)\s*\("));
static FOUNDATION_ENUM_MACRO: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"\b(?:(typedef)(\s+))?(NS_ENUM|NS_OPTIONS|NS_CLOSED_ENUM|NS_ERROR_ENUM|CF_ENUM|CF_OPTIONS|CF_CLOSED_ENUM)(\s*)\(([^(),]*),(\s*)([A-Za-z_]\w*)(\s*)\)",
    )
});

/// Rewrite a Foundation enum macro as the enum it declares, in place and at the
/// same length: `typedef NS_ENUM(NSInteger, T) {` becomes `        enum   ...  T  {`.
pub(crate) fn rewrite_foundation_enum_macros(source: &str) -> String {
    if !FOUNDATION_GUARD.is_match(source) {
        return source.to_string();
    }
    let code = mask_cpp_code(source);
    let mut out: Option<Vec<u8>> = None;
    let mut pos = 0;
    while let Some(m) = FOUNDATION_ENUM_MACRO.captures_at(&code, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let bytes = out.get_or_insert_with(|| source.as_bytes().to_vec());
        let len = |i: usize| m.get(i).map_or(0, |g| g.as_str().len());
        let mut at = whole.start();
        if m.get(1).is_some() {
            blank_range(bytes, at, at + len(1));
            at += len(1) + len(2);
        }
        let macro_len = len(3);
        bytes[at..at + 4].copy_from_slice(b"enum");
        blank_range(bytes, at + 4, at + macro_len);
        at += macro_len + len(4);
        // `(`, the first argument and the `,`.
        blank_range(bytes, at, at + 1 + len(5) + 1);
        at += 1 + len(5) + 1 + len(6) + len(7) + len(8);
        blank_range(bytes, at, at + 1); // `)`
    }
    match out {
        Some(bytes) => from_bytes(bytes),
        None => source.to_string(),
    }
}

// ---------------------------------------------------------------------------
// A leading attribute macro on a C function definition
// ---------------------------------------------------------------------------

static C_LEADING_ATTR_MACRO: LazyLock<Re> =
    LazyLock::new(|| Re::multiline(r"^([ \t]*)([A-Z][A-Z0-9_]{2,})(?=\s+[A-Za-z_]\w*[\s*]+[A-Za-z_]\w*\s*\()"));

pub(crate) fn blank_c_leading_attr_macros(source: &str) -> String {
    C_LEADING_ATTR_MACRO.replace_all(source, |c| {
        Some(format!("{}{}", c.get(1).unwrap().as_str(), spaces(c.get(2).unwrap().as_str().len())))
    })
}

// ---------------------------------------------------------------------------
// CUDA
// ---------------------------------------------------------------------------

static CUDA_LAUNCH_BOUNDS: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b__launch_bounds__\s*\([^()\n]*\)"));
static CUDA_SPECIFIER: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"\b__(?:global|device|host|constant|shared|managed|grid_constant|forceinline|noinline|launch_bounds)__\b")
});
static CUDA_LAUNCH_CONFIG: LazyLock<Re> = LazyLock::new(|| Re::new(r"<<<[^;]{0,400}?>>>"));

/// Blank CUDA-specific constructs (execution-space specifiers, `__launch_bounds__`,
/// `<<<…>>>` kernel launches) with equal-length whitespace.
pub(crate) fn blank_cuda_constructs(source: &str) -> String {
    let mut out = source.to_string();
    if out.contains("__") {
        out = CUDA_LAUNCH_BOUNDS.replace_all(&out, |c| Some(spaces(c.get(0).unwrap().as_str().len())));
        out = CUDA_SPECIFIER.replace_all(&out, |c| Some(spaces(c.get(0).unwrap().as_str().len())));
    }
    if out.contains("<<<") {
        out = CUDA_LAUNCH_CONFIG.replace_all(&out, |c| {
            let m = c.get(0).unwrap().as_str();
            let mut depth = 0i32;
            for ch in m.bytes() {
                if ch == b'{' {
                    depth += 1;
                } else if ch == b'}' {
                    depth -= 1;
                    if depth < 0 {
                        return None;
                    }
                }
            }
            if depth == 0 {
                Some(m.chars().map(|c| if c == '\n' { '\n' } else { ' ' }).collect())
            } else {
                None
            }
        });
    }
    out
}

/// Strong content markers for CUDA source in files without a CUDA extension.
pub(crate) fn looks_like_cuda_source(source: &str) -> bool {
    source.contains("__global__")
        || source.contains("__device__")
        || source.contains("__constant__")
        || source.contains("cudaStream_t")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotation_calls_before_declarations_are_blanked() {
        let src = "class A {\n  UPROPERTY(EditAnywhere)\n  float X;\n  FOO(x);\n};\n";
        let out = blank_cpp_annotation_macro_calls(src, false);
        assert_eq!(out, "class A {\n                         \n  float X;\n  FOO(x);\n};\n");
    }

    #[test]
    fn c_return_type_macro_is_kept_for_c() {
        let src = "CJSON_PUBLIC(char *) cJSON_Print(const cJSON *item)\n{\n}\n";
        assert_eq!(blank_cpp_annotation_macro_calls(src, true), src);
        assert_ne!(blank_cpp_annotation_macro_calls(src, false), src);
    }

    #[test]
    fn lone_macros_are_blanked_between_declarations() {
        let src = "namespace a {\nFMT_BEGIN_NAMESPACE\nint x;\n";
        assert_eq!(
            blank_lone_macro_lines(src, &C_LONE_MACROS),
            "namespace a {\n                   \nint x;\n"
        );
        let cont = "int x =\n  SOME_CONST\n  | OTHER;\n";
        assert_eq!(blank_lone_macro_lines(cont, &C_LONE_MACROS), cont);
    }

    #[test]
    fn foundation_enums_become_enums() {
        let src = "typedef NS_ENUM(NSInteger, SDImageCacheType) {\n  A,\n};\n";
        let out = rewrite_foundation_enum_macros(src);
        assert_eq!(out, "        enum                       SDImageCacheType  {\n  A,\n};\n".replace("                       SDImageCacheType  ", "               SDImageCacheType  "));
        assert_eq!(out.len(), src.len());
    }

    #[test]
    fn cuda_launch_and_specifiers() {
        let src = "__global__ void k() {}\nvoid f(){ k<<<g, b>>>(1); }";
        let out = blank_cuda_constructs(src);
        assert!(out.starts_with("           void k()"));
        assert!(out.contains("k          (1)"));
    }
}
