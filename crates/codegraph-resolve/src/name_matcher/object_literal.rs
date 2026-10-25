//! Members of object literals (`const api = { get() {} }`), `this` inside them, and calls through
//! dotted object paths (`app.services.cache.get()`, `window.lib.run()`) — the object-literal
//! region of `src/resolution/name-matcher.ts`.

use super::calls::bare_call_receiver;
use super::js_scope::js_function_local_scope;
use super::js_imports::is_import_binding;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{NodeRef, ResolvedBy, ResolvedRef, Tri, UnresolvedRef};
use codegraph_extract::strip_comments::{blank_string_contents, strip_comments_for_regex};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_from, utf16_len, utf16_prefix};
use codegraph_types::{Language, Node, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub fn host_global_prefix(s: &str) -> bool {
    re!(r"^(?:window|globalThis)\.").is_match(s)
}

/// The dotted path an object-literal holder is registered under (`window.` stripped).
pub fn holder_path(n: &Node) -> String {
    let last = last_qualified_segment(&n.qualified_name);
    if last.contains('.') {
        re!(r"^(?:window|globalThis)\.").replace_first(last, |_| String::new())
    } else {
        n.name.clone()
    }
}

pub fn is_path_holder(n: &Node) -> bool {
    last_qualified_segment(&n.qualified_name).contains('.')
}

/// `const x = { … }` in a JS-family file.
pub fn is_object_literal_owner(n: &Node) -> bool {
    matches!(n.kind, NodeKind::Constant | NodeKind::Variable)
        && is_js_family(n.language)
        && n.signature.as_deref().is_some_and(|s| re!(r"^=\s*\{").is_match(s))
}

memo_map!(OwnerOfMemo, String, Option<NodeRef>);
memo_map!(ThisCallerMemo, String, NodeRef);
memo_map!(ClassicScriptMemo, String, bool);
memo_map!(GlobalHolderMemo, String, bool);

/// The object literal a function member was declared inside.
pub fn object_literal_owner_of(n: &Node, ctx: &Ctx) -> Option<NodeRef> {
    if n.kind != NodeKind::Function || !is_js_family(n.language) {
        return None;
    }
    let cut = n.qualified_name.rfind("::").filter(|c| *c > 0)?;
    memoized::<OwnerOfMemo>(ctx, n.id.clone(), || {
        let parent = &n.qualified_name[..cut];
        ctx.get_nodes_by_qualified_name(parent)
            .iter()
            .find(|o| o.file_path == n.file_path && o.id != n.id && is_object_literal_owner(o) && range_within(n, o))
            .cloned()
    })
}

fn qualified_parent_of(n: &Node, ctx: &Ctx) -> Option<NodeRef> {
    let cut = n.qualified_name.rfind("::").filter(|c| *c > 0)?;
    ctx.get_nodes_by_qualified_name(&n.qualified_name[..cut]).iter().find(|p| p.file_path == n.file_path && p.id != n.id && range_within(n, p)).cloned()
}

fn is_arrow_member(n: &Node, ctx: &Ctx) -> bool {
    let line = line_at(ctx, &n.file_path, n.start_line).unwrap_or_default();
    re!(r"^(?:async\s*)?(?:[(<]|[A-Za-z_$][\w$]*\s*=>)").is_match(after_col(&line, n.start_column))
}

/// The function whose `this` a `this.x` reference means (arrow members and object literals defer to the enclosing scope).
pub fn this_scope_caller(caller: &NodeRef, ctx: &Ctx) -> NodeRef {
    if !is_js_family(caller.language) {
        return caller.clone();
    }
    memoized::<ThisCallerMemo>(ctx, caller.id.clone(), || {
        let mut cur = caller.clone();
        for _ in 0..8 {
            let owner = if is_object_literal_owner(&cur) {
                Some(cur.clone())
            } else {
                match object_literal_owner_of(&cur, ctx) {
                    Some(o) if is_arrow_member(&cur, ctx) => Some(o),
                    _ => None,
                }
            };
            let Some(owner) = owner else { break };
            let Some(up) = qualified_parent_of(&owner, ctx) else { break };
            cur = up;
        }
        cur
    })
}

pub fn this_object_of(caller: &NodeRef, ctx: &Ctx) -> Option<NodeRef> {
    let scope = this_scope_caller(caller, ctx);
    let owner = object_literal_owner_of(&scope, ctx)?;
    if is_arrow_member(&scope, ctx) {
        None
    } else {
        Some(owner)
    }
}

pub fn position_within(line: u32, column: u32, n: &Node) -> bool {
    let end = n.end_line;
    if line < n.start_line || line > end {
        return false;
    }
    if line == n.start_line && column < n.start_column {
        return false;
    }
    !(line == end && column >= n.end_column)
}

// -- Per-file JS scans ----------------------------------------------------------------------

pub struct JsFileScan {
    source: Option<Rc<str>>,
    code: RefCell<Option<Option<Rc<String>>>>,
    blocks: RefCell<Option<Option<Rc<JsBlockIndex>>>>,
    bindings: RefCell<HashMap<String, Rc<Vec<(i64, i64)>>>>,
    destructured: RefCell<Option<Rc<HashMap<String, std::collections::HashSet<String>>>>>,
}

type FileScans = Bounded<Rc<JsFileScan>, 32>;

fn js_file_scan(file_path: &str, ctx: &Ctx) -> Rc<JsFileScan> {
    ctx.memo::<FileScans>().get_or(file_path, true, || {
        Rc::new(JsFileScan {
            source: ctx.read_file(file_path),
            code: RefCell::new(None),
            blocks: RefCell::new(None),
            bindings: RefCell::new(HashMap::new()),
            destructured: RefCell::new(None),
        })
    })
}

/// The file with comments removed and string contents blanked (TypeScript flavor).
fn scanned_code(scan: &JsFileScan) -> Option<Rc<String>> {
    if let Some(c) = scan.code.borrow().as_ref() {
        return c.clone();
    }
    let code = scan.source.as_ref().map(|s| Rc::new(blank_string_contents(&strip_comments_for_regex(s, "typescript"), None)));
    *scan.code.borrow_mut() = Some(code.clone());
    code
}

pub fn masked_js_source(file_path: &str, ctx: &Ctx) -> Option<Rc<String>> {
    scanned_code(&js_file_scan(file_path, ctx))
}

pub struct JsBlockIndex {
    line_starts: Vec<usize>,
    events: Vec<usize>,
    open: Vec<i64>,
    close: HashMap<usize, usize>,
    length: usize,
}

fn read_js_blocks(code: &str) -> JsBlockIndex {
    let mut line_starts = vec![0usize];
    let mut events = Vec::new();
    let mut open = Vec::new();
    let mut close = HashMap::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut length = 0usize;
    for (i, unit) in code.encode_utf16().enumerate() {
        length = i + 1;
        if unit == 10 {
            line_starts.push(i + 1);
            continue;
        }
        if unit == 123 {
            stack.push(i);
        } else if unit == 125 {
            if let Some(at) = stack.pop() {
                close.insert(at, i);
            }
        } else {
            continue;
        }
        events.push(i);
        open.push(stack.last().map_or(-1, |&o| o as i64));
    }
    JsBlockIndex { line_starts, events, open, close, length }
}

pub fn js_block_index(file_path: &str, ctx: &Ctx) -> Option<Rc<JsBlockIndex>> {
    let scan = js_file_scan(file_path, ctx);
    if let Some(b) = scan.blocks.borrow().as_ref() {
        return b.clone();
    }
    let blocks = scanned_code(&scan).map(|code| Rc::new(read_js_blocks(&code)));
    *scan.blocks.borrow_mut() = Some(blocks.clone());
    blocks
}

fn js_offset(index: &JsBlockIndex, line: u32, column: u32) -> usize {
    index.line_starts.get((line as usize).wrapping_sub(1)).copied().unwrap_or(index.length) + column as usize
}

/// The innermost `{…}` block containing `offset`, as (open, close).
fn js_block_at(index: &JsBlockIndex, offset: usize) -> Option<(usize, usize)> {
    let mut lo: i64 = 0;
    let mut hi: i64 = index.events.len() as i64 - 1;
    let mut at: i64 = -1;
    while lo <= hi {
        let mid = (lo + hi) >> 1;
        if index.events[mid as usize] < offset {
            at = mid;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    let open = if at < 0 { -1 } else { index.open[at as usize] };
    if open < 0 {
        None
    } else {
        Some((open as usize, index.close.get(&(open as usize)).copied().unwrap_or(index.length)))
    }
}

/// Maps byte offsets of a string to UTF-16 unit offsets without re-scanning.
pub struct ByteUnits {
    table: Option<Vec<usize>>,
}

impl ByteUnits {
    pub fn new(text: &str) -> ByteUnits {
        if text.is_ascii() {
            return ByteUnits { table: None };
        }
        let mut table = vec![0usize; text.len() + 1];
        let mut units = 0usize;
        for (i, c) in text.char_indices() {
            for slot in table.iter_mut().skip(i).take(c.len_utf8()) {
                *slot = units;
            }
            units += c.len_utf16();
        }
        table[text.len()] = units;
        ByteUnits { table: Some(table) }
    }
    pub fn at(&self, byte: usize) -> usize {
        match &self.table {
            None => byte,
            Some(t) => t[byte.min(t.len() - 1)],
        }
    }
}

// -- Destructuring ---------------------------------------------------------------------------

/// `{ a, b: c, d = 1 }` → name bound → key read (`c → b`).
pub fn destructured_keys(pattern: &str) -> IndexMap<String, String> {
    let mut keys = IndexMap::new();
    for part in pattern.split(',') {
        let mut pieces = part.split(':').map(|x| re!(r"\s*=.*$").replace_first(js_trim(x), |_| String::new()));
        let k = pieces.next();
        let v = pieces.next();
        if let Some(k) = k {
            if is_js_ident(&k) {
                keys.insert(v.unwrap_or_else(|| k.clone()), k);
            }
        }
    }
    keys
}

fn destructures_member(file_path: &str, path: &str, member: &str, ctx: &Ctx) -> bool {
    let scan = js_file_scan(file_path, ctx);
    if scan.destructured.borrow().is_none() {
        let mut paths: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        let eligible = scan.source.as_ref().is_some_and(|s| re!(r"\}(?:\s|/\*(?:[^*]|\*(?!/))*\*/|//[^\n]*\n)*=(?!=)").is_match(s));
        let code = if eligible { scanned_code(&scan) } else { None };
        if let Some(code) = code {
            let off_path = re!(
                r"\{([^{}]*)\}\s*=\s*(?:(?:window|globalThis)\s*\.\s*)?([A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)(?![\w$])(?!\s*[.(\[])",
                "g"
            );
            for m in off_path.captures_iter(&code) {
                let own: Vec<String> = destructured_keys(m.text(1).unwrap_or("")).into_iter().filter(|(n, k)| n == k).map(|(n, _)| n).collect();
                if own.is_empty() {
                    continue;
                }
                let at = re!(r"\s+", "g").replace_all_str(m.text(2).unwrap_or(""), "");
                paths.entry(at).or_default().extend(own);
            }
        }
        *scan.destructured.borrow_mut() = Some(Rc::new(paths));
    }
    let hit = scan.destructured.borrow().as_ref().and_then(|p| p.get(path)).is_some_and(|s| s.contains(member));
    hit
}

/// A function declared as a member of an object literal is reachable by its bare name only from inside that object or through a destructuring of it.
pub fn is_object_member_reachable_by_name(n: &Node, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(owner) = object_literal_owner_of(n, ctx) else { return true };
    if r.file_path == n.file_path {
        if is_calls(r) && position_within(r.line, r.column, &owner) && bare_call_receiver(r, ctx).is_some_and(|b| b.receiver == "self") {
            if let Some(caller) = ctx.node_by_id(&r.from_node_id) {
                if this_object_of(&caller, ctx).is_some_and(|o| o.id == owner.id) {
                    return true;
                }
            }
        }
        if position_within(r.line, r.column, n) {
            let line = line_at(ctx, &n.file_path, n.start_line).unwrap_or_default();
            let own = Re::cached(&format!(r"^(?:async\s+)?function\s*\*?\s*{}(?![\w$])", escape_regex(&n.name)), "");
            if own.is_match(after_col(&line, n.start_column)) {
                return true;
            }
        }
    }
    destructures_member(&r.file_path, &holder_path(&owner), &n.name, ctx)
}

// -- Object literal members --------------------------------------------------------------------

pub enum ObjProp {
    /// The source could not be read as an object literal.
    Unknown,
    /// The literal has no such member.
    Absent,
    Present(PropInfo),
}

pub struct PropInfo {
    pub binding: Option<String>,
    start: i64,
    end: i64,
    container_line: u32,
    container_column: u32,
    lines: Rc<Vec<String>>,
}

impl PropInfo {
    fn contains(&self, node: &Node) -> bool {
        let mut result = node.start_column as i64 - self.container_column as i64;
        let mut line = self.container_line;
        while line < node.start_line {
            result += self.lines.get(line as usize - 1).map_or(0, |l| utf16_len(l)) as i64 + 1;
            line += 1;
        }
        result >= self.start && result < self.end
    }
}

/// The member of an object-literal constant that `member` names, when it is just an alias for another binding.
pub fn object_literal_member_binding(container: &Node, member: &str, ctx: &Ctx) -> Option<String> {
    match object_literal_property(container, member, ctx) {
        ObjProp::Present(p) => p.binding,
        _ => None,
    }
}

fn object_literal_property(container: &Node, member: &str, ctx: &Ctx) -> ObjProp {
    let Some(lines) = ctx.get_file_lines(&container.file_path) else { return ObjProp::Unknown };
    let from = container.start_line as usize - 1;
    let mut extent_lines: Vec<String> = lines.iter().skip(from).take((container.end_line as usize + 1).saturating_sub(container.start_line as usize)).cloned().collect();
    if extent_lines.is_empty() {
        return ObjProp::Unknown;
    }
    let last = extent_lines.len() - 1;
    extent_lines[last] = utf16_prefix(&extent_lines[last], container.end_column as usize).to_string();
    extent_lines[0] = utf16_from(&extent_lines[0], container.start_column as usize).to_string();
    let extent = strip_comments_for_regex(&extent_lines.join("\n"), "typescript");
    let code = blank_string_contents(&extent, None);
    let Some(open) = re!(r"^[^=]*=\s*(?:(?:Object\.(?:freeze|seal)\s*)?\(\s*)*\{").find(&code) else { return ObjProp::Unknown };
    let code_units: Vec<u16> = code.encode_utf16().collect();
    let extent_units: Vec<u16> = extent.encode_utf16().collect();
    let mut members: Vec<(usize, usize)> = Vec::new();
    let mut depth: i64 = 0;
    let mut start = utf16_len(open.as_str());
    let mut i = start;
    while i < code_units.len() {
        let ch = code_units[i];
        if ch == 123 || ch == 40 || ch == 91 {
            depth += 1;
        } else if ch == 41 || ch == 93 {
            depth -= 1;
        } else if ch == 125 {
            if depth == 0 {
                members.push((start, i));
                break;
            }
            depth -= 1;
        } else if ch == 44 && depth == 0 {
            members.push((start, i));
            start = i + 1;
        }
        i += 1;
    }
    let mut selected: Option<(usize, usize, Option<String>)> = None;
    for (s, e) in members {
        let text_full = String::from_utf16_lossy(&extent_units[s.min(extent_units.len())..e.min(extent_units.len())]);
        let text = js_trim(&text_full);
        if re!(r"^(?:\.\.\.|\[)").is_match(text) {
            selected = None;
            continue;
        }
        let Some(key) = re!(
            r#"^(?:(?:async|get|set)\s+)?\*?\s*(?:([A-Za-z_$][\w$]*)|['"]([^'"\\]*)['"])(?=\s*(?:[:(<,=]|$))"#
        )
        .captures(text) else {
            continue;
        };
        let name = key.text(1).or(key.text(2));
        if name != Some(member) {
            continue;
        }
        let value = js_trim(&text[key.get(0).unwrap().end()..]);
        let binding = if value.is_empty() {
            Some(member.to_string())
        } else {
            re!(r"^:\s*([A-Za-z_$][\w$]*)$").captures(value).and_then(|c| c.text(1).map(|s| s.to_string()))
        };
        selected = Some((s, e, binding));
    }
    match selected {
        None => ObjProp::Absent,
        Some((s, e, binding)) => ObjProp::Present(PropInfo {
            binding,
            start: s as i64,
            end: e as i64,
            container_line: container.start_line,
            container_column: container.start_column,
            lines: lines.clone(),
        }),
    }
}

/// The function/method/property of an object-literal constant that `member` names.
pub fn resolve_object_literal_member(
    container: &Node,
    member: &str,
    r: &UnresolvedRef,
    ctx: &Ctx,
    confidence: f64,
    resolved_by: ResolvedBy,
) -> Option<ResolvedRef> {
    if !matches!(container.kind, NodeKind::Constant | NodeKind::Variable) || !is_object_literal_language(container.language) {
        return None;
    }
    if !super::same_language_family(container.language, r.language) {
        return None;
    }
    let in_file = ctx.get_nodes_in_file(&container.file_path);
    let callable = |n: &Node| is_callable_kind(n.kind);
    let value_member = |n: &Node| callable(n) || matches!(n.kind, NodeKind::Property | NodeKind::Variable | NodeKind::Constant);
    let accepts_calls = is_calls(r);
    let inside: Vec<&NodeRef> = in_file.iter().filter(|n| n.id != container.id && range_within(n, container)).collect();
    let property = object_literal_property(container, member, ctx);
    let present = match &property {
        ObjProp::Absent => return None,
        ObjProp::Present(p) => {
            if p.binding.is_some() {
                return None;
            }
            Some(p)
        }
        ObjProp::Unknown => None,
    };
    let mut candidates: Vec<&NodeRef> = inside
        .iter()
        .copied()
        .filter(|n| n.name == member && (if accepts_calls { callable(n) } else { value_member(n) }) && present.map_or(true, |p| p.contains(n)))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let bodies: Vec<&&NodeRef> = inside.iter().filter(|n| callable(n)).collect();
    candidates.retain(|c| !bodies.iter().any(|b| b.id != c.id && !same_range(b, c) && range_within(c, b)));
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| {
        let ca = if callable(a) { 0 } else { 1 };
        let cb = if callable(b) { 0 } else { 1 };
        ca.cmp(&cb).then(a.start_line.cmp(&b.start_line)).then(a.start_column.cmp(&b.start_column))
    });
    Some(ResolvedRef::new(r, candidates[0].id.clone(), confidence, resolved_by))
}

/// `{ run }` / `{ run: runImpl }` — the member is an alias; find what the alias names in scope.
pub fn resolve_object_literal_binding(container: &Node, member: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let binding = object_literal_member_binding(container, member, ctx)?;
    let mut at = r.clone();
    at.file_path = container.file_path.clone();
    at.language = container.language;
    at.from_node_id = container.id.clone();
    at.line = container.start_line;
    at.column = container.start_column;
    let in_file = ctx.get_nodes_in_file(&container.file_path);
    let escaped = escape_regex(&binding);
    if in_file.iter().any(|n| {
        is_callable_kind(n.kind)
            && range_within(container, n)
            && n.signature.as_ref().is_some_and(|sig| super::js_store::has_parameter_binding(&format!("{sig} {{"), &escaped))
    }) {
        return None;
    }
    let lines = ctx.get_file_lines(&container.file_path)?;
    let code = blank_string_contents(&strip_comments_for_regex(&lines.join("\n"), "typescript"), None);
    let mut offsets = vec![0usize];
    let units: Vec<u16> = code.encode_utf16().collect();
    for (i, &u) in units.iter().enumerate() {
        if u == 10 {
            offsets.push(i + 1);
        }
    }
    let scope_at = |node: &Node| -> Vec<usize> {
        let end = offsets.get(node.start_line as usize - 1).copied().unwrap_or(units.len()) + node.start_column as usize;
        let mut scope = Vec::new();
        for (i, &u) in units.iter().enumerate().take(end) {
            if u == 123 {
                scope.push(i);
            } else if u == 125 {
                scope.pop();
            }
        }
        scope
    };
    let scope = scope_at(container);
    let callable = |n: &Node| matches!(n.kind, NodeKind::Function | NodeKind::Method | NodeKind::Class);
    let accepts = |n: &Node| {
        if is_calls(r) {
            callable(n)
        } else {
            callable(n) || matches!(n.kind, NodeKind::Constant | NodeKind::Variable | NodeKind::Component)
        }
    };
    let mut locals: Vec<(NodeRef, Vec<usize>)> = in_file
        .iter()
        .filter(|n| n.name == binding && n.id != container.id && matches!(n.kind, NodeKind::Function | NodeKind::Class | NodeKind::Constant | NodeKind::Variable | NodeKind::Component))
        .map(|n| (n.clone(), scope_at(n)))
        .filter(|(_, s)| s.iter().enumerate().all(|(i, p)| scope.get(i) == Some(p)))
        .collect();
    locals.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    if let Some((local, _)) = locals.first() {
        return if accepts(local) { Some(ResolvedRef::new(r, local.id.clone(), 0.85, ResolvedBy::InstanceMethod)) } else { None };
    }
    let mut probe = at.clone();
    probe.reference_name = binding.clone();
    let imported = crate::import_resolver::resolve_import(&probe, ctx);
    let target = imported.and_then(|i| ctx.node_by_id(&i.target_node_id));
    match target {
        Some(t) if accepts(&t) => Some(ResolvedRef::new(r, t.id.clone(), 0.85, ResolvedBy::InstanceMethod)),
        _ => None,
    }
}

// -- Binding scopes and holders ---------------------------------------------------------------

fn binding_scopes(name: &str, file_path: &str, ctx: &Ctx) -> Rc<Vec<(i64, i64)>> {
    let scan = js_file_scan(file_path, ctx);
    if let Some(hit) = scan.bindings.borrow().get(name) {
        return hit.clone();
    }
    let mut scopes: Vec<(i64, i64)> = Vec::new();
    let code = scanned_code(&scan);
    let index = js_block_index(file_path, ctx);
    if let (Some(code), Some(index)) = (code, index) {
        if code.contains(name) {
            let units = ByteUnits::new(&code);
            let code_len = utf16_len(&code) as i64;
            let code_units: Vec<u16> = code.encode_utf16().collect();
            let block_of = |at: usize| -> (i64, i64) { js_block_at(&index, at).map_or((-1, code_len), |(a, b)| (a as i64, b as i64)) };
            let body_after = |start: usize, end: usize| -> (i64, i64) {
                if end > 0 && code_units.get(end - 1) == Some(&123) {
                    return (end as i64 - 1, index.close.get(&(end - 1)).map_or(code_len, |&c| c as i64));
                }
                let mut i = end;
                while i < code_units.len() && char::from_u32(code_units[i] as u32).is_some_and(codegraph_types::jscompat::is_js_space) {
                    i += 1;
                }
                if code_units.get(i) == Some(&123) {
                    return (i as i64, index.close.get(&i).map_or(code_len, |&c| c as i64));
                }
                (start as i64, block_of(start).1)
            };
            let patterns = super::js_scope::local_binding_patterns(name, "");
            for m in patterns.decl.captures_iter(&code) {
                scopes.push(block_of(units.at(m.get(0).unwrap().start())));
            }
            for m in patterns.fn_.captures_iter(&code) {
                scopes.push(block_of(units.at(m.get(0).unwrap().start())));
            }
            for m in patterns.param.captures_iter(&code) {
                let g = m.get(0).unwrap();
                scopes.push(body_after(units.at(g.start()), units.at(g.end())));
            }
            let arrow = Re::cached(&format!(r"(?<![\w$.]){}\s*=>", escape_regex(name)), "");
            for m in arrow.captures_iter(&code) {
                let g = m.get(0).unwrap();
                scopes.push(body_after(units.at(g.start()), units.at(g.end())));
            }
        }
    }
    let scopes = Rc::new(scopes);
    scan.bindings.borrow_mut().insert(name.to_string(), scopes.clone());
    scopes
}

fn binds_name_at(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let Some(index) = js_block_index(&r.file_path, ctx) else { return false };
    let at = js_offset(&index, r.line, r.column) as i64;
    binding_scopes(name, &r.file_path, ctx).iter().any(|&(s, e)| at > s && at < e)
}

fn holder_scope_at(holder: &Node, r: &UnresolvedRef, via_global: bool, ctx: &Ctx) -> Option<i64> {
    let mut depth: i64 = -1;
    if !is_path_holder(holder) && is_js_ts(holder.language) {
        if let Some(index) = js_block_index(&holder.file_path, ctx) {
            if let Some(block) = js_block_at(&index, js_offset(&index, holder.start_line, holder.start_column)) {
                let at = js_offset(&index, r.line, r.column);
                if at <= block.0 || at >= block.1 {
                    return None;
                }
                depth = block.0 as i64;
            }
        }
    }
    let root = if via_global { None } else { holder_path(holder).split('.').next().map(|s| s.to_string()) };
    let local = root.as_deref().and_then(|root| js_function_local_scope(root, r, ctx));
    if let Some(local) = local {
        if !(holder.file_path == r.file_path && holder.start_line >= local.start && holder.start_line <= local.end) {
            return None;
        }
    }
    Some(depth)
}

fn is_classic_script(file_path: &str, ctx: &Ctx) -> bool {
    memoized::<ClassicScriptMemo>(ctx, file_path.to_string(), || {
        let Some(source) = ctx.read_file(file_path) else { return false };
        if re!(r"\.mjs$|\.cjs$", "i").is_match(file_path) || super::reach::has_cjs_export(&source) {
            return false;
        }
        let code = masked_js_source(file_path, ctx).map(|c| c.to_string()).unwrap_or_default();
        !super::reach::has_import_statement(&code) && !super::reach::has_esm_export(&code) && !re!(r"(?:^|[^\w$.])require\s*\(").is_match(&code)
    })
}

fn is_global_holder(n: &Node, ctx: &Ctx) -> bool {
    memoized::<GlobalHolderMemo>(ctx, n.id.clone(), || {
        if host_global_prefix(last_qualified_segment(&n.qualified_name)) {
            true
        } else if !is_js_ts(n.language) || !is_classic_script(&n.file_path, ctx) {
            false
        } else if !is_path_holder(n) {
            match js_block_index(&n.file_path, ctx) {
                Some(index) => js_block_at(&index, js_offset(&index, n.start_line, n.start_column)).is_none(),
                None => false,
            }
        } else {
            let hp = holder_path(n);
            let root = hp.split('.').next().unwrap_or("");
            ctx.get_nodes_by_name(root).iter().any(|r| {
                matches!(r.kind, NodeKind::Constant | NodeKind::Variable) && is_js_family(r.language) && holder_path(r) == root && r.id != n.id && is_global_holder(r, ctx)
            })
        }
    })
}

/// `path.member(...)`: the object literal registered under `path` that has `member`.
pub fn resolve_object_path_member(path: &str, member: &str, r: &UnresolvedRef, ctx: &Ctx, host: Option<&str>) -> Tri<ResolvedRef> {
    let via_global = host.is_some();
    let tail = &path[path.rfind('.').map_or(0, |i| i + 1)..];
    let named: Vec<NodeRef> = ctx
        .get_nodes_by_name(tail)
        .iter()
        .filter(|n| {
            matches!(n.kind, NodeKind::Constant | NodeKind::Variable)
                && is_object_literal_language(n.language)
                && super::same_language_family(n.language, r.language)
                && holder_path(n) == path
        })
        .cloned()
        .collect();
    if named.is_empty() {
        return Tri::Undefined;
    }
    let hit_on = |holder: &Node| -> Option<ResolvedRef> {
        resolve_object_literal_member(holder, member, r, ctx, 0.85, ResolvedBy::InstanceMethod).or_else(|| resolve_object_literal_binding(holder, member, r, ctx))
    };
    let mut local: Vec<(NodeRef, i64)> = named
        .iter()
        .filter(|n| n.file_path == r.file_path)
        .filter_map(|n| holder_scope_at(n, r, via_global, ctx).map(|d| (n.clone(), d)))
        .collect();
    local.sort_by(|a, b| b.1.cmp(&a.1));
    if !local.is_empty() {
        let top = local[0].1;
        for (n, _) in local.iter().filter(|e| e.1 == top) {
            if let Some(hit) = hit_on(n) {
                return Tri::Value(hit);
            }
        }
        return Tri::Null;
    }
    let hits: Vec<(NodeRef, ResolvedRef)> = named
        .iter()
        .filter(|n| n.file_path != r.file_path && is_object_literal_owner(n) && is_global_holder(n, ctx))
        .filter_map(|n| hit_on(n).map(|h| (n.clone(), h)))
        .collect();
    if hits.is_empty() {
        return Tri::Null;
    }
    let root = path.split('.').next().unwrap_or("");
    let shadowed = match host {
        Some(h) => binds_name_at(h, r, ctx),
        None => is_import_binding(root, r, ctx) || binds_name_at(root, r, ctx),
    };
    if shadowed {
        return Tri::Null;
    }
    let targets: std::collections::HashSet<&str> = hits.iter().map(|e| e.1.target_node_id.as_str()).collect();
    if targets.len() == 1 {
        let mut h = hits[0].1.clone();
        h.confidence = 0.8;
        return Tri::Value(h);
    }
    let dir = match r.file_path.rfind('/') {
        Some(i) => &r.file_path[..i + 1],
        None => "",
    };
    let near: Vec<&(NodeRef, ResolvedRef)> = hits.iter().filter(|e| e.0.file_path.starts_with(dir) && !e.0.file_path[dir.len()..].contains('/')).collect();
    let near_targets: std::collections::HashSet<&str> = near.iter().map(|e| e.1.target_node_id.as_str()).collect();
    if near_targets.len() == 1 {
        let mut h = near[0].1.clone();
        h.confidence = 0.75;
        Tri::Value(h)
    } else {
        Tri::Null
    }
}

/// `a.b.c()` / `window.a.b()` where `a.b` is an object literal registered somewhere.
pub fn match_object_path_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !is_calls(r) || !is_js_family(r.language) {
        return None;
    }
    let dot = r.reference_name.rfind('.').filter(|d| *d > 0)?;
    let mut path = r.reference_name[..dot].to_string();
    let host: Option<String> = if host_global_prefix(&format!("{path}.")) || re!(r"^(?:window|globalThis)\.").is_match(&path) {
        path.find('.').map(|i| path[..i].to_string())
    } else {
        None
    };
    if let Some(h) = &host {
        path = path[h.len() + 1..].to_string();
    }
    if !path.contains('.') && host.is_none() {
        return None;
    }
    resolve_object_path_member(&path, &r.reference_name[dot + 1..], r, ctx, host.as_deref()).value()
}

/// `this.name(...)` / `window.lib.name(...)` written with the receiver collapsed away.
pub fn match_collapsed_object_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !is_calls(r) || !is_js_family(r.language) || !is_js_ident(&r.reference_name) {
        return None;
    }
    if let Some(line) = line_at(ctx, &r.file_path, r.line) {
        if starts_with_at(&line, &r.reference_name, r.column) {
            let after = col_byte(&line, r.column) + r.reference_name.len();
            if re!(r"\s*\(", "y").captures_at(&line, after).is_some() {
                return None;
            }
        }
    }
    let written = bare_call_receiver(r, ctx)?;
    if written.receiver == "self" {
        let caller = ctx.node_by_id(&r.from_node_id)?;
        let owner = this_object_of(&caller, ctx)?;
        return resolve_object_literal_member(&owner, &r.reference_name, r, ctx, 0.85, ResolvedBy::InstanceMethod);
    }
    let host = re!(r"(?:^|[^\w$.])(window|globalThis)\s*\??\.\s*([A-Za-z_$][\w$]*(?:\s*\??\.\s*[A-Za-z_$][\w$]*)*)$").captures(&written.receiver)?;
    let path = re!(r"[\s?]", "g").replace_all_str(host.text(2)?, "");
    resolve_object_path_member(&path, &r.reference_name, r, ctx, host.text(1)).value()
}

