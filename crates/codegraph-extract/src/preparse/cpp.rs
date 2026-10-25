//! The C++ `preParse` pipeline (`preParseCppSource` and the passes it chains in
//! languages/c-cpp.ts): macro-annotated classes, macro-prefixed functions and
//! members, Unreal-style reflection markup, MSL attributes, CUDA.

use super::cmask::*;
use super::cshared::*;
use super::jsre::{Re, Sticky};
use std::collections::HashSet;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Keyword sets
// ---------------------------------------------------------------------------

const CPP_PAREN_KEYWORDS: &[&str] = &[
    "if", "while", "for", "switch", "catch", "return", "sizeof", "alignof", "alignas", "decltype", "typeid", "noexcept",
    "throw", "static_assert", "new", "delete", "case", "requires", "co_await", "co_yield", "co_return", "asm", "__asm__",
    "__attribute__", "__attribute", "__declspec", "_Pragma", "else", "do", "constexpr", "consteval", "explicit",
    "typeof", "__typeof__", "__typeof", "_Alignof", "__alignof__", "volatile", "__volatile__",
];
const CPP_TYPE_KEYWORDS: &[&str] = &[
    "void", "bool", "char", "char8_t", "char16_t", "char32_t", "wchar_t", "short", "int", "long", "float", "double",
    "signed", "unsigned", "auto",
];
const CPP_EXTRA_KEYWORDS: &[&str] = &[
    "const", "volatile", "static", "extern", "inline", "virtual", "explicit", "constexpr", "consteval", "constinit",
    "friend", "mutable", "thread_local", "register", "typedef", "using", "template", "typename", "class", "struct",
    "union", "enum", "namespace", "operator", "goto", "break", "continue", "default", "public", "private",
    "protected", "this", "true", "false", "nullptr", "and", "or", "not", "xor", "bitand", "bitor", "compl", "and_eq",
    "or_eq", "xor_eq", "not_eq",
];

static PAREN_KEYWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| CPP_PAREN_KEYWORDS.iter().copied().collect());
static TYPE_KEYWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| CPP_TYPE_KEYWORDS.iter().copied().collect());
static KEYWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    CPP_TYPE_KEYWORDS.iter().chain(CPP_PAREN_KEYWORDS).chain(CPP_EXTRA_KEYWORDS).copied().collect()
});

fn byte_at(s: &str, i: usize) -> Option<u8> {
    s.as_bytes().get(i).copied()
}

// ---------------------------------------------------------------------------
// class/struct EXPORT_MACRO Name
// ---------------------------------------------------------------------------

static CLASS_HEAD_GUARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b(?:class|struct)\s+[A-Z][A-Z0-9_]"));
static CLASS_HEAD_MACROS: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"\b(?:class|struct)\s+((?:[A-Z][A-Z0-9_]+(?:\s*\((?:[^()]|\([^()]*\))*\))?\s+)+)(?!final\b)[A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*",
    )
});
static FINAL_WORD: LazyLock<Re> = LazyLock::new(|| Re::new(r"^final\b"));

pub(crate) fn blank_cpp_export_macros(source: &str) -> String {
    if !source.contains("class") && !source.contains("struct") {
        return source.to_string();
    }
    if !CLASS_HEAD_GUARD.is_match(source) {
        return source.to_string();
    }
    let code = mask_cpp_code(source);
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(m) = CLASS_HEAD_MACROS.captures_at(&code, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let mut i = skip_cpp_space(&code, whole.end());
        if byte_at(&code, i) == Some(b'<') {
            let Some(after) = skip_cpp_angle_args(&code, i) else { continue };
            i = skip_cpp_space(&code, after);
        }
        if FINAL_WORD.is_match(&code[i.min(code.len())..(i + 6).min(code.len())]) {
            i = skip_cpp_space(&code, i + 5);
        }
        let c = byte_at(&code, i);
        if c != Some(b'{') && !(c == Some(b':') && byte_at(&code, i + 1) != Some(b':')) {
            continue;
        }
        let run = m.get(1).unwrap();
        push_cpp_run_tokens(&mut spans, run.as_str(), run.start());
    }
    blank_cpp_spans(source, &spans, Some(&code))
}

