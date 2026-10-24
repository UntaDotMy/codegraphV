//! Framework event / job / behaviour dispatch keyed by a type or class name: Spring application
//! events, MediatR, Sidekiq, Erlang behaviours, Nix module options, Laravel events. Port of the
//! matching section of `src/resolution/callback-synthesizer.ts`.

use super::common::*;
use crate::ctx::Ctx;
use crate::meta;
use crate::synth_utils::enclosing_fn;
use crate::types::NodeRef;
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim};
use codegraph_types::{Edge, EdgeKind, Language, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

// ── Spring application events (Java) ──────────────────────────────────────────

const SPRING_FANOUT_CAP: usize = 80;

/// The first parameter's type from a Java method `signature` (`"void (XEvent e)"` → `XEvent`).
fn spring_first_param_type(sig: Option<&str>) -> Option<String> {
    let sig = sig.filter(|s| !s.is_empty())?;
    let open = sig.find('(')?;
    let close = sig[open..].find(')').map(|c| c + open);
    let inner = js_trim(&sig[open + 1..close.unwrap_or(sig.len())]);
    if inner.is_empty() {
        return None;
    }
    let first = js_trim(inner.split(',').next().unwrap_or(""));
    let toks: Vec<&str> =
        re!(r"\s+").split(first).into_iter().filter(|t| !t.is_empty() && *t != "final" && !t.starts_with('@')).collect();
    if toks.len() < 2 {
        return None; // need `Type name`
    }
    let ty = re!(r"<.*$").replace_first(toks[toks.len() - 2], |_| String::new()); // drop generic args
    if re!(r"^[A-Z][A-Za-z0-9_]*$").is_match(&ty) {
        Some(ty)
    } else {
        None
    }
}

pub fn spring_event_edges(ctx: &Ctx) -> Vec<Edge> {
    let anno_re = re!(r"@(?:EventListener|TransactionalEventListener)\b");
    let anno_type_re = re!(r"@(?:EventListener|TransactionalEventListener)\s*\(\s*([A-Z][A-Za-z0-9_]*)\.class");
    let app_listener_re = re!(r"\bApplicationListener\s*<");
    let java_ext = re!(r"\.java$");
    // Pass 1 — event-type → listener methods, scanning only event-relevant files. Publisher files
    // are recorded here so pass 2 re-reads just those.
    let mut listeners: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
    let mut publisher_files: Vec<String> = Vec::new();
    for file in ctx.get_all_files() {
        if !java_ext.is_match(&file) {
            continue;
        }
        let Some(content) = ctx.read_file(&file) else { continue };
        if content.is_empty() {
            continue;
        }
        if content.contains(".publishEvent(") {
            publisher_files.push(file.clone());
        }
        let has_anno = content.contains("@EventListener") || content.contains("@TransactionalEventListener");
        let has_app_listener = app_listener_re.is_match(&content);
        if !has_anno && !has_app_listener {
            continue;
        }
        let lines: Vec<&str> = content.split('\n').collect();
        for node in ctx.get_nodes_in_file(&file).iter() {
            if node.kind != NodeKind::Method {
                continue;
            }
            // Collect this method's own leading annotation block (consecutive `@`-lines from
            // startLine).
            let mut anno_lines: Vec<&str> = Vec::new();
            let mut i = (node.start_line as usize).wrapping_sub(1);
            while i < lines.len() && i < node.start_line as usize + 7 {
                let t = js_trim(lines[i]);
                if !t.starts_with('@') {
                    break; // reached the declaration → stop (no bleed into next method)
                }
                anno_lines.push(t);
                i += 1;
            }
            let head = anno_lines.join("\n");
            let annotated = has_anno && anno_re.is_match(&head);
            let is_app_listener = has_app_listener && node.name == "onApplicationEvent";
            if !annotated && !is_app_listener {
                continue;
            }
            let mut ty = spring_first_param_type(node.signature.as_deref());
            if ty.is_none() && annotated {
                if let Some(m) = anno_type_re.captures(&head) {
                    ty = m.text(1).map(String::from);
                }
            }
            let Some(ty) = ty else { continue };
            listeners.entry(ty).or_default().push(node.clone());
        }
    }
    if listeners.is_empty() {
        return Vec::new();
    }

    // Pass 2 — link each publishEvent(new XEvent(...)) site → every listener of XEvent.
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let publish_re = re!(r"\.publishEvent\s*\(\s*new\s+([A-Z][A-Za-z0-9_]*)", "g");
    for file in &publisher_files {
        let Some(content) = ctx.read_file(file) else { continue };
        if !content.contains(".publishEvent(") {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "java");
        let nodes_in_file = ctx.get_nodes_in_file(file);
        let mut added = 0;
        for m in publish_re.captures_iter(&safe) {
            if added >= SPRING_FANOUT_CAP {
                break;
            }
            let event = m.text(1).unwrap_or("");
            let Some(targets) = listeners.get(event).filter(|t| !t.is_empty()) else { continue };
            let line = line_of(&safe, m.get(0).unwrap().start());
            let Some(disp) = enclosing_fn(&nodes_in_file, line) else { continue };
            for target in targets {
                if target.id == disp.id || !seen.insert(format!("{}>{}", disp.id, target.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &disp.id,
                    &target.id,
                    Some(line),
                    meta! { "synthesizedBy" => "spring-event", "via" => event, "registeredAt" => format!("{file}:{line}") },
                ));
                added += 1;
            }
        }
    }
    edges
}

// ── MediatR request/notification dispatch (C#/.NET) ───────────────────────────

const MEDIATR_FANOUT_CAP: usize = 80;
const MEDIATR_HANDLER_DECL_LOOKAHEAD: usize = 4; // lines from a class startLine to find a wrapped base list

/// The type sent at a MediatR `.Send(arg)`/`.Publish(arg)` site: an inline `new X(…)`, else `arg`
/// as an identifier resolved within the enclosing method.
fn resolve_mediatr_arg_type(arg: &str, lines: &[&str], method_start: u32, dispatch_line: u32) -> Option<String> {
    if let Some(inl) = re!(r"^new\s+([A-Z]\w*)").captures(arg) {
        return inl.text(1).map(String::from);
    }
    if !re!(r"^[A-Za-z_]\w*$").is_match(arg) {
        return None;
    }
    let assign_re = Re::cached(&format!(r"\b{arg}\b\s*=\s*new\s+([A-Z]\w*)"), "");
    let decl_re = Re::cached(&format!(r"\b([A-Z]\w*)\b\s+{arg}\b"), "");
    let mut decl_type: Option<String> = None;
    let mut i = (method_start as usize).saturating_sub(1);
    while i < dispatch_line as usize && i < lines.len() {
        let ln = lines[i];
        if let Some(a) = assign_re.captures(ln) {
            return a.text(1).map(String::from); // an explicit `arg = new X` is the most specific
        }
        if decl_type.is_none() {
            if let Some(d) = decl_re.captures(ln) {
                decl_type = d.text(1).map(String::from); // a `X arg` declaration — keep scanning
            }
        }
        i += 1;
    }
    decl_type
}

pub fn mediatr_dispatch_edges(ctx: &Ctx) -> Vec<Edge> {
    let cs_ext = re!(r"\.cs$");
    let base_re = re!(r"(?:IRequestHandler|INotificationHandler)\s*<\s*([A-Za-z_]\w*)");
    // Pass 1 — request/notification type → the Handle method of each handler class.
    let mut handlers: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
    let files = ctx.get_all_files();
    for file in &files {
        if !cs_ext.is_match(file) {
            continue;
        }
        let Some(content) = ctx.read_file(file) else { continue };
        if content.is_empty() || (!content.contains("IRequestHandler<") && !content.contains("INotificationHandler<")) {
            continue;
        }
        let lines: Vec<&str> = content.split('\n').collect();
        let nodes_in_file = ctx.get_nodes_in_file(file);
        for cls in nodes_in_file.iter() {
            if cls.kind != NodeKind::Class {
                continue;
            }
            let from = (cls.start_line as usize).saturating_sub(1).min(lines.len());
            let to = (from + MEDIATR_HANDLER_DECL_LOOKAHEAD).min(lines.len());
            let decl = lines[from..to].join("\n");
            let Some(m) = base_re.captures(&decl) else { continue };
            let ty = m.text(1).unwrap_or("").to_string();
            let handle = nodes_in_file
                .iter()
                .find(|n| n.kind == NodeKind::Method && n.name == "Handle" && n.start_line >= cls.start_line && n.start_line <= cls.end_line);
            let Some(handle) = handle else { continue };
            handlers.entry(ty).or_default().push(handle.clone());
        }
    }
    if handlers.is_empty() {
        return Vec::new();
    }

    // Pass 2 — link each mediator-ish .Send(x)/.Publish(x) → the handler of x's type.
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let dispatch_re = re!(r"([A-Za-z_][\w.]*)\s*\.\s*(?:Send|Publish)\s*\(\s*(new\s+[A-Z]\w*|[A-Za-z_]\w*)", "g");
    let receiver_re = re!(r"(?:mediator|sender|publisher)", "i");
    for file in &files {
        if !cs_ext.is_match(file) {
            continue;
        }
        let Some(content) = ctx.read_file(file) else { continue };
        if content.is_empty() || (!content.contains(".Send(") && !content.contains(".Publish(")) {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "csharp");
        let safe_lines: Vec<&str> = safe.split('\n').collect();
        let nodes_in_file = ctx.get_nodes_in_file(file);
        let mut added = 0;
        for m in dispatch_re.captures_iter(&safe) {
            if added >= MEDIATR_FANOUT_CAP {
                break;
            }
            if !receiver_re.is_match(m.text(1).unwrap_or("")) {
                continue; // not a mediator (MessagingCenter, HttpClient, …)
            }
            let line = line_of(&safe, m.get(0).unwrap().start());
            let Some(disp) = enclosing_fn(&nodes_in_file, line) else { continue };
            let Some(ty) = resolve_mediatr_arg_type(m.text(2).unwrap_or(""), &safe_lines, disp.start_line, line) else { continue };
            let Some(targets) = handlers.get(&ty) else { continue };
            for target in targets {
                if target.id == disp.id || !seen.insert(format!("{}>{}", disp.id, target.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &disp.id,
                    &target.id,
                    Some(line),
                    meta! { "synthesizedBy" => "mediatr-dispatch", "via" => ty.as_str(), "registeredAt" => format!("{file}:{line}") },
                ));
                added += 1;
            }
        }
    }
    edges
}

// ── Sidekiq job dispatch (Ruby) ───────────────────────────────────────────────

const SIDEKIQ_FANOUT_CAP: usize = 80;

pub fn sidekiq_dispatch_edges(ctx: &Ctx) -> Vec<Edge> {
    let worker_re = re!(r"\binclude\s+Sidekiq::(?:Job|Worker)\b");
    // class node id → its instance `perform` method (None if the class isn't a Sidekiq worker),
    // memoized.
    let perform_cache: RefCell<HashMap<String, Option<NodeRef>>> = RefCell::new(HashMap::new());
    let perform_of = |cls: &NodeRef| -> Option<NodeRef> {
        if let Some(v) = perform_cache.borrow().get(&cls.id) {
            return v.clone();
        }
        let mut v: Option<NodeRef> = None;
        if let Some(content) = ctx.read_file(&cls.file_path) {
            if !content.is_empty() {
                let body = slice_lines(&content, cls.start_line.max(1), cls.end_line.max(1)).unwrap_or_default();
                if worker_re.is_match(&body) {
                    v = ctx
                        .get_nodes_in_file(&cls.file_path)
                        .iter()
                        .find(|n| n.kind == NodeKind::Method && n.name == "perform" && n.start_line >= cls.start_line && n.start_line <= cls.end_line)
                        .cloned();
                }
            }
        }
        perform_cache.borrow_mut().insert(cls.id.clone(), v.clone());
        v
    };

    // Resolve a (possibly namespaced) worker reference to its `perform`.
    let resolve = |reference: &str| -> Option<NodeRef> {
        if reference.contains("::") {
            let q = ctx
                .get_nodes_by_qualified_name(reference)
                .iter()
                .find(|n| n.kind == NodeKind::Class && perform_of(n).is_some())
                .cloned();
            if let Some(q) = q {
                return perform_of(&q);
            }
        }
        let simple = reference.rsplit("::").next().unwrap_or(reference);
        let workers: Vec<NodeRef> =
            ctx.get_nodes_by_name(simple).iter().filter(|n| n.kind == NodeKind::Class && perform_of(n).is_some()).cloned().collect();
        if workers.len() == 1 {
            perform_of(&workers[0])
        } else {
            None
        }
    };

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let rb_ext = re!(r"\.rb$");
    let gate = re!(r"\.perform_(?:async|in|at)\b");
    let dispatch_re =
        re!(r"([A-Z][A-Za-z0-9_]*(?:::[A-Z][A-Za-z0-9_]*)*)\s*\.\s*perform_(?:async|in|at)\b", "g");
    for file in ctx.get_all_files() {
        if !rb_ext.is_match(&file) {
            continue;
        }
        let Some(content) = ctx.read_file(&file) else { continue };
        if content.is_empty() || !gate.is_match(&content) {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "ruby");
        let nodes_in_file = ctx.get_nodes_in_file(&file);
        let mut added = 0;
        for m in dispatch_re.captures_iter(&safe) {
            if added >= SIDEKIQ_FANOUT_CAP {
                break;
            }
            let line = line_of(&safe, m.get(0).unwrap().start());
            let Some(disp) = enclosing_fn(&nodes_in_file, line) else { continue };
            let reference = m.text(1).unwrap_or("");
            let Some(target) = resolve(reference) else { continue };
            if target.id == disp.id || !seen.insert(format!("{}>{}", disp.id, target.id)) {
                continue;
            }
            edges.push(calls_edge(
                &disp.id,
                &target.id,
                Some(line),
                meta! { "synthesizedBy" => "sidekiq-dispatch", "via" => reference, "registeredAt" => format!("{file}:{line}") },
            ));
            added += 1;
        }
    }
    edges
}

// ── Erlang behaviour-callback dispatch ────────────────────────────────────────

const ERLANG_BEHAVIOUR_FANOUT_CAP: usize = 24;

/// Argument count of the call/declaration whose `(` sits at byte `open_idx` — top-level commas + 1,
/// `()` → 0, unbalanced/oversized → -1. Skips nested (), [], {}, <<>> content, `"strings"`,
/// `'atoms'`, and `$c` char literals.
fn erlang_arity_at(src: &str, open_idx: usize) -> i32 {
    let u: Vec<u16> = src[open_idx..].encode_utf16().take(4000).collect();
    let at = |i: usize| -> i32 { u.get(i).map_or(-1, |c| *c as i32) };
    let mut depth = 1;
    // `<<1,2,3>>` binary literals: commas inside are element separators, not argument separators.
    let mut bin_depth = 0;
    let mut commas = 0;
    let mut saw_arg = false;
    let limit = u.len();
    let mut i = 1;
    while i < limit {
        let ch = at(i);
        if ch == b'"' as i32 || ch == b'\'' as i32 {
            i += 1;
            while i < limit && at(i) != ch {
                if at(i) == b'\\' as i32 {
                    i += 1;
                }
                i += 1;
            }
            saw_arg = true;
            i += 1;
            continue;
        }
        if ch == b'$' as i32 {
            i += 1;
            if at(i) == b'\\' as i32 {
                i += 1;
            }
            saw_arg = true;
            i += 1;
            continue;
        }
        if ch == b'<' as i32 && at(i + 1) == b'<' as i32 {
            bin_depth += 1;
            i += 2;
            saw_arg = true;
            continue;
        }
        if ch == b'>' as i32 && at(i + 1) == b'>' as i32 && bin_depth > 0 {
            bin_depth -= 1;
            i += 2;
            continue;
        }
        if ch == b'(' as i32 || ch == b'[' as i32 || ch == b'{' as i32 {
            depth += 1;
            saw_arg = true;
            i += 1;
            continue;
        }
        if ch == b')' as i32 || ch == b']' as i32 || ch == b'}' as i32 {
            depth -= 1;
            if depth == 0 {
                return if saw_arg { commas + 1 } else { 0 };
            }
            i += 1;
            continue;
        }
        if ch == b',' as i32 && depth == 1 && bin_depth == 0 {
            commas += 1;
            i += 1;
            continue;
        }
        if !char::from_u32(ch as u32).is_some_and(is_js_space) {
            saw_arg = true;
        }
        i += 1;
    }
    -1
}

pub fn erlang_behaviour_dispatch_edges(ctx: &Ctx) -> Vec<Edge> {
    // Cheap language gate: no Erlang modules → no cost beyond one streamed kind scan.
    let mut erlang_modules: Vec<NodeRef> = Vec::new();
    each_node_in(ctx, &[NodeKind::Namespace], &[Language::Erlang], |n| erlang_modules.push(n));
    if erlang_modules.is_empty() {
        return Vec::new();
    }

    // Pass 1 — scan every Erlang file with `-callback` decls: behaviour module → its (name, arity)
    // callback set, and the global `name/arity` → declaring behaviours map that drives the
    // ambiguity gate.
    let mut module_by_file: HashMap<String, NodeRef> = HashMap::new();
    for ns in &erlang_modules {
        module_by_file.entry(ns.file_path.clone()).or_insert_with(|| ns.clone());
    }
    let mut declaring_behaviours: IndexMap<String, Vec<NodeRef>> = IndexMap::new(); // `fn/arity` → behaviour namespaces
    let mut callback_names: HashSet<String> = HashSet::new();
    let erl_ext = re!(r"\.(?:erl|hrl)$");
    let files = ctx.get_all_files();
    let callback_re = re!(r"(^|\n)\s*-callback\s+('[^'\n]+'|[a-z][A-Za-z0-9_@]*)\s*\(", "g");
    for file in &files {
        if !erl_ext.is_match(file) {
            continue;
        }
        let Some(behaviour) = module_by_file.get(file) else { continue }; // a .hrl or module-less file can't be a behaviour
        let Some(content) = ctx.read_file(file) else { continue };
        if !content.contains("-callback") {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "erlang");
        for m in callback_re.captures_iter(&safe) {
            let raw = m.text(2).unwrap_or("");
            let name = raw.strip_prefix('\'').unwrap_or(raw);
            let name = name.strip_suffix('\'').unwrap_or(name);
            let arity = erlang_arity_at(&safe, m.get(0).unwrap().end() - 1);
            if arity < 0 {
                continue;
            }
            let arr = declaring_behaviours.entry(format!("{name}/{arity}")).or_default();
            if !arr.iter().any(|b| b.id == behaviour.id) {
                arr.push(behaviour.clone());
            }
            callback_names.insert(name.to_string());
        }
    }
    if declaring_behaviours.is_empty() {
        return Vec::new();
    }

    // Implementer target lookup, lazy per (behaviour, fn, arity).
    let target_cache: RefCell<HashMap<String, Vec<NodeRef>>> = RefCell::new(HashMap::new());
    let arity_re = re!(r"/(\d{1,3})$");
    let qn_arity = |qn: &str| -> i64 { arity_re.captures(qn).and_then(|m| m.text(1)?.parse::<i64>().ok()).unwrap_or(-1) };
    let targets_of = |behaviour: &NodeRef, func: &str, arity: i32| -> Vec<NodeRef> {
        let cache_key = format!("{}#{}/{}", behaviour.id, func, arity);
        if let Some(t) = target_cache.borrow().get(&cache_key) {
            return t.clone();
        }
        let mut targets: Vec<NodeRef> = Vec::new();
        for e in incoming(ctx, &behaviour.id, &[EdgeKind::Implements]) {
            let Some(imp) = node(ctx, &e.source) else { continue };
            if imp.language != Language::Erlang || imp.kind != NodeKind::Namespace {
                continue;
            }
            let in_file = ctx.get_nodes_in_file(&imp.file_path);
            let fn_node = in_file.iter().find(|n| {
                n.kind == NodeKind::Function && n.name == func && qn_arity(&n.qualified_name) == arity as i64 && n.is_exported != Some(false)
            });
            if let Some(f) = fn_node {
                targets.push(f.clone());
            }
        }
        target_cache.borrow_mut().insert(cache_key, targets.clone());
        targets
    };

    // Pass 2 — dispatch sites. Only files containing a var-module call shape are scanned in full.
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let gate = re!(r"[A-Z][A-Za-z0-9_@]*:[a-z]");
    let dispatch_re = re!(r"(^|[^?\w@'])([A-Z][A-Za-z0-9_@]*):([a-z][A-Za-z0-9_@]*)\(", "g");
    for file in &files {
        if !erl_ext.is_match(file) {
            continue;
        }
        let Some(content) = ctx.read_file(file) else { continue };
        if content.is_empty() || !gate.is_match(&content) {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "erlang");
        let nodes_in_file = ctx.get_nodes_in_file(file);
        for m in dispatch_re.captures_iter(&safe) {
            let func = m.text(3).unwrap_or("");
            if !callback_names.contains(func) {
                continue;
            }
            let whole = m.get(0).unwrap();
            let open_idx = whole.end() - 1;
            let arity = erlang_arity_at(&safe, open_idx);
            if arity < 0 {
                continue;
            }
            let Some(behaviours) = declaring_behaviours.get(&format!("{func}/{arity}")) else { continue };
            if behaviours.len() != 1 {
                continue; // unknown or ambiguous
            }
            let behaviour = &behaviours[0];
            let targets = targets_of(behaviour, func, arity);
            if targets.is_empty() || targets.len() > ERLANG_BEHAVIOUR_FANOUT_CAP {
                continue;
            }
            let line = line_of(&safe, whole.start());
            let Some(disp) = enclosing_fn(&nodes_in_file, line) else { continue };
            for target in &targets {
                if target.id == disp.id || !seen.insert(format!("{}>{}", disp.id, target.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &disp.id,
                    &target.id,
                    Some(line),
                    meta! {
                        "synthesizedBy" => "erlang-behaviour", "via" => format!("{}:{}/{}", behaviour.name, func, arity),
                        "registeredAt" => format!("{file}:{line}"),
                    },
                ));
            }
        }
    }
    edges
}

// ── Nix module-system option wiring ───────────────────────────────────────────

/// The leading static segments of a Nix attribute path: plain identifiers, plus quoted segments as
/// opaque verbatim tokens; an interpolated (`${name}`) segment ends the prefix.
fn nix_leading_plain_segments(name: &str) -> Vec<String> {
    let c: Vec<char> = name.chars().collect();
    let n = c.len();
    let mut segs: Vec<String> = Vec::new();
    let mut i = 0;
    let seg_re = re!(r"^[A-Za-z_][A-Za-z0-9_'-]*$");
    while i < n {
        if c[i] == '"' {
            // Quoted segment — an opaque verbatim token (quotes kept).
            let mut j = i + 1;
            while j < n && c[j] != '"' {
                if c[j] == '\\' {
                    j += 1;
                }
                j += 1;
            }
            if j >= n {
                return segs; // unterminated — stop at the static head
            }
            let tok: String = c[i..=j].iter().collect();
            if tok.contains("${") {
                return segs; // interpolated → dynamic → stop
            }
            segs.push(tok);
            i = j + 1;
            if i >= n {
                break;
            }
            if c[i] != '.' {
                return segs;
            }
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && c[j] != '.' {
            if c[j] == '"' || (c[j] == '$' && c.get(j + 1) == Some(&'{')) {
                return segs;
            }
            j += 1;
        }
        let seg: String = c[i..j].iter().collect();
        if !seg_re.is_match(&seg) {
            return segs;
        }
        segs.push(seg);
        i = j + 1;
    }
    segs
}

#[derive(Clone)]
struct NixRec {
    id: String,
    file_path: String,
    start_line: u32,
    end_line: u32,
    segs: Vec<String>,
}

pub fn nix_option_path_edges(ctx: &Ctx) -> Vec<Edge> {
    // One streaming pass over nix bindings (variables + the odd function-valued option).
    let mut by_file: IndexMap<String, Vec<NixRec>> = IndexMap::new();
    for kind in [NodeKind::Variable, NodeKind::Function] {
        each_node_in(ctx, &[kind], &[Language::Nix], |node| {
            let segs = nix_leading_plain_segments(&node.name);
            if segs.is_empty() {
                return;
            }
            by_file.entry(node.file_path.clone()).or_default().push(NixRec {
                id: node.id.clone(),
                file_path: node.file_path.clone(),
                start_line: node.start_line,
                end_line: node.end_line,
                segs,
            });
        });
    }

    // Per file: walk bindings outermost-first with a stack of active option spans, composing
    // nested declaration paths. An `options` binding nested inside another option span is a
    // SUBMODULE's own namespace — its internals are not globally addressable.
    const SUBMODULE: &str = "\u{0}submodule";
    let mut decls: IndexMap<String, Vec<NixRec>> = IndexMap::new();
    let mut writes: Vec<NixRec> = Vec::new();
    let register = |decls: &mut IndexMap<String, Vec<NixRec>>, path: &[String], rec: &NixRec| {
        if path.len() < 2 || path.iter().any(|s| s == SUBMODULE) {
            return;
        }
        decls.entry(path.join(".")).or_default().push(rec.clone());
    };
    struct Span {
        start: u32,
        end: u32,
        prefix: Vec<String>,
    }
    for recs in by_file.values_mut() {
        recs.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(b.end_line.cmp(&a.end_line)));
        let mut stack: Vec<Span> = Vec::new();
        for rec in recs.iter() {
            while stack.last().is_some_and(|s| s.end < rec.start_line) {
                stack.pop();
            }
            // Strict containment at line granularity: a one-line nested binding is
            // indistinguishable from its container, so it stays unclassified.
            let enclosing_prefix: Option<Vec<String>> = stack.last().and_then(|top| {
                (rec.start_line >= top.start
                    && rec.end_line <= top.end
                    && !(rec.start_line == top.start && rec.end_line == top.end))
                    .then(|| top.prefix.clone())
            });

            if rec.segs[0] == "options" {
                let own_path: Vec<String> = rec.segs[1..].to_vec(); // [] for the bare `options = { ... }` spelling
                let prefix = if enclosing_prefix.is_some() { vec![SUBMODULE.to_string()] } else { own_path };
                register(&mut decls, &prefix, rec);
                stack.push(Span { start: rec.start_line, end: rec.end_line, prefix });
                continue;
            }
            if let Some(ep) = enclosing_prefix {
                let mut composed = ep;
                composed.extend(rec.segs.iter().cloned());
                register(&mut decls, &composed, rec);
                stack.push(Span { start: rec.start_line, end: rec.end_line, prefix: composed });
                continue;
            }
            if rec.segs.len() >= 2 {
                writes.push(rec.clone());
            }
        }
    }
    if decls.is_empty() || writes.is_empty() {
        return Vec::new();
    }

    let mut edges = Vec::new();
    for w in &writes {
        // `config.services.x = ...` spells the same write with an explicit prefix.
        let segs: &[String] = if w.segs[0] == "config" { &w.segs[1..] } else { &w.segs };
        if segs.len() < 2 {
            continue;
        }
        // Longest prefix wins; an ambiguous longest match does NOT fall back to a shorter one.
        for len in (2..=segs.len().min(6)).rev() {
            let Some(candidates) = decls.get(&segs[..len].join(".")).filter(|c| !c.is_empty()) else { continue };
            let files: HashSet<&String> = candidates.iter().map(|c| &c.file_path).collect();
            if files.len() == 1 {
                let target = &candidates[0];
                if target.id != w.id {
                    edges.push(heuristic_edge(
                        &w.id,
                        &target.id,
                        EdgeKind::References,
                        Some(w.start_line),
                        meta! {
                            "synthesizedBy" => "nix-option-path", "optionPath" => segs[..len].join("."),
                            "registeredAt" => format!("{}:{}", target.file_path, target.start_line),
                        },
                    ));
                }
            }
            break; // longest hit decides, matched or ambiguous
        }
    }
    edges
}

// ── Laravel events (PHP) ──────────────────────────────────────────────────────

const LARAVEL_FANOUT_CAP: usize = 200;

/// Short class name from a PHP reference: `\App\Events\Foo` / `App\Events::Foo` → `Foo`.
fn php_simple_name(s: &str) -> String {
    let s = s.strip_prefix('\\').unwrap_or(s);
    let last = s.rsplit('\\').next().unwrap_or(s);
    let last = last.rsplit("::").next().unwrap_or(last);
    js_trim(last).to_string()
}

/// The first-parameter class type(s) of a `handle(...)` declaration — union-split, short-named,
/// primitives dropped.
fn laravel_handle_event_types(decl: &str) -> Vec<String> {
    let Some(m) = re!(r"function\s+handle\s*\(\s*(?:\.\.\.\s*)?(\??[A-Za-z_\\][\w\\|]*)\s+&?\s*(?:\.\.\.\s*)?\$").captures(decl) else {
        return Vec::new();
    };
    let t = m.text(1).unwrap_or("");
    let t = t.strip_prefix('?').unwrap_or(t);
    t.split('|').map(php_simple_name).filter(|t| re!(r"^[A-Z]\w*$").is_match(t)).collect()
}

/// From an opening `[`, the bracket-balanced body up to its matching `]`.
fn php_array_body(src: &str, open_idx: usize) -> Option<&str> {
    let mut depth = 0;
    for (i, c) in src[open_idx..].char_indices() {
        if c == '[' {
            depth += 1;
        } else if c == ']' {
            depth -= 1;
            if depth == 0 {
                return Some(&src[open_idx + 1..open_idx + i]);
            }
        }
    }
    None
}

pub fn laravel_event_edges(ctx: &Ctx) -> Vec<Edge> {
    // event short name → its listener `handle` methods (deduped by node id).
    let mut listeners: IndexMap<String, IndexMap<String, NodeRef>> = IndexMap::new();
    let handle_of = |cls: &NodeRef| -> Option<NodeRef> {
        ctx.get_nodes_in_file(&cls.file_path)
            .iter()
            .find(|n| n.kind == NodeKind::Method && n.name == "handle" && n.start_line >= cls.start_line && n.start_line <= cls.end_line)
            .cloned()
    };
    let php_ext = re!(r"\.php$");
    let files = ctx.get_all_files();
    let listen_decl = re!(r"\$listen\s*=\s*\[");
    let entry_re = re!(r##"(?:([A-Za-z_\\][\w\\]*)::class|'([^']+)'|"([^"]+)")\s*=>\s*\[([^\]]*)\]"##, "g");
    let class_re = re!(r##"(?:([A-Za-z_\\][\w\\]*)::class|'([^']+)'|"([^"]+)")"##, "g");

    // Pass 1 — build the event→handle map from both registration mechanisms.
    for file in &files {
        if !php_ext.is_match(file) {
            continue;
        }
        let Some(content) = ctx.read_file(file) else { continue };
        if content.is_empty() {
            continue;
        }

        // (A) typed listener handles — node-driven, so a commented-out method can't leak in.
        if content.contains("function handle") {
            let lines: Vec<&str> = content.split('\n').collect();
            for node in ctx.get_nodes_in_file(file).iter() {
                if node.kind != NodeKind::Method || node.name != "handle" {
                    continue;
                }
                let from = (node.start_line as usize).saturating_sub(1).min(lines.len());
                let to = (node.start_line as usize + 2).min(lines.len()).max(from);
                let decl = lines[from..to].join("\n");
                for ev in laravel_handle_event_types(&decl) {
                    listeners.entry(ev).or_default().insert(node.id.clone(), node.clone());
                }
            }
        }

        // (B) the EventServiceProvider `$listen` map — parsed from comment-stripped source so a
        // fully-commented map contributes nothing.
        if content.contains("$listen") {
            let safe = codegraph_text::strip_comments_for_regex(&content, "php");
            let body = listen_decl
                .find(&safe)
                .and_then(|d| safe[d.start()..].find('[').map(|b| d.start() + b))
                .and_then(|open| php_array_body(&safe, open));
            if let Some(body) = body {
                for em in entry_re.captures_iter(body) {
                    let event = php_simple_name(em.text(1).or(em.text(2)).or(em.text(3)).unwrap_or(""));
                    for lm in class_re.captures_iter(em.text(4).unwrap_or("")) {
                        let ln = php_simple_name(lm.text(1).or(lm.text(2)).or(lm.text(3)).unwrap_or(""));
                        let cls = ctx.get_nodes_by_name(&ln).iter().find(|n| n.kind == NodeKind::Class && handle_of(n).is_some()).cloned();
                        if let Some(cls) = cls {
                            let handle = handle_of(&cls).unwrap();
                            listeners.entry(event.clone()).or_default().insert(handle.id.clone(), handle);
                        }
                    }
                }
            }
        }
    }
    if listeners.is_empty() {
        return Vec::new();
    }

    // Pass 2 — link each event(new X(...)) site → every listener of X.
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let dispatch_re = re!(r"\bevent\s*\(\s*new\s+\\?([A-Za-z_][\w\\]*)", "g");
    for file in &files {
        if !php_ext.is_match(file) {
            continue;
        }
        let Some(content) = ctx.read_file(file) else { continue };
        if content.is_empty() || !content.contains("event(") {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "php");
        let nodes_in_file = ctx.get_nodes_in_file(file);
        let mut added = 0;
        for m in dispatch_re.captures_iter(&safe) {
            if added >= LARAVEL_FANOUT_CAP {
                break;
            }
            let simple = php_simple_name(m.text(1).unwrap_or(""));
            let Some(targets) = listeners.get(&simple) else { continue };
            let line = line_of(&safe, m.get(0).unwrap().start());
            let Some(disp) = enclosing_fn(&nodes_in_file, line) else { continue };
            for target in targets.values() {
                if target.id == disp.id || !seen.insert(format!("{}>{}", disp.id, target.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &disp.id,
                    &target.id,
                    Some(line),
                    meta! { "synthesizedBy" => "laravel-event", "via" => simple.as_str(), "registeredAt" => format!("{file}:{line}") },
                ));
                added += 1;
            }
        }
    }
    edges
}
