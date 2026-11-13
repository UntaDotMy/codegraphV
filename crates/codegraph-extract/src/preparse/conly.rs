//! The C `preParse` pipeline (`preParseCSource` and the C-only passes in
//! languages/c-cpp.ts): kernel annotations, statement macros, type-keyword
//! macro arguments, `__cplusplus` guards, designated-initializer macro calls.

use super::cconditional::blank_c_unbalanced_conditional_branches;
use super::cmask::*;
use super::cshared::*;
use super::jsre::Re;
use std::collections::HashSet;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// `#ifdef __cplusplus` guard bodies
// ---------------------------------------------------------------------------

static CPLUSPLUS_GUARD_OPEN: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"^[ \t]*#[ \t]*(?:ifdef[ \t]+__cplusplus\b|if[ \t]+defined[ \t]*\(?[ \t]*__cplusplus[ \t]*\)?)")
});
static PREPROC_DIRECTIVE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*#"));
static PREPROC_ENDIF: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*#[ \t]*endif\b"));
const CPLUSPLUS_GUARD_MAX_BODY_LINES: usize = 40;

pub(super) fn strip_cr(l: &str) -> &str {
    l.strip_suffix('\r').unwrap_or(l)
}

/// Replace every character of `line` except a `\r` with a space.
pub(super) fn blank_keep_cr(line: &str) -> String {
    line.chars().map(|c| if c == '\r' { c } else { ' ' }).collect()
}

pub(crate) fn blank_c_cplusplus_guard_bodies(source: &str) -> String {
    if !source.contains("__cplusplus") {
        return source.to_string();
    }
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if !CPLUSPLUS_GUARD_OPEN.is_match(strip_cr(&lines[i])) {
            i += 1;
            continue;
        }
        let mut end = None;
        let mut j = i + 1;
        while j < lines.len() && j - i - 1 <= CPLUSPLUS_GUARD_MAX_BODY_LINES {
            let line = strip_cr(&lines[j]);
            if PREPROC_ENDIF.is_match(line) {
                end = Some(j);
                break;
            }
            if PREPROC_DIRECTIVE.is_match(line) {
                break; // nested directive — bail
            }
            j += 1;
        }
        let Some(end) = end else {
            i += 1;
            continue;
        };
        for k in i + 1..end {
            lines[k] = blank_keep_cr(&lines[k]);
        }
        changed = true;
        i = end + 1;
    }
    if changed {
        lines.join("\n")
    } else {
        source.to_string()
    }
}

// ---------------------------------------------------------------------------
// Iterator macros in statement position
// ---------------------------------------------------------------------------

const C_STMT_MACRO_KEYWORDS: &[&str] = &["if", "while", "for", "switch", "return", "do", "else", "sizeof"];
// Lowercase iterator macros, or an all-caps block macro (`ATOMIC_BLOCK(NVIC_PRIO_MAX) {`,
// `PG_FOREACH(reg) {` — betaflight); a PascalCase name is left alone (a constructor, if the file is
// C++).
static STMT_MACRO_HEAD: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]+([a-z_][a-z0-9_]*|[A-Z_][A-Z0-9_]*)[ \t]*\("));
static CONTROL_HEAD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b(?:if|for|while|switch)\s*$"));
static ELSE_OR_DO: LazyLock<Re> = LazyLock::new(|| Re::new(r"(?:^|\W)(?:else|do)$"));

/// What an open brace holds. A `(` or `[` is held as its byte offset.
const C_BLOCK: i64 = -1;
const C_OTHER_BRACES: i64 = -2;

/// Can a statement follow this code byte, inside a block? After `)`, a macro without its `;`.
fn c_statement_follows(ch: u8) -> bool {
    matches!(ch, b';' | b'{' | b'}' | b':' | b')')
}

/// Which lines start a statement of a function body. `code` is `mask_cpp_code`:
/// directives and comments are spaces. `= {`, `struct s {` and `enum {` hold no
/// statements. The unbraced body of `if`, `for`, `else` or `do` is left out: a
/// `;` there would part the following block from its `else` or `while`. A `#if`
/// branch that leaves a bracket open costs at most the rest of its block.
fn c_statement_lines(code: &str) -> Vec<bool> {
    let bytes = code.as_bytes();
    let mut statements = Vec::new();
    let mut open: Vec<i64> = Vec::new();
    let mut line = 0usize;
    let mut at_line_start = true;
    let mut prev: isize = -1;
    let mut after_control_head = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b'\n' {
            line += 1;
            at_line_start = true;
            i += 1;
            continue;
        }
        if ch == b' ' || ch == b'\t' || ch == b'\r' {
            i += 1;
            continue;
        }
        let top = open.last().copied();
        if at_line_start {
            at_line_start = false;
            if statements.len() <= line {
                statements.resize(line + 1, false);
            }
            let prev_byte = (prev >= 0).then(|| bytes[prev as usize]);
            statements[line] = top == Some(C_BLOCK) && !after_control_head && prev_byte.is_some_and(c_statement_follows);
        }
        after_control_head = false;
        if ch == b'(' || ch == b'[' {
            open.push(i as i64);
        } else if ch == b')' || ch == b']' {
            if let Some(open_at) = top.filter(|t| *t >= 0) {
                open.pop();
                let open_at = open_at as usize;
                let start = open_at.saturating_sub(10);
                after_control_head = CONTROL_HEAD.is_match(&code[start..open_at]);
            }
        } else if ch == b'{' {
            let prev_byte = (prev >= 0).then(|| bytes[prev as usize]);
            let else_or_do = if prev >= 0 {
                let end = prev as usize;
                let start = end.saturating_sub(4);
                ELSE_OR_DO.is_match(&code[start..=end])
            } else {
                false
            };
            let opens_block = prev_byte == Some(b'(')
                || (prev_byte == Some(b')') && top.is_none())
                || (top == Some(C_BLOCK) && (prev_byte.is_some_and(c_statement_follows) || else_or_do));
            open.push(if opens_block { C_BLOCK } else { C_OTHER_BRACES });
        } else if ch == b'}' {
            while let Some(popped) = open.pop() {
                if popped < 0 {
                    break;
                }
            }
        }
        prev = i as isize;
        i += 1;
    }
    statements
}

