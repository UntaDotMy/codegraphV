//! Dart local bindings — port of `src/resolution/dart-local-scope.ts`.
//!
//! A call to a name a parameter, a local variable or a local function binds is not a call to the
//! project-wide namesake. The scopes are read from the source with a small bracket matcher.
//!
//! Offsets: the source is turned into a byte string with exactly one byte per UTF-16 unit
//! (non-ASCII units become a blank or an inert byte — Dart identifiers are ASCII), so every
//! index here is the UTF-16 index the TypeScript works with.

use crate::ctx::Ctx;
use crate::types::{ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::jscompat::is_js_space;
use codegraph_types::{EdgeKind, Language, ReferenceKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[derive(Clone, Copy)]
struct Scope {
    start: usize,
    end: usize,
    func: bool,
}

struct DartFileScopes {
    line_starts: Vec<usize>,
    names: HashMap<String, Vec<Scope>>,
}

#[derive(Default)]
struct Memo(RefCell<HashMap<String, Option<Rc<DartFileScopes>>>>);

const DART_RESERVED: &[&str] = &[
    "assert", "break", "case", "catch", "class", "const", "continue", "default", "do", "else", "enum", "extends", "false", "final",
    "finally", "for", "if", "in", "is", "new", "null", "rethrow", "return", "super", "switch", "this", "throw", "true", "try", "var",
    "void", "while", "with",
];

const DART_EXTRA_KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "base", "covariant", "deferred", "export", "extension", "external", "factory", "get", "hide",
    "implements", "import", "interface", "late", "library", "mixin", "of", "on", "operator", "part", "required", "sealed", "set", "show",
    "static", "sync", "typedef", "when", "yield",
];

fn is_reserved(w: &str) -> bool {
    DART_RESERVED.contains(&w)
}

fn is_keyword(w: &str) -> bool {
    DART_RESERVED.contains(&w) || DART_EXTRA_KEYWORDS.contains(&w)
}

/// One byte per UTF-16 unit.
fn units(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let fill = if is_js_space(c) { ' ' } else { '\u{7f}' };
            for _ in 0..c.len_utf16() {
                out.push(fill);
            }
        }
    }
    out
}

fn b(code: &str, i: usize) -> u8 {
    code.as_bytes().get(i).copied().unwrap_or(0)
}

fn bi(code: &str, i: isize) -> u8 {
    if i < 0 {
        0
    } else {
        b(code, i as usize)
    }
}

fn slice(code: &str, from: usize, to: usize) -> &str {
    let to = to.min(code.len());
    let from = from.min(to);
    &code[from..to]
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

fn is_space_byte(c: u8) -> bool {
    c == b' ' || (9..=13).contains(&c)
}

fn trim_end(s: &str) -> &str {
    s.trim_end_matches(|c: char| c.is_ascii() && is_space_byte(c as u8))
}

fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii() && is_space_byte(c as u8))
}

pub fn gate_dart_local(resolved: Option<ResolvedRef>, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let resolved = resolved?;
    if r.language != Language::Dart {
        return Some(resolved);
    }
    if !matches!(r.reference_kind, ReferenceKind::Edge(EdgeKind::Calls) | ReferenceKind::FunctionRef) {
        return Some(resolved);
    }
    let name = &r.reference_name;
    if !re!(r"^[A-Za-z_$][\w$]*$").is_match(name) {
        return Some(resolved);
    }
    let Some(file) = file_scopes(&r.file_path, ctx) else { return Some(resolved) };
    let Some(bound) = file.names.get(name) else { return Some(resolved) };
    let lines: Vec<String> = match ctx.get_file_lines(&r.file_path) {
        Some(l) => l.iter().cloned().collect(),
        None => match ctx.read_file(&r.file_path) {
            Some(t) => t.split('\n').map(str::to_string).collect(),
            None => return Some(resolved),
        },
    };
    let Some(line) = lines.get((r.line as usize).wrapping_sub(1)) else { return Some(resolved) };
    let line = units(line);
    let column = name_column(&line, name, r.column as i64);
    let before = if column >= 0 { slice(&line, 0, column as usize) } else { "" };
    if column < 0 || re!(r"\.\s*$").is_match(before) {
        return Some(resolved);
    }
    if trim(before).is_empty() {
        let prev = lines.get((r.line as usize).wrapping_sub(2)).map(|s| units(s)).unwrap_or_default();
        if re!(r"\.\s*$").is_match(&re!(r"//.*$").replace_first(&prev, |_| String::new())) {
            return Some(resolved);
        }
    }
    let at = file.line_starts.get((r.line as usize).wrapping_sub(1)).copied().unwrap_or(0) + column as usize;
    let mut nearest: Option<Scope> = None;
    for scope in bound {
        if at >= scope.start && at <= scope.end && nearest.is_none_or(|n| scope.start > n.start) {
            nearest = Some(*scope);
        }
    }
    match nearest {
        Some(n) if !n.func => None,
        _ => Some(resolved),
    }
}

