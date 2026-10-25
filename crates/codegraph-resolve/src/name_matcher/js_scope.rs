//! Does a JavaScript/TypeScript name resolve to something declared inside the function that uses
//! it (a `const`, a parameter, an inner `function`) rather than to the project symbol of the same
//! name? — the local-binding scans of `src/resolution/name-matcher.ts`.

use super::calls::bare_call_receiver;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedRef, UnresolvedRef};
use codegraph_extract::strip_comments::{blank_string_contents, strip_comments_for_regex};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{is_js_space, utf16_len, utf16_prefix};
use codegraph_types::{EdgeKind, Node, NodeKind, ReferenceKind};
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

memo_map!(FnLocalMemo, String, Option<Span>);
memo_map!(CodeBindsMemo, String, bool);
memo_map!(FnCodeMemo, String, Rc<FnCode>);
memo_map!(LocalBindingMemo, String, bool);
memo_map!(SitesMemo, String, Rc<LocalBindingSites>);

pub struct FnCode {
    code: String,
    /// Byte index of the end of each source line within `code`.
    line_ends: Vec<usize>,
}

pub struct LocalBindingSites {
    var_decls: Vec<usize>,
    fn_decls: Vec<usize>,
    arrows: Vec<usize>,
}

// -- Binding patterns -------------------------------------------------------------------------

const PARAM_PAREN: &str = r"\((?:[^()]|\([^()]*\))*\)";
const PARAM_BRACE: &str = r"\{(?:[^{}]|\{[^{}]*\})*\}";
const PARAM_BRACKET: &str = r"\[(?:[^\[\]]|\[[^\[\]]*\])*\]";
const PARAM_ANGLE: &str = r"<(?!\s)(?:=>|=(?!>)|[^<>=]|<(?:=>|=(?!>)|[^<>=])*>)*>";
const PARAM_MODIFIERS: &str = r"(?:(?:public|private|protected|readonly|override)\s+)*";

fn param_part() -> String {
    format!(r"(?:=>|=(?!>)|[^,(){{}}\[\]<>=]|<(?=\s)|>(?=[\s=])|{PARAM_PAREN}|{PARAM_BRACE}|{PARAM_BRACKET}|{PARAM_ANGLE})")
}

pub struct LocalBindingPatterns {
    pub decl: Arc<Re>,
    pub fn_: Arc<Re>,
    pub param: Arc<Re>,
}

fn param_source(n: &str) -> String {
    let part = param_part();
    format!(
        r"\(\s*(?:(?:{PARAM_MODIFIERS}(?:\.\.\.)?[\w$]+|{PARAM_BRACE}|{PARAM_BRACKET})(?:\s*(?:\?\s*)?:{part}+|\s*={part}+|\s*),\s*)*{PARAM_MODIFIERS}{n}(?![\w$])(?:\s*\??\s*:{part}*)?(?:\s*={part}*)?(?:\s*,(?:[^()]|{PARAM_PAREN})*)?\)\s*(?::[^=;{{]*)?(?:=>|\{{)"
    )
}

pub fn local_binding_patterns(name: &str, flags: &str) -> LocalBindingPatterns {
    let n = escape_regex(name);
    LocalBindingPatterns {
        decl: Re::cached(&format!(r"\b(?:const|let|var)\s+(?:{n}(?![\w$])|[{{\[][^;=]*?\b{n}(?![\w$])[^;=]*?[}}\]])\s*(?:=\s*([^;\n]*))?"), flags),
        fn_: Re::cached(&format!(r"\b(?:function|class)\s+{n}(?![\w$])"), flags),
        param: Re::cached(&param_source(&n), flags),
    }
}

/// The parameter-list pattern with a return-type annotation that may hold braces, minus the
/// `if (…) {` / `while (…) {` heads the TypeScript source excludes with a variable-length
/// lookbehind (which PCRE2 cannot compile; `param_list_at` applies it as a filter).
fn param_list_regex(name: &str) -> Arc<Re> {
    let n = escape_regex(name);
    let source = param_source(&n);
    let return_type = format!(r"(?::(?:[^=;{{}}()\n]|{PARAM_BRACE})*)?");
    let replaced = source.replacen(r"(?::[^=;{]*)?", &return_type, 1);
    Re::cached(&format!(r"(?=\()(?:{replaced})"), "")
}

