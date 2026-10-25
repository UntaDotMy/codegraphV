//! C++ declarator scanning — port of the first half of the C++ block of `name-matcher.ts`
//! (`CPP_DECLARATOR_TYPE`, `matchCppDeclarator`, `cppDeclaratorsAfterFirst`, the range-for reader).
//!
//! Positions in this module are byte offsets into the line being read; the scanners only ever
//! stop on ASCII punctuation, so a non-ASCII identifier byte never splits a character.

use super::util::slice_bytes;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{is_js_space, utf16_len};
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

/// Words that can never be a declared type.
pub static CPP_NON_TYPE_TOKENS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "return", "if", "else", "for", "while", "do", "switch", "case", "default", "break", "continue", "goto", "throw", "new", "delete",
        "co_await", "co_yield", "co_return", "static_cast", "const_cast", "dynamic_cast", "reinterpret_cast", "sizeof", "alignof", "typeid",
        "and", "or", "not", "xor",
    ]
    .into_iter()
    .collect()
});

pub static CPP_DECL_SPECIFIERS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "static", "extern", "inline", "constexpr", "constinit", "thread_local", "register", "mutable", "const", "volatile", "typename",
        "struct", "class", "union", "enum",
    ]
    .into_iter()
    .collect()
});

pub static CPP_NOT_A_DECLARED_TYPE: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut set: HashSet<&'static str> = CPP_NON_TYPE_TOKENS.iter().copied().collect();
    set.extend([
        "typedef", "using", "namespace", "template", "friend", "operator", "public", "private", "protected", "virtual", "explicit",
        "static_assert", "asm", "try", "catch", "this", "true", "false", "nullptr",
    ]);
    set
});

pub static CPP_NOT_A_DECLARED_NAME: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut set: HashSet<&'static str> = CPP_NOT_A_DECLARED_TYPE.iter().copied().collect();
    set.extend(CPP_DECL_SPECIFIERS.iter().copied());
    set.extend([
        "auto", "void", "bool", "char", "wchar_t", "char8_t", "char16_t", "char32_t", "short", "int", "long", "signed", "unsigned", "float",
        "double",
    ]);
    set
});

pub const CPP_DECLARATOR_CV: &str = r"\b(?:const|volatile|__restrict__|__restrict)\b";
pub const CPP_DECLARATOR_ANNOTATION: &str =
    r"\b(?:absl_nonnull|absl_nullable|absl_nullability_unknown|_Nonnull|_Nullable|_Null_unspecified|[A-Z][A-Z0-9]*_[A-Z0-9_]*)\b";
const CPP_FUNCTION_WRAPPER_TYPE: &str =
    r"(?:::\s*)?(?:std\s*::\s*(?:function|move_only_function|copyable_function)|absl\s*::\s*(?:FunctionRef|AnyInvocable))\s*<[^;={}]*>";
const CPP_TEMPLATE_ARGUMENTS: &str = r"<(?:[^<>;=(){}]|<(?:[^<>;=(){}]|<[^<>;=(){}]*>)*>)*>";
const CPP_TRAILING_ANNOTATION: &str = r"\b[A-Z][A-Z0-9]*_[A-Z0-9_]*\s*\([^()]*\)";

/// `CPP_DECLARATOR_TYPE`: one declared type with its pointer/reference/cv decoration.
pub static CPP_DECLARATOR_TYPE: LazyLock<String> = LazyLock::new(|| {
    format!(
        r"(?:{CPP_FUNCTION_WRAPPER_TYPE}|(?:(?<![\w>):])::\s*)?[A-Za-z_][\w:]*(?:\s*{CPP_TEMPLATE_ARGUMENTS}(?:\s*::\s*[A-Za-z_][\w:]*)?)?)(?:\s*(?:{CPP_DECLARATOR_CV}|[*&]+(?:\s*{CPP_DECLARATOR_ANNOTATION})*))*"
    )
});