/// Is `name` bound by a parameter, local or local function covering offset `at` of `file_path`?
pub fn is_dart_locally_bound(file_path: &str, name: &str, at: usize, ctx: &Ctx) -> bool {
    file_scopes(file_path, ctx).and_then(|f| f.names.get(name).map(|bound| bound.iter().any(|s| at >= s.start && at <= s.end))).unwrap_or(false)
}

fn name_column(line: &str, name: &str, column: i64) -> i64 {
    let is_word = |at: i64| -> bool {
        if at < 0 {
            return false;
        }
        let a = at as usize;
        line[a.min(line.len())..].starts_with(name) && !is_word_byte(if a > 0 { b(line, a - 1) } else { 0 }) && !is_word_byte(b(line, a + name.len()))
    };
    if column >= 0 && is_word(column) {
        return column;
    }
    if column >= name.len() as i64 && is_word(column - name.len() as i64) {
        return column - name.len() as i64;
    }
    let mut best: i64 = -1;
    let mut from = 0usize;
    while let Some(p) = line[from.min(line.len())..].find(name) {
        let at = (from + p) as i64;
        if is_word(at) && (best < 0 || (at - column).abs() < (best - column).abs()) {
            best = at;
        }
        from = from + p + 1;
        if from > line.len() {
            break;
        }
    }
    best
}

fn file_scopes(file_path: &str, ctx: &Ctx) -> Option<Rc<DartFileScopes>> {
    let memo = ctx.memo::<Memo>();
    if let Some(hit) = memo.0.borrow().get(file_path) {
        return hit.clone();
    }
    let scopes = ctx.read_file(file_path).map(|s| Rc::new(read_scopes(&units(&s))));
    memo.0.borrow_mut().insert(file_path.to_string(), scopes.clone());
    scopes
}

type Bind<'a> = &'a mut dyn FnMut(&str, usize, usize, bool);