// ---------------------------------------------------------------------------
// Pointer annotations, trailing attributes, declarator attributes
// ---------------------------------------------------------------------------

static POINTER_ANNOTATION_GUARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"[*&]\s*[A-Z][A-Z0-9]*_"));
static POINTER_ANNOTATION: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"([*&])\s*((?:[A-Z][A-Z0-9]*_[A-Z0-9_]*(?:\s+|(?=\*)))+)(?=\*|::|(?!(?:and|and_eq|bitand|bitor|compl|not|not_eq|or|or_eq|xor|xor_eq)\b)[A-Za-z_~])",
    )
});
static AFTER_CLOSER_OR_NUMBER: LazyLock<Re> = LazyLock::new(|| Re::new(r"(?:[)\]]|\b\d[\w.]*)\s*$"));

pub(crate) fn blank_cpp_pointer_annotation_macros(source: &str) -> String {
    if !POINTER_ANNOTATION_GUARD.is_match(source) {
        return source.to_string();
    }
    let code = mask_cpp_code(source);
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(m) = POINTER_ANNOTATION.captures_at(&code, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let run = m.get(2).unwrap();
        let start = whole.end() - run.as_str().len();
        // After `&`, only a name: `flags & FOO_BIT\n  && ok` is an expression.
        if m.get(1).unwrap().as_str() == "&" && byte_at(&code, start + run.as_str().len()) == Some(b'*') {
            continue;
        }
        // A `*` after a closing bracket or a number multiplies.
        let window = &code[whole.start().saturating_sub(40)..whole.start()];
        if AFTER_CLOSER_OR_NUMBER.is_match(window) {
            continue;
        }
        push_cpp_run_tokens(&mut spans, run.as_str(), start);
    }
    blank_cpp_spans(source, &spans, Some(&code))
}

static TRAILING_ATTR_GUARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\)[\s\w&]*?\b[A-Z][A-Z0-9_]{2,}\b"));
static TRAILING_ATTR_CANDIDATE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"\)((?:\s*(?:const|volatile|noexcept|override|final|&&|&)(?![\w]))*)\s*(?=[A-Z][A-Z0-9_]{2,}\b)")
});
static TRAILING_ATTR_RUN: LazyLock<Sticky> = LazyLock::new(|| {
    Sticky::new(r"(?:[A-Z][A-Z0-9_]{2,}\b(?:\s*\((?:[^()]|\((?:[^()]|\([^()]*\))*\))*\))?\s*)+")
});
static TRAILING_ATTR_FOLLOW: LazyLock<Sticky> =
    LazyLock::new(|| Sticky::new(r"(?:\{|;|=(?!=)|:(?!:)|->|(?:override|final|const|noexcept)\b)"));
static BLANK_LINE: LazyLock<Re> = LazyLock::new(|| Re::new(r"\n[ \t]*\r?\n"));

pub(crate) fn blank_cpp_trailing_attribute_macros(source: &str) -> String {
    if !TRAILING_ATTR_GUARD.is_match(source) {
        return source.to_string();
    }
    let code = mask_cpp_code(source);
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(m) = TRAILING_ATTR_CANDIDATE.captures_at(&code, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let macro_start = whole.end();
        if BLANK_LINE.is_match(whole.as_str()) {
            continue;
        }
        let Some((_, run_end, _)) = TRAILING_ATTR_RUN.at(&code, macro_start) else { continue };
        if TRAILING_ATTR_FOLLOW.matches_at(&code, run_end).is_none() {
            continue;
        }
        if !closes_parameter_list(&code, whole.start()) {
            continue;
        }
        push_cpp_run_tokens(&mut spans, &code[macro_start..run_end], macro_start);
        pos = run_end;
    }
    blank_cpp_spans(source, &spans, Some(&code))
}