static CPP_TYPE_LINE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(&format!(r"^\s*(?:(?:const|volatile|static|mutable|constexpr|inline)\s+)*{}\s*$", &*CPP_DECLARATOR_TYPE), "")
});

/// `CPP_TYPE_LINE.test(line)`: the line is nothing but a type.
pub fn is_cpp_type_line(line: &str) -> bool {
    CPP_TYPE_LINE.is_match(line)
}

static CPP_DECLARED_TYPE_TAIL: LazyLock<Re> = LazyLock::new(|| Re::new(&format!(r"({})\s*$", &*CPP_DECLARATOR_TYPE), ""));
static CPP_RANGE_FOR_DECLARATION: LazyLock<Re> =
    LazyLock::new(|| Re::new(r#"\bfor\s*\(([^;=(){}?."']*?)\b([A-Za-z_]\w*)\s*:(?!:)"#, "g"));
static CPP_DECLARATOR_ANNOTATION_WORD: LazyLock<Re> = LazyLock::new(|| Re::new(&format!("^{CPP_DECLARATOR_ANNOTATION}$"), ""));
static CPP_DECLARATOR_CV_WORD: LazyLock<Re> = LazyLock::new(|| Re::new(&format!("^{CPP_DECLARATOR_CV}$"), ""));
static CPP_TRAILING_ANNOTATION_AT: LazyLock<Re> = LazyLock::new(|| Re::new(CPP_TRAILING_ANNOTATION, "y"));

// -- Byte scanners --------------------------------------------------------------------------

pub fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The byte length of the whitespace character at `i`, or 0.
pub fn space_len_at(code: &str, i: usize) -> usize {
    match code.get(i..).and_then(|rest| rest.chars().next()) {
        Some(c) if is_js_space(c) => c.len_utf8(),
        _ => 0,
    }
}

/// `cppSkipSpace`.
pub fn cpp_skip_space(code: &str, mut i: usize) -> usize {
    loop {
        let n = space_len_at(code, i);
        if n == 0 {
            return i;
        }
        i += n;
    }
}

/// `cppWordAt`: the identifier starting at `i`.
pub fn cpp_word_at(code: &str, i: usize) -> Option<&str> {
    let b = code.as_bytes();
    if i >= b.len() || !(b[i].is_ascii_alphabetic() || b[i] == b'_') {
        return None;
    }
    let mut end = i + 1;
    while end < b.len() && is_word_byte(b[end]) {
        end += 1;
    }
    Some(&code[i..end])
}

/// `cppWordBefore`: the `\w+` run ending (modulo whitespace) at `i`.
pub fn cpp_word_before(code: &str, i: usize) -> &str {
    let head = code.get(..i).unwrap_or(code);
    let trimmed = head.trim_end_matches(is_js_space);
    let b = trimmed.as_bytes();
    let mut start = b.len();
    while start > 0 && is_word_byte(b[start - 1]) {
        start -= 1;
    }
    &trimmed[start..]
}

fn byte_at(code: &str, i: usize) -> Option<u8> {
    code.as_bytes().get(i).copied()
}

/// `cppOpensLiteral`.
pub fn cpp_opens_literal(code: &str, i: usize) -> bool {
    match byte_at(code, i) {
        Some(b'"') => true,
        Some(b'\'') => !(i > 0 && byte_at(code, i - 1).is_some_and(|c| c.is_ascii_digit())),
        _ => false,
    }
}

/// `cppLiteralEnd`: the index of the quote closing the literal opened at `start`, or the text length.
pub fn cpp_literal_end(text: &str, start: usize) -> usize {
    let b = text.as_bytes();
    let quote = b[start];
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 1;
        } else if b[i] == quote {
            return i;
        }
        i += 1;
    }
    b.len()
}

