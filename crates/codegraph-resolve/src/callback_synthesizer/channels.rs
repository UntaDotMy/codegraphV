//! Field-backed observer channels, closure collections, string-keyed emitters, and the
//! framework re-render hops (React / Flutter / ArkUI), plus C++ virtual overrides.
//! Port of the first half of `src/resolution/callback-synthesizer.ts`.

use super::common::*;
use crate::ctx::Ctx;
use crate::meta;
use crate::name_matcher::cpp_overload::cpp_specialization_arguments;
use crate::synth_utils::{enclosing_fn, make_line_at};
use crate::types::NodeRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::{js_trim, utf16_len};
use codegraph_types::{Edge, EdgeKind, Language, NodeKind};
use indexmap::{IndexMap, IndexSet};
use serde_json::Value;
use std::collections::HashSet;
use std::rc::Rc;

const CC_FANOUT_CAP: usize = 8; // skip a field name with more dispatchers/registrars than this
const ARKUI_EMITTER_FANOUT_CAP: usize = 8;
const ARKUI_ARRAY_MUTATORS: &str = "push|pop|shift|unshift|splice|sort|reverse|fill";
const ARKUI_REACTIVE_DECORATORS: [&str; 13] = [
    "State", "Prop", "Link", "Provide", "Consume", "StorageLink", "StorageProp", "LocalStorageLink",
    "LocalStorageProp", "ObjectLink", "Local", "Provider", "Consumer",
];

fn is_registrar_name(name: &str) -> bool {
    let re = re!(r"^(on[A-Z]\w*|subscribe|addListener|addEventListener|register|watch|listen|addCallback)$");
    re.is_match(name)
}

fn is_dispatcher_name(name: &str) -> bool {
    re!(r"(emit|trigger|notify|dispatch|fire|publish|flush)", "i").is_match(name)
}

fn registrar_field(src: &str) -> Option<String> {
    re!(r"this\.(\w+)\.(?:add|push|set)\(").captures(src).and_then(|m| m.text(1).map(String::from))
}

fn dispatcher_field(src: &str) -> Option<String> {
    if let Some(m) = re!(r"\bof\s+(?:Array\.from\(\s*)?this\.(\w+)").captures(src) {
        if re!(r"\b\w+\s*\(").is_match(src) {
            return m.text(1).map(String::from);
        }
    }
    re!(r"this\.(\w+)\.forEach\(").captures(src).and_then(|m| m.text(1).map(String::from))
}