/// The first parameter-list match at or after `from`, honouring the keyword-head exclusion.
fn param_list_at(re: &Re, text: &str, from: usize) -> Option<(usize, usize)> {
    let mut pos = from;
    while pos <= text.len() {
        let caps = re.captures_at(text, pos)?;
        let m = caps.get(0)?;
        let head = &text[..m.start()];
        let window_start = {
            let mut s = head.len().saturating_sub(160);
            while !head.is_char_boundary(s) {
                s += 1;
            }
            s
        };
        if !re!(r"\b(?:if|while|for|switch|with)\s*$").is_match(&head[window_start..]) {
            return Some((m.start(), m.end()));
        }
        let mut next = m.start() + 1;
        while next < text.len() && !text.is_char_boundary(next) {
            next += 1;
        }
        pos = next;
    }
    None
}

fn param_list_matches(name: &str, text: &str) -> Vec<(usize, usize)> {
    let re = param_list_regex(name);
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((s, e)) = param_list_at(&re, text, pos) {
        out.push((s, e));
        pos = if e > s { e } else { s + 1 };
        if pos > text.len() {
            break;
        }
    }
    out
}

fn has_param_list(name: &str, text: &str) -> bool {
    param_list_at(&param_list_regex(name), text, 0).is_some()
}

// -- Function-local scope ---------------------------------------------------------------------

/// A function's source up to the end of line `line`, comments blanked.
pub fn js_function_code_through(func: &Node, file_path: &str, line: u32, ctx: &Ctx) -> String {
    let key = format!("{file_path}\0{}\0{}", func.start_line, func.end_line);
    let own = memoized::<FnCodeMemo>(ctx, key, || {
        let lines = lines_of(ctx, file_path);
        let slice: Vec<&str> = lines.iter().skip(func.start_line as usize - 1).take((func.end_line - func.start_line + 1) as usize).map(|s| s.as_str()).collect();
        let code = strip_comments_for_regex(&slice.join("\n"), "javascript");
        let mut line_ends: Vec<usize> = code.match_indices('\n').map(|(i, _)| i).collect();
        line_ends.truncate(slice.len().saturating_sub(1));
        line_ends.push(code.len());
        Rc::new(FnCode { code, line_ends })
    });
    let last = (line as i64).min(func.start_line as i64 - 1 + own.line_ends.len() as i64) - func.start_line as i64;
    if last < 0 {
        return String::new();
    }
    own.code[..own.line_ends[last as usize]].to_string()
}

/// Is `name` declared by `func` (before `line`) as a local `const`/`let`/`var` or a parameter?
pub fn js_code_binds_name(name: &str, func: &Node, file_path: &str, line: u32, ctx: &Ctx) -> bool {
    let key = format!("{file_path}\0{}\0{line}\0{name}", func.start_line);
    memoized::<CodeBindsMemo>(ctx, key, || {
        let text = js_function_code_through(func, file_path, line, ctx);
        let n = escape_regex(name);
        let declared = Re::cached(&format!(r"\b(?:const|let|var)\s+{n}(?![\w$])(?!\s*[,\]}}])"), "").is_match(&text);
        declared || has_param_list(name, &text)
    })
}