static DECLARATOR_ATTR_GUARD: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"[\w\]]\s+[A-Z][A-Z0-9]*_[A-Z0-9_]*\s*[(;=,)]"));
static DECLARATOR_ATTR_CANDIDATE: LazyLock<Re> = LazyLock::new(|| Re::new(r"(?<![\w$])[A-Z][A-Z0-9]*_[A-Z0-9_]*\b"));
static DECLARATOR_ATTR_RUN: LazyLock<Sticky> = LazyLock::new(|| {
    Sticky::new(r"(?:[A-Z][A-Z0-9]*_[A-Z0-9_]*\b(?:\s*\((?:[^()]|\((?:[^()]|\([^()]*\))*\))*\))?\s*)+")
});

pub(crate) fn blank_cpp_declarator_attribute_macros(source: &str) -> String {
    if !DECLARATOR_ATTR_GUARD.is_match(source) {
        return source.to_string();
    }
    let code = mask_cpp_code(source);
    let cb = code.as_bytes();
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(m) = DECLARATOR_ATTR_CANDIDATE.captures_at(&code, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let macro_start = whole.start();
        // Whitespace before the macro, and before that a name or `]`.
        let mut last = macro_start as isize - 1;
        while last >= 0 && is_space(cb[last as usize]) {
            last -= 1;
        }
        if last == macro_start as isize - 1 || last < 0 || !(is_word(cb[last as usize]) || cb[last as usize] == b']') {
            continue;
        }
        let last = last as usize;
        let Some((_, run_end, _)) = DECLARATOR_ATTR_RUN.at(&code, macro_start) else { continue };
        let tail_ok = match byte_at(&code, run_end) {
            Some(b';') | Some(b',') | Some(b')') => true,
            Some(b'=') => byte_at(&code, run_end + 1) != Some(b'='),
            _ => false,
        };
        if !tail_ok {
            continue;
        }
        if cb[last] != b']' && !follows_declared_name(&code, last) {
            continue;
        }
        push_cpp_run_tokens(&mut spans, &code[macro_start..run_end], macro_start);
        pos = run_end;
    }
    blank_cpp_spans(source, &spans, Some(&code))
}

static IDENT_FULL: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[A-Za-z_]\w*$"));
static HAS_LOWER: LazyLock<Re> = LazyLock::new(|| Re::new(r"[a-z]"));

/// True when the identifier ending at `code[last]` is a name a declaration declares.
fn follows_declared_name(code: &str, last: usize) -> bool {
    let b = code.as_bytes();
    let mut start = last;
    while start > 0 && is_word(b[start - 1]) {
        start -= 1;
    }
    let name = &code[start..=last];
    if !IDENT_FULL.is_match(name) || !HAS_LOWER.is_match(name) || KEYWORDS.contains(name) {
        return false;
    }
    let mut i = start as isize - 1;
    while i >= 0 && is_space(b[i as usize]) {
        i -= 1;
    }
    let c = if i >= 0 { Some(b[i as usize]) } else { None };
    if c == Some(b'*') || c == Some(b'&') {
        return true;
    }
    if c == Some(b'>') {
        return i < 1 || b[i as usize - 1] != b'-';
    }
    let Some(c) = c else { return false };
    if !is_word(c) {
        return false;
    }
    let i = i as usize;
    let mut j = i;
    while j > 0 && is_word(b[j - 1]) {
        j -= 1;
    }
    let before = &code[j..=i];
    if before == "const" || before == "volatile" {
        let mut k = j as isize - 1;
        while k >= 0 && is_space(b[k as usize]) {
            k -= 1;
        }
        return k >= 0 && (b[k as usize] == b'*' || b[k as usize] == b'&');
    }
    if TYPE_KEYWORDS.contains(before) {
        return true;
    }
    IDENT_FULL.is_match(before) && !KEYWORDS.contains(before)
}

static OPERATOR_BEFORE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\boperator\s*(?:\(\s*\)|\[\s*\]|[^\s\w()\[\]]{1,3}|\s+[A-Za-z_][\w\s:<>*&]*)\s*$"));
static TRAILING_IDENT: LazyLock<Re> = LazyLock::new(|| Re::new(r"([A-Za-z_]\w*)\s*$"));
static ALL_CAPS: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[A-Z][A-Z0-9_]*$"));

