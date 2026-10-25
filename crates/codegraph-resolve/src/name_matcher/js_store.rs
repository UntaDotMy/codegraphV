//! Calls through store accessors and destructured results — `useStore.getState().act()`,
//! `const { act } = useStore.getState(); act()`, `const { run } = makeApi(); run()` — and the
//! scope checks that keep a local re-declaration from being mistaken for the binding.

use super::calls::is_bare_js_call;
use super::object_literal::{destructured_keys, resolve_object_literal_member, ByteUnits};
use super::reach::is_lexically_reachable;
use super::util::*;
use crate::ctx::Ctx;
use crate::types::{ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_extract::strip_comments::{blank_string_contents, strip_comments_for_regex};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_prefix};
use codegraph_types::{EdgeKind, Node, NodeKind, ReferenceKind};
use std::collections::HashMap;
use std::rc::Rc;

memo_map!(GetStateFileMemo, String, bool);
memo_map!(SelectorNamesMemo, String, Rc<std::collections::HashSet<String>>);

/// `foo.bar.baz(...)` — a member call through at least two dots that no earlier strategy resolved.
pub fn is_unresolved_js_member_call(r: &UnresolvedRef) -> bool {
    is_calls(r)
        && is_js_ts(r.language)
        && !re!(r"^(?:this|window)\.").is_match(&r.reference_name)
        && re!(r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*){2,}$").is_match(&r.reference_name)
}

/// The smallest function/method line a reference sits inside (1 at file level).
pub fn enclosing_scope_start_line(r: &UnresolvedRef, ctx: &Ctx) -> u32 {
    let mut start = 1;
    for n in ctx.get_nodes_in_file(&r.file_path).iter() {
        if !matches!(n.kind, NodeKind::Function | NodeKind::Method) || n.language != r.language {
            continue;
        }
        if n.start_line <= r.line && n.end_line >= r.line && n.start_line >= start {
            start = n.start_line;
        }
    }
    start
}

/// `lines.slice(from, line - 1)` plus the head of the reference's own line, joined with newlines.
fn text_before(source: &str, from_line: usize, line: u32, column: u32) -> String {
    let lines: Vec<&str> = source.split('\n').collect();
    let to = (line as usize).saturating_sub(1);
    let mut parts: Vec<&str> = if from_line < to { lines[from_line.min(lines.len())..to.min(lines.len())].to_vec() } else { Vec::new() };
    let own = lines.get(to).map_or("", |l| utf16_prefix(l, column as usize));
    parts.push(own);
    parts.join("\n")
}

fn masked(text: &str) -> String {
    blank_string_contents(&strip_comments_for_regex(text, "typescript"), None)
}

/// The stack of open `{` positions (UTF-16) before `end`.
fn stack_at(code: &[u16], end: usize) -> Vec<usize> {
    let mut stack = Vec::new();
    for (i, &c) in code.iter().enumerate().take(end) {
        if c == 123 {
            stack.push(i);
        } else if c == 125 {
            stack.pop();
        }
    }
    stack
}

/// `stackAt(at).every((pos, i) => scope[i] === pos)` — the position's open blocks all enclose the call.
fn encloses(code: &[u16], at: usize, call_scope: &[usize]) -> bool {
    stack_at(code, at).iter().enumerate().all(|(i, p)| call_scope.get(i) == Some(p))
}