/// Scan `line` from `from` for the `)` that balances `depth`, skipping literals.
/// `Err(())` when a `;`, `{` or `}` shows up (only with `bail_on_stmt`).
fn scan_parens(line: &[u8], from: usize, depth: &mut i32, bail_on_stmt: bool) -> Result<Option<usize>, ()> {
    let mut k = from;
    while k < line.len() {
        let ch = line[k];
        if ch == b'"' || ch == b'\'' {
            k += 1;
            while k < line.len() && line[k] != ch {
                if line[k] == b'\\' {
                    k += 1;
                }
                k += 1;
            }
            k += 1;
            continue;
        }
        if bail_on_stmt && (ch == b';' || ch == b'{' || ch == b'}') {
            return Err(());
        }
        if ch == b'(' {
            *depth += 1;
        } else if ch == b')' {
            *depth -= 1;
            if *depth == 0 {
                return Ok(Some(k));
            }
        }
        k += 1;
    }
    Ok(None)
}

pub(crate) fn blank_c_statement_macro_calls(source: &str) -> String {
    let raw = mask_cpp_raw_strings(source);
    let source = raw.source.as_str();
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    let mut changed = false;
    let mut statement_lines: Option<Vec<bool>> = None;
    let content = |l: &str| -> String { trim(strip_cr(l)).to_string() };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].clone();
        let Some(m) = STMT_MACRO_HEAD.captures(&line) else {
            i += 1;
            continue;
        };
        let ident = m.get(1).unwrap().as_str();
        if C_STMT_MACRO_KEYWORDS.contains(&ident) {
            i += 1;
            continue;
        }
        let head_len = m.get(0).unwrap().end();
        let open = head_len - 1; // line.indexOf('(', m[0].length - 1)
        let mut depth = 0i32;
        let mut close = scan_parens(line.as_bytes(), open, &mut depth, false).unwrap();
        let mut end_line = i;
        if close.is_none() {
            // Parens don't balance on the head line — the kernel wraps iterator
            // macros. Continue over a few continuation lines; a `;` or a brace
            // anywhere in the span means a real statement — bail.
            if line.contains(';') {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j <= i + 5 && j < lines.len() && close.is_none() {
                match scan_parens(lines[j].as_bytes(), 0, &mut depth, true) {
                    Err(()) => break,
                    Ok(Some(k)) => {
                        close = Some(k);
                        end_line = j;
                    }
                    Ok(None) => {}
                }
                j += 1;
            }
            if close.is_none() {
                i += 1;
                continue;
            }
        }
        let close = close.unwrap();
        let end_line_str = lines[end_line].clone();
        let after = trim(strip_cr(&end_line_str[close + 1..])).to_string();
        if after.is_empty() {
            // Brace on the next line, or a brace-less single-statement body.
            let mut next = end_line + 1;
            while next < lines.len() && content(&lines[next]).is_empty() {
                next += 1;
            }
            if next >= lines.len() {
                i += 1;
                continue;
            }
            let first = content(&lines[next]).as_bytes().first().copied();
            match first {
                Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'{' => {}
                _ => {
                    i += 1;
                    continue;
                }
            }
        } else if after != "{" {
            i += 1;
            continue;
        }
        let ident_start = line.find(ident).unwrap();
        if statement_lines.is_none() {
            statement_lines = Some(c_statement_lines(&mask_cpp_code(source)));
        }
        let keep_name = statement_lines.as_ref().and_then(|rows| rows.get(i).copied()).unwrap_or(false);
        let kept = if keep_name { format!("{ident};") } else { String::new() };
        if end_line == i {
            let span = close + 1 - ident_start;
            let mut padded = kept;
            if padded.len() < span {
                padded.push_str(&spaces(span - padded.len()));
            }
            lines[i] = format!("{}{}{}", &line[..ident_start], padded, &line[close + 1..]);
        } else {
            let rest = &line[ident_start + kept.len()..];
            lines[i] = format!("{}{}{}", &line[..ident_start], kept, blank_keep_cr(rest));
            for j in i + 1..end_line {
                lines[j] = blank_keep_cr(&lines[j]);
            }
            lines[end_line] = format!("{}{}", blank_keep_cr(&end_line_str[..close + 1]), &end_line_str[close + 1..]);
            i = end_line; // the blanked span can't host another head
        }
        changed = true;
        i += 1;
    }
    raw.restore(&if changed { lines.join("\n") } else { source.to_string() })
}