/// The function that declares `name` locally, when the reference sits inside it.
pub fn js_function_local_scope(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<Span> {
    if !is_js_family(r.language) || !is_js_ident(name) {
        return None;
    }
    let key = format!("{}\0{name}\0{}", r.from_node_id, r.line);
    memoized::<FnLocalMemo>(ctx, key, || {
        let func = ctx.node_by_id(&r.from_node_id)?;
        if is_callable_kind(func.kind) && func.start_line <= r.line && func.end_line >= r.line && js_code_binds_name(name, &func, &r.file_path, r.line, ctx) {
            Some(Span { start: func.start_line, end: func.end_line })
        } else {
            None
        }
    })
}

/// A declaration of `n` outside the function-local scope of the name the reference spells.
pub fn is_outside_js_local(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    if is_calls(r) && bare_call_receiver(r, ctx).is_some() {
        return false;
    }
    match js_function_local_scope(&r.reference_name, r, ctx) {
        Some(scope) => !(n.file_path == r.file_path && n.start_line >= scope.start && n.start_line <= scope.end),
        None => false,
    }
}

// -- Block and body scanners (UTF-16 unit slices, as the offsets they work with are) ------------

fn u(c: char) -> u16 {
    c as u16
}

fn unit_is_space(c: u16) -> bool {
    char::from_u32(c as u32).is_some_and(is_js_space)
}

fn unit_is_word(c: u16) -> bool {
    c < 128 && ((c as u8).is_ascii_alphanumeric() || c == u('_'))
}

pub fn js_block_end(code: &[u16], at: usize) -> usize {
    let mut depth: i64 = 0;
    for (i, &c) in code.iter().enumerate().skip(at) {
        if c == u('{') {
            depth += 1;
        } else if c == u('}') {
            depth -= 1;
            if depth < 0 {
                return i;
            }
        }
    }
    code.len()
}

pub fn js_block_open_at_end(code: &[u16], at: usize) -> bool {
    js_block_end(code, at) == code.len()
}

fn operand_end(c: u16) -> bool {
    unit_is_word(c) || [u('$'), u(')'), u(']'), u('}'), u('\''), u('"'), u('`')].contains(&c)
}

fn operand_start(c: u16) -> bool {
    unit_is_word(c) || [u('$'), u('\''), u('"'), u('`')].contains(&c)
}

pub fn js_body_end(code: &[u16], at: usize) -> usize {
    let mut i = at;
    while i < code.len() && unit_is_space(code[i]) {
        i += 1;
    }
    if code.get(i) == Some(&u('{')) {
        return js_block_end(code, i + 1);
    }
    let mut depth: i64 = 0;
    while i < code.len() {
        let c = code[i];
        if c == u('(') || c == u('[') || c == u('{') {
            depth += 1;
        } else if c == u(')') || c == u(']') || c == u('}') {
            depth -= 1;
            if depth < 0 {
                return i;
            }
        } else if depth == 0 && (c == u(',') || c == u(';')) {
            return i;
        } else if depth == 0 && c == u('\n') {
            let mut before = i as i64 - 1;
            while before >= 0 && unit_is_space(code[before as usize]) {
                before -= 1;
            }
            let mut after = i + 1;
            while after < code.len() && unit_is_space(code[after]) {
                after += 1;
            }
            if before >= 0 && operand_end(code[before as usize]) && (after == code.len() || operand_start(code[after])) {
                return i;
            }
            i = after - 1;
        }
        i += 1;
    }
    code.len()
}

pub fn js_body_open_at_end(code: &[u16], at: usize) -> bool {
    js_body_end(code, at) == code.len()
}

fn js_for_statement_open_at_end(code: &[u16], at: usize) -> bool {
    let mut depth: i64 = 0;
    for (i, &c) in code.iter().enumerate().skip(at) {
        if c == u('(') || c == u('[') || c == u('{') {
            depth += 1;
        } else if c == u(')') || c == u(']') || c == u('}') {
            depth -= 1;
            if depth < 0 {
                return c == u(')') && js_body_open_at_end(code, i + 1);
            }
        }
    }
    true
}

/// `blankStringContents(text)` with regex-literal bodies blanked too, as UTF-16 units.
pub fn blanked_code_units(text: &str, strip_language: Option<&str>) -> Vec<u16> {
    let source = match strip_language {
        Some(lang) => strip_comments_for_regex(text, lang),
        None => text.to_string(),
    };
    let mut regexes: Vec<usize> = Vec::new();
    let blanked = blank_string_contents(&source, Some(&mut regexes));
    let mut units: Vec<u16> = blanked.encode_utf16().collect();
    let mut k = 0;
    while k + 1 < regexes.len() {
        let (from, to) = (regexes[k] + 1, regexes[k + 1].min(units.len()));
        for slot in units.iter_mut().take(to).skip(from) {
            *slot = u(' ');
        }
        k += 2;
    }
    units
}

/// `text.slice(from, to)` where the bounds are byte positions that may fall inside a character.
pub fn safe_slice(text: &str, from: usize, to: usize) -> &str {
    let mut a = from.min(text.len());
    while a < text.len() && !text.is_char_boundary(a) {
        a += 1;
    }
    let mut b = to.min(text.len());
    while b > 0 && !text.is_char_boundary(b) {
        b -= 1;
    }
    if a >= b {
        ""
    } else {
        &text[a..b]
    }
}

/// Does a declaration of `name` that is still open at the reference enclose it? (An inner block
/// scope, a parameter list whose body is still open, or a lone arrow parameter.)
fn js_binding_encloses_reference(name: &str, func: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let through = js_function_code_through(func, &r.file_path, r.line, ctx);
    let line_start = through.rfind('\n').map_or(0, |p| p + 1);
    let tail = utf16_prefix(&through[line_start..], r.column as usize);
    let cut = format!("{}{}", &through[..line_start], tail);
    if utf16_len(&cut) > 100_000 {
        return false;
    }
    let units = blanked_code_units(&cut, None);
    let code = String::from_utf16_lossy(&units);
    let unit_at = |byte: usize| utf16_len(&code[..byte]);
    let n = escape_regex(name);
    let declaration = Re::cached(
        &format!(r"\b(?:(?:const|let|var)\s+{n}(?![\w$])(?!\s*[,\]}}])(\s*=\s*(?:await\s+)?(?:require|import)\s*\()?|(?:function|class)\s+{n}(?![\w$]))"),
        "g",
    );
    for m in declaration.captures_iter(&code) {
        if m.text(1).is_some() {
            continue;
        }
        let at = m.get(0).map_or(0, |g| g.start());
        let in_for_head = re!(r"\bfor\s*(?:await\s*)?\(\s*$").is_match(safe_slice(&code, at.saturating_sub(16), at));
        let open = if in_for_head { js_for_statement_open_at_end(&units, unit_at(at)) } else { js_block_open_at_end(&units, unit_at(at)) };
        if open {
            return true;
        }
    }
    for (_, end) in param_list_matches(name, &code) {
        let end_unit = unit_at(end);
        let opens_block = end_unit > 0 && units.get(end_unit - 1) == Some(&u('{'));
        let open = if opens_block { js_block_open_at_end(&units, end_unit) } else { js_body_open_at_end(&units, end_unit) };
        if open {
            return true;
        }
    }
    let lone = Re::cached(&format!(r"(^|[(,=\[{{?:]|=>|\b(?:return|async|default|yield|await))\s*{n}\s*=>"), "g");
    for m in lone.captures_iter(&code) {
        let whole = m.get(0).unwrap();
        if m.text(1) == Some(":") && re!(r"\)\s*$").is_match(safe_slice(&code, whole.start().saturating_sub(200), whole.start())) {
            continue;
        }
        if js_body_open_at_end(&units, unit_at(whole.end())) {
            return true;
        }
    }
    false
}

/// A resolved reference whose target a local binding of the same name in an enclosing scope shadows.
pub fn is_import_shadowed_by_js_binding(result: &ResolvedRef, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let kind_ok = matches!(
        r.reference_kind,
        ReferenceKind::Edge(EdgeKind::Calls | EdgeKind::References | EdgeKind::Instantiates) | ReferenceKind::FunctionRef
    );
    if !is_js_family(r.language) || !kind_ok || !is_js_ident(&r.reference_name) {
        return false;
    }
    let Some(target) = ctx.node_by_id(&result.target_node_id) else { return false };
    if target.name != r.reference_name || matches!(target.kind, NodeKind::Interface | NodeKind::TypeAlias) {
        return false;
    }
    let Some(func) = ctx.node_by_id(&r.from_node_id) else { return false };
    if !is_callable_kind(func.kind) || func.start_line > r.line || func.end_line < r.line {
        return false;
    }
    if target.file_path == r.file_path && target.start_line >= func.start_line && target.start_line <= func.end_line {
        return false;
    }
    if !is_locally_bound_js_name(&r.reference_name, &r.file_path, ctx) {
        return false;
    }
    if is_calls(r) && bare_call_receiver(r, ctx).is_some() {
        return false;
    }
    js_binding_encloses_reference(&r.reference_name, &func, r, ctx)
}

// -- File-wide binding scan -------------------------------------------------------------------

fn local_binding_sites(file_path: &str, source: &str, ctx: &Ctx) -> Rc<LocalBindingSites> {
    memoized::<SitesMemo>(ctx, file_path.to_string(), || {
        let var_decls = re!(r"\b(?:const|let|var)\s", "g").captures_iter(source).filter_map(|c| c.get(0).map(|m| m.start())).collect();
        let fn_decls = re!(r"\b(?:function|class)\s", "g").captures_iter(source).filter_map(|c| c.get(0).map(|m| m.start())).collect();
        let arrows = source.match_indices("=>").map(|(i, _)| i).collect();
        Rc::new(LocalBindingSites { var_decls, fn_decls, arrows })
    })
}

fn import_binding_value(text: &str) -> bool {
    re!(r"^\s*(?:await\s+)?(?:require|import)\s*\(").is_match(text)
}

/// The `(` that opens the innermost unclosed parenthesis before `at`, within 2000 units.
fn enclosing_open_paren(source: &str, at: usize) -> Option<usize> {
    let mut depth: i64 = 0;
    let mut back = 0usize;
    for (i, c) in source[..at].char_indices().rev() {
        back += c.len_utf16();
        if back > 2000 {
            break;
        }
        match c {
            ')' | ']' | '}' => depth += 1,
            '(' | '[' | '{' => {
                if depth == 0 {
                    return if c == '(' { Some(i) } else { None };
                }
                depth -= 1;
            }
            ';' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

fn binds_at_sites(source: &str, name: &str, sites: &LocalBindingSites) -> bool {
    let patterns = local_binding_patterns(name, "y");
    let mut from = 0usize;
    for &at in &sites.var_decls {
        if at < from {
            continue;
        }
        let Some(m) = patterns.decl.captures_at(source, at) else { continue };
        let whole = m.get(0).unwrap();
        from = at + (whole.end() - whole.start());
        if !import_binding_value(m.text(1).unwrap_or("")) {
            return true;
        }
    }
    for &at in &sites.fn_decls {
        if patterns.fn_.captures_at(source, at).is_some() {
            return true;
        }
    }
    let mut tried: Option<usize> = None;
    let mut search = 0usize;
    while let Some(rel) = source[search..].find(name) {
        let at = search + rel;
        search = at + 1;
        while search < source.len() && !source.is_char_boundary(search) {
            search += 1;
        }
        let Some(open) = enclosing_open_paren(source, at) else { continue };
        if tried == Some(open) {
            continue;
        }
        tried = Some(open);
        if patterns.param.captures_at(source, open).is_some() {
            return true;
        }
    }
    let bytes = source.as_bytes();
    for &arrow in &sites.arrows {
        let mut end = arrow;
        while end > 0 && matches!(bytes[end - 1], b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r') {
            end -= 1;
        }
        let Some(start) = end.checked_sub(name.len()) else { continue };
        if source.get(start..).is_some_and(|s| s.starts_with(name)) {
            let prev = if start == 0 { None } else { Some(bytes[start - 1]) };
            if prev.map_or(true, |b| !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'.'))) {
                return true;
            }
        }
    }
    false
}

fn binds_anywhere(source: &str, name: &str) -> bool {
    let patterns = local_binding_patterns(name, "");
    for m in patterns.decl.captures_iter(source) {
        if !import_binding_value(m.text(1).unwrap_or("")) {
            return true;
        }
    }
    let n = escape_regex(name);
    patterns.fn_.is_match(source) || patterns.param.is_match(source) || Re::cached(&format!(r"(?:^|[^\w$.]){n}\s*=>"), "").is_match(source)
}

/// Does the file declare `name` locally anywhere (a `const`, a function, a parameter)?
pub fn is_locally_bound_js_name(name: &str, file_path: &str, ctx: &Ctx) -> bool {
    memoized::<LocalBindingMemo>(ctx, format!("{file_path}\0{name}"), || {
        let source = ctx.read_file(file_path).map(|s| s.to_string()).unwrap_or_default();
        let simple = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
        if simple {
            binds_at_sites(&source, name, &local_binding_sites(file_path, &source, ctx))
        } else {
            binds_anywhere(&source, name)
        }
    })
}

/// The enclosing function of `r`, as the node the matchers see.
pub fn enclosing_function(r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    ctx.node_by_id(&r.from_node_id).filter(|f| is_callable_kind(f.kind) && f.start_line <= r.line && f.end_line >= r.line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn a_parameter_list_pattern_finds_an_arrow_function_head() {
        assert!(has_param_list("x", "const f = (x) => x + 1;"));
        assert!(has_param_list("x", "function f(a, x: number = 3): void {"));
        assert!(!has_param_list("x", "call(x)"));
    }

    #[test]
    fn a_keyword_head_is_not_a_parameter_list() {
        assert!(!has_param_list("x", "if (x) {"));
        assert!(!has_param_list("x", "while  (x) {"));
        assert!(has_param_list("x", "function f(x) {"));
    }

    #[test]
    fn block_ends_track_braces() {
        let open = units("{ a { b } c ");
        assert_eq!(js_block_end(&open, 1), open.len());
        let closed = units("a } rest");
        assert_eq!(js_block_end(&closed, 0), 2);
    }

    #[test]
    fn a_statement_body_ends_at_a_comma_or_semicolon() {
        let code = units("a + b; rest");
        assert_eq!(js_body_end(&code, 0), 5);
        let open = units("{ still open");
        assert!(js_body_open_at_end(&open, 0));
    }

    #[test]
    fn local_declarations_are_found_by_the_site_scan() {
        let src = "const x = 1;\nfunction g() { const y = require('y'); }";
        let sites = LocalBindingSites {
            var_decls: re!(r"\b(?:const|let|var)\s", "g").captures_iter(src).filter_map(|c| c.get(0).map(|m| m.start())).collect(),
            fn_decls: re!(r"\b(?:function|class)\s", "g").captures_iter(src).filter_map(|c| c.get(0).map(|m| m.start())).collect(),
            arrows: Vec::new(),
        };
        assert!(binds_at_sites(src, "x", &sites));
        assert!(binds_at_sites(src, "g", &sites));
        assert!(!binds_at_sites(src, "y", &sites));
    }
}