/// A parameter list `(…name…) =>` / `(…name…) {` or an arrow `name =>` in `code` binds `escaped_name`.
pub fn has_parameter_binding(code: &str, escaped_name: &str) -> bool {
    let name = Re::cached(&format!(r"(?<![\w$]){escaped_name}(?![\w$])"), "");
    if Re::cached(&format!(r"(?<![\w$]){escaped_name}\s*=>"), "").is_match(code) {
        return true;
    }
    let bytes = code.as_bytes();
    let keyword = re!(r"\b(?:if|while|for|switch|with)\s*$");
    for i in 0..bytes.len() {
        if bytes[i] != b'(' || keyword.is_match(&code[..i]) {
            continue;
        }
        let mut depth = 1;
        let mut j = i + 1;
        while j < bytes.len() && depth != 0 {
            if bytes[j] == b'(' {
                depth += 1;
            } else if bytes[j] == b')' {
                depth -= 1;
            }
            j += 1;
        }
        if depth == 0 && name.is_match(&code[i + 1..j - 1]) && re!(r"^\s*(?::[^=;{]*)?(?:=>|\{)").is_match(&code[j..]) {
            return true;
        }
    }
    false
}

/// A function's parameter, or a `const`/`function`/`class` declaration in an enclosing block, rebinds `name` where `r` sits.
pub fn import_shadowed_at(name: &str, r: &UnresolvedRef, ctx: &Ctx) -> bool {
    let escaped = escape_regex(name);
    for f in ctx.get_nodes_in_file(&r.file_path).iter() {
        if matches!(f.kind, NodeKind::Function | NodeKind::Method)
            && f.start_line <= r.line
            && f.end_line >= r.line
            && f.signature.as_ref().is_some_and(|sig| has_parameter_binding(&format!("{sig} {{"), &escaped))
        {
            return true;
        }
    }
    let source = ctx.read_file(&r.file_path).map(|s| s.to_string()).unwrap_or_default();
    let code = masked(&text_before(&source, 0, r.line, r.column));
    let units: Vec<u16> = code.encode_utf16().collect();
    let bu = ByteUnits::new(&code);
    let scope = stack_at(&units, units.len());
    let declarations =
        Re::cached(&format!(r"\b(?:const|let|var|function|class)\s+(?:{escaped}(?![\w$])|\{{[^}}]*(?<![\w$]){escaped}(?![\w$]))"), "g");
    declarations.captures_iter(&code).any(|m| encloses(&units, bu.at(m.get(0).unwrap().start()), &scope))
}

fn redeclares(rest: &str, escaped: &str) -> bool {
    Re::cached(&format!(r"\b(?:const|let|var|function|class)\s+(?:{escaped}\b|\{{[^}}]*\b{escaped}\b)"), "").is_match(rest)
}

// -- Store accessors ---------------------------------------------------------------------------

/// `useStore.getState().action()` / `get().action()` written as one dotted reference.
pub fn match_store_accessor_chain(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let m = re!(r"^([\w$.]+)\(\)\.(\w+)$").captures(&r.reference_name)?;
    let inner = m.text(1).filter(|s| !s.is_empty())?;
    let method = m.text(2).filter(|s| !s.is_empty())?;
    if !(inner == "get" || inner == "getState" || inner.ends_with(".getState")) {
        return None;
    }
    if is_js_family(r.language) {
        return resolve_store_action(inner, method, r, ctx, false);
    }
    let callables: Vec<_> = ctx
        .get_nodes_by_name(method)
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method) && super::same_language_family(n.language, r.language) && n.id != r.from_node_id)
        .cloned()
        .collect();
    if callables.len() != 1 {
        return None;
    }
    Some(ResolvedRef::new(r, callables[0].id.clone(), 0.6, ResolvedBy::ExactMatch))
}