/// True when the `)` at `code[close]` closes a function declarator's parameter list.
fn closes_parameter_list(code: &str, close: usize) -> bool {
    let b = code.as_bytes();
    let mut depth = 0i32;
    let mut open: Option<usize> = None;
    let mut i = close as isize;
    while i >= 0 && (close as isize - i) < 4000 {
        let c = b[i as usize];
        if c == b')' {
            depth += 1;
        } else if c == b'(' {
            depth -= 1;
            if depth == 0 {
                open = Some(i as usize);
                break;
            }
        } else if c == b';' || c == b'{' || c == b'}' {
            return false;
        }
        i -= 1;
    }
    let Some(open) = open else { return false };
    let before = &code[open.saturating_sub(200)..open];
    // `operator()(…)`, `operator==(…)`, `operator bool(…)`
    if OPERATOR_BEFORE.is_match(before) {
        return true;
    }
    match TRAILING_IDENT.captures(before) {
        Some(c) => {
            let name = c.get(1).unwrap().as_str();
            !PAREN_KEYWORDS.contains(name) && !ALL_CAPS.is_match(name)
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Attribute macros that open a declaration
// ---------------------------------------------------------------------------

const CPP_TYPE_ARGS: &str = r"<(?:[^<>;\{\}]|<(?:[^<>;\{\}]|<[^<>;\{\}]*>)*>)*>";

static LEADING_ATTR_GUARD: LazyLock<Re> =
    LazyLock::new(|| Re::multiline(r"(?:^|\]\])[ \t]*[A-Z][A-Z0-9]*_[A-Z0-9_]*\s"));
static LEADING_ATTR: LazyLock<Re> = LazyLock::new(|| {
    let type_name = format!(
        r"(?:::\s*)?(?!(?:const|volatile|operator|return)\b)[A-Za-z_]\w*(?:\s*{a})?(?:\s*::\s*[A-Za-z_]\w*(?:\s*{a})?)*",
        a = CPP_TYPE_ARGS
    );
    let declared_name = r"(?:(?:::\s*)?~?[A-Za-z_]\w*(?:\s*::\s*~?[A-Za-z_]\w*)*\s*[(;=\[\{]|operator\b)";
    Re::multiline(&format!(
        r"(^[ \t]*|\]\][ \t]*)((?:[A-Z][A-Z0-9]*_[A-Z0-9_]*\s+)+)(?=(?:static|inline|virtual|explicit|constexpr|consteval|constinit|friend|extern|thread_local|mutable|typedef|using|template|typename|class|struct|union|enum|void|bool|char|char8_t|char16_t|char32_t|wchar_t|short|int|long|float|double|signed|unsigned|auto|decltype|return)\b|\[\[|(?:[A-Za-z_]\w*\s*::\s*)*([A-Za-z_]\w*)\s*::\s*~?\3\s*\(|(?:(?:const|volatile)\s+)*{type_name}(?:\s*(?:const|volatile)\b)?(?:\s*[*&]+\s*|\s+)(?:(?:const|volatile)\s+)?{declared_name})"
    ))
});

pub(crate) fn blank_cpp_leading_attribute_macros(source: &str) -> String {
    if !LEADING_ATTR_GUARD.is_match(source) {
        return source.to_string();
    }
    let code = mask_cpp_code(source);
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(m) = LEADING_ATTR.captures_at(&code, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let start = whole.start() + m.get(1).unwrap().as_str().len();
        push_cpp_run_tokens(&mut spans, m.get(2).unwrap().as_str(), start);
    }
    blank_cpp_spans(source, &spans, Some(&code))
}

// ---------------------------------------------------------------------------
// Curated inline-specifier macros
// ---------------------------------------------------------------------------

const CPP_INLINE_MACROS: &[&str] = &[
    "FORCEINLINE_DEBUGGABLE", "FORCENOINLINE", "FORCEINLINE",
    "PUGI__FN_NO_INLINE", "PUGI__FN", "PUGIXML_FUNCTION",
    "_ALWAYS_INLINE_", "_FORCE_INLINE_",
    "BOOST_FORCEINLINE", "BOOST_NOINLINE",
    "Q_INVOKABLE", "Q_SCRIPTABLE", "Q_ALWAYS_INLINE", "Q_SLOT", "Q_SIGNAL",
    "FOLLY_ALWAYS_INLINE", "FOLLY_NOINLINE",
    "ABSL_ATTRIBUTE_ALWAYS_INLINE", "ABSL_ATTRIBUTE_NOINLINE",
    "LLVM_ATTRIBUTE_ALWAYS_INLINE", "LLVM_ATTRIBUTE_NOINLINE",
    "V8_INLINE", "V8_NOINLINE",
    "EIGEN_STRONG_INLINE", "EIGEN_ALWAYS_INLINE", "EIGEN_DEVICE_FUNC",
    "RAPIDJSON_FORCEINLINE",
    "MOZ_ALWAYS_INLINE", "MOZ_NEVER_INLINE",
    "PROTOBUF_ALWAYS_INLINE", "PROTOBUF_NOINLINE",
    "FMT_CONSTEXPR20", "FMT_CONSTEXPR", "FMT_INLINE",
    "JSON_HEDLEY_ALWAYS_INLINE", "JSON_HEDLEY_NEVER_INLINE",
    "HEDLEY_ALWAYS_INLINE", "HEDLEY_NEVER_INLINE",
    "GLM_FUNC_QUALIFIER", "GLM_FUNC_DECL", "GLM_CONSTEXPR", "GLM_INLINE",
    "SIMD_FORCE_INLINE",
    "SK_ALWAYS_INLINE",
    "CV_ALWAYS_INLINE", "CV_INLINE",
    "EA_FORCE_INLINE", "EA_NOINLINE",
    "CC_INLINE",
    "NEVER_INLINE",
    "G_INLINE_FUNC", "SQLITE_PRIVATE", "SQLITE_API",
    "STDMETHODCALLTYPE", "WINAPIV", "WINAPI", "APIENTRY",
    "ALWAYS_INLINE", "FORCE_INLINE", "NOINLINE",
];

static INLINE_MACRO: LazyLock<Re> = LazyLock::new(|| {
    let mut macros: Vec<&str> = CPP_INLINE_MACROS.to_vec();
    macros.sort_by(|a, b| b.len().cmp(&a.len())); // longest token first (stable)
    Re::new(&format!(r"\b({})\b(?=\s+[A-Za-z_])", macros.join("|")))
});

pub(crate) fn blank_cpp_inline_macros(source: &str) -> String {
    if !CPP_INLINE_MACROS.iter().any(|m| source.contains(m)) {
        return source.to_string();
    }
    INLINE_MACRO.replace_all(source, |c| Some(spaces(c.get(0).unwrap().as_str().len())))
}

static API_PREFIX_GUARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"_(?:API|EXPORT|ABI)\b"));
static API_PREFIX: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\b[A-Z][A-Z0-9_]*(?:_API|_EXPORT|_ABI)\b(?=\s+[A-Za-z_])"));

pub(crate) fn blank_cpp_api_prefix_macros(source: &str) -> String {
    if !API_PREFIX_GUARD.is_match(source) {
        return source.to_string();
    }
    API_PREFIX.replace_all(source, |c| Some(spaces(c.get(0).unwrap().as_str().len())))
}

static INLINE_ANNOTATION_GUARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b(?:UMETA|UPARAM|UE_DEPRECATED)"));
static INLINE_ANNOTATION: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b(?:UMETA|UPARAM|UE_DEPRECATED\w*)\s*\("));

pub(crate) fn blank_cpp_inline_annotation_macros(source: &str) -> String {
    if !INLINE_ANNOTATION_GUARD.is_match(source) {
        return source.to_string();
    }
    let raw = mask_cpp_raw_strings(source);
    let source = raw.source.as_str();
    let mut chars = source.as_bytes().to_vec();
    let mut pos = 0;
    while let Some(m) = INLINE_ANNOTATION.captures_at(source, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let Some(end) = balanced_close(source, whole.end() - 1) else { continue };
        blank_range(&mut chars, whole.start(), end);
        pos = end;
    }
    raw.restore(&from_bytes(chars))
}

// ---------------------------------------------------------------------------
// Metal
// ---------------------------------------------------------------------------

static METAL_ATTRIBUTE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"\[\[\s*[A-Za-z_]\w*(?:\s*\([^()\n]*\))?(?:\s*,\s*[A-Za-z_]\w*(?:\s*\([^()\n]*\))?)*\s*\]\]",
    )
});