// ---------------------------------------------------------------------------
// Annotations
// ---------------------------------------------------------------------------

static SANDWICH: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"\b(static|extern|inline)([ \t]+)(noinline_for_stack|nokprobe_inline|noinline|notrace|noinstr)\b(?=[ \t]+[A-Za-z_])")
});

pub(crate) fn blank_c_sandwiched_annotations(source: &str) -> String {
    if !SANDWICH.is_match(source) {
        return source.to_string();
    }
    SANDWICH.replace_all(source, |c| {
        Some(format!("{}{}{}", &c[1], &c[2], spaces(c[3].len())))
    })
}

static AUTO_INFER: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bauto(?=[ \t]+[A-Za-z_]\w*[ \t]*=)"));

pub(crate) fn blank_c_auto_inference(source: &str) -> String {
    if !AUTO_INFER.is_match(source) {
        return source.to_string();
    }
    AUTO_INFER.replace_all(source, |_| Some("    ".into()))
}

static TRAILING_PARAM_ATTR: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\b([A-Za-z_]\w*)([ \t]+)([A-Z][A-Z0-9_]{2,})(?=[ \t]*[,)])"));

pub(crate) fn blank_c_trailing_param_attr_macros(source: &str) -> String {
    if !TRAILING_PARAM_ATTR.is_match(source) {
        return source.to_string();
    }
    TRAILING_PARAM_ATTR.replace_all(source, |c| Some(format!("{}{}{}", &c[1], &c[2], spaces(c[3].len()))))
}

const C_KERNEL_ANNOTATIONS: &[&str] = &[
    "__init", "__exit", "__initdata", "__initconst", "__exitdata", "__devinit", "__devexit", "__cpuinit", "__meminit",
    "__meminitdata", "__net_init", "__net_exit", "__net_initdata", "__init_or_module", "__user", "__kernel", "__iomem",
    "__percpu", "__rcu", "__force", "__nocast", "__must_check", "__maybe_unused", "__always_unused", "__used", "__cold",
    "__hot", "__weak", "__pure", "__sched", "__malloc", "__visible", "__deprecated", "__ro_after_init", "__read_mostly",
    "__refdata", "__latent_entropy", "__randomize_layout", "__no_randomize_layout", "__bpf_kfunc", "__function_aligned",
    "__always_inline", "__noreturn", "__cacheline_aligned_in_smp", "__cacheline_aligned",
    "__cacheline_internodealigned_in_smp", "____cacheline_aligned_in_smp", "____cacheline_aligned",
    "____cacheline_internodealigned_in_smp", "__noclone", "__lockfunc", "__ref", "__private", "__bitwise",
    "__nosavedata", "__no_kcsan", "__cpuidle", "__ksym", "__initdata_memblock", "__initdata_or_meminfo",
];

fn sorted_alternation(words: &[&str]) -> String {
    let mut w: Vec<&str> = words.to_vec();
    w.sort_by(|a, b| b.len().cmp(&a.len())); // longest first, ties keep list order
    w.join("|")
}

static KERNEL_ANNOTATION: LazyLock<Re> =
    LazyLock::new(|| Re::new(&format!(r"\b({})\b(?!\s*\()", sorted_alternation(C_KERNEL_ANNOTATIONS))));
static CONTAINER_OF: LazyLock<Re> = LazyLock::new(|| Re::new(r"(\bcontainer_of\s*\([^;()]*?,\s*)(struct|union)(\s+)"));

pub(crate) fn blank_c_kernel_annotations(source: &str) -> String {
    if !source.contains("__") {
        return source.to_string();
    }
    if !KERNEL_ANNOTATION.is_match(source) {
        return source.to_string();
    }
    let mut out = KERNEL_ANNOTATION.replace_all(source, |c| Some(spaces(c[0].len())));
    // `container_of(ptr, struct T, member)` — blank just the type keyword.
    if out.contains("container_of") {
        out = CONTAINER_OF.replace_all(&out, |c| Some(format!("{}{}{}", &c[1], spaces(c[2].len()), &c[3])));
    }
    out
}

const C_PARAMETERIZED_ANNOTATIONS: &[&str] = &[
    "__free", "__printf", "__scanf", "__counted_by", "__counted_by_le", "__counted_by_be", "__guarded_by",
    "__pt_guarded_by", "__acquires", "__releases", "__must_hold", "__cleanup", "__aligned", "__section", "__bpf_md_ptr",
    "__assume_aligned",
];
static PARAM_ANNOTATION: LazyLock<Re> = LazyLock::new(|| {
    Re::new(&format!(
        r"\b({})[ \t]*\((?:[^()]|\([^()]*\))*\)",
        sorted_alternation(C_PARAMETERIZED_ANNOTATIONS)
    ))
});
static BLANK_LEADING_LINE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*$"));
static SEMICOLON_AHEAD: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[ \t]*;"));