/// The store object literal behind `inner` (`get`/`getState`/`<store>.getState`) and its `member` action.
pub fn resolve_store_action(inner: &str, member: &str, r: &UnresolvedRef, ctx: &Ctx, selector: bool) -> Option<ResolvedRef> {
    let holders: Vec<Rc<Node>> = if inner == "get" || inner == "getState" {
        let caller = ctx.node_by_id(&r.from_node_id)?;
        let pattern = Re::cached(&format!(r"\(\s*[\w$]+\s*,\s*{inner}\s*(?:,\s*[\w$]+\s*)?\)\s*=>"), "");
        ctx.get_nodes_in_file(&r.file_path)
            .iter()
            .filter(|n| {
                if !matches!(n.kind, NodeKind::Constant | NodeKind::Variable) || !range_within(&caller, n) {
                    return false;
                }
                let source = ctx
                    .read_file(&n.file_path)
                    .map(|s| {
                        let lines: Vec<&str> = s.split('\n').collect();
                        let from = (n.start_line as usize - 1).min(lines.len());
                        let to = (caller.start_line as usize).min(lines.len()).max(from);
                        lines[from..to].join("\n")
                    })
                    .unwrap_or_default();
                pattern.is_match(&source)
            })
            .cloned()
            .collect()
    } else {
        let name = &inner[..inner.len() - ".getState".len()];
        if !re!(r"^[\w$]+$").is_match(name) {
            return None;
        }
        let mut probe = r.clone();
        probe.reference_name = name.to_string();
        probe.reference_kind = ReferenceKind::Edge(EdgeKind::References);
        let node = crate::import_resolver::resolve_import(&probe, ctx).and_then(|i| ctx.node_by_id(&i.target_node_id));
        if node.is_some() && import_shadowed_at(name, r, ctx) {
            return None;
        }
        match node {
            Some(n) => vec![n],
            None => ctx.get_nodes_by_name(name).iter().filter(|n| n.file_path == r.file_path && is_lexically_reachable(n, r, ctx)).cloned().collect(),
        }
    };
    if holders.len() != 1 {
        return None;
    }
    let holder = &holders[0];
    if selector {
        let text = ctx
            .read_file(&holder.file_path)
            .map(|s| {
                let lines: Vec<&str> = s.split('\n').collect();
                let from = (holder.start_line as usize - 1).min(lines.len());
                let to = (holder.end_line as usize).min(lines.len()).max(from);
                lines[from..to].join("\n")
            })
            .unwrap_or_default();
        let factory = Re::cached(&format!(r"\b(?:const|let)\s+{}\s*=\s*([\w$]+)\s*[<(]", escape_regex(&holder.name)), "")
            .captures(&text)
            .and_then(|c| c.text(1).map(|s| s.to_string()));
        let ok = factory.is_some_and(|f| {
            ctx.get_import_mappings(&holder.file_path, holder.language).iter().any(|m| m.local_name == f && m.source == "zustand" && (m.exported_name == "create" || m.is_default))
        });
        if !ok {
            return None;
        }
    }
    resolve_object_literal_member(holder, member, r, ctx, 0.9, ResolvedBy::InstanceMethod)
}

/// `const { act } = useStore.getState(); act()`.
fn match_destructured_store_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let eligible = memoized::<GetStateFileMemo>(ctx, r.file_path.clone(), || ctx.read_file(&r.file_path).is_some_and(|s| s.contains(".getState")));
    if !eligible {
        return None;
    }
    let source = ctx.read_file(&r.file_path)?;
    let start = enclosing_scope_start_line(r, ctx) as usize - 1;
    let code = masked(&text_before(&source, start, r.line, r.column));
    let units: Vec<u16> = code.encode_utf16().collect();
    let bu = ByteUnits::new(&code);
    let name = escape_regex(&r.reference_name);
    let binding = re!(r"\bconst\s*\{([^{}]*)\}\s*=\s*([\w$]+)\.getState\s*\(\s*\)", "g");
    let call_scope = stack_at(&units, units.len());
    let found: Vec<_> = binding.captures_iter(&code).collect();
    for m in found.iter().rev() {
        if !m.text(1).unwrap_or("").split(',').any(|part| js_trim(part) == r.reference_name) {
            continue;
        }
        let g = m.get(0).unwrap();
        if !encloses(&units, bu.at(g.start()), &call_scope) {
            continue;
        }
        if redeclares(&code[g.end()..], &name) {
            return None;
        }
        return resolve_store_action(&format!("{}.getState", m.text(2).unwrap_or("")), &r.reference_name, r, ctx, false);
    }
    None
}