// -- ES class inheritance ----------------------------------------------------------------------

pub fn es_inherited_method(type_name: &str, method_name: &str, file: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<NodeRef> {
    let decls: Vec<NodeRef> =
        ctx.get_nodes_by_name(type_name).iter().filter(|n| n.file_path == file && matches!(n.kind, NodeKind::Class | NodeKind::Interface)).cloned().collect();
    let mut seen: std::collections::HashSet<String> = decls.iter().map(|n| n.id.clone()).collect();
    fn inherited(decl: &Node, depth: u32, method_name: &str, r: &UnresolvedRef, ctx: &Ctx, seen: &mut std::collections::HashSet<String>) -> Option<NodeRef> {
        for base in ctx.get_supertype_nodes(&decl.id).iter() {
            if seen.contains(&base.id) {
                continue;
            }
            seen.insert(base.id.clone());
            let members = ctx.get_method_matches(&base.name, method_name, r.language);
            if let Some(own) = members.iter().find(|m| m.file_path == base.file_path && m.start_line >= base.start_line && m.end_line <= base.end_line) {
                return Some(own.clone());
            }
            if depth < 3 {
                if let Some(above) = inherited(base, depth + 1, method_name, r, ctx, seen) {
                    return Some(above);
                }
            }
        }
        None
    }
    for decl in &decls {
        if let Some(found) = inherited(decl, 0, method_name, r, ctx, &mut seen) {
            return Some(found);
        }
    }
    None
}

pub fn es_class_supertypes(type_name: &str, language: Language, ctx: &Ctx) -> Vec<String> {
    let mut names: indexmap::IndexSet<String> = indexmap::IndexSet::new();
    for n in ctx.get_nodes_by_name(type_name).iter() {
        if n.language != language || n.kind != NodeKind::Class {
            continue;
        }
        for s in ctx.get_supertype_nodes(&n.id).iter() {
            if !s.name.is_empty() && s.name != type_name {
                names.insert(s.name.clone());
            }
        }
    }
    names.into_iter().collect()
}