/// Phase 1: field-backed observer channels (registrar/dispatcher share a store).
pub fn field_channel_edges(ctx: &Ctx) -> Vec<Edge> {
    struct Site {
        node: NodeRef,
        field: String,
    }
    let mut registrars: Vec<Site> = Vec::new();
    let mut dispatchers: Vec<Site> = Vec::new();

    each_method_and_function(ctx, |m| {
        let is_reg = is_registrar_name(&m.name);
        let is_disp = is_dispatcher_name(&m.name);
        if !is_reg && !is_disp {
            return;
        }
        let Some(src) = node_source(ctx, &m) else { return };
        if is_reg {
            if let Some(f) = registrar_field(&src) {
                registrars.push(Site { node: Rc::clone(&m), field: f });
            }
        }
        if is_disp {
            if let Some(f) = dispatcher_field(&src) {
                dispatchers.push(Site { node: Rc::clone(&m), field: f });
            }
        }
    });

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for reg in &registrars {
        let ch_dispatchers: Vec<&Site> =
            dispatchers.iter().filter(|d| d.node.file_path == reg.node.file_path && d.field == reg.field).collect();
        if ch_dispatchers.is_empty() {
            continue;
        }
        let arg_re = Re::cached(&format!(r"{}\s*\(\s*(this\.\w+|\w+)\s*(?=[,)])", escape_regex(&reg.node.name)), "");
        let mut added = 0;
        for e in incoming(ctx, &reg.node.id, &[EdgeKind::Calls]) {
            if added >= MAX_CALLBACKS_PER_CHANNEL {
                break;
            }
            let Some(e_line) = e.line.filter(|l| *l != 0) else { continue };
            let Some(caller) = node(ctx, &e.source) else { continue };
            let Some(content) = ctx.read_file(&caller.file_path) else { continue };
            let Some(line) = content.split('\n').nth(e_line as usize - 1) else { continue };
            let Some(am) = arg_re.captures(line) else { continue };
            let arg = am.text(1).unwrap_or("");
            // Reuse the resolved value at this registration site: it retains the receiver's
            // class/inheritance and import binding, unlike a name lookup.
            let refs: Vec<Edge> = outgoing(ctx, &caller.id, &[EdgeKind::References])
                .into_iter()
                .filter(|r| {
                    r.line == Some(e_line)
                        && r.metadata.as_ref().is_some_and(|m| {
                            m.get("fnRef") == Some(&Value::Bool(true)) && m.get("refName").and_then(Value::as_str) == Some(arg)
                        })
                })
                .collect();
            if refs.len() != 1 {
                continue;
            }
            let Some(f) = node(ctx, &refs[0].target) else { continue };
            if f.kind != NodeKind::Method && f.kind != NodeKind::Function {
                continue;
            }
            let resolved_by_import =
                refs[0].metadata.as_ref().and_then(|m| m.get("resolvedBy")).and_then(Value::as_str) == Some("import");
            if !arg.starts_with("this.") && f.file_path != caller.file_path && !resolved_by_import {
                continue;
            }
            for disp in &ch_dispatchers {
                if disp.node.id == f.id {
                    continue;
                }
                let key = format!("{}>{}", disp.node.id, f.id);
                if !seen.insert(key) {
                    continue;
                }
                edges.push(calls_edge(
                    &disp.node.id,
                    &f.id,
                    Some(disp.node.start_line),
                    meta! {
                        "synthesizedBy" => "callback", "via" => reg.node.name.as_str(), "field" => reg.field.as_str(),
                        // Where the callback was wired up (`scene.onUpdate(this.triggerRender)`).
                        "registeredAt" => format!("{}:{}", caller.file_path, e_line),
                    },
                ));
                added += 1;
            }
        }
    }
    edges
}