pub(crate) fn blank_c_parameterized_annotation_macros(source: &str) -> String {
    if !source.contains("__") {
        return source.to_string();
    }
    let raw = mask_cpp_raw_strings(source);
    let source = raw.source.as_str();
    let mut result = String::new();
    let mut last = 0;
    let mut pos = 0;
    while let Some(m) = PARAM_ANNOTATION.captures_at(source, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let start = whole.start();
        let mut end = whole.end();
        // Field-position form — the whole line is the annotation: the `;` goes too.
        let line_start = if start == 0 {
            // `lastIndexOf('\n', -1)` checks index 0 only
            if source.as_bytes()[0] == b'\n' { 1 } else { 0 }
        } else {
            source.as_bytes()[..start].iter().rposition(|c| *c == b'\n').map_or(0, |i| i + 1)
        };
        if BLANK_LEADING_LINE.is_match(&source[line_start..start]) {
            if let Some(after) = SEMICOLON_AHEAD.captures(&source[end..]) {
                end += after.get(0).unwrap().end();
            }
        }
        result.push_str(&source[last..start]);
        result.extend(source[start..end].chars().map(|c| if c == '\n' || c == '\r' { c } else { ' ' }));
        last = end;
    }
    result.push_str(&source[last..]);
    raw.restore(&result)
}

// ---------------------------------------------------------------------------
// Type-keyword macro arguments: kzalloc_obj(struct foo)
// ---------------------------------------------------------------------------

static TYPE_ARG_HEAD_EXCLUSIONS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "sizeof", "alignof", "_Alignof", "typeof", "__typeof__", "__typeof", "offsetof", "_Generic", "va_arg", "if",
        "while", "for", "switch", "case",
    ]
    .into_iter()
    .collect()
});
static TYPE_ARG_GUARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b(?:struct|union|enum)[ \t\r\n]"));
static TYPE_ARG_HEAD: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b([A-Za-z_]\w*)[ \t]*\("));
static TYPE_ARG_OPENER: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"^(struct|union|enum)([ \t\r\n]+)([A-Za-z_]\w*)([ \t\r\n]*\*+)?(?=[ \t\r\n]*[,)])")
});
const TYPE_ARG_SCAN_CAP: usize = 600;

pub(crate) fn blank_c_type_keyword_args(source: &str) -> String {
    if !TYPE_ARG_GUARD.is_match(source) {
        return source.to_string();
    }
    let raw = mask_cpp_raw_strings(source);
    let source = raw.source.as_str();
    let b = source.as_bytes();
    let mut chars: Option<Vec<u8>> = None;
    let mut pos = 0;
    while let Some(m) = TYPE_ARG_HEAD.captures_at(source, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let head = m.get(1).unwrap().as_str();
        if TYPE_ARG_HEAD_EXCLUSIONS.contains(head) {
            continue;
        }
        // Reject declaration shapes: an identifier or `*` immediately before the
        // head — unless that word is `return`.
        let mut j = whole.start() as isize - 1;
        while j >= 0 && (b[j as usize] == b' ' || b[j as usize] == b'\t') {
            j -= 1;
        }
        if j >= 0 && (is_word(b[j as usize]) || b[j as usize] == b'*') {
            let mut w = j;
            while w >= 0 && is_word(b[w as usize]) {
                w -= 1;
            }
            if &source[(w + 1) as usize..=(j as usize)] != "return" {
                continue;
            }
        }
        let open = whole.end() - 1;
        let mut depth = 1i32;
        let mut at_arg_start = true;
        let mut k = open + 1;
        while k < b.len() && k - open <= TYPE_ARG_SCAN_CAP {
            let ch = b[k];
            if ch == b'"' || ch == b'\'' {
                k += 1;
                while k < b.len() && b[k] != ch {
                    if b[k] == b'\\' {
                        k += 1;
                    }
                    k += 1;
                }
                at_arg_start = false;
                k += 1;
                continue;
            }
            if matches!(ch, b' ' | b'\t' | b'\n' | b'\r') {
                k += 1;
                continue;
            }
            if ch == b'(' {
                depth += 1;
                at_arg_start = false;
                k += 1;
                continue;
            }
            if ch == b')' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                at_arg_start = false;
                k += 1;
                continue;
            }
            if ch == b';' || ch == b'{' || ch == b'}' {
                break; // not an argument list
            }
            if ch == b',' {
                if depth == 1 {
                    at_arg_start = true;
                }
                k += 1;
                continue;
            }
            if depth == 1 && at_arg_start {
                let window = &source[k..(k + 160).min(source.len())];
                if let Some(opener) = TYPE_ARG_OPENER.captures(window) {
                    let out = chars.get_or_insert_with(|| b.to_vec());
                    let kw = opener.get(1).unwrap().as_str().len();
                    for byte in &mut out[k..k + kw] {
                        *byte = b' ';
                    }
                    if let Some(tail) = opener.get(4) {
                        let tail = tail.as_str();
                        let stars = tail.bytes().rev().take_while(|c| *c == b'*').count();
                        let stars_start = k
                            + kw
                            + opener.get(2).unwrap().as_str().len()
                            + opener.get(3).unwrap().as_str().len()
                            + (tail.len() - stars);
                        for byte in &mut out[stars_start..stars_start + stars] {
                            *byte = b' ';
                        }
                    }
                    k += opener.get(0).unwrap().as_str().len() - 1;
                }
            }
            at_arg_start = false;
            k += 1;
        }
    }
    raw.restore(&match chars {
        Some(bytes) => from_bytes(bytes),
        None => source.to_string(),
    })
}