pub(crate) fn blank_metal_attributes(source: &str) -> String {
    if !source.contains("[[") {
        return source.to_string();
    }
    METAL_ATTRIBUTE.replace_all(source, |c| Some(spaces(c.get(0).unwrap().as_str().len())))
}

// ---------------------------------------------------------------------------
// COM `interface` alias
// ---------------------------------------------------------------------------

static COM_ALIAS: LazyLock<Re> = LazyLock::new(|| Re::multiline(r"^[ \t]*#[ \t]*define[ \t]+interface[ \t]+struct\b"));
static DIRECTIVE_LINES: LazyLock<Re> = LazyLock::new(|| Re::multiline(r"^[ \t]*#(?:\\\r?\n|[^\r\n])*"));
static COM_DECLARATION: LazyLock<Re> =
    LazyLock::new(|| Re::multiline(r"(^|[;{}])\s*\binterface\s+[A-Za-z_]\w*\s*(:[^;\{\}]+)?\{"));
static VIRTUAL_WORD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bvirtual\b"));

/// Recover MSVC's COM alias only at type-definition sites.
fn normalize_cpp_com_interfaces(source: &str) -> String {
    if !source.contains("interface") {
        return source.to_string();
    }
    let mut code = mask_cpp_comments_and_literals(source);
    let has_alias = COM_ALIAS.is_match(&code);
    code = DIRECTIVE_LINES.replace_all(&code, |c| {
        Some(c.get(0).unwrap().as_str().chars().map(|ch| if ch == '\r' || ch == '\n' { ch } else { '\0' }).collect())
    });
    let mut offsets = Vec::new();
    let mut pos = 0;
    while let Some(m) = COM_DECLARATION.captures_at(&code, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let body_end = code[pos..].find('}').map(|i| i + pos);
        let hay = &code[pos..body_end.unwrap_or(code.len())];
        let has_virtual = VIRTUAL_WORD.is_match(hay);
        if !has_alias && m.get(2).is_none() && !has_virtual {
            continue;
        }
        offsets.push(whole.start() + whole.as_str().find("interface").unwrap());
        pos = whole.end() - 1; // allow the opening brace to start a nested declaration
    }
    if offsets.is_empty() {
        return source.to_string();
    }
    let mut bytes = source.as_bytes().to_vec();
    for off in offsets {
        bytes[off..off + 9].copy_from_slice(b"struct   ");
    }
    from_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Access specifiers spelled as macros
// ---------------------------------------------------------------------------

static ACCESS_GUARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"_[A-Z0-9_]*[ \t]*:(?!:)"));
static ACCESS_MACRO_LINE: LazyLock<Re> = LazyLock::new(|| {
    Re::multiline(r"^([ \t]*)([A-Z][A-Z0-9]*_[A-Z0-9_]*)([ \t]*):(?!:)(?=[ \t]*(?://[^\n]*)?\r?$)")
});