/// Closure-collection dispatch: dispatcher iterates a closure-collection property invoking each
/// element; registrar appends a closure to the same-named property.
pub fn closure_collection_edges(ctx: &Ctx) -> Vec<Edge> {
    struct Site {
        node: NodeRef,
        line: u32,
    }
    let mut dispatchers: IndexMap<String, Vec<Site>> = IndexMap::new();
    let mut registrars: IndexMap<String, Vec<Site>> = IndexMap::new();

    fn add_reg(registrars: &mut IndexMap<String, Vec<Site>>, field: Option<&str>, node: &NodeRef, abs_line: u32) {
        let Some(field) = field.filter(|f| !f.is_empty() && !is_all_digits(f)) else { return };
        // `$0.append` mis-captures the `0`; the write-RE owns that field.
        let arr = registrars.entry(field.to_string()).or_default();
        if !arr.iter().any(|r| r.node.id == node.id) {
            arr.push(Site { node: Rc::clone(node), line: abs_line });
        }
    }

    each_method_and_function(ctx, |m| {
        if m.language != Language::Swift && m.language != Language::Kotlin {
            return;
        }
        let Some(src) = node_source(ctx, &m) else { return };
        let has_for_each = src.contains(".forEach");
        let has_append = src.contains(".append(") || src.contains(".add(") || src.contains(".push(") || src.contains(".insert(");
        if !has_for_each && !has_append {
            return;
        }
        let line_at = make_line_at(&src, m.start_line);

        if has_for_each {
            for d in re!(r"(\w+)\.forEach\s*\{\s*(?:\$0|it)\s*\(", "g").captures_iter(&src) {
                let field = d.text(1).unwrap_or("").to_string();
                let arr = dispatchers.entry(field).or_default();
                if !arr.iter().any(|n| n.node.id == m.id) {
                    arr.push(Site { node: Rc::clone(&m), line: line_at.at(d.get(0).unwrap().start()) });
                }
            }
        }
        if has_append {
            for w in re!(r"(\w+)\.write\s*\{\s*\$0(?:\.(\w+))?\.(?:append|add|push|insert)\s*\(", "g").captures_iter(&src) {
                // nested `$0.streams` else the `.write` receiver
                let field = w.text(2).filter(|s| !s.is_empty()).or(w.text(1));
                add_reg(&mut registrars, field, &m, line_at.at(w.get(0).unwrap().start()));
            }
            for a in re!(r"(\w+)\.(?:append|add|push|insert)\s*\(", "g").captures_iter(&src) {
                add_reg(&mut registrars, a.text(1), &m, line_at.at(a.get(0).unwrap().start()));
            }
        }
    });

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (field, disps) in &dispatchers {
        let Some(regs) = registrars.get(field).filter(|r| !r.is_empty()) else { continue };
        if disps.len() > CC_FANOUT_CAP || regs.len() > CC_FANOUT_CAP {
            continue; // generic field — can't pair confidently
        }
        for disp in disps {
            for reg in regs {
                if disp.node.id == reg.node.id {
                    continue;
                }
                if !seen.insert(format!("{}>{}", disp.node.id, reg.node.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &disp.node.id,
                    &reg.node.id,
                    Some(disp.line),
                    meta! {
                        "synthesizedBy" => "closure-collection", "field" => field.as_str(),
                        "registeredAt" => format!("{}:{}", reg.node.file_path, reg.line),
                    },
                ));
            }
        }
    }
    edges
}

/// Phase 2: string-keyed EventEmitter channels (on('e', fn) ↔ emit('e')).
pub fn event_emitter_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut emits_by_event: IndexMap<String, IndexSet<String>> = IndexMap::new();
    let mut handlers_by_event: IndexMap<String, IndexMap<String, String>> = IndexMap::new();

    for file in ctx.get_all_files() {
        let Some(content) = ctx.read_file(&file) else { continue };
        if content.is_empty() {
            continue;
        }
        let has_emit = content.contains(".emit(") || content.contains(".fire(") || content.contains(".dispatchEvent(");
        let has_on = content.contains(".on(") || content.contains(".once(") || content.contains(".addListener(");
        if !has_emit && !has_on {
            continue;
        }
        let nodes_in_file = ctx.get_nodes_in_file(&file);
        let line_of = make_line_at(&content, 1);

        if has_emit {
            for m in re!(r##"\.(?:emit|fire|dispatchEvent)\(\s*['"]([^'"]+)['"]"##, "g").captures_iter(&content) {
                let Some(disp) = enclosing_fn(&nodes_in_file, line_of.at(m.get(0).unwrap().start())) else { continue };
                emits_by_event.entry(m.text(1).unwrap_or("").to_string()).or_default().insert(disp.id.clone());
            }
        }
        if has_on {
            let on_re = re!(
                r##"\.(?:on|once|addListener)\(\s*['"]([^'"]+)['"]\s*,\s*(?:function\s+(\w+)|(?:this\.)?(\w+))"##,
                "g"
            );
            for m in on_re.captures_iter(&content) {
                let Some(handler_name) = m.text(2).filter(|s| !s.is_empty()).or(m.text(3)).filter(|s| !s.is_empty()) else {
                    continue;
                };
                let Some(handler) =
                    ctx.get_nodes_by_name(handler_name).iter().find(|n| n.kind == NodeKind::Function || n.kind == NodeKind::Method).cloned()
                else {
                    continue;
                };
                handlers_by_event
                    .entry(m.text(1).unwrap_or("").to_string())
                    .or_default()
                    .insert(handler.id.clone(), format!("{}:{}", file, line_of.at(m.get(0).unwrap().start())));
            }
        }
    }

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (event, dispatchers) in &emits_by_event {
        let Some(handlers) = handlers_by_event.get(event) else { continue };
        // Precision guard: a generic event name with many handlers/dispatchers can't be matched
        // without receiver-type info (Phase 3) — skip rather than over-link.
        if dispatchers.len() > EVENT_FANOUT_CAP || handlers.len() > EVENT_FANOUT_CAP {
            continue;
        }
        for d in dispatchers {
            for (h, registered_at) in handlers {
                if d == h {
                    continue;
                }
                if !seen.insert(format!("{d}>{h}")) {
                    continue;
                }
                edges.push(calls_edge(
                    d,
                    h,
                    None,
                    meta! { "synthesizedBy" => "event-emitter", "event" => event.as_str(), "registeredAt" => registered_at.as_str() },
                ));
            }
        }
    }
    edges
}