/// `const pick = useStore((s) => s.pick); pick()` — a selector that reads one action off the store.
fn match_selected_store_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let source = ctx.read_file(&r.file_path)?;
    if !source.contains("=>") {
        return None;
    }
    let names = memoized::<SelectorNamesMemo>(ctx, r.file_path.clone(), || {
        Rc::new(
            re!(r"\bconst\s+([\w$]+)\s*=\s*[\w$]+\s*\(\s*(?:\(\s*[\w$]+\s*\)|[\w$]+)\s*=>", "g")
                .captures_iter(&source)
                .filter_map(|m| m.text(1).map(|s| s.to_string()))
                .collect(),
        )
    });
    if !names.contains(&r.reference_name) {
        return None;
    }
    let name = escape_regex(&r.reference_name);
    let code = masked(&text_before(&source, 0, r.line, r.column));
    let units: Vec<u16> = code.encode_utf16().collect();
    let bu = ByteUnits::new(&code);
    let binding = Re::cached(
        &format!(r"\bconst\s+{name}\s*=\s*([\w$]+)\s*\(\s*(?:\(\s*([\w$]+)\s*\)|([\w$]+))\s*=>\s*([\w$]+)\.([\w$]+)\s*\)"),
        "g",
    );
    let call_scope = stack_at(&units, units.len());
    let found: Vec<_> = binding.captures_iter(&code).collect();
    for m in found.iter().rev() {
        if m.text(2).or(m.text(3)) != m.text(4) {
            continue;
        }
        let g = m.get(0).unwrap();
        if !encloses(&units, bu.at(g.start()), &call_scope) {
            continue;
        }
        let rest = &code[g.end()..];
        if redeclares(rest, &name) || has_parameter_binding(rest, &name) {
            return None;
        }
        return resolve_store_action(&format!("{}.getState", m.text(1).unwrap_or("")), m.text(5).unwrap_or(""), r, ctx, true);
    }
    None
}

/// A bare call to a name that a store destructuring or selector bound.
pub fn match_js_store_binding_call(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    if !is_bare_js_call(r, ctx) {
        return None;
    }
    match_destructured_store_call(r, ctx).or_else(|| match_selected_store_call(r, ctx))
}

// -- Destructured call results -----------------------------------------------------------------------

struct DestructuredBinding {
    end: usize,
    callee: String,
    depth: i64,
    scope_end: f64,
}

pub struct DestructuredFile {
    code: Vec<u16>,
    source: Vec<u16>,
    line_starts: Vec<usize>,
    bindings: Vec<DestructuredBinding>,
    by_name: HashMap<String, Vec<(usize, String)>>,
    literals: Vec<usize>,
    literal_depths: Vec<i64>,
}

type DestructuredFiles = Bounded<Option<Rc<DestructuredFile>>, 16>;

fn unit_str(units: &[u16], from: usize, to: usize) -> String {
    String::from_utf16_lossy(&units[from.min(units.len())..to.min(units.len()).max(from.min(units.len()))])
}

fn destructured_binding_regex() -> &'static Re {
    re!(r"\b(?:const|let|var)\s*\{([^{}]*)\}\s*=\s*(?:await\s+)?([A-Za-z_$][\w$]*)\s*(?:<[^<>()]*>)?\s*\(", "g")
}

fn destructured_bindings(file_path: &str, source: &str, ctx: &Ctx) -> Option<Rc<DestructuredFile>> {
    ctx.memo::<DestructuredFiles>().get_or(file_path, false, || {
        if !re!(r"\b(?:const|let|var)\s*\{").is_match(source) {
            return None;
        }
        Some(Rc::new(read_destructured_bindings(source)))
    })
}

