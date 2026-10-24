//! State-container and task-queue dispatch: Redux thunks, object-literal registries, RTK Query
//! hooks, Pinia stores, Vuex dispatch, Celery tasks. Port of the matching section of
//! `src/resolution/callback-synthesizer.ts`.

use super::common::*;
use crate::ctx::Ctx;
use crate::meta;
use crate::synth_utils::{enclosing_fn, make_line_at};
use crate::types::NodeRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_len, utf16_prefix};
use codegraph_types::{Edge, Language, NodeKind};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

const THUNK_FANOUT_CAP: usize = 24;

fn thunk_decl(sig: &str) -> bool {
    re!(r"create(?:Async)?Thunk").is_match(sig)
}

/// Redux-thunk dispatch chain: body-scan each thunk constant for `dispatch(Y(...))` and link
/// `X → Y`.
pub fn redux_thunk_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let dispatch_re = re!(r"\bdispatch\s*\(\s*([A-Za-z_]\w*)\s*[(),]", "g");
    each_node(ctx, NodeKind::Constant, |node| {
        // Cheap gate: the initializer (captured in `signature`) must be a create(Async)Thunk call.
        if !node.signature.as_deref().is_some_and(thunk_decl) {
            return;
        }
        let Some(src) = node_source(ctx, &node) else { return };
        // Thunks are TS/JS-family (same // and /* */ comment syntax).
        let lang = if matches!(node.language, Language::JavaScript | Language::Jsx) { "javascript" } else { "typescript" };
        let safe = codegraph_text::strip_comments_for_regex(&src, lang);
        let mut added = 0;
        for m in dispatch_re.captures_iter(&safe) {
            if added >= THUNK_FANOUT_CAP {
                break;
            }
            let name = m.text(1).unwrap_or("");
            if name == node.name {
                continue; // self-dispatch (recursive thunk) — skip
            }
            // Resolve the dispatched name, PREFERRING the thunk/action-creator over a same-named
            // service function. Order: thunk const > other const > same-file callable > first match.
            let cands: Vec<NodeRef> = ctx
                .get_nodes_by_name(name)
                .iter()
                .filter(|n| matches!(n.kind, NodeKind::Constant | NodeKind::Function | NodeKind::Method))
                .cloned()
                .collect();
            let target = cands
                .iter()
                .find(|n| n.signature.as_deref().is_some_and(thunk_decl))
                .or_else(|| cands.iter().find(|n| n.kind == NodeKind::Constant))
                .or_else(|| cands.iter().find(|n| n.file_path == node.file_path))
                .or(cands.first());
            let Some(target) = target else { continue };
            if target.id == node.id {
                continue;
            }
            if !seen.insert(format!("{}>{}", node.id, target.id)) {
                continue;
            }
            let line = node.start_line + line_of(&safe, m.get(0).unwrap().start()) - 1;
            edges.push(calls_edge(
                &node.id,
                &target.id,
                Some(line),
                meta! { "synthesizedBy" => "redux-thunk", "via" => name, "registeredAt" => format!("{}:{}", node.file_path, line) },
            ));
            added += 1;
        }
    });
    edges
}

// ── Object-literal registry dispatch ─────────────────────────────────────────

const REGISTRY_MIN_ENTRIES: usize = 2;
const REGISTRY_FANOUT_CAP: usize = 40;
const REGISTRY_CLASS_ENTRY: [&str; 8] = ["execute", "run", "handle", "perform", "process", "call", "apply", "dispatch"];

/// Where a registry name may start: not inside an identifier, or at a `this.`. (The original's
/// lookbehind `(?<![A-Za-z_$][\w$]+)` is bounded to 200 characters, which PCRE2 requires.)
const REGISTRY_NAME_START: &str = r"(?:(?<![A-Za-z_$])(?<![A-Za-z_$][\w$]{1,200})|(?=this\.))";
const REGISTRY_DISPATCH_SRC: &str =
    r"(?:\bnew\s+)?((?:this\.)?[A-Za-z_$][\w$]*)\s*\[\s*([A-Za-z_$][\w$.]*)\s*\]\s*(?:\(|\.[A-Za-z_$])";