/// Phase 4: React class-component re-render. `this.setState(...)` → the class's `render()`.
pub fn react_render_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // A class can only emit here if it CONTAINS a method named `render` — one indexed name lookup
    // bounds the candidate set up front.
    let mut render_owners: HashSet<String> = HashSet::new();
    for n in ctx.get_nodes_by_name("render").iter() {
        if n.kind != NodeKind::Method {
            continue;
        }
        for e in incoming(ctx, &n.id, &[EdgeKind::Contains]) {
            render_owners.insert(e.source);
        }
    }
    if render_owners.is_empty() {
        return edges;
    }
    let setstate = re!(r"this\.setState\s*\(");
    each_node(ctx, NodeKind::Class, |cls| {
        if !render_owners.contains(&cls.id) {
            return;
        }
        let children: Vec<NodeRef> = contained_nodes(ctx, &cls.id).into_iter().filter(|n| n.kind == NodeKind::Method).collect();
        let Some(render) = children.iter().find(|n| n.name == "render") else { return };
        let mut added = 0;
        for m in &children {
            if added >= MAX_CALLBACKS_PER_CHANNEL {
                break;
            }
            if m.id == render.id {
                continue;
            }
            let Some(src) = node_source(ctx, m) else { continue };
            if !setstate.is_match(&src) {
                continue;
            }
            if !seen.insert(format!("{}>{}", m.id, render.id)) {
                continue;
            }
            edges.push(calls_edge(
                &m.id,
                &render.id,
                Some(m.start_line),
                meta! {
                    "synthesizedBy" => "react-render", "via" => "setState",
                    "registeredAt" => format!("{}:{}", render.file_path, render.start_line),
                },
            ));
            added += 1;
        }
    });
    edges
}

/// Phase 4b: Flutter setState → build (the Dart analog of react-render).
pub fn flutter_build_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let setstate = re!(r"\bsetState\s*\(");
    each_node(ctx, NodeKind::Class, |cls| {
        let children: Vec<NodeRef> = contained_nodes(ctx, &cls.id).into_iter().filter(|n| n.kind == NodeKind::Method).collect();
        let Some(build) = children.iter().find(|n| n.name == "build") else { return };
        if !build.file_path.ends_with(".dart") {
            return;
        }
        let mut added = 0;
        for m in &children {
            if added >= MAX_CALLBACKS_PER_CHANNEL {
                break;
            }
            if m.id == build.id {
                continue;
            }
            let Some(src) = node_source(ctx, m) else { continue };
            if !setstate.is_match(&src) {
                continue;
            }
            if !seen.insert(format!("{}>{}", m.id, build.id)) {
                continue;
            }
            edges.push(calls_edge(
                &m.id,
                &build.id,
                Some(m.start_line),
                meta! {
                    "synthesizedBy" => "flutter-build", "via" => "setState",
                    "registeredAt" => format!("{}:{}", build.file_path, build.start_line),
                },
            ));
            added += 1;
        }
    });
    edges
}