/// `cppPastBrackets`: the index just past the bracket group opening at `i`; `None` on a mismatch.
pub fn cpp_past_brackets(code: &str, mut i: usize) -> Option<usize> {
    let b = code.as_bytes();
    let mut closers: Vec<u8> = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b'(' => closers.push(b')'),
            b'[' => closers.push(b']'),
            b'{' => closers.push(b'}'),
            b')' | b']' | b'}' => {
                if closers.pop() != Some(c) {
                    return None;
                }
                if closers.is_empty() {
                    return Some(i + 1);
                }
            }
            _ => {
                if cpp_opens_literal(code, i) {
                    i = cpp_literal_end(code, i);
                }
            }
        }
        i += 1;
    }
    Some(b.len())
}

/// `cppPastInitializer`.
fn cpp_past_initializer(code: &str, mut i: usize) -> Option<usize> {
    let b = code.as_bytes();
    while i < b.len() {
        let c = b[i];
        if matches!(c, b',' | b';' | b')' | b']' | b'}') {
            return Some(i);
        }
        if matches!(c, b'(' | b'[' | b'{') {
            i = cpp_past_brackets(code, i)?;
        } else if cpp_opens_literal(code, i) {
            i = cpp_literal_end(code, i) + 1;
        } else {
            i += 1;
        }
    }
    Some(i)
}

/// `cppPastTemplateArguments`.
fn cpp_past_template_arguments(code: &str, mut i: usize) -> Option<usize> {
    let b = code.as_bytes();
    let (mut angles, mut parens) = (0i64, 0i64);
    while i < b.len() {
        let c = b[i];
        if c == b'(' {
            parens += 1;
        } else if c == b')' {
            parens -= 1;
            if parens < 0 {
                return None;
            }
        } else if parens > 0 {
        } else if c == b'<' {
            angles += 1;
        } else if c == b'>' {
            angles -= 1;
            if angles == 0 {
                return Some(i + 1);
            }
        } else if matches!(c, b';' | b'{' | b'}') {
            return None;
        }
        i += 1;
    }
    None
}

/// `cppPastDeclaredType`.
fn cpp_past_declared_type(code: &str, mut i: usize) -> Option<usize> {
    if code[i.min(code.len())..].starts_with("::") {
        i = cpp_skip_space(code, i + 2);
    }
    let mut segment = 0usize;
    loop {
        let mut word = cpp_word_at(code, i);
        if word == Some("template") && segment > 0 {
            i = cpp_skip_space(code, i + "template".len());
            word = cpp_word_at(code, i);
        }
        let word = word?;
        if CPP_NOT_A_DECLARED_TYPE.contains(word) {
            return None;
        }
        i += word.len();
        let mut next = cpp_skip_space(code, i);
        if byte_at(code, next) == Some(b'<') {
            i = cpp_past_template_arguments(code, next)?;
            next = cpp_skip_space(code, i);
        }
        if !code[next.min(code.len())..].starts_with("::") {
            return Some(i);
        }
        i = cpp_skip_space(code, next + 2);
        segment += 1;
    }
}

/// `CppListedName`: a name declared after the first in `int a, *b = 0, c;`.
#[derive(Debug, Clone)]
pub struct CppListedName {
    pub name: String,
    pub ty: String,
    pub type_at: usize,
    pub end: usize,
}