fn read_scopes(source: &str) -> DartFileScopes {
    let mut line_starts = vec![0usize];
    for (i, c) in source.bytes().enumerate() {
        if c == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let code = blank_type_arguments(&blank_dart_literals(source));
    let brackets = match_brackets(&code);
    let mut names: HashMap<String, Vec<Scope>> = HashMap::new();
    let mut bind = |name: &str, start: usize, end: usize, func: bool| {
        if name == "_" || is_reserved(name) {
            return;
        }
        names.entry(name.to_string()).or_default().push(Scope { start, end, func });
    };
    let parameter_lists = bind_parameters(&code, &brackets, &mut bind);
    bind_locals(&code, &brackets, &parameter_lists, &mut bind);
    DartFileScopes { line_starts, names }
}

struct Token {
    word: Option<String>,
    at: isize,
}

fn bind_parameters(code: &str, br: &Brackets, bind: Bind) -> HashSet<usize> {
    let mut lists: HashSet<usize> = HashSet::new();
    for k in 0..br.at.len() {
        let open = br.partner[k];
        if b(code, br.at[k]) != b')' || open < 0 {
            continue;
        }
        let from = br.at[open as usize];
        let head = token_before(code, from);
        let mut i = skip_space(code, br.at[k] + 1);
        if let Some(m) = re!(r"^(?:async\s*\*?|sync\s*\*)").find(slice(code, i, i + 8)) {
            i = skip_space(code, i + m.as_str().len());
        }
        let list = slice(code, from + 1, br.at[k]);
        let block: isize = if b(code, i) == b'{' && br.at.get(k + 1) == Some(&i) && br.partner[k + 1] >= 0 {
            br.at[br.partner[k + 1] as usize] as isize
        } else {
            -1
        };
        if head.word.as_deref() == Some("for") {
            let end = if block >= 0 { block as usize } else { end_of_statement(code, br, i, false) };
            for name in for_head_names(list) {
                bind(&name, from, end, false);
            }
            lists.insert(from);
            continue;
        }
        if head.word.as_deref().is_some_and(|w| matches!(w, "if" | "while" | "switch")) {
            continue;
        }
        let scope: Option<(usize, usize)> = if block >= 0 {
            Some((i, block as usize))
        } else if slice(code, i, i + 2) == "=>" {
            Some((i, end_of_statement(code, br, i + 2, true)))
        } else if b(code, i) == b':' && is_constructor_head(code, &head) {
            Some((i, end_of_constructor(code, br, i + 1)))
        } else {
            None
        };
        let Some((s_start, s_end)) = scope else { continue };
        if is_call_arguments(code, &head) {
            continue;
        }
        lists.insert(from);
        for name in parameter_names(list) {
            bind(&name, s_start, s_end, false);
        }
        let around: isize = match head.word.as_deref() {
            Some(w) if !is_keyword(w) => enclosing_block(code, br, head.at as usize),
            _ => -1,
        };
        if around >= 0 {
            bind(head.word.as_deref().unwrap(), head.at as usize, block_end(code, br, around as usize), true);
        }
    }
    lists
}

fn is_call_arguments(code: &str, head: &Token) -> bool {
    let Some(word) = head.word.as_deref() else { return false };
    if matches!(word, "super" | "this" | "assert") {
        return true;
    }
    let before = token_before(code, head.at as usize);
    if before.word.is_none() && matches!(bi(code, before.at), b'=' | b':' | b',') {
        return true;
    }
    if before.word.is_none() && bi(code, before.at) == b'.' {
        let owner = token_before(code, before.at as usize);
        return matches!(owner.word.as_deref(), Some("super" | "this"));
    }
    false
}

fn is_constructor_head(code: &str, head: &Token) -> bool {
    let Some(word) = head.word.as_deref() else { return false };
    if is_keyword(word) {
        return false;
    }
    let mut ty = word.to_string();
    let mut before = token_before(code, head.at as usize);
    if before.word.is_none() && bi(code, before.at) == b'.' {
        let owner = token_before(code, before.at as usize);
        let Some(ow) = owner.word.clone() else { return false };
        ty = ow;
        before = token_before(code, owner.at as usize);
    }
    if !re!(r"^_*[A-Z$]").is_match(&ty) {
        return false;
    }
    if let Some(w) = before.word.as_deref() {
        return w == "const" || w == "external";
    }
    before.at < 0 || matches!(bi(code, before.at), b';' | b'{' | b'}')
}

fn end_of_constructor(code: &str, br: &Brackets, from: usize) -> usize {
    let mut k = first_bracket_from(br, from);
    let mut pos = from;
    loop {
        let next = if k < br.at.len() { br.at[k] } else { code.len() };
        let semicolon = code[pos.min(code.len())..].find(';').map(|p| p + pos);
        if let Some(s) = semicolon {
            if s < next {
                return s;
            }
        }
        if k >= br.at.len() {
            return code.len();
        }
        let ch = b(code, next);
        if matches!(ch, b')' | b']' | b'}') {
            return next;
        }
        let close = br.partner[k];
        if close < 0 {
            return code.len();
        }
        if ch == b'{' {
            let prior = token_before(code, next);
            if prior.word.is_some() || matches!(bi(code, prior.at), b')' | b']' | b'\'' | b'"') {
                return br.at[close as usize];
            }
        }
        pos = br.at[close as usize] + 1;
        k = close as usize + 1;
    }
}

fn parameter_names(list: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for raw in split_parameters(list) {
        let item = re!(r"@[\w$.]+(?:\s*\([^()]*\))?", "g").replace_all_str(&raw, " ");
        let mut item: &str = &item;
        if let Some(eq) = item.find('=') {
            item = &item[..eq];
        }
        item = trim_end(item);
        let mut owned: String;
        if item.ends_with(')') {
            let bytes = item.as_bytes();
            let mut depth = 0i32;
            let mut at = bytes.len() as isize - 1;
            while at >= 0 {
                if bytes[at as usize] == b')' {
                    depth += 1;
                } else if bytes[at as usize] == b'(' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                at -= 1;
            }
            owned = item[..at.max(0) as usize].to_string();
            owned = trim_end(&owned).to_string();
            item = &owned;
            if let Some(n) = re!(r"([A-Za-z_$][\w$]*)$").captures(item).and_then(|c| c.text(1).map(str::to_string)) {
                names.push(n);
            }
            continue;
        }
        if let Some(n) = re!(r"([A-Za-z_$][\w$]*)$").captures(item).and_then(|c| c.text(1).map(str::to_string)) {
            names.push(n);
        }
    }
    names
}

fn split_parameters(list: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut item = String::new();
    let bytes = list.as_bytes();
    for i in 0..bytes.len() {
        let ch = bytes[i];
        if depth == 0 && (ch == b'{' || ch == b'[') && trim(&item).is_empty() {
            let closer = if ch == b'{' { '}' } else { ']' };
            let close = list.rfind(closer);
            let end = match close {
                Some(c) if c > i => c,
                _ => list.len(),
            };
            items.extend(split_parameters(&list[i + 1..end]));
            return items;
        }
        if matches!(ch, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(ch, b')' | b']' | b'}') {
            depth -= 1;
        }
        if ch == b',' && depth == 0 {
            if !trim(&item).is_empty() {
                items.push(std::mem::take(&mut item));
            }
            item.clear();
        } else {
            item.push(ch as char);
        }
    }
    if !trim(&item).is_empty() {
        items.push(item);
    }
    items
}

fn for_head_names(head: &str) -> Vec<String> {
    if let Some(lp) = re!(r"^([^;]*?)\bin\b").captures(head) {
        let decl = trim(lp.text(1).unwrap_or("")).to_string();
        if let Some(pattern) = re!(r"^(?:final|var)\s*(?:[\w$.]+\s*)?([([{][\s\S]*)$").captures(&decl) {
            return pattern_binders(pattern.text(1).unwrap_or(""));
        }
        let declared = re!(r"^(?:(?:final|var|const)\s+(?:[\w$.]+\s*\??\s+)?|[\w$.]+\s*\??\s+)([A-Za-z_$][\w$]*)$").captures(&decl);
        return declared.and_then(|d| d.text(1).map(|s| vec![s.to_string()])).unwrap_or_default();
    }
    let init = head.split(';').next().unwrap_or("");
    if !re!(r"^\s*(?:(?:final|var|const|late)\b|[\w$.]+\s*\??\s+[A-Za-z_$][\w$]*\s*=)").is_match(init) {
        return Vec::new();
    }
    re!(r"(?:^|,|\s)([A-Za-z_$][\w$]*)\s*=(?![=>])", "g").captures_iter(init).filter_map(|m| m.text(1).map(str::to_string)).collect()
}

fn pattern_binders(pattern: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for m in re!(r"(?<![\w$.])([A-Za-z_$][\w$]*)(?![\w$])(?!\s*[:(])(?!\s+[A-Za-z_$(])", "g").captures_iter(pattern) {
        let n = m.text(1).unwrap_or("");
        if n != "_" && !is_reserved(n) {
            names.push(n.to_string());
        }
    }
    names
}

fn bind_locals(code: &str, br: &Brackets, parameter_lists: &HashSet<usize>, bind: Bind) {
    let mut bind_in_block = |name: &str, at: usize, direct: bool| {
        let mut k = innermost_bracket(code, br, at);
        while !direct && k >= 0 && b(code, br.at[k as usize]) != b'{' {
            if parameter_lists.contains(&br.at[k as usize]) {
                return;
            }
            k = br.parent[k as usize];
        }
        if k >= 0 && b(code, br.at[k as usize]) == b'{' && !is_type_body(code, br.at[k as usize]) {
            bind(name, at, block_end(code, br, k as usize), false);
        }
    };
    for m in re!(r"(?<![\w$.])([A-Za-z_$][\w$]*)(?=\s*(?:=(?![=>])|;))", "g").captures_iter(code) {
        let idx = m.get(0).unwrap().start();
        if is_declaration_type(code, br, &token_before(code, idx)) {
            bind_in_block(m.text(1).unwrap_or(""), idx, true);
        }
    }
    for m in
        re!(r"(?<![\w$.])(?:final|var)\s+(?:[\w$.]+\s*\??\s+)?([A-Za-z_$][\w$]*)(?=\s*(?:[)\]},:]|\?(?!\s*[\w$])|=>|\bwhen\b|&&|\|\|))", "g")
            .captures_iter(code)
    {
        bind_in_block(m.text(1).unwrap_or(""), m.get(0).unwrap().start(), false);
    }
    for m in re!(r"(?<![\w$.])(?:final|var)\s*(?:[A-Z][\w$]*\s*)?([([{])", "g").captures_iter(code) {
        let whole = m.get(0).unwrap();
        let open = whole.start() + whole.as_str().len() - 1;
        let k = bracket_index_at(br, open);
        if k < 0 || br.partner[k as usize] < 0 {
            continue;
        }
        let close = br.at[br.partner[k as usize] as usize];
        if !re!(r"^\s*=(?![=>])").is_match(slice(code, close + 1, close + 4)) {
            continue;
        }
        for name in pattern_binders(slice(code, open + 1, close)) {
            bind_in_block(&name, whole.start(), true);
        }
    }
}

fn is_declaration_type(code: &str, br: &Brackets, before: &Token) -> bool {
    if let Some(w) = before.word.as_deref() {
        return matches!(w, "final" | "var" | "const") || !is_keyword(w);
    }
    let ch = bi(code, before.at);
    if ch == b'?' {
        let p = bi(code, before.at - 1);
        return is_word_byte(p) || p == b')';
    }
    if ch == b')' {
        let k = if before.at >= 0 { bracket_index_at(br, before.at as usize) } else { -1 };
        let open = if k >= 0 { br.partner[k as usize] } else { -1 };
        return open >= 0 && token_before(code, br.at[open as usize]).word.as_deref() == Some("Function");
    }
    false
}

fn is_type_body(code: &str, brace: usize) -> bool {
    let upto = &code[..brace.min(code.len())];
    let from = [upto.rfind(';'), upto.rfind('{'), upto.rfind('}')].into_iter().flatten().max();
    let start = from.map_or(0, |f| f + 1);
    re!(r"^\s*(?:@[\w$.]+(?:\s*\((?:[^()]|\([^()]*\))*\))?\s*)*(?:(?:abstract|base|final|sealed|interface|mixin|augment)\s+)*(?:class|mixin|enum|extension)\b")
        .is_match(slice(code, start, brace))
}

fn enclosing_block(code: &str, br: &Brackets, at: usize) -> isize {
    let k = innermost_bracket(code, br, at);
    if k >= 0 && b(code, br.at[k as usize]) == b'{' && !is_type_body(code, br.at[k as usize]) {
        k
    } else {
        -1
    }
}

fn block_end(code: &str, br: &Brackets, k: usize) -> usize {
    if br.partner[k] >= 0 {
        br.at[br.partner[k] as usize]
    } else {
        code.len()
    }
}

fn end_of_statement(code: &str, br: &Brackets, from: usize, expression: bool) -> usize {
    let mut k = first_bracket_from(br, from);
    let mut pos = from;
    loop {
        let next = if k < br.at.len() { br.at[k] } else { code.len() };
        for i in pos..next {
            let c = b(code, i);
            if c == b';' || (expression && c == b',') {
                return i;
            }
        }
        if k >= br.at.len() {
            return code.len();
        }
        let ch = b(code, next);
        if matches!(ch, b')' | b']' | b'}') {
            return next;
        }
        let close = br.partner[k];
        if close < 0 {
            return code.len();
        }
        pos = br.at[close as usize] + 1;
        k = close as usize + 1;
    }
}

fn token_before(code: &str, at: usize) -> Token {
    let mut i = at as isize - 1;
    while i >= 0 && is_space_byte(b(code, i as usize)) {
        i -= 1;
    }
    let mut start = i;
    while start >= 0 && is_word_byte(b(code, start as usize)) {
        start -= 1;
    }
    if start == i {
        return Token { word: None, at: i };
    }
    Token { word: Some(slice(code, (start + 1) as usize, (i + 1) as usize).to_string()), at: start + 1 }
}

fn skip_space(code: &str, mut at: usize) -> usize {
    while at < code.len() && is_space_byte(b(code, at)) {
        at += 1;
    }
    at
}

struct Brackets {
    at: Vec<usize>,
    partner: Vec<isize>,
    parent: Vec<isize>,
}

fn match_brackets(code: &str) -> Brackets {
    let mut at: Vec<usize> = Vec::new();
    let mut partner: Vec<isize> = Vec::new();
    let mut parent: Vec<isize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, ch) in code.bytes().enumerate() {
        if matches!(ch, b'(' | b'[' | b'{') {
            at.push(i);
            partner.push(-1);
            parent.push(stack.last().map_or(-1, |&s| s as isize));
            stack.push(at.len() - 1);
        } else if matches!(ch, b')' | b']' | b'}') {
            let want = match ch {
                b')' => b'(',
                b']' => b'[',
                _ => b'{',
            };
            let mut s = stack.len() as isize - 1;
            while s >= 0 && b(code, at[stack[s as usize]]) != want {
                s -= 1;
            }
            at.push(i);
            partner.push(-1);
            if s < 0 {
                parent.push(stack.last().map_or(-1, |&x| x as isize));
                continue;
            }
            let open = stack[s as usize];
            stack.truncate(s as usize);
            partner[open] = at.len() as isize - 1;
            let last = at.len() - 1;
            partner[last] = open as isize;
            parent.push(parent[open]);
        }
    }
    Brackets { at, partner, parent }
}

fn first_bracket_from(br: &Brackets, from: usize) -> usize {
    br.at.partition_point(|&p| p < from)
}

fn bracket_index_at(br: &Brackets, at: usize) -> isize {
    let k = first_bracket_from(br, at);
    if br.at.get(k) == Some(&at) {
        k as isize
    } else {
        -1
    }
}

fn innermost_bracket(code: &str, br: &Brackets, at: usize) -> isize {
    let k = first_bracket_from(br, at) as isize - 1;
    if k < 0 {
        return -1;
    }
    let ch = b(code, br.at[k as usize]);
    if matches!(ch, b'(' | b'[' | b'{') {
        k
    } else {
        br.parent[k as usize]
    }
}

fn blank_dart_literals(source: &str) -> String {
    let src = source.as_bytes();
    let n = src.len();
    let mut out: Vec<u8> = src.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for k in from..to.min(n) {
            if src[k] != b'\n' && src[k] != b'\r' {
                out[k] = b' ';
            }
        }
    };
    struct Suspended {
        quote: Vec<u8>,
        braces: i32,
    }
    let mut suspended: Vec<Suspended> = Vec::new();
    let mut quote: Option<Vec<u8>> = None;
    let mut raw = false;
    let mut i = 0usize;
    let at = |k: usize| src.get(k).copied().unwrap_or(0);
    while i < n {
        if let Some(q) = quote.clone() {
            if src[i..].starts_with(&q) {
                i += q.len();
                quote = None;
                continue;
            }
            let ch = src[i];
            if !raw && ch == b'\\' {
                blank(&mut out, i, i + 2);
                i += 2;
                continue;
            }
            if !raw && ch == b'$' && at(i + 1) == b'{' {
                blank(&mut out, i, i + 2);
                suspended.push(Suspended { quote: q, braces: 0 });
                quote = None;
                i += 2;
                continue;
            }
            if q.len() == 1 && ch == b'\n' {
                quote = None;
                i += 1;
                continue;
            }
            blank(&mut out, i, i + 1);
            i += 1;
            continue;
        }
        let ch = src[i];
        if ch == b'/' && at(i + 1) == b'/' {
            let end = src[i..].iter().position(|&c| c == b'\n').map(|p| p + i);
            blank(&mut out, i, end.unwrap_or(n));
            i = end.unwrap_or(n);
            continue;
        }
        if ch == b'/' && at(i + 1) == b'*' {
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if src[j] == b'/' && at(j + 1) == b'*' {
                    depth += 1;
                    j += 2;
                } else if src[j] == b'*' && at(j + 1) == b'/' {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
            continue;
        }
        if !suspended.is_empty() && (ch == b'{' || ch == b'}') {
            let top = suspended.last_mut().unwrap();
            if ch == b'{' {
                top.braces += 1;
            } else if top.braces > 0 {
                top.braces -= 1;
            } else {
                blank(&mut out, i, i + 1);
                let done = suspended.pop().unwrap();
                quote = Some(done.quote);
                raw = false;
            }
            i += 1;
            continue;
        }
        if ch == b'"' || ch == b'\'' {
            raw = i >= 1 && src[i - 1] == b'r' && !(i >= 2 && is_word_byte(src[i - 2]));
            let q = if at(i + 1) == ch && at(i + 2) == ch { vec![ch; 3] } else { vec![ch] };
            i += q.len();
            quote = Some(q);
            continue;
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn blank_type_arguments(code: &str) -> String {
    if !code.contains('<') {
        return code.to_string();
    }
    let mut out: Vec<u8> = code.as_bytes().to_vec();
    let mut i = code.find('<');
    while let Some(start) = i {
        let end = type_arguments_end(code, start);
        if end > start as isize {
            let end = end as usize;
            let glued = start > 0 && is_word_byte(b(code, start - 1));
            let next = b(code, skip_space(code, end + 1));
            if glued || matches!(next, b'(' | b'[' | b'{' | b'.') {
                for k in start..=end {
                    if out[k] != b'\n' && out[k] != b'\r' {
                        out[k] = b' ';
                    }
                }
                i = code[(end + 1).min(code.len())..].find('<').map(|p| p + end + 1);
                continue;
            }
        }
        i = code[(start + 1).min(code.len())..].find('<').map(|p| p + start + 1);
    }
    String::from_utf8(out).unwrap_or_default()
}

fn type_arguments_end(code: &str, open: usize) -> isize {
    let mut angles = 0i32;
    let mut parens = 0i32;
    let mut i = open;
    while i < code.len() && i < open + 400 {
        let ch = b(code, i);
        if ch == b'<' {
            angles += 1;
        } else if ch == b'>' {
            if i > 0 && b(code, i - 1) == b'=' {
                return -1;
            }
            angles -= 1;
            if angles == 0 {
                return if parens == 0 { i as isize } else { -1 };
            }
        } else if ch == b'(' || ch == b'{' {
            parens += 1;
        } else if ch == b')' || ch == b'}' {
            parens -= 1;
            if parens < 0 {
                return -1;
            }
        } else if !(is_word_byte(ch) || is_space_byte(ch) || matches!(ch, b'.' | b',' | b'?')) {
            return -1;
        }
        i += 1;
    }
    -1
}