/// Phase 4b-ets: ArkUI state → build. Assigning a reactive-decorated property re-runs `build()`.
pub fn arkui_state_build_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    each_node_in(ctx, &[NodeKind::Struct], &[Language::ArkTs], |strukt| {
        let children = contained_nodes(ctx, &strukt.id);
        let Some(build) = children.iter().find(|n| n.kind == NodeKind::Method && n.name == "build") else { return };
        let reactive_props: Vec<&NodeRef> = children
            .iter()
            .filter(|n| {
                n.kind == NodeKind::Property
                    && n.decorators.as_ref().is_some_and(|ds| ds.iter().any(|d| ARKUI_REACTIVE_DECORATORS.contains(&d.as_str())))
            })
            .collect();
        if reactive_props.is_empty() {
            return;
        }
        let prop_alternation = reactive_props.iter().map(|p| escape_regex(&p.name)).collect::<Vec<_>>().join("|");
        // `this.count = …` / `+=` / `++` / `--` / `this.todos.push(…)`. The `=(?!=)` keeps
        // `this.done == x` comparisons out.
        let mutation_re = Re::cached(
            &format!(r"this\.(?:{prop_alternation})\s*(?:=(?!=)|\+\+|--|[+\-*/%&|^]=|\.(?:{ARKUI_ARRAY_MUTATORS})\s*\()"),
            "",
        );
        let mut added = 0;
        for m in &children {
            if added >= MAX_CALLBACKS_PER_CHANNEL {
                break;
            }
            if m.kind != NodeKind::Method || m.id == build.id {
                continue;
            }
            let Some(src) = node_source(ctx, m) else { continue };
            if !mutation_re.is_match(&codegraph_text::strip_comments_for_regex(&src, "typescript")) {
                continue;
            }
            if !seen.insert(format!("{}>{}", m.id, build.id)) {
                continue;
            }
            edges.push(calls_edge(
                &m.id,
                &build.id,
                Some(m.start_line),
                meta! {
                    "synthesizedBy" => "arkui-state", "via" => "state assignment",
                    "registeredAt" => format!("{}:{}", build.file_path, build.start_line),
                },
            ));
            added += 1;
        }
    });
    edges
}

/// The workspace module directories, longest first.
fn module_dirs(ctx: &Ctx) -> Vec<String> {
    let Some(ws) = ctx.get_workspace_packages() else { return Vec::new() };
    let mut seen: IndexSet<String> = IndexSet::new();
    for name in &ws.names_in_order {
        if let Some(dir) = ws.by_name.get(name) {
            seen.insert(dir.clone());
        }
    }
    let mut dirs: Vec<String> = seen.into_iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(utf16_len(d)));
    dirs
}

fn module_scope_of<'a>(dirs: &'a [String], file: &str) -> &'a str {
    for dir in dirs {
        if file == dir || (file.starts_with(dir.as_str()) && file[dir.len()..].starts_with('/')) {
            return dir;
        }
    }
    ""
}

/// The smallest method/function node enclosing `line`, by line span (first wins a tie).
fn tightest_enclosing(nodes: &[NodeRef], line: u32) -> Option<NodeRef> {
    nodes
        .iter()
        .filter(|n| (n.kind == NodeKind::Method || n.kind == NodeKind::Function) && n.start_line <= line && n.end_line >= line)
        .min_by_key(|n| n.end_line as i64 - n.start_line as i64)
        .cloned()
}