/// `cppDeclaratorsAfterFirst`: the names a declaration statement lists after its first, or
/// `None` when `code[start..]` is not a declaration.
pub fn cpp_declarators_after_first(code: &str, start: usize, in_header: bool) -> Option<Vec<CppListedName>> {
    let mut i = cpp_skip_space(code, start);
    while let Some(word) = cpp_word_at(code, i) {
        if !CPP_DECL_SPECIFIERS.contains(word) {
            break;
        }
        i = cpp_skip_space(code, i + word.len());
    }
    let type_at = i;
    i = cpp_past_declared_type(code, i)?;
    let ty = &code[type_at..i];
    let mut listed: Vec<CppListedName> = Vec::new();
    let mut first = true;
    loop {
        let mut pointer = String::new();
        i = cpp_skip_space(code, i);
        loop {
            if let Some(c @ (b'*' | b'&')) = byte_at(code, i) {
                pointer.push(c as char);
                i = cpp_skip_space(code, i + 1);
                continue;
            }
            let Some(word) = cpp_word_at(code, i) else { break };
            let after = cpp_skip_space(code, i + word.len());
            let annotation = !pointer.is_empty()
                && CPP_DECLARATOR_ANNOTATION_WORD.is_match(word)
                && (matches!(byte_at(code, after), Some(b'*' | b'&')) || cpp_word_at(code, after).is_some());
            if !CPP_DECLARATOR_CV_WORD.is_match(word) && !annotation {
                break;
            }
            i = cpp_skip_space(code, i + word.len());
        }
        if i >= code.len() && !first && pointer.is_empty() {
            return Some(listed);
        }
        let name = cpp_word_at(code, i)?;
        if CPP_NOT_A_DECLARED_NAME.contains(name) {
            return None;
        }
        i = cpp_skip_space(code, i + name.len());
        if code[i.min(code.len())..].starts_with("::") {
            return None;
        }
        while let Some(m) = CPP_TRAILING_ANNOTATION_AT.captures_at(code, i) {
            i = cpp_skip_space(code, m.get(0).unwrap().end());
        }
        if !first {
            listed.push(CppListedName { name: name.to_string(), ty: format!("{ty}{pointer}"), type_at, end: i });
        }
        while byte_at(code, i) == Some(b'[') {
            i = cpp_past_brackets(code, i)?;
            i = cpp_skip_space(code, i);
        }
        if matches!(byte_at(code, i), Some(b'(' | b'{')) {
            i = cpp_past_brackets(code, i)?;
        } else if byte_at(code, i) == Some(b'=') && byte_at(code, i + 1) != Some(b'=') {
            i = cpp_past_initializer(code, i + 1)?;
        }
        i = cpp_skip_space(code, i);
        if i >= code.len() || byte_at(code, i) == Some(b';') || (in_header && byte_at(code, i) == Some(b')')) {
            return Some(listed);
        }
        if byte_at(code, i) != Some(b',') {
            return None;
        }
        i += 1;
        first = false;
    }
}

// -- Matches ----------------------------------------------------------------------------------

/// What `matchCppDeclarator` / `matchCppListedDeclarator` return: `m.index`, the end of `m[0]`
/// and `m[1]` (the declared type, already trimmed of annotations).
#[derive(Debug, Clone)]
pub struct CppDeclMatch {
    pub index: usize,
    pub end: usize,
    pub ty: String,
}

impl CppDeclMatch {
    /// The text after the match.
    pub fn rest<'a>(&self, code: &'a str) -> &'a str {
        &code[self.end.min(code.len())..]
    }
}

/// `buildDeclaratorRegex(escapedReceiver)`.
pub fn build_declarator_regex(escaped_receiver: &str) -> Arc<Re> {
    Re::cached(
        &format!(
            r"({})\s*\b{escaped_receiver}\b(?:\s*{CPP_TRAILING_ANNOTATION})*\s*(?=[;=,)\[{{(]|$)",
            &*CPP_DECLARATOR_TYPE
        ),
        "",
    )
}

/// `buildListedDeclaratorRegex(escapedReceiver)`.
pub fn build_listed_declarator_regex(escaped_receiver: &str) -> Arc<Re> {
    Re::cached(&format!(r",(?:\s|[*&]|{CPP_DECLARATOR_CV}|{CPP_DECLARATOR_ANNOTATION})*\b{escaped_receiver}\b"), "")
}

/// `matchCppDeclarator`.
pub fn match_cpp_declarator(line: &str, regex: &Re) -> Option<CppDeclMatch> {
    let caps = regex.captures(line)?;
    let whole = caps.get(0)?;
    let ty = cpp_declarator_type(caps.text(1)?);
    Some(CppDeclMatch { index: whole.start(), end: whole.end(), ty })
}

/// `matchCppDeclaratorFrom`: the first match at or after `from`.
pub fn match_cpp_declarator_from(line: &str, regex: &Re, from: usize) -> Option<CppDeclMatch> {
    let caps = regex.captures_at(line, from)?;
    let whole = caps.get(0)?;
    let ty = cpp_declarator_type(caps.text(1)?);
    Some(CppDeclMatch { index: whole.start(), end: whole.end(), ty })
}