fn is_ident_run_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// The first registry dispatch at or after `from`, exactly as a `g` scan of the unguarded pattern
/// resuming at `from` would find it: `(start, end, ref)`.
fn next_registry_dispatch(src: &str, from: usize) -> Option<(usize, usize, String)> {
    let at_re = Re::cached(REGISTRY_DISPATCH_SRC, "y");
    let dispatch_re = Re::cached(&format!(r"(?:(?<!\w)|{REGISTRY_NAME_START}){REGISTRY_DISPATCH_SRC}"), "g");
    let b = src.as_bytes();
    let mut at = from;
    if at > 0 && at < b.len() && is_ident_run_char(b[at - 1]) && is_ident_run_char(b[at]) {
        while at < b.len() && is_ident_run_char(b[at]) {
            if let Some(m) = at_re.captures_at(src, at) {
                let whole = m.get(0).unwrap();
                return Some((whole.start(), whole.end(), m.text(1).unwrap_or("").to_string()));
            }
            at += 1;
        }
    }
    let m = dispatch_re.captures_at(src, at)?;
    let whole = m.get(0).unwrap();
    Some((whole.start(), whole.end(), m.text(1).unwrap_or("").to_string()))
}

/// From the index of an opening `{`, the brace-balanced body up to its matching `}`.
fn brace_body(src: &str, open_idx: usize) -> Option<&str> {
    let mut depth = 0;
    for (i, c) in src[open_idx..].char_indices() {
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(&src[open_idx + 1..open_idx + i]);
            }
        }
    }
    None
}