/// Phase 4b-ets2: HarmonyOS `@ohos.events.emitter` bridge.
pub fn arkui_emitter_edges(ctx: &Ctx) -> Vec<Edge> {
    struct Site {
        node_id: String,
        file: String,
        line: u32,
    }
    let mut emits: IndexMap<String, Vec<Site>> = IndexMap::new();
    let mut handlers: IndexMap<String, Vec<Site>> = IndexMap::new();
    let dirs = module_dirs(ctx);
    let call_re = re!(
        r"\bemitter\s*\.\s*(emit|on|once)\s*\(\s*([A-Za-z_$][\w$.]*|\{[^)]{0,120}?\beventId\s*:\s*[^,}]+[^)]*?\})",
        "g"
    );

    for file in ctx.get_all_files() {
        if !file.ends_with(".ets") {
            continue;
        }
        let Some(content) = ctx.read_file(&file) else { continue };
        if !content.contains("emitter.") {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "typescript");
        let nodes = ctx.get_nodes_in_file(&file);

        for m in call_re.captures_iter(&safe) {
            let verb = m.text(1).unwrap_or("");
            let arg = js_trim(m.text(2).unwrap_or(""));
            let line = line_of(&safe, m.get(0).unwrap().start());
            let Some(encl) = tightest_enclosing(&nodes, line) else { continue };

            // Recover the event key from the first argument.
            let mut key: Option<String> = None;
            let id_lit = if arg.starts_with('{') {
                re!(r"\beventId\s*:\s*([\w$.]+)").captures(arg).and_then(|c| c.text(1).map(String::from))
            } else {
                None
            };
            let token = id_lit.as_deref().unwrap_or(arg);
            if is_all_digits(token) {
                key = Some(format!("num:{file}:{token}")); // numeric: same-file only
            } else if token.contains('.') {
                key = Some(format!("name:{}:{token}", module_scope_of(&dirs, &file)));
            } else {
                // Local variable — chase its same-file declaration one level:
                // `let x = new EventsId(K)` / `const x = K`.
                let decl_re = Re::cached(
                    &format!(
                        r"\b{}\b\s*(?::[^=\n]+)?=\s*(?:new\s+[\w$.]+\(\s*([^)\n]+?)\s*\)|([\w$.]+))",
                        escape_regex(token)
                    ),
                    "",
                );
                let inner = decl_re.captures(&safe).and_then(|d| d.text(1).or(d.text(2)).map(|s| js_trim(s).to_string()));
                if let Some(inner) = inner.filter(|s| !s.is_empty()) {
                    if is_all_digits(&inner) {
                        key = Some(format!("num:{file}:{inner}"));
                    } else if re!(r"^[\w$.]+$").is_match(&inner) {
                        key = Some(format!("name:{}:{inner}", module_scope_of(&dirs, &file)));
                    }
                }
            }
            let Some(key) = key else { continue };

            let site = Site { node_id: encl.id.clone(), file: file.clone(), line };
            if verb == "emit" {
                emits.entry(key).or_default().push(site);
            } else {
                handlers.entry(key).or_default().push(site);
            }
        }
    }

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (key, emit_sites) in &emits {
        let Some(handler_sites) = handlers.get(key) else { continue };
        if emit_sites.len() > ARKUI_EMITTER_FANOUT_CAP || handler_sites.len() > ARKUI_EMITTER_FANOUT_CAP {
            continue;
        }
        let event_label = &key[key.rfind(':').map_or(0, |i| i + 1)..];
        for e in emit_sites {
            for h in handler_sites {
                if e.node_id == h.node_id {
                    continue;
                }
                if !seen.insert(format!("{}>{}", e.node_id, h.node_id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &e.node_id,
                    &h.node_id,
                    Some(e.line),
                    meta! { "synthesizedBy" => "arkui-emitter", "event" => event_label, "registeredAt" => format!("{}:{}", h.file, h.line) },
                ));
            }
        }
    }
    edges
}