// ---------------------------------------------------------------------------
// File-scope declaration macros
// ---------------------------------------------------------------------------

static PREFIXED_DECL_GUARD: LazyLock<Re> = LazyLock::new(|| Re::multiline(r"^[ \t]*(?:static|extern)[ \t]+[A-Z]"));
static PREFIXED_DECL_MACRO: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^[ \t]*(?:static|extern)[ \t]+[A-Z][A-Z0-9_]{2,}[ \t]*\("));

pub(crate) fn blank_c_file_scope_prefixed_decl_macros(source: &str) -> String {
    if !PREFIXED_DECL_GUARD.is_match(source) {
        return source.to_string();
    }
    let raw = mask_cpp_raw_strings(source);
    let source = raw.source.as_str();
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    let mut changed = false;
    for line in lines.iter_mut() {
        let Some(m) = PREFIXED_DECL_MACRO.captures(line) else { continue };
        let open = m.get(0).unwrap().end() - 1;
        drop(m);
        let mut depth = 0i32;
        let Some(close) = scan_parens(line.as_bytes(), open, &mut depth, false).unwrap() else { continue };
        if trim(strip_cr(&line[close + 1..])) != ";" {
            continue;
        }
        *line = line.chars().map(|c| if c == '\n' || c == '\r' { c } else { ' ' }).collect();
        changed = true;
    }
    raw.restore(&if changed { lines.join("\n") } else { source.to_string() })
}

static PREFIXED_DECL_INIT_GUARD: LazyLock<Re> = LazyLock::new(|| Re::multiline(r"^[ \t]*static[ \t]+[A-Z]"));
static PREFIXED_DECL_INIT: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r#"^([ \t]*)static[ \t]+[A-Z][A-Z0-9_]{2,}[ \t]*\(([^();'"]*?),[ \t]*([A-Za-z_]\w*)[ \t]*\)([ \t]*=[ \t]*\{?[ \t]*\r?)$"#,
    )
});
static TYPE_TOKEN_RUN: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[A-Za-z_][\w \t*]*$"));
static HSPACE_RUN: LazyLock<Re> = LazyLock::new(|| Re::new(r"[ \t]+"));

/// Rewrite a 2-argument `static CAPS_MACRO(type, name) = {` into the declaration
/// it expands to, keeping the name's column and the `= {` tail's offsets.
pub(crate) fn rewrite_c_prefixed_decl_macro_initializers(source: &str) -> String {
    if !PREFIXED_DECL_INIT_GUARD.is_match(source) {
        return source.to_string();
    }
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    let mut changed = false;
    for line in lines.iter_mut() {
        let Some(m) = PREFIXED_DECL_INIT.captures(line) else { continue };
        let arg1 = HSPACE_RUN.replace_all(trim(m.get(2).unwrap().as_str()), |_| Some(" ".into()));
        if !TYPE_TOKEN_RUN.is_match(&arg1) {
            continue; // a type-token run only
        }
        let name = m.get(3).unwrap();
        let name_start = name.start();
        let tail_start = m.get(4).unwrap().start();
        let prefix = format!("{}static {}", m.get(1).unwrap().as_str(), arg1);
        if prefix.len() + 1 > name_start {
            continue; // the rewrite must fit left of the name
        }
        let rewritten = format!(
            "{}{}{}{}{}",
            prefix,
            spaces(name_start - prefix.len()),
            name.as_str(),
            spaces(tail_start - name_start - name.as_str().len()),
            &line[tail_start..]
        );
        drop(m);
        *line = rewritten;
        changed = true;
    }
    if changed {
        lines.join("\n")
    } else {
        source.to_string()
    }
}

// ---------------------------------------------------------------------------
// va_arg, named variadics, designated macro arguments
// ---------------------------------------------------------------------------

static VA_ARG: LazyLock<Re> = LazyLock::new(|| Re::new(r"\bva_arg[ \t]*\(([^(),]+)(,[^()]*)\)"));
static SINGLE_TOKEN_REST: LazyLock<Re> = LazyLock::new(|| Re::new(r"^,[ \t]*[A-Za-z_]\w*[ \t]*$"));

/// Blank a qualified/pointer type argument of `va_arg`. (The replacement writes
/// `va_arg(` without any gap the match carried, so the text can get shorter.)
pub(crate) fn blank_c_va_arg_qualified_type_args(source: &str) -> String {
    if !source.contains("va_arg") {
        return source.to_string();
    }
    VA_ARG.replace_all(source, |c| {
        let arg1 = c.get(1).unwrap().as_str();
        let rest = c.get(2).unwrap().as_str();
        if SINGLE_TOKEN_REST.is_match(rest) {
            return None; // single token — parses natively
        }
        let blanked: String = rest.chars().map(|ch| if ch == '\n' || ch == '\r' { ch } else { ' ' }).collect();
        Some(format!("va_arg({arg1}{blanked})"))
    })
}