/// Top-level `key: Identifier` entries of an object-literal body (depth-aware).
fn registry_entry_names(body: &str) -> Vec<String> {
    let mut segs: Vec<&str> = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in body.char_indices() {
        match c {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                segs.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    segs.push(&body[start..]);
    let entry_re = re!(r##"^\s*(?:\[[^\]]+\]|['"]?[\w$]+['"]?)\s*:\s*([A-Za-z_$][\w$]*)\s*$"##);
    let mut names: Vec<String> = Vec::new();
    for seg in segs {
        if let Some(m) = entry_re.captures(seg) {
            let n = m.text(1).unwrap_or("");
            if utf16_len(n) >= 3 && !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        }
    }
    names
}

/// Resolve a registered handler name to its callable entry: a function value, or a class's
/// `execute`-like method (preferring the method chained at the dispatch site), else the class.
fn resolve_registry_handler(ctx: &Ctx, name: &str, chained: Option<&str>) -> Option<NodeRef> {
    let cands = ctx.get_nodes_by_name(name);
    if let Some(f) = cands.iter().find(|n| n.kind == NodeKind::Function) {
        return Some(f.clone());
    }
    if let Some(cls) = cands.iter().find(|n| matches!(n.kind, NodeKind::Class | NodeKind::Struct)) {
        let methods: Vec<NodeRef> = ctx
            .get_nodes_in_file(&cls.file_path)
            .iter()
            .filter(|n| n.kind == NodeKind::Method && n.start_line >= cls.start_line && n.start_line <= cls.end_line)
            .cloned()
            .collect();
        let want = chained.filter(|c| REGISTRY_CLASS_ENTRY.contains(c));
        let entry = want
            .and_then(|w| methods.iter().find(|m| m.name == w))
            .or_else(|| methods.iter().find(|m| REGISTRY_CLASS_ENTRY.contains(&m.name.as_str())))
            .or_else(|| methods.iter().find(|m| m.name == "constructor"));
        return Some(entry.cloned().unwrap_or_else(|| cls.clone()));
    }
    // Require a CALLABLE target — never a data `constant`.
    cands.iter().find(|n| n.kind == NodeKind::Method).cloned()
}

fn comment_lang_for(file: &str) -> &'static str {
    if re!(r"\.(?:jsx?|mjs|cjs)$").is_match(file) {
        "javascript"
    } else {
        "typescript"
    }
}

/// Object-literal registry dispatch: `this.commands = { [Cmd.ADD]: AddObjectCommand, … }` +
/// `new this.commands[command](args).execute()`.
pub fn object_registry_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let js_ext = re!(r"\.(?:ts|tsx|js|jsx|mjs|cjs)$");
    let gate = re!(r"[\w$]\s*\[\s*[A-Za-z_$]");
    let assign_re = Re::cached(
        &format!(
            r"(?:(?:const|let|var)\s+([A-Za-z_$][\w$]*)|{REGISTRY_NAME_START}((?:this\.)?[A-Za-z_$][\w$]*))\s*=\s*\{{"
        ),
        "g",
    );
    for file in ctx.get_all_files() {
        if !js_ext.is_match(&file) {
            continue;
        }
        let Some(content) = ctx.read_file(&file) else { continue };
        // Cheap pre-filter: a computed member access BY NAME (`ident[ident`) — the dispatch shape.
        if content.is_empty() || !gate.is_match(&content) {
            continue;
        }
        // Skip minified/generated bundles: average line length is the reliable tell.
        let newlines = content.matches('\n').count() + 1;
        if utf16_len(&content) as f64 / newlines as f64 > 200.0 {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, comment_lang_for(&file));

        // 1. Dispatch sites: `(new )?<ref>[<ident-key>]` followed by a call or a chained method.
        let line_at = make_line_at(&safe, 1);
        struct Dispatch {
            r#ref: String,
            line: u32,
            chained: Option<String>,
        }
        let mut dispatches: Vec<Dispatch> = Vec::new();
        let mut dm = next_registry_dispatch(&safe, 0);
        while let Some((start, end, r)) = dm {
            let win = utf16_prefix(&safe[start..], 160);
            let cm = re!(r"\]\s*\([^)]*\)\s*\.\s*([A-Za-z_$][\w$]*)")
                .captures(win)
                .or_else(|| re!(r"\]\s*\.\s*([A-Za-z_$][\w$]*)").captures(win));
            dispatches.push(Dispatch {
                r#ref: r,
                line: line_at.at(start),
                chained: cm.and_then(|c| c.text(1).map(String::from)),
            });
            dm = next_registry_dispatch(&safe, end);
        }
        if dispatches.is_empty() {
            continue;
        }
        // Normalize a leading `this.` so a class FIELD-INITIALIZER registry matches a
        // `this.commands[k]` dispatch.
        let norm = |r: &str| -> String { r.strip_prefix("this.").unwrap_or(r).to_string() };
        let refs: HashSet<String> = dispatches.iter().map(|d| norm(&d.r#ref)).collect();

        // 2. Registries: an object literal assigned to a dispatched ref, ≥2 entries resolving to
        //    callables.
        struct Registry {
            names: Vec<String>,
            line: u32,
        }
        let mut registries: IndexMap<String, Registry> = IndexMap::new();
        for am in assign_re.captures_iter(&safe) {
            let lhs = norm(am.text(1).filter(|s| !s.is_empty()).or(am.text(2)).unwrap_or(""));
            if !refs.contains(&lhs) || registries.contains_key(&lhs) {
                continue;
            }
            let whole = am.get(0).unwrap();
            let Some(body) = brace_body(&safe, whole.end() - 1) else { continue };
            let names = registry_entry_names(body); // depth-0 `key: Identifier` entries only
            if names.len() >= REGISTRY_MIN_ENTRIES {
                registries.insert(lhs, Registry { names, line: line_at.at(whole.start()) });
            }
        }
        if registries.is_empty() {
            continue;
        }

        // 3. Link each dispatcher → each registered handler's callable entry.
        let nodes_in_file = ctx.get_nodes_in_file(&file);
        for d in &dispatches {
            let Some(reg) = registries.get(&norm(&d.r#ref)) else { continue };
            let Some(disp) = enclosing_fn(&nodes_in_file, d.line) else { continue };
            let mut added = 0;
            for name in &reg.names {
                if added >= REGISTRY_FANOUT_CAP {
                    break;
                }
                let Some(target) = resolve_registry_handler(ctx, name, d.chained.as_deref()) else { continue };
                if target.id == disp.id {
                    continue;
                }
                if !seen.insert(format!("{}>{}", disp.id, target.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &disp.id,
                    &target.id,
                    Some(d.line),
                    meta! { "synthesizedBy" => "object-registry", "via" => name.as_str(), "registeredAt" => format!("{}:{}", file, reg.line) },
                ));
                added += 1;
            }
        }
    }
    edges
}

// ── RTK Query generated-hook → endpoint ──────────────────────────────────────

/// MUST match the signature set in the extractor's RTK hook bindings.
const RTK_GENERATED_HOOK_SIGNATURE: &str = "= RTK Query generated hook";

/// Derive the endpoint key from a generated-hook name (`useLazyGetRecordsQuery` → `getRecords`).
fn rtk_endpoint_name_from_hook(hook: &str) -> Option<String> {
    let m = re!(r"^use([A-Z][A-Za-z0-9]*?)(?:Query|Mutation)$").captures(hook)?;
    let mut mid = m.text(1)?;
    if let Some(rest) = mid.strip_prefix("Lazy") {
        mid = rest; // useLazyGetX → getX (same endpoint)
    }
    let mut cs = mid.chars();
    let first = cs.next()?;
    Some(first.to_lowercase().collect::<String>() + cs.as_str())
}

pub fn rtk_query_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    each_node(ctx, NodeKind::Function, |hook| {
        // Only our extracted generated-hook bindings (sentinel) — not a real hook fn.
        if hook.signature.as_deref() != Some(RTK_GENERATED_HOOK_SIGNATURE) {
            return;
        }
        let Some(endpoint_name) = rtk_endpoint_name_from_hook(&hook.name) else { return };
        // The endpoint is a same-file function by the derived name.
        let target = ctx
            .get_nodes_by_name(&endpoint_name)
            .iter()
            .find(|n| n.kind == NodeKind::Function && n.file_path == hook.file_path)
            .cloned();
        let Some(target) = target else { return };
        if target.id == hook.id || !seen.insert(format!("{}>{}", hook.id, target.id)) {
            return;
        }
        edges.push(calls_edge(
            &hook.id,
            &target.id,
            Some(hook.start_line),
            meta! {
                "synthesizedBy" => "rtk-query", "via" => endpoint_name.as_str(),
                "registeredAt" => format!("{}:{}", hook.file_path, hook.start_line),
            },
        ));
    });
    edges
}

// ── Pinia useStore().action() dispatch bridge ────────────────────────────────

const PINIA_FANOUT_CAP: usize = 80;

fn pinia_consumer_ext(file: &str) -> bool {
    re!(r"\.(?:ts|tsx|js|jsx|mjs|cjs|vue)$").is_match(file)
}

pub fn pinia_store_edges(ctx: &Ctx) -> Vec<Edge> {
    // 1. Map each `const useXStore = defineStore(...)` factory → its store file.
    let mut factory_file: HashMap<String, String> = HashMap::new();
    let factory_re = re!(r"\b(?:export\s+)?const\s+(\w+)\s*=\s*defineStore\s*\(", "g");
    let files = ctx.get_all_files();
    for file in &files {
        if !pinia_consumer_ext(file) {
            continue;
        }
        let Some(content) = ctx.read_file(file) else { continue };
        if !content.contains("defineStore") {
            continue;
        }
        for m in factory_re.captures_iter(&content) {
            factory_file.insert(m.text(1).unwrap_or("").to_string(), file.clone());
        }
    }
    if factory_file.is_empty() {
        return Vec::new();
    }

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let bind_re = re!(r"\bconst\s+(\w+)\s*=\s*(?:await\s+)?(\w+)\s*\(", "g");
    let call_re = re!(r"(\w+)\s*\.\s*(\w+)\s*\(", "g");
    for file in &files {
        if !pinia_consumer_ext(file) {
            continue;
        }
        let Some(content) = ctx.read_file(file) else { continue };
        if !content.contains("Store") {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, comment_lang_for(file));

        // 2. Bind store vars in this file: `const <var> = <known-factory>(...)`.
        let mut var_store: HashMap<String, String> = HashMap::new();
        for bm in bind_re.captures_iter(&safe) {
            if let Some(sf) = factory_file.get(bm.text(2).unwrap_or("")) {
                var_store.insert(bm.text(1).unwrap_or("").to_string(), sf.clone());
            }
        }
        if var_store.is_empty() {
            continue;
        }

        // 3. Link `<var>.<method>(` → the action function node in the store's file.
        let nodes_in_file = ctx.get_nodes_in_file(file);
        let fallback_dispatcher = nodes_in_file.iter().find(|n| n.kind == NodeKind::Component).cloned(); // .vue top-level setup
        let mut added = 0;
        for cm in call_re.captures_iter(&safe) {
            if added >= PINIA_FANOUT_CAP {
                break;
            }
            let Some(store_file) = var_store.get(cm.text(1).unwrap_or("")) else { continue };
            let method = cm.text(2).unwrap_or("");
            let line = line_of(&safe, cm.get(0).unwrap().start());
            let Some(disp) = enclosing_fn(&nodes_in_file, line).or_else(|| fallback_dispatcher.clone()) else { continue };
            let target = ctx
                .get_nodes_by_name(method)
                .iter()
                .find(|n| n.kind == NodeKind::Function && &n.file_path == store_file)
                .cloned();
            let Some(target) = target else { continue };
            if target.id == disp.id || !seen.insert(format!("{}>{}", disp.id, target.id)) {
                continue;
            }
            edges.push(calls_edge(
                &disp.id,
                &target.id,
                Some(line),
                meta! { "synthesizedBy" => "pinia-store", "via" => method, "registeredAt" => format!("{file}:{line}") },
            ));
            added += 1;
        }
    }
    edges
}

// ── Vuex string-keyed dispatch / commit bridge ───────────────────────────────

const VUEX_FANOUT_CAP: usize = 120;

/// A path segment (dir or filename stem) equals `seg`.
fn path_has_segment(file_path: &str, seg: &str) -> bool {
    Re::cached(&format!(r"[\\/]{}[\\/.]", escape_regex(seg)), "").is_match(file_path)
}

pub fn vuex_dispatch_edges(ctx: &Ctx) -> Vec<Edge> {
    let store_file_cache: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
    let signal_re = re!(r"\bdefineStore\b|\bcreateStore\b|\bVuex\b|\bmutations\b|\bactions\b|\bgetters\b|\bnamespaced\b", "g");
    let is_store_file = |file: &str| -> bool {
        if let Some(v) = store_file_cache.borrow().get(file) {
            return *v;
        }
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(c) = ctx.read_file(file) {
            for sm in signal_re.captures_iter(&c) {
                seen.insert(sm.text(0).unwrap_or("").to_string());
                if seen.len() >= 2 {
                    break;
                }
            }
        }
        let v = seen.len() >= 2;
        store_file_cache.borrow_mut().insert(file.to_string(), v);
        v
    };

    let resolve = |key: &str, dispatch_file: &str| -> Option<NodeRef> {
        let segs: Vec<&str> = key.split('/').collect();
        let action = segs[segs.len() - 1];
        let cands: Vec<NodeRef> =
            ctx.get_nodes_by_name(action).iter().filter(|n| n.kind == NodeKind::Function && is_store_file(&n.file_path)).cloned().collect();
        if cands.is_empty() {
            return None;
        }
        if segs.len() > 1 {
            let module = segs[segs.len() - 2]; // immediate namespace ≈ the module file
            return cands
                .iter()
                .find(|c| path_has_segment(&c.file_path, module))
                .cloned()
                .or_else(|| if cands.len() == 1 { Some(cands[0].clone()) } else { None });
        }
        // Root key: a local `commit('M')` inside an action targets the same module file;
        // otherwise accept only an unambiguous single store-wide match.
        cands.iter().find(|c| c.file_path == dispatch_file).cloned().or_else(|| if cands.len() == 1 { Some(cands[0].clone()) } else { None })
    };

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let dispatch_re = re!(r##"\b(?:dispatch|commit)\s*\(\s*['"]([A-Za-z][\w/]*)['"]"##, "g");
    for file in ctx.get_all_files() {
        if !pinia_consumer_ext(&file) {
            continue;
        }
        let Some(content) = ctx.read_file(&file) else { continue };
        if content.is_empty() || (!content.contains("dispatch(") && !content.contains("commit(")) {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, comment_lang_for(&file));
        let nodes_in_file = ctx.get_nodes_in_file(&file);
        let fallback = nodes_in_file.iter().find(|n| n.kind == NodeKind::Component).cloned(); // .vue top-level
        let mut added = 0;
        for m in dispatch_re.captures_iter(&safe) {
            if added >= VUEX_FANOUT_CAP {
                break;
            }
            let key = m.text(1).unwrap_or("");
            let line = line_of(&safe, m.get(0).unwrap().start());
            let Some(disp) = enclosing_fn(&nodes_in_file, line).or_else(|| fallback.clone()) else { continue };
            let Some(target) = resolve(key, &file) else { continue };
            if target.id == disp.id || !seen.insert(format!("{}>{}", disp.id, target.id)) {
                continue;
            }
            edges.push(calls_edge(
                &disp.id,
                &target.id,
                Some(line),
                meta! { "synthesizedBy" => "vuex-dispatch", "via" => key, "registeredAt" => format!("{file}:{line}") },
            ));
            added += 1;
        }
    }
    edges
}

// ── Celery task dispatch (Python) ─────────────────────────────────────────────

const CELERY_FANOUT_CAP: usize = 80;
const CELERY_DECORATOR_LOOKBACK: i64 = 12; // max lines above a `def` to scan for its decorators

pub fn celery_dispatch_edges(ctx: &Ctx) -> Vec<Edge> {
    let py_ext = re!(r"\.py$");
    let decl_re = re!(r"^(?:async\s+def|def|class)\b");
    let deco_re = re!(r"@\s*(?:[A-Za-z_][\w.]*\.)?(?:shared_task|task)\b");
    // Memoize the decorator check per task-candidate node.
    let task_cache: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
    let is_celery_task = |node: &NodeRef| -> bool {
        if let Some(v) = task_cache.borrow().get(&node.id) {
            return *v;
        }
        let mut v = false;
        if node.kind == NodeKind::Function && py_ext.is_match(&node.file_path) {
            if let Some(content) = ctx.read_file(&node.file_path) {
                let lines: Vec<&str> = content.split('\n').collect();
                // startLine is the `def` line (decorators sit ABOVE it). Walk upward, stopping at
                // the previous declaration.
                let stop = (node.start_line as i64 - 1 - CELERY_DECORATOR_LOOKBACK).max(0);
                let mut i = node.start_line as i64 - 2;
                while i >= stop {
                    let t = js_trim(lines.get(i as usize).copied().unwrap_or(""));
                    if decl_re.is_match(t) {
                        break; // previous decl → stop
                    }
                    if deco_re.is_match(t) {
                        v = true;
                        break;
                    }
                    i -= 1;
                }
            }
        }
        task_cache.borrow_mut().insert(node.id.clone(), v);
        v
    };

    let resolve = |name: &str, dispatch_file: &str| -> Option<NodeRef> {
        let cands: Vec<NodeRef> =
            ctx.get_nodes_by_name(name).iter().filter(|n| n.kind == NodeKind::Function && is_celery_task(n)).cloned().collect();
        if cands.is_empty() {
            return None;
        }
        if cands.len() == 1 {
            return Some(cands[0].clone());
        }
        // Cross-module name collision: prefer a task defined in the dispatching file, else bail.
        cands.iter().find(|c| c.file_path == dispatch_file).cloned()
    };

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let dispatch_re = re!(r"\b([A-Za-z_]\w*)\s*\.\s*(?:delay|apply_async)\s*\(", "g");
    for file in ctx.get_all_files() {
        if !py_ext.is_match(&file) {
            continue;
        }
        let Some(content) = ctx.read_file(&file) else { continue };
        if content.is_empty() || (!content.contains(".delay(") && !content.contains(".apply_async(")) {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "python");
        let nodes_in_file = ctx.get_nodes_in_file(&file);
        let mut added = 0;
        for m in dispatch_re.captures_iter(&safe) {
            if added >= CELERY_FANOUT_CAP {
                break;
            }
            let name = m.text(1).unwrap_or("");
            let line = line_of(&safe, m.get(0).unwrap().start());
            let Some(disp) = enclosing_fn(&nodes_in_file, line) else { continue }; // module-level dispatch
            let Some(target) = resolve(name, &file) else { continue };
            if target.id == disp.id || !seen.insert(format!("{}>{}", disp.id, target.id)) {
                continue;
            }
            edges.push(calls_edge(
                &disp.id,
                &target.id,
                Some(line),
                meta! { "synthesizedBy" => "celery-dispatch", "via" => name, "registeredAt" => format!("{file}:{line}") },
            ));
            added += 1;
        }
    }
    edges
}