fn read_destructured_bindings(source: &str) -> DestructuredFile {
    let mut literals: Vec<usize> = Vec::new();
    let code_text = blank_string_contents(&strip_comments_for_regex(source, "typescript"), Some(&mut literals));
    let code: Vec<u16> = code_text.encode_utf16().collect();
    let source_units: Vec<u16> = source.encode_utf16().collect();
    let mut line_starts = vec![0usize];
    for (i, &u) in source_units.iter().enumerate() {
        if u == 10 {
            line_starts.push(i + 1);
        }
    }
    let bu = ByteUnits::new(&code_text);
    struct Found {
        index: usize,
        binding: usize,
        keys: indexmap::IndexMap<String, String>,
    }
    let mut bindings: Vec<DestructuredBinding> = Vec::new();
    let mut found: Vec<Found> = Vec::new();
    for m in destructured_binding_regex().captures_iter(&code_text) {
        let g = m.get(0).unwrap();
        let index = bu.at(g.start());
        bindings.push(DestructuredBinding { end: bu.at(g.end()), callee: m.text(2).unwrap_or("").to_string(), depth: -1, scope_end: f64::INFINITY });
        found.push(Found { index, binding: bindings.len() - 1, keys: destructured_keys(m.text(1).unwrap_or("")) });
    }
    let patterns: std::collections::HashSet<usize> =
        re!(r"\b(?:const|let|var)\s*\{", "g").captures_iter(&code_text).map(|m| bu.at(m.get(0).unwrap().end()) - 1).collect();
    let mut literal_depths: Vec<i64> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut scoped: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut last_brace: i64 = -1;
    let mut last_bracket: i64 = -1;
    let (mut next, mut literal) = (0usize, 0usize);
    for i in 0..code.len() {
        if found.get(next).is_some_and(|f| f.index == i) {
            let b = found[next].binding;
            next += 1;
            bindings[b].depth = stack.len() as i64 - 1;
            if let Some(&open) = stack.get(bindings[b].depth as usize) {
                scoped.entry(open).or_default().push(b);
            }
        }
        if literals.get(2 * literal) == Some(&i) {
            let after_pattern = last_brace >= 0 && patterns.contains(&(last_brace as usize));
            let after_angle = last_bracket >= 0 && code.get(last_bracket as usize) == Some(&60);
            literal_depths.push(if after_pattern || after_angle { -1 } else { stack.len() as i64 });
            literal += 1;
        }
        let c = code[i];
        if c == 123 {
            stack.push(i);
            last_brace = i as i64;
        } else if c == 125 {
            last_brace = i as i64;
            if let Some(open) = stack.pop() {
                if let Some(list) = scoped.get(&open) {
                    for &b in list {
                        bindings[b].scope_end = i as f64;
                    }
                }
            }
        } else if c == 60 || c == 62 || c == 40 || c == 41 {
            last_bracket = i as i64;
        }
    }
    let mut by_name: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for f in &found {
        for (name, key) in &f.keys {
            by_name.entry(name.clone()).or_default().push((f.binding, key.clone()));
        }
    }
    DestructuredFile { code, source: source_units, line_starts, bindings, by_name, literals, literal_depths }
}

/// The offset of the reference in the file, or -1 when it sits right after a comment opener.
fn destructured_call_offset(file: &DestructuredFile, r: &UnresolvedRef) -> i64 {
    let starts = &file.line_starts;
    let (line, column) = (r.line as usize, r.column as usize);
    if line < 1 || line > starts.len() {
        return -1;
    }
    let limit = if line < starts.len() { starts[line] as i64 - 1 } else { file.source.len() as i64 };
    let cut = ((starts[line - 1] + column) as i64).min(limit);
    let at = |i: i64| -> Option<u16> { if i < 0 { None } else { file.source.get(i as usize).copied() } };
    if at(cut - 1) == Some(47) && (at(cut) == Some(47) || at(cut) == Some(42)) {
        -1
    } else {
        cut
    }
}