/// `cppDeclaratorType`: drop an annotation macro written after a pointer (`T* FOO_BAR p`).
pub fn cpp_declarator_type(text: &str) -> String {
    let b = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let (mut depth, mut after_pointer, mut dropped) = (0i64, false, false);
    let mut i = 0usize;
    while i < b.len() {
        let prev_ok = i == 0 || !(is_word_byte(b[i - 1]) || b[i - 1] == b':');
        let word = if depth == 0 && prev_ok { cpp_word_at(text, i) } else { None };
        if let Some(word) = word {
            if after_pointer && CPP_DECLARATOR_ANNOTATION_WORD.is_match(word) {
                dropped = true;
            } else {
                out.extend_from_slice(word.as_bytes());
                after_pointer = false;
            }
            i += word.len();
            continue;
        }
        let c = b[i];
        if c == b'<' {
            depth += 1;
        } else if c == b'>' {
            depth -= 1;
        } else if depth == 0 && (c == b'*' || c == b'&') {
            after_pointer = true;
        } else if depth == 0 && space_len_at(text, i) == 0 {
            after_pointer = false;
        }
        out.push(c);
        i += 1;
    }
    if !dropped {
        return text.to_string();
    }
    let out = String::from_utf8_lossy(&out).into_owned();
    re!(r"\s+").replace_all_str(&out, " ").trim_matches(is_js_space).to_string()
}

/// `cppListedDeclaratorOn`.
pub fn cpp_listed_declarator_on(line: &str, receiver_name: &str) -> Option<CppDeclMatch> {
    let b = line.as_bytes();
    let mut i: isize = -1;
    while i < b.len() as isize {
        let c = if i >= 0 { Some(b[i as usize]) } else { None };
        let header = c == Some(b'(') && matches!(cpp_word_before(line, i as usize), "for" | "if" | "switch");
        if i >= 0 && !matches!(c, Some(b';' | b'{' | b'}')) && !header {
            i += 1;
            continue;
        }
        if let Some(named) =
            cpp_declarators_after_first(line, (i + 1) as usize, header).and_then(|l| l.into_iter().find(|d| d.name == receiver_name))
        {
            return Some(CppDeclMatch { index: named.type_at, end: named.end, ty: named.ty });
        }
        i += 1;
    }
    None
}

/// `matchCppListedDeclarator`.
pub fn match_cpp_listed_declarator(line: &str, receiver_name: &str, listed_regex: &Re) -> Option<CppDeclMatch> {
    if listed_regex.is_match(line) {
        cpp_listed_declarator_on(line, receiver_name)
    } else {
        None
    }
}

/// `cppTypeBeganAbove`: a `>` with no `<` before it on this line.
pub fn cpp_type_began_above(type_name: &str) -> bool {
    let mut depth = 0i64;
    for c in type_name.chars() {
        if c == '<' {
            depth += 1;
        } else if c == '>' {
            depth -= 1;
            if depth < 0 {
                return true;
            }
        }
    }
    false
}

/// `cppEndsDeclarationBegunAbove`.
pub fn cpp_ends_declaration_begun_above(line: &str, escaped_receiver: &str) -> bool {
    let pattern = format!(
        r"(?:^|[^\w:])([A-Za-z_][\w:]*\s*<[^;=(){{}}]+>)(?:\s*(?:const\b|volatile\b|[*&]+))*\s*{escaped_receiver}\b"
    );
    match Re::cached(&pattern, "").captures(line) {
        Some(caps) => caps.text(1).is_some_and(cpp_type_began_above),
        None => false,
    }
}