pub(crate) fn rewrite_cpp_access_macros(source: &str) -> String {
    if !ACCESS_GUARD.is_match(source) {
        return source.to_string();
    }
    ACCESS_MACRO_LINE.replace_all(source, |c| {
        let indent = c.get(1).unwrap().as_str();
        let name = c.get(2).unwrap().as_str();
        let gap = c.get(3).unwrap().as_str();
        let keyword = if name.contains("PRIVATE") {
            "private"
        } else if name.contains("PROTECTED") {
            "protected"
        } else if name.contains("PUBLIC") || name == "Q_SIGNALS" || name == "Q_SLOTS" {
            "public"
        } else {
            return None;
        };
        if keyword.len() > name.len() + gap.len() {
            return None;
        }
        Some(format!("{indent}{keyword}{}:", spaces(name.len() + gap.len() - keyword.len())))
    })
}

// ---------------------------------------------------------------------------
// Preprocessor conditionals in the middle of a statement
// ---------------------------------------------------------------------------

static DIRECTIVE_NAME: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*#[ \t]*(\w+)"));
static LINE_COMMENT_TAIL: LazyLock<Re> = LazyLock::new(|| Re::new(r"//.*$"));
static BLOCK_COMMENT_INLINE: LazyLock<Re> = LazyLock::new(|| Re::new(r"/\*.*?\*/"));
static TRAILING_BACKSLASH: LazyLock<Re> = LazyLock::new(|| Re::new(r"\\\s*$"));
static STARTS_WITH_IF: LazyLock<Re> = LazyLock::new(|| Re::new(r"^if"));
static STATEMENT_END: LazyLock<Re> = LazyLock::new(|| Re::new(r"[;{}]$|^[\w\s]*:$|\*/$"));
static ACCESS_LABEL: LazyLock<Re> = LazyLock::new(|| Re::new(r"^(?:public|private|protected)\s*:$"));
static STATEMENT_MACRO: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[A-Z][A-Z0-9_]*(?:\s*\(.*\))?$"));
static CONDITIONAL_DIRECTIVE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^(?:if|ifdef|ifndef|else|elif|elifdef|elifndef|endif)$"));
static OPENS_BLOCK_COMMENT: LazyLock<Re> = LazyLock::new(|| Re::new(r"/\*(?!.*\*/)"));
static NON_CR: LazyLock<Re> = LazyLock::new(|| Re::new(r"[^\r]"));