/// The regex literal (by index) containing `cut`, or -1.
fn regex_literal_around(literals: &[usize], cut: usize) -> i64 {
    let (mut lo, mut hi) = (0usize, literals.len() / 2);
    while lo < hi {
        let mid = (lo + hi) >> 1;
        if literals[2 * mid] < cut {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo > 0 && literals[2 * lo - 1] >= cut {
        lo as i64 - 1
    } else {
        -1
    }
}

struct BlocksThrough {
    enclosing: i64,
    opened: Vec<usize>,
}

fn blocks_through(text: &str, enclosing: i64) -> BlocksThrough {
    let mut enclosing = enclosing;
    let mut opened: Vec<usize> = Vec::new();
    for (i, u) in text.encode_utf16().enumerate() {
        if u == 123 {
            opened.push(i);
        } else if u == 125 {
            if !opened.is_empty() {
                opened.pop();
            } else if enclosing > 0 {
                enclosing -= 1;
            }
        }
    }
    BlocksThrough { enclosing, opened }
}

/// A bare call to a name destructured from another call's result (`const { a } = make(); a()`).
pub fn match_destructured_call_result(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let source = ctx.read_file(&r.file_path)?;
    let file = destructured_bindings(&r.file_path, &source, ctx)?;
    let cut = destructured_call_offset(&file, r);
    if cut < 0 {
        return match_destructured_call_above(r, &source, ctx);
    }
    let cut = cut as usize;
    let literal = regex_literal_around(&file.literals, cut);
    if literal >= 0 {
        let depth = file.literal_depths.get(literal as usize).copied().unwrap_or(-1);
        return if depth < 0 {
            match_destructured_call_above(r, &source, ctx)
        } else {
            match_destructured_call_in_literal(r, &file, file.literals[2 * literal as usize], depth, cut, ctx)
        };
    }
    let empty = Vec::new();
    let bindings = file.by_name.get(&r.reference_name).unwrap_or(&empty);
    for (b, key) in bindings.iter().rev() {
        let binding = &file.bindings[*b];
        if binding.end > cut || binding.scope_end < cut as f64 {
            continue;
        }
        return destructured_call_target(&binding.callee, key, &unit_str(&file.code, binding.end, cut), r, ctx);
    }
    None
}

fn match_destructured_call_in_literal(
    r: &UnresolvedRef,
    file: &DestructuredFile,
    start: usize,
    depth: i64,
    cut: usize,
    ctx: &Ctx,
) -> Option<ResolvedRef> {
    let tail = blank_string_contents(&unit_str(&file.code, start, cut), None);
    let at_call = blocks_through(&tail, depth);
    let found: Vec<_> = destructured_binding_regex().captures_iter(&tail).collect();
    for m in found.iter().rev() {
        let keys = destructured_keys(m.text(1).unwrap_or(""));
        let Some(key) = keys.get(&r.reference_name).filter(|k| !k.is_empty()) else { continue };
        let g = m.get(0).unwrap();
        let at_binding = blocks_through(&tail[..g.start()], depth);
        let block = at_binding.opened.last().copied();
        let skip = match block {
            Some(b) => !at_call.opened.contains(&b),
            None => at_binding.enclosing > at_call.enclosing,
        };
        if skip {
            continue;
        }
        return destructured_call_target(m.text(2).unwrap_or(""), key, &tail[g.end()..], r, ctx);
    }
    let empty = Vec::new();
    let bindings = file.by_name.get(&r.reference_name).unwrap_or(&empty);
    for (b, key) in bindings.iter().rev() {
        let binding = &file.bindings[*b];
        if binding.end > start {
            continue;
        }
        if binding.depth >= 0 && (binding.scope_end < start as f64 || binding.depth >= at_call.enclosing) {
            continue;
        }
        let rest = format!("{}{}", unit_str(&file.code, binding.end, start), tail);
        return destructured_call_target(&binding.callee, key, &rest, r, ctx);
    }
    None
}

fn match_destructured_call_above(r: &UnresolvedRef, source: &str, ctx: &Ctx) -> Option<ResolvedRef> {
    let code = masked(&text_before(source, 0, r.line, r.column));
    let units: Vec<u16> = code.encode_utf16().collect();
    let bu = ByteUnits::new(&code);
    let call_scope = stack_at(&units, units.len());
    let found: Vec<_> = destructured_binding_regex().captures_iter(&code).collect();
    for m in found.iter().rev() {
        let keys = destructured_keys(m.text(1).unwrap_or(""));
        let Some(key) = keys.get(&r.reference_name).filter(|k| !k.is_empty()) else { continue };
        let g = m.get(0).unwrap();
        if !encloses(&units, bu.at(g.start()), &call_scope) {
            continue;
        }
        return destructured_call_target(m.text(2).unwrap_or(""), key, &code[g.end()..], r, ctx);
    }
    None
}

fn destructured_call_target(callee: &str, key: &str, rest: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let target = destructured_key_target(callee, key, r, ctx)?;
    if redeclares(rest, &escape_regex(&r.reference_name)) {
        None
    } else {
        Some(target)
    }
}

/// The inner function `key` that `callee()` returns in its `return { key }` object.
fn destructured_key_target(callee_name: &str, key: &str, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    let mut probe = r.clone();
    probe.reference_name = callee_name.to_string();
    probe.reference_kind = ReferenceKind::Edge(EdgeKind::Calls);
    let imported = crate::import_resolver::resolve_import(&probe, ctx);
    let holders: Vec<Rc<Node>> = ctx
        .get_nodes_by_name(callee_name)
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Constant | NodeKind::Variable) && super::same_language_family(n.language, r.language))
        .cloned()
        .collect();
    let callee = imported
        .and_then(|i| ctx.node_by_id(&i.target_node_id))
        .or_else(|| holders.iter().find(|n| n.file_path == r.file_path).cloned())
        .or_else(|| if holders.len() == 1 { Some(holders[0].clone()) } else { None })?;
    if !super::same_language_family(callee.language, r.language) {
        return None;
    }
    let callee_text = {
        let lines = lines_of(ctx, &callee.file_path);
        let from = (callee.start_line as usize - 1).min(lines.len());
        let to = (callee.end_line as usize).min(lines.len()).max(from);
        lines[from..to].join("\n")
    };
    if !Re::cached(&format!(r"\breturn\s*\{{[\s\S]*?\b{}\b", escape_regex(key)), "").is_match(&callee_text) {
        return None;
    }
    let callable = |n: &Node| matches!(n.kind, NodeKind::Function | NodeKind::Method | NodeKind::Constant | NodeKind::Variable);
    let in_file = ctx.get_nodes_in_file(&callee.file_path);
    let is_fn = |f: &Node| matches!(f.kind, NodeKind::Function | NodeKind::Method);
    let inner: Vec<Rc<Node>> = in_file
        .iter()
        .filter(|n| {
            n.name == key
                && callable(n)
                && n.id != callee.id
                && range_within(n, &callee)
                && !in_file.iter().any(|f| f.id != callee.id && f.id != n.id && is_fn(f) && range_within(f, &callee) && range_within(n, f) && !same_range(f, n))
        })
        .cloned()
        .collect();
    let mut top: Vec<Rc<Node>> = if !inner.is_empty() {
        inner
    } else {
        in_file
            .iter()
            .filter(|n| {
                n.name == key
                    && callable(n)
                    && !n.qualified_name.contains("::")
                    && !in_file.iter().any(|f| is_fn(f) && f.id != n.id && range_within(n, f) && !same_range(f, n))
            })
            .cloned()
            .collect()
    };
    top.sort_by_key(|n| if n.kind == NodeKind::Function { 0 } else { 1 });
    let target = top.first()?;
    Some(ResolvedRef::new(r, target.id.clone(), 0.85, ResolvedBy::InstanceMethod))
}