/// Phase 4b-ets3: HarmonyOS page navigation — `router.pushUrl({ url: 'pages/Detail' })` → the
/// `@Entry struct` of that page.
pub fn arkui_router_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let all_files = ctx.get_all_files();
    let dirs = module_dirs(ctx);
    let router_re = re!(
        r##"\brouter\s*\.\s*(?:pushUrl|replaceUrl)\s*\(\s*\{[^)]{0,200}?\burl\s*:\s*['"]([\w\-./]+)['"]"##,
        "g"
    );

    for file in &all_files {
        if !file.ends_with(".ets") {
            continue;
        }
        let Some(content) = ctx.read_file(file) else { continue };
        if !content.contains("router.") {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "typescript");
        let nodes = ctx.get_nodes_in_file(file);

        for m in router_re.captures_iter(&safe) {
            let url = m.text(1).unwrap_or("");
            let line = line_of(&safe, m.get(0).unwrap().start());
            let Some(encl) = tightest_enclosing(&nodes, line) else { continue };

            let suffix = format!("/src/main/ets/{url}.ets");
            let mut candidates: Vec<&String> = all_files.iter().filter(|f| f.ends_with(&suffix)).collect();
            if candidates.len() > 1 {
                let scope = module_scope_of(&dirs, file);
                let same_module: Vec<&String> = candidates.iter().copied().filter(|f| module_scope_of(&dirs, f) == scope).collect();
                if !same_module.is_empty() {
                    candidates = same_module;
                }
            }
            if candidates.len() != 1 {
                continue; // ambiguous or unresolved — never guess
            }
            let cand = candidates[0];
            let page = ctx
                .get_nodes_in_file(cand)
                .iter()
                .find(|n| n.kind == NodeKind::Struct && n.decorators.as_ref().is_some_and(|d| d.iter().any(|x| x == "Entry")))
                .cloned();
            let Some(page) = page else { continue };

            if !seen.insert(format!("{}>{}", encl.id, page.id)) {
                continue;
            }
            edges.push(calls_edge(
                &encl.id,
                &page.id,
                Some(line),
                meta! { "synthesizedBy" => "arkui-route", "event" => url, "registeredAt" => format!("{}:{}", cand, page.start_line) },
            ));
        }
    }
    edges
}

/// Phase 4c: C++ virtual override — each base method → the subclass method of the same name.
pub fn cpp_override_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let methods_of = |class_id: &str| -> Vec<NodeRef> {
        contained_nodes(ctx, class_id).into_iter().filter(|n| n.kind == NodeKind::Method).collect()
    };
    let overridable = |m: &NodeRef| cpp_specialization_arguments(m, ctx).is_none();
    each_node(ctx, NodeKind::Class, |cls| {
        let sub_methods: Vec<NodeRef> = methods_of(&cls.id).into_iter().filter(|n| n.language == Language::Cpp).collect();
        if sub_methods.is_empty() {
            return;
        }
        for ext in outgoing(ctx, &cls.id, &[EdgeKind::Extends]) {
            let Some(base) = node(ctx, &ext.target) else { continue };
            if base.language != Language::Cpp || base.id == cls.id {
                continue;
            }
            let mut base_methods: IndexMap<String, NodeRef> = IndexMap::new();
            for m in methods_of(&base.id).into_iter().filter(|m| overridable(m)) {
                base_methods.insert(m.name.clone(), m);
            }
            let mut added = 0;
            for m in &sub_methods {
                if added >= MAX_CALLBACKS_PER_CHANNEL {
                    break;
                }
                let Some(bm) = base_methods.get(&m.name) else { continue };
                if bm.id == m.id || !overridable(m) {
                    continue;
                }
                if !seen.insert(format!("{}>{}", bm.id, m.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &bm.id,
                    &m.id,
                    Some(bm.start_line),
                    meta! {
                        "synthesizedBy" => "cpp-override", "via" => m.name.as_str(),
                        "registeredAt" => format!("{}:{}", m.file_path, m.start_line),
                    },
                ));
                added += 1;
            }
        }
    });
    edges
}
