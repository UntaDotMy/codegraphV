//! The binding a JS/TS receiver has in scope at a call, read across the whole file (#2589) — the
//! `jsScopeFile` / `jsBindingAt` / `jsReceiverBindingReading` reader of
//! `src/resolution/name-matcher.ts`, and the React lifecycle filter that rides with it.
//!
//! Offsets are UTF-16 units, as the TypeScript string indices are; a regex runs on the blanked
//! code as a `String` at the byte offset a unit offset maps to.

use super::infer::normalize_inferred_type_name;
use super::js_scope::{blanked_code_units, js_block_end, js_body_end, local_binding_patterns};
use super::util::{lines_of, memo_map};
use crate::ctx::Ctx;
use crate::types::UnresolvedRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_search::generated::is_minified_content;
use codegraph_types::jscompat::is_js_space;
use codegraph_types::{Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// The type a TS annotation writes, captured: `Logger`, `Repository<User>`, and with brackets
/// after it whole (`Item[]`, `Element["ids"]`) — TS_ANNOTATED_TYPE.
pub const TS_ANNOTATED_TYPE: &str = r"([A-Z][\w.$]*(?:(?:\s*<[^;=(){}<>]*(?:<[^;=(){}<>]*>[^;=(){}<>]*)*>)?\s*\[[^\]]*\])?)";

/// What a binding says its value is: an instance of the type it is written with, or of the class
/// its whole value makes with `new` (`raw` as written, on `line`).
#[derive(Debug, Clone, PartialEq)]
pub struct JsBindingReading {
    pub ty: String,
    pub raw: String,
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SiteKind {
    Declaration,
    Parameter,
    Function,
    Other,
}

/// A binding of a name: where it holds (`from`, its block's `{` or its body's start, through
/// `to`) and where it is written (`at`).
struct Site {
    from: i64,
    to: usize,
    at: usize,
    kind: SiteKind,
    reading: RefCell<Option<Option<JsBindingReading>>>,
}

struct Scope {
    start: i64,
    end: usize,
    parent: usize,
}

/// A JS/TS file's code read for the bindings its names have: comments, strings, templates and
/// regex literals blanked; line starts; braces as a tree of scopes, the first the file's own.
pub struct JsScopeFile {
    code: String,
    units: Vec<u16>,
    /// Byte offset of each unit offset in `code` (`None` for ASCII code, where they agree).
    byte_of_unit: Option<Vec<u32>>,
    line_starts: Vec<usize>,
    scopes: Vec<Scope>,
    var_decls: Vec<usize>,
    fn_decls: Vec<usize>,
    arrows: Vec<usize>,
    bindings: RefCell<HashMap<String, Rc<Vec<Site>>>>,
}

impl JsScopeFile {
    fn new(source: &str) -> JsScopeFile {
        let units = blanked_code_units(source, Some("javascript"));
        let code = String::from_utf16_lossy(&units);
        let byte_of_unit = (!code.is_ascii()).then(|| {
            let mut map = Vec::with_capacity(units.len() + 1);
            for (b, c) in code.char_indices() {
                for _ in 0..c.len_utf16() {
                    map.push(b as u32);
                }
            }
            map.push(code.len() as u32);
            map
        });
        let mut line_starts = vec![0usize];
        let mut scopes = vec![Scope { start: -1, end: units.len(), parent: usize::MAX }];
        let mut open = vec![0usize];
        for (i, &c) in units.iter().enumerate() {
            if c == b'\n' as u16 {
                line_starts.push(i + 1);
            } else if c == b'{' as u16 {
                scopes.push(Scope { start: i as i64, end: units.len(), parent: *open.last().unwrap() });
                open.push(scopes.len() - 1);
            } else if c == b'}' as u16 && open.len() > 1 {
                let s = open.pop().unwrap();
                scopes[s].end = i;
            }
        }
        let mut file = JsScopeFile {
            code,
            units,
            byte_of_unit,
            line_starts,
            scopes,
            var_decls: Vec::new(),
            fn_decls: Vec::new(),
            arrows: Vec::new(),
            bindings: RefCell::new(HashMap::new()),
        };
        file.var_decls = re!(r"\b(?:const|let|var)\s", "g").captures_iter(&file.code).filter_map(|m| m.get(0)).map(|m| file.unit(m.start())).collect();
        file.fn_decls = re!(r"\b(?:function|class)\s", "g").captures_iter(&file.code).filter_map(|m| m.get(0)).map(|m| file.unit(m.start())).collect();
        file.arrows = file.code.match_indices("=>").map(|(b, _)| file.unit(b)).collect();
        file
    }

    /// The byte offset of unit offset `u` (clamped to the code's end).
    fn byte(&self, u: usize) -> usize {
        match &self.byte_of_unit {
            None => u.min(self.code.len()),
            Some(map) => map[u.min(map.len() - 1)] as usize,
        }
    }

    /// The unit offset of byte offset `b`.
    fn unit(&self, b: usize) -> usize {
        match &self.byte_of_unit {
            None => b,
            Some(map) => map.partition_point(|&x| (x as usize) < b),
        }
    }

    /// `code.slice(from, to)` in units.
    fn slice(&self, from: usize, to: usize) -> &str {
        let (a, b) = (self.byte(from), self.byte(to.max(from)));
        &self.code[a..b]
    }

    /// The UTF-16 length of `s`.
    fn len16(s: &str) -> usize {
        s.encode_utf16().count()
    }

    fn at(&self, i: usize) -> Option<u16> {
        self.units.get(i).copied()
    }

    /// The innermost scope that holds `offset`.
    fn scope_at(&self, offset: usize) -> usize {
        let (mut lo, mut hi) = (0usize, self.scopes.len());
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.scopes[mid].start < offset as i64 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        while lo > 0 && self.scopes[lo].end < offset {
            lo = self.scopes[lo].parent;
        }
        lo
    }

    /// The 1-based line of a unit offset.
    fn line_of(&self, offset: usize) -> u32 {
        (self.line_starts.partition_point(|&s| s <= offset).max(1)) as u32
    }
}

/// Per context, the files `js_scope_file` read last, most recent last.
#[derive(Default)]
struct JsScopeFiles(RefCell<IndexMap<String, Option<Rc<JsScopeFile>>>>);
const JS_SCOPE_FILES_KEEP: usize = 16;

fn js_scope_file(file_path: &str, ctx: &Ctx) -> Option<Rc<JsScopeFile>> {
    let memo = ctx.memo::<JsScopeFiles>();
    let cached = memo.0.borrow_mut().shift_remove(file_path);
    let file = match cached {
        Some(file) => file,
        None => {
            let source = ctx.read_file(file_path);
            // A bundle's names are nothing a person declares a receiver with.
            let file = source
                .filter(|s| s.encode_utf16().count() <= 1_000_000 && !is_minified_content(file_path, s))
                .map(|s| Rc::new(JsScopeFile::new(&s)));
            let mut files = memo.0.borrow_mut();
            if files.len() >= JS_SCOPE_FILES_KEEP {
                files.shift_remove_index(0);
            }
            file
        }
    };
    memo.0.borrow_mut().insert(file_path.to_string(), file.clone());
    file
}

fn is_word_unit(c: u16) -> bool {
    c < 128 && ((c as u8).is_ascii_alphanumeric() || c == b'_' as u16 || c == b'$' as u16)
}

fn is_space_unit(c: u16) -> bool {
    char::from_u32(c as u32).is_some_and(is_js_space)
}

/// The `(` that opens the innermost unclosed parenthesis before `at`, within 2000 units.
fn enclosing_open_paren(units: &[u16], at: usize) -> Option<usize> {
    let mut depth = 0i64;
    let mut i = at;
    while i > 0 && at - (i - 1) <= 2000 {
        i -= 1;
        match units[i] {
            c if c == b')' as u16 || c == b']' as u16 || c == b'}' as u16 => depth += 1,
            c if c == b'(' as u16 || c == b'[' as u16 || c == b'{' as u16 => {
                if depth == 0 {
                    return (c == b'(' as u16).then_some(i);
                }
                depth -= 1;
            }
            c if c == b';' as u16 && depth == 0 => return None,
            _ => {}
        }
    }
    None
}

/// Where a `for` statement whose head holds `at` ends: past the statement after its head.
fn js_for_statement_end(units: &[u16], at: usize) -> usize {
    let mut depth = 0i64;
    for (i, &c) in units.iter().enumerate().skip(at) {
        if c == b'(' as u16 || c == b'[' as u16 || c == b'{' as u16 {
            depth += 1;
        } else if c == b')' as u16 || c == b']' as u16 || c == b'}' as u16 {
            depth -= 1;
            if depth < 0 {
                return if c == b')' as u16 { js_body_end(units, i + 1) } else { i };
            }
        }
    }
    units.len()
}

fn control_head_before(file: &JsScopeFile, open: usize) -> bool {
    re!(r"\b(?:if|while|for|switch|with)\s*$").is_match(file.slice(open.saturating_sub(12), open))
}

/// The body of the function whose parameter list destructures the name at `at`, or `None` when
/// the name is in no parameter list.
fn destructured_parameter_body(file: &JsScopeFile, at: usize) -> Option<(i64, usize)> {
    let units = &file.units;
    let mut depth = 0i64;
    let mut nested = false;
    let mut open: Option<usize> = None;
    let mut i = at;
    while i > 0 && at - (i - 1) <= 2000 {
        i -= 1;
        let c = units[i];
        if c == b')' as u16 || c == b']' as u16 || c == b'}' as u16 {
            depth += 1;
        } else if c == b'(' as u16 || c == b'[' as u16 || c == b'{' as u16 {
            if depth > 0 {
                depth -= 1;
            } else if c == b'(' as u16 {
                open = Some(i);
                break;
            } else {
                nested = true;
            }
        } else if c == b';' as u16 && depth == 0 {
            return None;
        }
    }
    let open = open?;
    if !nested || control_head_before(file, open) {
        return None;
    }
    let mut close = open + 1;
    let mut d = 1;
    while close < units.len() && d > 0 {
        if units[close] == b'(' as u16 {
            d += 1;
        } else if units[close] == b')' as u16 {
            d -= 1;
        }
        close += 1;
    }
    let head = re!(r"^\s*(?::(?:[^=;{}()\n]|\{(?:[^{}]|\{[^{}]*\})*\})*)?(=>|\{)").captures(file.slice(close, close + 2000))?;
    let end = close + JsScopeFile::len16(head.text(0).unwrap_or(""));
    Some(if head.text(1) == Some("{") { (end as i64 - 1, js_block_end(units, end)) } else { (end as i64, js_body_end(units, end)) })
}

/// Every binding of `name` in the file, read once per file and name.
fn js_binding_sites(name: &str, file: &JsScopeFile) -> Rc<Vec<Site>> {
    if let Some(known) = file.bindings.borrow().get(name) {
        return Rc::clone(known);
    }
    let mut sites: Vec<Site> = Vec::new();
    let push = |sites: &mut Vec<Site>, from: i64, to: usize, at: usize, kind: SiteKind| {
        sites.push(Site { from, to, at, kind, reading: RefCell::new(None) });
    };
    let block_of = |at: usize| {
        let scope = &file.scopes[file.scope_at(at)];
        (scope.start, scope.end)
    };
    let patterns = local_binding_patterns(name, "y");
    let mut resume = 0usize;
    for &at in &file.var_decls {
        if at < resume {
            continue;
        }
        let Some(m) = patterns.decl.captures_at(&file.code, file.byte(at)) else { continue };
        let whole = m.text(0).unwrap_or("");
        resume = at + JsScopeFile::len16(whole);
        if re!(r"\bfor\s*(?:await\s*)?\(\s*$").is_match(file.slice(at.saturating_sub(16), at)) {
            // A `for` head's binding holds in the head and the statement after it.
            let from = file.units[..at].iter().rposition(|&c| c == b'(' as u16).map_or(-1, |p| p as i64);
            push(&mut sites, from, js_for_statement_end(&file.units, at), at, SiteKind::Other);
            continue;
        }
        // `const x = require('./x')` binds an import, which the file's imports answer for.
        let plain = !re!(r"^(?:const|let|var)\s+[{\[]").is_match(whole)
            && !re!(r"^\s*(?:await\s+)?(?:require|import)\s*\(").is_match(m.text(1).unwrap_or(""));
        let (from, to) = block_of(at);
        push(&mut sites, from, to, at, if plain { SiteKind::Declaration } else { SiteKind::Other });
    }
    for &at in &file.fn_decls {
        if patterns.fn_.captures_at(&file.code, file.byte(at)).is_some() {
            let (from, to) = block_of(at);
            push(&mut sites, from, to, at, SiteKind::Function);
        }
    }
    let n = escape_regex(name);
    let param_name = Re::cached(&format!(r"(?:^\(|,)\s*(?:(?:public|private|protected|readonly|override)\s+)*(?:\.\.\.)?({n})(?![\w$])"), "");
    let name_len = JsScopeFile::len16(name);
    let mut tried: Option<usize> = None;
    let mut search = 0usize;
    while let Some(rel) = file.code.get(search..).and_then(|s| s.find(name)) {
        let byte_at = search + rel;
        search = byte_at + 1;
        while search < file.code.len() && !file.code.is_char_boundary(search) {
            search += 1;
        }
        let at = file.unit(byte_at);
        // A whole name, and no member's (`el.root`), though a rest parameter's (`...root`) is one.
        if (at > 0 && is_word_unit(file.units[at - 1]))
            || file.at(at + name_len).is_some_and(is_word_unit)
            || (at > 0 && file.units[at - 1] == b'.' as u16 && file.slice(at.saturating_sub(3), at) != "...")
        {
            continue;
        }
        let open = enclosing_open_paren(&file.units, at);
        // A control-flow head, `if (root) {`, binds nothing.
        if open.is_some_and(|o| control_head_before(file, o)) {
            continue;
        }
        let Some(open) = open else {
            // A destructured parameter: `({ root }) => …`, `function f([root]) {`.
            if let Some((from, to)) = destructured_parameter_body(file, at) {
                push(&mut sites, from, to, at, SiteKind::Other);
            }
            continue;
        };
        if tried == Some(open) {
            continue;
        }
        tried = Some(open);
        let Some(m) = patterns.param.captures_at(&file.code, file.byte(open)) else { continue };
        let whole = m.text(0).unwrap_or("");
        let end = open + JsScopeFile::len16(whole);
        let own_at = param_name.captures(whole).and_then(|own| {
            let g = own.get(0)?;
            Some(open + JsScopeFile::len16(&whole[..g.end()]) - name_len)
        });
        let (from, to) = if end > 0 && file.units[end - 1] == b'{' as u16 {
            (end as i64 - 1, js_block_end(&file.units, end))
        } else {
            (end as i64, js_body_end(&file.units, end))
        };
        push(&mut sites, from, to, own_at.unwrap_or(at), SiteKind::Parameter);
    }
    // `name =>`: the name ends where the whitespace before the arrow starts.
    let name_units: Vec<u16> = name.encode_utf16().collect();
    for &arrow in &file.arrows {
        let mut end = arrow;
        while end > 0 && is_space_unit(file.units[end - 1]) {
            end -= 1;
        }
        let Some(start) = end.checked_sub(name_len) else { continue };
        let head_char = |c: u16| is_word_unit(c) || c == b'.' as u16;
        if file.units[start..].starts_with(&name_units) && (start == 0 || !head_char(file.units[start - 1])) {
            push(&mut sites, arrow as i64 + 2, js_body_end(&file.units, arrow + 2), start, SiteKind::Other);
        }
    }
    let sites = Rc::new(sites);
    file.bindings.borrow_mut().insert(name.to_string(), Rc::clone(&sites));
    sites
}

/// The binding of `name` in scope at `offset`: the one whose scope holds it innermost, and of two
/// in one scope the declaration nearest before it.
fn js_binding_at(name: &str, file: &JsScopeFile, offset: usize) -> Option<(Rc<Vec<Site>>, usize)> {
    let sites = js_binding_sites(name, file);
    let mut best: Option<usize> = None;
    for (i, site) in sites.iter().enumerate() {
        if site.from > offset as i64 || site.to < offset {
            continue;
        }
        let better = match best {
            None => true,
            Some(b) => {
                let b = &sites[b];
                site.from > b.from
                    || (site.from == b.from
                        && if site.at <= offset { b.at > offset || site.at > b.at } else { b.at > offset && site.at < b.at })
            }
        };
        if better {
            best = Some(i);
        }
    }
    best.map(|i| (sites, i))
}

/// What a binding of `name` says its value is, or `None` when it says nothing.
fn js_binding_reading(name: &str, site: &Site, file: &JsScopeFile) -> Option<JsBindingReading> {
    if let Some(known) = site.reading.borrow().as_ref() {
        return known.clone();
    }
    let reading = match site.kind {
        SiteKind::Parameter => js_parameter_reading(name, site, file),
        SiteKind::Declaration => js_declaration_reading(name, site, file),
        _ => None,
    };
    *site.reading.borrow_mut() = Some(reading.clone());
    reading
}

/// The type a parameter is written with (`root: Root`, `svc?: Store`).
fn js_parameter_reading(name: &str, site: &Site, file: &JsScopeFile) -> Option<JsBindingReading> {
    let typed = Re::cached(&format!(r"{}(?![\w$])\s*\??\s*:\s*{TS_ANNOTATED_TYPE}", escape_regex(name)), "y");
    let raw = typed.captures_at(&file.code, file.byte(site.at))?.text(1)?.to_string();
    let ty = normalize_inferred_type_name(&raw)?;
    Some(JsBindingReading { ty, raw, line: file.line_of(site.at) })
}

/// What a `const`/`let`/`var` says its value is: the type it is written with, or the class its
/// whole value makes with `new` — not a `new` of a parameter or variable, which names no class.
fn js_declaration_reading(name: &str, site: &Site, file: &JsScopeFile) -> Option<JsBindingReading> {
    let head = Re::cached(&format!(r"(?:const|let|var)\s+{}(?![\w$])\s*(?:!?\s*:\s*{TS_ANNOTATED_TYPE})?", escape_regex(name)), "y");
    let m = head.captures_at(&file.code, file.byte(site.at))?;
    let value_at = site.at + JsScopeFile::len16(m.text(0).unwrap_or(""));
    let rest = file.slice(value_at, value_at + 400);
    // An object literal is its own members', whatever type it is written with.
    if re!(r"^[^=;]*=(?![=>])\s*\{").is_match(rest) {
        return None;
    }
    let line = file.line_of(site.at);
    if let Some(annotated) = m.text(1) {
        let ty = normalize_inferred_type_name(annotated)?;
        return Some(JsBindingReading { ty, raw: annotated.to_string(), line });
    }
    let made = re!(r"^\s*=\s*new\s+([A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)\s*(?:<[^;(){}]*>\s*)?\(").captures(rest)?;
    let class_name: String = made.text(1).unwrap_or("").chars().filter(|c| !c.is_whitespace()).collect();
    if let Some((sites, i)) = js_binding_at(class_name.split('.').next().unwrap_or(""), file, value_at) {
        if sites[i].kind != SiteKind::Function {
            return None;
        }
    }
    // The `new` is the whole value: no member, index, call or operator follows it.
    let mut close = value_at + JsScopeFile::len16(made.text(0).unwrap_or(""));
    let mut depth = 1;
    let limit = close + 5000;
    while close < file.units.len() && close < limit && depth > 0 {
        if file.units[close] == b'(' as u16 {
            depth += 1;
        } else if file.units[close] == b')' as u16 {
            depth -= 1;
        }
        close += 1;
    }
    if depth > 0 || !re!(r"^[ \t]*(?:[;,)}]|\r?\n(?![ \t]*(?:[.(\[?:]|\?\?|\|\||&&|as\b))|$)").is_match(file.slice(close, close + 200)) {
        return None;
    }
    let ty = normalize_inferred_type_name(&class_name)?;
    Some(JsBindingReading { ty, raw: class_name, line })
}

/// Per context and file: how many functions and methods hold each line, and the names the file's
/// module scope gives a variable or constant node.
#[derive(Default)]
struct JsModuleValues(RefCell<IndexMap<String, Rc<(Vec<i32>, HashSet<String>)>>>);

/// Whether the binding a call's receiver has may lie outside the function making the call: in a
/// function around that one, or at module scope, which declares the name.
fn js_may_bind_outside(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let memo = ctx.memo::<JsModuleValues>();
    let cached = memo.0.borrow().get(&r.file_path).cloned();
    let scope = match cached {
        Some(s) => s,
        None => {
            let nodes = ctx.get_nodes_in_file(&r.file_path);
            let functions: Vec<&Node> = nodes.iter().map(|n| n.as_ref()).filter(|n| n.kind == NodeKind::Function || n.kind == NodeKind::Method).collect();
            let last = functions.iter().map(|n| n.end_line as usize).max().unwrap_or(0);
            let mut depth = vec![0i32; last + 2];
            for n in &functions {
                depth[n.start_line as usize] += 1;
                depth[n.end_line as usize + 1] -= 1;
            }
            for line in 1..depth.len() {
                depth[line] += depth[line - 1];
            }
            let names: HashSet<String> = nodes
                .iter()
                .filter(|n| (n.kind == NodeKind::Variable || n.kind == NodeKind::Constant) && n.qualified_name == n.name)
                .map(|n| n.name.clone())
                .collect();
            let scope = Rc::new((depth, names));
            let mut files = memo.0.borrow_mut();
            if files.len() >= 64 {
                files.shift_remove_index(0);
            }
            files.insert(r.file_path.clone(), Rc::clone(&scope));
            scope
        }
    };
    let around = scope.0.get(r.line as usize).copied().unwrap_or(0);
    around > 1 || (around == 1 && scope.1.contains(name))
}

/// A JS/TS receiver the calling function doesn't type, read by its binding in scope at the call
/// (#2589): a module's `let root: Root | null`, a `let popupRoot: Root` in the function around the
/// one calling, or an outer function's typed parameter. `None` when the binding says nothing.
pub fn js_receiver_binding_reading(receiver: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<JsBindingReading> {
    if !js_may_bind_outside(receiver, r, ctx) {
        return None;
    }
    let file = js_scope_file(&r.file_path, ctx)?;
    let line_start = file.line_starts.get((r.line as usize).wrapping_sub(1)).copied().unwrap_or(file.units.len());
    let (sites, i) = js_binding_at(receiver, &file, line_start + r.column as usize)?;
    js_binding_reading(receiver, &sites[i], &file)
}

/// The methods React calls on a class component itself.
const REACT_LIFECYCLE_METHODS: [&str; 13] = [
    "render",
    "componentDidMount",
    "componentDidUpdate",
    "componentWillUnmount",
    "shouldComponentUpdate",
    "getSnapshotBeforeUpdate",
    "componentDidCatch",
    "componentWillMount",
    "componentWillReceiveProps",
    "componentWillUpdate",
    "UNSAFE_componentWillMount",
    "UNSAFE_componentWillReceiveProps",
    "UNSAFE_componentWillUpdate",
];

memo_map!(ReactLifecycleMemo, String, bool);

/// A lifecycle method (`render`, `componentDidMount`, …) of a class that extends React's
/// `Component` or `PureComponent` (or Preact's), read from the class's head.
pub fn is_react_lifecycle_method(n: &Node, ctx: &Ctx) -> bool {
    if n.kind != NodeKind::Method || !REACT_LIFECYCLE_METHODS.contains(&n.name.as_str()) {
        return false;
    }
    super::util::memoized::<ReactLifecycleMemo>(ctx, n.id.clone(), || {
        let owner_name = n.qualified_name.rfind("::").map(|i| &n.qualified_name[..i]).and_then(|q| q.rsplit("::").next()).unwrap_or("");
        if owner_name.is_empty() {
            return false;
        }
        let nodes = ctx.get_nodes_in_file(&n.file_path);
        let Some(owner) = nodes.iter().find(|c| c.kind == NodeKind::Class && c.name == owner_name && c.start_line <= n.start_line && c.end_line >= n.end_line) else {
            return false;
        };
        let lines = lines_of(ctx, &n.file_path);
        let from = (owner.start_line as usize).saturating_sub(1);
        let to = ((owner.start_line + 3).min(n.start_line) as usize).min(lines.len());
        let joined = if from < to { lines[from..to].join(" ") } else { String::new() };
        let head = joined.split('{').next().unwrap_or("");
        re!(r"\bextends\s+(?:React\s*\.\s*)?(?:Pure)?Component\b").is_match(head)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(src: &str, name: &str, at_text: &str) -> Option<JsBindingReading> {
        let file = JsScopeFile::new(src);
        let offset = JsScopeFile::len16(&src[..src.find(at_text).unwrap()]);
        let (sites, i) = js_binding_at(name, &file, offset)?;
        js_binding_reading(name, &sites[i], &file)
    }

    #[test]
    fn a_module_binding_types_a_call_in_a_nested_function() {
        let src = "let root: Root | null = null;\nfunction mount() {\n  function go() { root.render(x); }\n}\n";
        let r = reading(src, "root", "root.render").unwrap();
        assert_eq!((r.ty.as_str(), r.raw.as_str(), r.line), ("Root", "Root", 1));
    }

    #[test]
    fn an_inner_binding_shadows_the_module_one() {
        let src = "let root: Root;\nfunction f(root: Other) {\n  root.render();\n}\n";
        assert_eq!(reading(src, "root", "root.render").unwrap().ty, "Other");
    }

    #[test]
    fn a_new_of_a_parameter_names_no_class() {
        let src = "function f(Backend) {\n  const svc = new Backend(1);\n  return () => svc.go();\n}\n";
        assert!(reading(src, "svc", "svc.go").is_none());
        let made = "const svc = new Store(1);\nfunction g() { svc.go(); }\n";
        assert_eq!(reading(made, "svc", "svc.go").unwrap().ty, "Store");
    }

    #[test]
    fn an_object_literal_keeps_its_own_members() {
        let src = "const server: ViteDevServer = { close() {} };\nfunction g() { server.close(); }\n";
        assert!(reading(src, "server", "server.close").is_none());
    }

    #[test]
    fn non_ascii_code_keeps_unit_offsets() {
        let src = "const é = 'ü';\nlet root: Root;\nfunction g() { root.render(); }\n";
        assert_eq!(reading(src, "root", "root.render").unwrap().line, 2);
    }
}