/// `cppDeclarationBegunAbove`: the type of a declaration whose type is written on the lines above.
pub fn cpp_declaration_begun_above(
    lines: &[String],
    index: usize,
    line: &str,
    receiver_pattern: &Re,
    declarator_regex: &Re,
    raw_lines: &crate::cpp_raw_strings::NonCodeLines,
) -> Option<String> {
    let at = receiver_pattern.find(line)?.start();
    if index == 0 {
        return None;
    }
    let code_at = |j: usize| -> String {
        cpp_code_of(&crate::cpp_raw_strings::cpp_literals_blanked(lines.get(j).map(String::as_str).unwrap_or(""), j + 1, raw_lines)).unwrap_or_default()
    };
    let head = &line[..at];
    if re!(r"^\s*[*&]*\s*$").is_match(head) {
        let above = code_at(index - 1);
        return if is_cpp_type_line(&above) {
            match_cpp_declarator(&format!("{above} {line}"), declarator_regex).map(|m| m.ty)
        } else {
            None
        };
    }
    if !cpp_type_began_above(head) {
        return None;
    }
    let mut text = line.to_string();
    let mut before = head.to_string();
    let mut j = index as isize - 1;
    while j >= (index as isize - 6).max(0) {
        let above = code_at(j as usize);
        text = format!("{above} {text}");
        before = format!("{above} {before}");
        if !cpp_type_began_above(&before) {
            return match_cpp_declarator(&text, declarator_regex).map(|m| m.ty);
        }
        j -= 1;
    }
    None
}

/// `normalizeCppTypeName`: the bare class name of a written type, or `None`.
pub fn normalize_cpp_type_name(type_name: &str) -> Option<String> {
    if cpp_type_began_above(type_name) {
        return None;
    }
    let spaced = re!(r"\b(const|volatile|mutable|typename|class|struct)\b").replace_all_str(type_name, " ");
    let spaced = re!(r"[&*]+").replace_all_str(&spaced, " ");
    let stripped = crate::cpp_type_aliases::strip_cpp_template_arguments(&spaced);
    let normalized = re!(r"\s+").replace_all_str(&stripped, " ");
    let normalized = normalized.trim_matches(is_js_space);
    if normalized.is_empty() {
        return None;
    }
    let last = normalized.split("::").filter(|p| !p.is_empty()).last()?;
    if CPP_NON_TYPE_TOKENS.contains(last) {
        return None;
    }
    Some(last.to_string())
}

// -- Comments and the range-for header -----------------------------------------------------------

/// `cppCodeOf`: the code part of a line (comments blanked or cut), or `None` for a comment line.
pub fn cpp_code_of(line: &str) -> Option<String> {
    if re!(r"^\s*//").is_match(line) {
        return None;
    }
    let code = re!(r"/\*.*?\*/", "g").replace_all(line, |c| Some(" ".repeat(utf16_len(c.text(0).unwrap_or("")))));
    let mut code = code;
    if let Some(open) = code.find("/*") {
        code.truncate(open);
    }
    if let Some(comment) = code.find("//") {
        code.truncate(comment);
    }
    Some(code)
}

/// `cppRebindsReceiver`: the line re-declares the receiver in a range-for or structured binding.
pub fn cpp_rebinds_receiver(line: &str, escaped_receiver: &str) -> bool {
    Re::cached(&format!(r"\bfor\s*\(.*\b{escaped_receiver}\s*:(?!:)"), "").is_match(line)
        || Re::cached(&format!(r"\bauto\s*&{{0,2}}\s*\[[^\]]*\b{escaped_receiver}\b[^\]]*\]"), "").is_match(line)
}

/// A `for (T name : range)` header: the declared type, and the byte offsets of the `(` and the
/// character after the `:`.
#[derive(Debug, Clone)]
pub struct CppRangeFor {
    pub ty: String,
    pub paren: usize,
    pub colon: usize,
}