static NAMED_VARIADIC_DEFINE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^([ \t]*#[ \t]*define[ \t]+[A-Za-z_]\w*\([^()\n]*?\b\w+)\.\.\."));

/// Blank the dots of a GNU named-variadic `#define` parameter (`args...`).
pub(crate) fn blank_c_named_variadic_define_dots(source: &str) -> String {
    if !source.contains("...") {
        return source.to_string();
    }
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    let mut changed = false;
    for line in lines.iter_mut() {
        let Some(m) = NAMED_VARIADIC_DEFINE.captures(line) else { continue };
        let keep_len = m.get(1).unwrap().as_str().len();
        drop(m);
        *line = format!("{}   {}", &line[..keep_len], &line[keep_len + 3..]);
        changed = true;
    }
    if changed {
        lines.join("\n")
    } else {
        source.to_string()
    }
}

static DESIGNATED_HEAD: LazyLock<Re> = LazyLock::new(|| Re::multiline(r"^[ \t]*([A-Z_][A-Z0-9_]*)\s*\("));
static DESIGNATOR: LazyLock<Re> = LazyLock::new(|| Re::new(r"(^|[,\{(\s])(\.[A-Za-z_]\w*|\[[^\]]+\])\s*=[^=]"));
static STARTS_WITH_SEMICOLON: LazyLock<Re> = LazyLock::new(|| Re::new(r"^\s*;"));

/// Blank the argument list of a statement-level `MACRO( … );` call whose
/// arguments are designated initializers (#1729).
pub(crate) fn blank_c_designated_macro_args(source: &str) -> String {
    if !source.contains('=') {
        return source.to_string();
    }
    let raw = mask_cpp_raw_strings(source);
    let source = raw.source.as_str();
    let b = source.as_bytes();
    let mut out = b.to_vec();
    let mut pos = 0;
    while let Some(m) = DESIGNATED_HEAD.captures_at(source, pos) {
        let whole = m.get(0).unwrap();
        pos = whole.end();
        let open = whole.end() - 1;
        let mut depth = 1i32;
        let mut i = open + 1;
        while i < b.len() && depth > 0 {
            match b[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            continue;
        }
        let close = i - 1;
        let args = &source[open + 1..close];
        // A designator at argument depth: `.name =` or `[index] =`.
        if !DESIGNATOR.is_match(args) {
            continue;
        }
        if !STARTS_WITH_SEMICOLON.is_match(&source[close + 1..]) {
            continue;
        }
        for byte in &mut out[open + 1..close] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        pos = close;
    }
    raw.restore(&from_bytes(out))
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// `preParseCSource`.
pub(crate) fn pre_parse_c(source: &str) -> String {
    let raw = mask_cpp_raw_strings(source);
    let source = rewrite_foundation_enum_macros(&raw.source);
    let inner = blank_c_designated_macro_args(&blank_c_kernel_annotations(&blank_c_cplusplus_guard_bodies(&source)));
    let mut s = blank_c_sandwiched_annotations(&inner);
    s = blank_c_auto_inference(&s);
    s = blank_c_parameterized_annotation_macros(&s);
    s = blank_c_type_keyword_args(&s);
    s = blank_c_va_arg_qualified_type_args(&s);
    s = blank_c_file_scope_prefixed_decl_macros(&s);
    s = rewrite_c_prefixed_decl_macro_initializers(&s);
    s = blank_cpp_annotation_macro_calls(&s, true);
    s = blank_c_trailing_param_attr_macros(&s);
    s = blank_c_statement_macro_calls(&s);
    s = blank_lone_macro_lines(&s, &C_LONE_MACROS);
    let mut blanked = blank_c_leading_attr_macros(&s);
    if looks_like_cuda_source(&blanked) {
        blanked = blank_cuda_constructs(&blanked);
    }
    // The named-variadic `#define` pass and the conditional-group collapse run after the
    // directive restore — they deliberately edit directive lines — and before the raw-string
    // restore: both are length-preserving, and the collapse must count braces with raw-string
    // bodies still masked.
    raw.restore(&blank_c_unbalanced_conditional_branches(&blank_c_named_variadic_define_dots(&restore_directive_lines(&source, &blanked))))
}

#[cfg(test)]
mod tests {
    //! A function that writes a macro before a block still uses the macro
    //! (upstream b23c8805, #3172). The name stays as `name;` only when that
    //! line is a statement of a function body.

    use super::blank_c_statement_macro_calls;
    use codegraph_types::{EdgeKind, Language, NodeKind, ReferenceKind};

    fn lines(body: &[&str]) -> String {
        let mut source = body.join("\n");
        source.push('\n');
        source
    }

    fn extracted(source: &str) -> codegraph_types::ExtractionResult {
        crate::extract_source("t.c", source, Language::C).unwrap_or_else(|err| panic!("{err}"))
    }

    fn macros_used(source: &str, func: &str) -> Vec<String> {
        let result = extracted(source);
        let holder = result
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Function && n.name == func)
            .unwrap_or_else(|| panic!("missing {func}: {:?}", result.nodes.iter().map(|n| format!("{} {}", n.kind, n.name)).collect::<Vec<_>>()));
        let mut names: Vec<String> = result
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::References && e.source == holder.id)
            .filter_map(|e| result.nodes.iter().find(|n| n.id == e.target).map(|n| n.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn calls_from(source: &str, func: &str) -> Vec<String> {
        let result = extracted(source);
        let holder = result.nodes.iter().find(|n| n.kind == NodeKind::Function && n.name == func).unwrap();
        let mut names: Vec<String> = result
            .unresolved_references
            .iter()
            .filter(|r| r.from_node_id == holder.id && r.reference_kind == ReferenceKind::Edge(EdgeKind::Calls))
            .map(|r| r.reference_name.clone())
            .collect();
        names.sort();
        names
    }

    fn assert_parses(source: &str) {
        let result = extracted(source);
        let (has_error, _) = crate::stack::parse_notes();
        assert!(
            !has_error,
            "parse error; symbols {:?}",
            result.nodes.iter().map(|n| format!("{} {}", n.kind, n.name)).collect::<Vec<_>>()
        );
    }

    fn kept_prefix(annotation: &str, kept: &str) -> String {
        let mut rest = String::new();
        for ch in annotation[kept.len()..].chars() {
            rest.push(if ch == '\n' || ch == '\r' { ch } else { ' ' });
        }
        rest
    }

    #[test]
    fn a_function_still_uses_a_macro_written_before_a_block() {
        let before_block = lines(&[
            "#define TEST_CONTEXT(context) printf(\"%s\", context);",
            "int use(const char *s);",
            "int mstrTest(void) {",
            "    TEST_CONTEXT(\"Create simple short mstr\") {",
            "        use(\"foo\");",
            "    }",
            "    return 0;",
            "}",
        ]);
        assert_eq!(macros_used(&before_block, "mstrTest"), vec!["TEST_CONTEXT".to_string()]);
        assert!(calls_from(&before_block, "mstrTest").iter().any(|name| name == "use"));

        let next_statement = lines(&[
            "#define XXH_ASSERT(c) ((void)0)",
            "int h(int len) {",
            "    int a = len;",
            "    XXH_ASSERT(a != 0)",
            "    {   int b = a;",
            "        return b;",
            "    }",
            "}",
        ]);
        assert_eq!(macros_used(&next_statement, "h"), vec!["XXH_ASSERT".to_string()]);

        let run = lines(&[
            "#define check_len(c) ((void)0)",
            "int h(int len) {",
            "    check_len(len != 0)",
            "    check_len(len != 1)",
            "    {",
            "        return len;",
            "    }",
            "}",
        ]);
        assert_eq!(blank_c_statement_macro_calls(&run).matches("check_len;").count(), 2);
        assert_eq!(macros_used(&run, "h"), vec!["check_len".to_string()]);
    }

    #[test]
    fn a_name_stays_only_on_a_statement_of_a_function_body() {
        let over_lines = lines(&[
            "#define EACH(a, b) for (;;)",
            "void f(int x, void *head) {",
            "    EACH(x,",
            "         head) {",
            "        x--;",
            "    }",
            "}",
            "void g(int x) {",
            "    if (x) {",
            "        EACH(x, 0) {",
            "            x--;",
            "        }",
            "    } else",
            "        EACH(x, 1) {",
            "            x++;",
            "        }",
            "    do",
            "        EACH(x, 2) {",
            "            x++;",
            "        }",
            "    while (x);",
            "out:",
            "    EACH(x, 3) {",
            "        x++;",
            "    }",
            "}",
        ]);
        let over_out = blank_c_statement_macro_calls(&over_lines);
        assert_eq!(over_out.len(), over_lines.len());
        assert_eq!(over_out.matches("EACH;").count(), 3);
        assert_eq!(macros_used(&over_lines, "f"), vec!["EACH".to_string()]);
        assert_eq!(macros_used(&over_lines, "g"), vec!["EACH".to_string()]);
        assert_parses(&over_lines);

        let braced = lines(&[
            "#define EACH(a) for (;;)",
            "int g(int x) {",
            "    if (x) {",
            "        x--;",
            "    } else {",
            "        EACH(x) {",
            "            x--;",
            "        }",
            "    }",
            "    do {",
            "        EACH(x) {",
            "            x--;",
            "        }",
            "    } while (x);",
            "    switch (x) {",
            "    case 1:",
            "        EACH(x) {",
            "            x--;",
            "        }",
            "        break;",
            "    }",
            "    return ({",
            "        EACH(x) {",
            "            x--;",
            "        }",
            "        x;",
            "    });",
            "}",
        ]);
        assert_eq!(blank_c_statement_macro_calls(&braced).matches("EACH;").count(), 4);
        assert_parses(&braced);

        let unbraced = lines(&[
            "void g(int x) {",
            "    if (x)",
            "        EACH(x, 0) {",
            "            x--;",
            "        }",
            "    else",
            "        EACH(x, 1) {",
            "            x++;",
            "        }",
            "    do",
            "        EACH(x, 2) {",
            "            x++;",
            "        }",
            "    while (x);",
            "}",
        ]);
        assert!(!blank_c_statement_macro_calls(&unbraced).contains("EACH"));
        assert_parses(&unbraced);

        let forbidden = ["entry;", "members;", "values;", "inner;", "tail;"];
        let samples = [
            lines(&["static const int table[] = {", "    entry(a)", "    entry(b)", "    last", "};"]),
            lines(&["struct s {", "    members(a)", "    int x;", "};"]),
            lines(&["enum e {", "    values(a)", "    last", "};"]),
            lines(&["void f(void) {", "    int t[] = {", "        entry(a)", "        entry(b)", "        last", "    };", "    (void)t;", "}"]),
            lines(&["void f(void) {", "    OUTER(", "        inner(a)", "        inner(b)", "        last", "    );", "}"]),
            lines(&["int f(int a, int b) {", "    return a +", "        tail(a)", "        b;", "}"]),
        ];
        for sample in samples {
            let blanked = blank_c_statement_macro_calls(&sample);
            let parsed = crate::pre_parse("t.c", &sample, Language::C);
            for name in forbidden {
                assert!(!blanked.contains(name), "{name} in blank:\n{blanked}");
                assert!(!parsed.contains(name), "{name} in preparse:\n{parsed}");
            }
            assert_eq!(blanked.len(), sample.len());
            assert_parses(&sample);
        }

        let crlf = "void f(void) {\r\n    EACH(1) {\r\n    }\r\n    ASSERT(2)\r\n    g();\r\n}\r\n";
        assert_eq!(
            blank_c_statement_macro_calls(crlf),
            "void f(void) {\r\n    EACH;   {\r\n    }\r\n    ASSERT;  \r\n    g();\r\n}\r\n"
        );

        let broken_if = lines(&[
            "void f(int a) {",
            "#ifdef X",
            "    if (a) {",
            "#else",
            "    if (!a) {",
            "#endif",
            "        a--;",
            "    }",
            "}",
            "void g(int a) {",
            "    EACH(a) {",
            "        a--;",
            "    }",
            "}",
        ]);
        assert!(blank_c_statement_macro_calls(&broken_if).contains("    EACH;   {"));
    }

    #[test]
    fn an_iterator_macro_keeps_its_name_and_a_real_call_stays() {
        let walk = lines(&[
            "static void walk(struct list *head) {",
            "\tlist_for_each_entry(pos, head, member) {",
            "\t\tuse(pos);",
            "\t}",
            "}",
        ]);
        let out = blank_c_statement_macro_calls(&walk);
        assert_eq!(out.len(), walk.len());
        assert!(out.contains("list_for_each_entry;"), "{out}");
        assert!(!out.contains("head, member"), "{out}");
        assert!(out.contains("use(pos);"));

        let call = lines(&["void f(void) {", "\tdo_thing(a, b);", "}"]);
        assert_eq!(blank_c_statement_macro_calls(&call), call);
        let kandr = lines(&["main(argc, argv)", "{", "\treturn 0;", "}"]);
        assert_eq!(blank_c_statement_macro_calls(&kandr), kandr);
        let ctrl = lines(&["void g(int x) {", "\twhile (x) {", "\t\tx--;", "\t}", "}"]);
        assert_eq!(blank_c_statement_macro_calls(&ctrl), ctrl);

        let wrapped = lines(&[
            "static void walk(void *head) {",
            "\thlist_for_each_entry_rcu(p, head, hlist,",
            "\t\t\t\t lockdep_is_held(&kprobe_mutex)) {",
            "\t\tuse(p);",
            "\t}",
            "}",
        ]);
        let wrapped_out = blank_c_statement_macro_calls(&wrapped);
        assert_eq!(wrapped_out.len(), wrapped.len());
        assert!(wrapped_out.contains("hlist_for_each_entry_rcu;"), "{wrapped_out}");
        assert!(!wrapped_out.contains("lockdep_is_held"), "{wrapped_out}");
        assert!(wrapped_out.contains("use(p);"));
        let real = "void f(void) {\n\tdo_thing(a,\n\t\t b);\n}\n";
        assert_eq!(blank_c_statement_macro_calls(real), real);
        let cond = "void g(int a) {\n\tif (check(a,\n\t\t  a)) {\n\t\tuse(a);\n\t}\n}\n";
        assert_eq!(blank_c_statement_macro_calls(cond), cond);

        let head = "void iterate() {\n  ";
        let tail = " {\n    visit();\n  }\n}\n";
        let annotation = "for_each_item(R\"TAG(\" ) unbalanced ( \" \\\nUE_DEPRECATED(\n)TAG\")";
        let kept = "for_each_item;";
        let source = format!("{head}{annotation}{tail}");
        assert_eq!(
            blank_c_statement_macro_calls(&source),
            format!("{head}{kept}{}{tail}", kept_prefix(annotation, kept))
        );
    }
}