/// A preprocessor conditional in the middle of a declaration or expression is an
/// error to tree-sitter-cpp. Where the code before the `#if` has not ended a
/// statement, its directives are blanked and so is every branch after the first.
pub(crate) fn flatten_mid_statement_conditionals(source: &str) -> String {
    if !source.contains('#') {
        return source.to_string();
    }
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    let directive = |lines: &[String], i: usize| -> Option<String> {
        DIRECTIVE_NAME.captures(lines.get(i).map_or("", |s| s.as_str())).map(|c| c.get(1).unwrap().as_str().to_string())
    };
    let code = |lines: &[String], i: usize| -> String {
        let l = lines.get(i).map_or("", |s| s.as_str());
        let l = LINE_COMMENT_TAIL.replace_all(l, |_| Some(String::new()));
        let l = BLOCK_COMMENT_INLINE.replace_all(&l, |_| Some(String::new()));
        trim(&l).to_string()
    };
    let mut changed = false;
    let mut last_code = String::new();
    let mut in_block_comment = false;
    let mut region = 0i32;
    let mut skip_from = 0i32;
    let mut continued = false;
    for i in 0..lines.len() {
        if continued {
            continued = TRAILING_BACKSLASH.is_match(&lines[i]);
            continue;
        }
        let d = directive(&lines, i);
        if let (Some(d), false) = (d.as_deref(), in_block_comment) {
            // A directive continued with `\` spans lines; its body is not code.
            if TRAILING_BACKSLASH.is_match(&lines[i]) {
                continued = true;
                if region == 0 {
                    continue;
                }
            }
            let opens = STARTS_WITH_IF.is_match(d);
            if region == 0 {
                let mid = opens
                    && !last_code.is_empty()
                    && !STATEMENT_END.is_match(&last_code)
                    && !ACCESS_LABEL.is_match(&last_code)
                    && !STATEMENT_MACRO.is_match(&last_code);
                if !mid {
                    continue;
                }
                region = 1;
                lines[i] = NON_CR.replace_all(&lines[i], |_| Some(" ".into()));
                changed = true;
                continue;
            }
            if opens {
                region += 1;
            } else if (d == "else" || d == "elif") && skip_from == 0 {
                skip_from = region;
            } else if d == "endif" {
                if skip_from == region {
                    skip_from = 0;
                }
                region -= 1;
            }
            if CONDITIONAL_DIRECTIVE.is_match(d) {
                lines[i] = NON_CR.replace_all(&lines[i], |_| Some(" ".into()));
                changed = true;
            }
            continue;
        }
        let text = lines[i].clone();
        if region > 0 && skip_from > 0 {
            if !trim(&text).is_empty() {
                lines[i] = NON_CR.replace_all(&lines[i], |_| Some(" ".into()));
                changed = true;
            }
            continue;
        }
        // Track block comments so a `#` or `;` inside one is not read as code.
        let trimmed = code(&lines, i);
        if in_block_comment {
            if text.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        let no_line_comment = LINE_COMMENT_TAIL.replace_all(&text, |_| Some(String::new()));
        if OPENS_BLOCK_COMMENT.is_match(&no_line_comment) {
            in_block_comment = true;
            let before = trim(trimmed.split("/*").next().unwrap_or("")).to_string();
            if !before.is_empty() {
                last_code = before;
            }
            continue;
        }
        if !trimmed.is_empty() {
            last_code = trimmed;
        }
    }
    if changed {
        lines.join("\n")
    } else {
        source.to_string()
    }
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// `preParseCppSource`: the whole C++ pass chain, ending in the directive restore.
pub(crate) fn pre_parse_cpp(source: &str, file_path: Option<&str>) -> String {
    let raw = mask_cpp_raw_strings(source);
    let source = super::stage("prelude(foundation,access,flatten)", || flatten_mid_statement_conditionals(&rewrite_cpp_access_macros(&rewrite_foundation_enum_macros(&raw.source))));
    // blankCLeadingAttrMacros runs after the api-prefix blank so a stacked
    // `FMT_NORETURN FMT_API void f(…)` reduces to the `MACRO Ret name(` shape.
    let mut s = super::stage("normalize_cpp_com_interfaces", || normalize_cpp_com_interfaces(&source));
    s = super::stage("blank_cpp_export_macros", || blank_cpp_export_macros(&s));
    s = super::stage("blank_cpp_inline_macros", || blank_cpp_inline_macros(&s));
    s = super::stage("blank_cpp_api_prefix_macros", || blank_cpp_api_prefix_macros(&s));
    s = super::stage("blank_cpp_inline_annotation_macros", || blank_cpp_inline_annotation_macros(&s));
    s = super::stage("blank_cpp_pointer_annotation_macros", || blank_cpp_pointer_annotation_macros(&s));
    s = super::stage("blank_cpp_trailing_attribute_macros", || blank_cpp_trailing_attribute_macros(&s));
    s = super::stage("blank_cpp_declarator_attribute_macros", || blank_cpp_declarator_attribute_macros(&s));
    s = super::stage("blank_cpp_annotation_macro_calls", || blank_cpp_annotation_macro_calls(&s, false));
    s = super::stage("blank_c_leading_attr_macros", || blank_c_leading_attr_macros(&s));
    s = super::stage("blank_cpp_leading_attribute_macros", || blank_cpp_leading_attribute_macros(&s));
    let mut blanked = super::stage("blank_lone_macro_lines", || blank_lone_macro_lines(&s, &C_LONE_MACROS));
    let lower = file_path.map(str::to_ascii_lowercase).unwrap_or_default();
    if lower.ends_with(".metal") {
        blanked = blank_metal_attributes(&blanked);
    } else if lower.ends_with(".cu") || lower.ends_with(".cuh") || looks_like_cuda_source(&source) {
        blanked = blank_cuda_constructs(&blanked);
    }
    raw.restore(&restore_directive_lines(&source, &blanked))
}