/// `cppRangeForDeclaration`.
pub fn cpp_range_for_declaration(line: &str, receiver_name: &str) -> Option<CppRangeFor> {
    if !line.contains("for") {
        return None;
    }
    for m in CPP_RANGE_FOR_DECLARATION.captures_iter(line) {
        if m.text(2) != Some(receiver_name) {
            continue;
        }
        let whole = m.get(0)?;
        let tail = CPP_DECLARED_TYPE_TAIL.captures(m.text(1)?)?;
        let paren = whole.start() + whole.as_str().find('(')?;
        return Some(CppRangeFor { ty: cpp_declarator_type(tail.text(1)?), paren, colon: whole.end() });
    }
    None
}

/// `cppRangeExpression`: the range expression of a `for` header whose `:` is at `column` of
/// `lines[line]` (0-based line index).
pub fn cpp_range_expression(lines: &[String], line: usize, column: usize) -> Option<String> {
    let mut depth = 1i64;
    let mut text = String::new();
    let mut l = line;
    while l < lines.len().min(line + 4) {
        let stripped = re!(r"/\*.*?\*/", "g").replace_all_str(&lines[l], " ");
        let code = re!(r"//.*$").replace_all_str(&stripped, "");
        let from = if l == line { column.min(code.len()) } else { 0 };
        for ch in slice_bytes(&code, from, code.len()).chars() {
            if ch == '"' || ch == '\'' {
                return None;
            }
            if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 {
                    let t = text.trim_matches(is_js_space);
                    return if t.is_empty() { None } else { Some(t.to_string()) };
                }
            }
            text.push(ch);
        }
        text.push(' ');
        l += 1;
    }
    None
}

/// `cppForBodyEncloses`: does the body (or statement) of the `for` whose `(` is at `paren` of
/// `lines[line]` contain the use at `use_line`:`use_column`?
pub fn cpp_for_body_encloses(lines: &[String], line: usize, paren: usize, use_line: usize, use_column: usize) -> bool {
    #[derive(PartialEq)]
    enum Phase {
        Before,
        Header,
        Gap,
        Block,
        Statement,
    }
    let mut phase = Phase::Before;
    let mut depth = 0i64;
    let mut comment = false;
    let mut l = line;
    while l <= use_line && l < lines.len() {
        let text = lines[l].as_bytes();
        let end = if l == use_line { use_column.min(text.len()) } else { text.len() };
        let mut c = 0usize;
        while c < end {
            let ch = text[c];
            if comment {
                if ch == b'*' && text.get(c + 1) == Some(&b'/') {
                    comment = false;
                    c += 1;
                }
                c += 1;
                continue;
            }
            if ch == b'/' && text.get(c + 1) == Some(&b'/') {
                break;
            }
            if ch == b'/' && text.get(c + 1) == Some(&b'*') {
                comment = true;
                c += 2;
                continue;
            }
            if ch == b'"' || (ch == b'\'' && !(c > 0 && text[c - 1].is_ascii_digit())) {
                c = cpp_literal_end(&lines[l], c) + 1;
                continue;
            }
            if phase == Phase::Before {
                if l == line && c == paren {
                    phase = Phase::Header;
                    depth = 1;
                }
                c += 1;
                continue;
            }
            if phase == Phase::Header {
                if ch == b'(' {
                    depth += 1;
                } else if ch == b')' {
                    depth -= 1;
                    if depth == 0 {
                        phase = Phase::Gap;
                    }
                }
                c += 1;
                continue;
            }
            if phase == Phase::Gap {
                if space_len_at(&lines[l], c) > 0 {
                    c += 1;
                    continue;
                }
                phase = if ch == b'{' { Phase::Block } else { Phase::Statement };
            }
            if matches!(ch, b'(' | b'[' | b'{') {
                depth += 1;
            } else if ch == b')' || ch == b']' {
                depth -= 1;
            } else if ch == b'}' {
                depth -= 1;
                if depth <= 0 {
                    return false;
                }
            } else if ch == b';' && depth <= 0 && phase == Phase::Statement {
                return false;
            }
            c += 1;
        }
        l += 1;
    }
    phase != Phase::Before && phase != Phase::Header
}

/// `escapeRegExp` for names spliced into patterns.
pub fn esc(name: &str) -> String {
    escape_regex(name)
}
