//! Cross-language and file-convention channels: React Native events, Expo / classic RN native
//! pairing, Fabric views, MyBatis, Gin middleware, Delphi forms, SvelteKit loaders. Port of the
//! middle of `src/resolution/callback-synthesizer.ts`.

use super::common::*;
use crate::ctx::{q, Ctx};
use crate::import_resolver::resolve_import_path;
use crate::meta;
use crate::synth_utils::{enclosing_fn, enclosing_value, make_line_at};
use crate::types::NodeRef;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::{Edge, EdgeKind, Language, NodeKind};
use indexmap::{IndexMap, IndexSet};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

fn constant_kind(k: NodeKind) -> bool {
    matches!(k, NodeKind::Constant | NodeKind::Variable | NodeKind::Field | NodeKind::Property | NodeKind::EnumMember)
}

/// The string an event-name constant holds, read off its declaration line, scoped as the language
/// scopes the name. `None` unless those declarations agree on one literal.
fn constant_event_name(expr: &str, file: &str, ctx: &Ctx, memo: &RefCell<HashMap<String, Option<String>>>) -> Option<String> {
    let key = format!("{file}\0{expr}");
    if let Some(v) = memo.borrow().get(&key) {
        return v.clone();
    }
    let stripped = re!(r"\.rawValue$").replace_first(expr, |_| String::new());
    let parts: Vec<&str> = stripped.split('.').collect();
    let name = parts[parts.len() - 1];
    let owner = if parts.len() > 1 { Some(parts[parts.len() - 2]) } else { None };
    let mut value: Option<String> = None;
    if re!(r"^[A-Za-z_$][\w$]*$").is_match(name) && name != "this" && (parts.len() <= 2 || parts[0] != "this") {
        let read = |n: &NodeRef| -> Option<String> {
            let lines = ctx.get_file_lines(&n.file_path);
            let line = lines.as_ref().and_then(|l| l.get((n.start_line as usize).wrapping_sub(1))).map(String::as_str).unwrap_or("");
            let at = line.find(name)?;
            let rest = &line[at + name.len()..];
            let m = re!(r##"^\s*(?::[^=;]*)?[=:]\s*@?(["'`])([^"'`]+)\1"##)
                .captures(rest)
                .or_else(|| re!(r##"^\s+@?(")([^"]+)""##).captures(rest));
            m.and_then(|m| m.text(2).map(String::from))
        };
        let declared_in = |f: &str| -> Vec<NodeRef> {
            ctx.get_nodes_in_file(f).iter().filter(|n| n.name == name && constant_kind(n.kind)).cloned().collect()
        };
        let js = re!(r"\.(?:[cm]?[jt]sx?)$").is_match(file);
        let language = if re!(r"\.tsx$").is_match(file) {
            Language::Tsx
        } else if re!(r"\.[cm]?ts$").is_match(file) {
            Language::TypeScript
        } else if file.ends_with(".jsx") {
            Language::Jsx
        } else {
            Language::JavaScript
        };
        let imported_file = |local: &str| -> Option<String> {
            if !js {
                return None;
            }
            let mappings = ctx.get_import_mappings(file, language);
            let binding = mappings.iter().find(|m| m.local_name == local)?;
            resolve_import_path(&binding.source, file, language, ctx)
        };
        let decls: Vec<NodeRef> = match owner {
            None => {
                let mut d = declared_in(file);
                if d.is_empty() {
                    if let Some(from) = imported_file(name) {
                        d = declared_in(&from);
                    }
                }
                d
            }
            Some(owner) => match imported_file(owner) {
                Some(from) => declared_in(&from),
                None => {
                    let owner_re = Re::cached(&format!(r"(?:^|[.:]){}(?:[.:]|$)", escape_regex(owner)), "");
                    ctx.get_nodes_by_name(name)
                        .iter()
                        .filter(|n| {
                            constant_kind(n.kind) && {
                                let qn = &n.qualified_name;
                                let head = match qn.rfind(name) {
                                    Some(i) => &qn[..i],
                                    // `slice(0, -1)`: everything but the last character
                                    None => &qn[..qn.char_indices().next_back().map_or(0, |(i, _)| i)],
                                };
                                owner_re.is_match(head)
                            }
                        })
                        .cloned()
                        .collect()
                }
            },
        };
        let mut values: IndexSet<String> = IndexSet::new();
        for d in &decls {
            if let Some(v) = read(d) {
                values.insert(v);
            }
        }
        value = if values.len() == 1 { values.into_iter().next() } else { None };
    }
    memo.borrow_mut().insert(key, value.clone());
    value
}

/// React Native cross-language event channel: native dispatch site → JS handler, keyed by the
/// literal event name.
pub fn rn_event_edges(ctx: &Ctx) -> Vec<Edge> {
    let constants: RefCell<HashMap<String, Option<String>>> = RefCell::new(HashMap::new());
    // Native dispatchers (source = the native method whose body sends the event) and JS handlers
    // (target = the function/method registered as the listener) keyed by event name.
    let mut native_dispatchers_by_event: IndexMap<String, IndexSet<String>> = IndexMap::new();
    let mut js_handlers_by_event: IndexMap<String, IndexMap<String, String>> = IndexMap::new();

    let objc_send = re!(r##"\bsendEventWithName\s*:\s*@"([^"]+)""##, "g");
    let swift_send = re!(r##"\bsendEvent\s*\(\s*withName\s*:\s*"([^"]+)""##, "g");
    let jvm_emit = re!(r##"\.emit\s*\(\s*"([^"]+)"\s*,"##, "g");
    let native_sendevent = re!(r##"\bsendEvent\s*\([^;{}]*?"([^"]+)""##, "g");
    let objc_send_const = re!(r"\bsendEventWithName\s*:\s*([A-Za-z_]\w*)\b", "g");
    let swift_send_const = re!(r"\bsendEvent\s*\(\s*withName\s*:\s*([A-Za-z_][\w.]*)", "g");
    let jvm_emit_const = re!(r"\.emit\s*\(\s*([A-Za-z_][\w.]*)\s*,", "g");
    let native_sendevent_const = re!(r"\bsendEvent\s*\(\s*[A-Za-z_][\w.]*\s*,\s*([A-Za-z_][\w.]*)\s*,", "g");
    let js_listen_const = re!(
        r"\.(?:on|once|addListener)\(\s*((?:[A-Za-z_$][\w$]*\.)*[A-Za-z_$][\w$]*)\s*,\s*([A-Za-z_$][\w$.]*|(?:async\s*)?(?:\([^)]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>|function\s*\())",
        "g"
    );
    let addlistener_any = re!(r##"\.(?:on|once|addListener)\(\s*['"]([^'"]+)['"]\s*,\s*([A-Za-z_][\w.]*)"##, "g");
    let addlistener_inline = re!(
        r##"\.(?:on|once|addListener)\(\s*['"]([^'"]+)['"]\s*,\s*(?:async\s*)?(?:\([^)]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>|function\s*\()"##,
        "g"
    );

    for file in ctx.get_all_files() {
        let Some(content) = ctx.read_file(&file) else { continue };
        if content.is_empty() {
            continue;
        }
        let nodes_in_file = ctx.get_nodes_in_file(&file);
        let line_of = make_line_at(&content, 1);
        let mut add_dispatcher = |event: &str, line: u32| {
            let Some(disp) = enclosing_fn(&nodes_in_file, line) else { return };
            native_dispatchers_by_event.entry(event.to_string()).or_default().insert(disp.id.clone());
        };

        // ObjC side: `sendEventWithName:@"X"` only fires inside `.m`/`.mm` files.
        if file.ends_with(".m") || file.ends_with(".mm") {
            for m in objc_send.captures_iter(&content) {
                if let Some(ev) = m.text(1).filter(|s| !s.is_empty()) {
                    add_dispatcher(ev, line_of.at(m.get(0).unwrap().start()));
                }
            }
            for m in objc_send_const.captures_iter(&content) {
                if let Some(event) = constant_event_name(m.text(1).unwrap_or(""), &file, ctx, &constants) {
                    add_dispatcher(&event, line_of.at(m.get(0).unwrap().start()));
                }
            }
        }

        // Swift side: same RCTEventEmitter method, parens/named-args syntax.
        if file.ends_with(".swift") {
            for m in swift_send.captures_iter(&content) {
                if let Some(ev) = m.text(1).filter(|s| !s.is_empty()) {
                    add_dispatcher(ev, line_of.at(m.get(0).unwrap().start()));
                }
            }
            for m in native_sendevent.captures_iter(&content) {
                if let Some(ev) = m.text(1).filter(|s| !s.is_empty()) {
                    add_dispatcher(ev, line_of.at(m.get(0).unwrap().start()));
                }
            }
            for m in swift_send_const.captures_iter(&content) {
                if let Some(event) = constant_event_name(m.text(1).unwrap_or(""), &file, ctx, &constants) {
                    add_dispatcher(&event, line_of.at(m.get(0).unwrap().start()));
                }
            }
        }

        // JVM side: `.emit("X", …)` in Java/Kotlin, plus the common `sendEvent(ctx, "X", body)`
        // wrapper.
        if file.ends_with(".java") || file.ends_with(".kt") {
            for m in jvm_emit.captures_iter(&content) {
                if let Some(ev) = m.text(1).filter(|s| !s.is_empty()) {
                    add_dispatcher(ev, line_of.at(m.get(0).unwrap().start()));
                }
            }
            for m in native_sendevent.captures_iter(&content) {
                if let Some(ev) = m.text(1).filter(|s| !s.is_empty()) {
                    add_dispatcher(ev, line_of.at(m.get(0).unwrap().start()));
                }
            }
            for re in [&jvm_emit_const, &native_sendevent_const] {
                for m in re.captures_iter(&content) {
                    if let Some(event) = constant_event_name(m.text(1).unwrap_or(""), &file, ctx, &constants) {
                        add_dispatcher(&event, line_of.at(m.get(0).unwrap().start()));
                    }
                }
            }
        }

        // JS subscribers. Restrict to JS-family files so a native file's `addListener:` (the ObjC
        // method) doesn't get mistaken for a JS subscription.
        if [".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs"].iter().any(|e| file.ends_with(e)) {
            for m in addlistener_any.captures_iter(&content) {
                let (Some(event), Some(arg)) =
                    (m.text(1).filter(|s| !s.is_empty()), m.text(2).filter(|s| !s.is_empty()))
                else {
                    continue;
                };
                let bare_name = if arg.contains('.') { &arg[arg.rfind('.').unwrap() + 1..] } else { arg };
                // Try a named-symbol match first (matches the in-language semantic).
                let named = ctx
                    .get_nodes_by_name(bare_name)
                    .iter()
                    .find(|n| n.kind == NodeKind::Function || n.kind == NodeKind::Method)
                    .map(|n| n.id.clone());
                let at_line = line_of.at(m.get(0).unwrap().start());
                // Fall back to the enclosing function — the subscribe-wrapper pattern — and then to
                // the smallest enclosing constant / variable (a JS object-literal API shape).
                let target_id = named
                    .or_else(|| enclosing_fn(&nodes_in_file, at_line).map(|n| n.id.clone()))
                    .or_else(|| enclosing_value(&nodes_in_file, at_line).map(|n| n.id.clone()));
                let Some(target_id) = target_id else { continue };
                js_handlers_by_event
                    .entry(event.to_string())
                    .or_default()
                    .insert(target_id, format!("{file}:{at_line}"));
            }
            // Inline listeners — attributed to the enclosing function.
            for m in addlistener_inline.captures_iter(&content) {
                let Some(event) = m.text(1).filter(|s| !s.is_empty()) else { continue };
                let at_line = line_of.at(m.get(0).unwrap().start());
                let Some(enclosing) = enclosing_fn(&nodes_in_file, at_line) else { continue };
                let map = js_handlers_by_event.entry(event.to_string()).or_default();
                if !map.contains_key(&enclosing.id) {
                    map.insert(enclosing.id.clone(), format!("{file}:{at_line}"));
                }
            }
            // A constant event name: the listener lands where a literal one would — the named
            // handler when it is a node, else the enclosing function.
            for m in js_listen_const.captures_iter(&content) {
                let Some(event) = constant_event_name(m.text(1).unwrap_or(""), &file, ctx, &constants) else { continue };
                let arg = m.text(2).unwrap_or("");
                let bare = if re!(r"^[A-Za-z_$][\w$.]*$").is_match(arg) {
                    Some(&arg[arg.rfind('.').map_or(0, |i| i + 1)..])
                } else {
                    None
                };
                let line = line_of.at(m.get(0).unwrap().start());
                // The handler this file names — never a same-named function elsewhere.
                let named = bare.and_then(|b| {
                    nodes_in_file.iter().find(|n| (n.kind == NodeKind::Function || n.kind == NodeKind::Method) && n.name == b).cloned()
                });
                let Some(target) = named.or_else(|| enclosing_fn(&nodes_in_file, line)) else { continue };
                let map = js_handlers_by_event.entry(event).or_default();
                if !map.contains_key(&target.id) {
                    map.insert(target.id.clone(), format!("{file}:{line}"));
                }
            }
        }
    }

    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (event, dispatchers) in &native_dispatchers_by_event {
        let Some(handlers) = js_handlers_by_event.get(event) else { continue };
        // Same fan-out guard as the in-language channel.
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
                    meta! { "synthesizedBy" => "rn-event-channel", "event" => event.as_str(), "registeredAt" => registered_at.as_str() },
                ));
            }
        }
    }
    edges
}

const FABRIC_NATIVE_SUFFIXES: [&str; 5] = ["", "View", "ViewManager", "ComponentView", "Manager"];

/// Expo Modules cross-platform pairing: the iOS and Android impls of the same
/// `<module>.<method>` link to each other (both directions).
pub fn expo_cross_platform_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut by_key: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
    each_node(ctx, NodeKind::Method, |m| {
        if !m.id.starts_with("expo-module:") {
            return;
        }
        let key = m.qualified_name.rsplit("::").next().unwrap_or(""); // `<module>.<method>`
        if key.is_empty() {
            return;
        }
        by_key.entry(key.to_string()).or_default().push(m);
    });
    for group in by_key.values() {
        if group.len() < 2 {
            continue;
        }
        for a in group {
            for b in group {
                if a.id == b.id || a.language == b.language {
                    continue; // cross-platform only
                }
                if !seen.insert(format!("{}>{}", a.id, b.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &a.id,
                    &b.id,
                    Some(a.start_line),
                    meta! { "synthesizedBy" => "expo-cross-platform", "via" => a.name.as_str() },
                ));
            }
        }
    }
    edges
}

/// Classic React Native NativeModules cross-platform pairing: a native module method that HAS a JS
/// caller is a confirmed bridge method; link it to the same-named native method in another
/// language, both directions.
pub fn rn_cross_platform_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let js_lang = |l: Language| matches!(l, Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx);
    // RN module INFRASTRUCTURE methods exist on every native module, so pairing them by name would
    // cross-link unrelated modules in a multi-module repo.
    const RN_INFRA: [&str; 11] = [
        "addListener", "removeListeners", "getConstants", "constantsToExport", "getName", "invalidate", "initialize",
        "getDefaultEventTypes", "supportedEvents", "requiresMainQueueSetup", "methodQueue",
    ];
    let norm = |name: &str| -> String {
        match name.find(':') {
            Some(i) => name[..i].to_string(),
            None => name.to_string(),
        }
    };

    // Index native methods by their JS-visible (normalized) name.
    let mut by_name: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
    each_node_in(ctx, &[NodeKind::Method], &[Language::Java, Language::Kotlin, Language::ObjC, Language::Cpp], |m| {
        by_name.entry(norm(&m.name)).or_default().push(m);
    });

    for (group_name, group) in &by_name {
        if RN_INFRA.contains(&group_name.as_str()) {
            continue;
        }
        let langs: HashSet<Language> = group.iter().map(|m| m.language).collect();
        if langs.len() < 2 {
            continue; // single-platform — nothing to pair
        }
        for m in group {
            // Is m a bridge method? (a JS-language `calls` edge points at it)
            let incoming_edges = incoming(ctx, &m.id, &[EdgeKind::Calls]);
            if incoming_edges.is_empty() {
                continue;
            }
            let ids: Vec<String> = incoming_edges.iter().map(|e| e.source.clone()).collect();
            let sources = q("getNodesByIds", ctx.store().get_nodes_by_ids(&ids));
            let is_bridge = incoming_edges.iter().any(|e| sources.get(&e.source).is_some_and(|s| js_lang(s.language)));
            if !is_bridge {
                continue;
            }
            // Link to the other-platform impls (both directions).
            for sib in group {
                if sib.id == m.id || sib.language == m.language {
                    continue;
                }
                for (a, b) in [(m, sib), (sib, m)] {
                    if !seen.insert(format!("{}>{}", a.id, b.id)) {
                        continue;
                    }
                    edges.push(calls_edge(
                        &a.id,
                        &b.id,
                        Some(a.start_line),
                        meta! { "synthesizedBy" => "rn-cross-platform", "via" => norm(&m.name) },
                    ));
                }
            }
        }
    }
    edges
}

/// Phase 6 — React Native Fabric/Codegen view component → its native class by RN's naming
/// convention.
pub fn fabric_native_impl_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // The Fabric extractor IDs are prefixed `fabric-component:` so we can filter to just those
    // while streaming.
    let mut components: Vec<NodeRef> = Vec::new();
    each_node(ctx, NodeKind::Component, |n| {
        if n.id.starts_with("fabric-component:") {
            components.push(n);
        }
    });
    if components.is_empty() {
        return edges;
    }

    // Pre-index native classes by name for O(1) lookup.
    let mut native_classes_by_name: HashMap<String, Vec<NodeRef>> = HashMap::new();
    each_node(ctx, NodeKind::Class, |n| {
        if !matches!(n.language, Language::ObjC | Language::Kotlin | Language::Java | Language::Cpp) {
            return;
        }
        native_classes_by_name.entry(n.name.clone()).or_default().push(n);
    });

    for component in &components {
        for suffix in FABRIC_NATIVE_SUFFIXES {
            let candidate = format!("{}{}", component.name, suffix);
            let Some(matches) = native_classes_by_name.get(&candidate).filter(|m| !m.is_empty()) else { continue };
            // Link the component node to every matching native class (iOS + Android each have one).
            for native in matches {
                if !seen.insert(format!("{}>{}", component.id, native.id)) {
                    continue;
                }
                edges.push(calls_edge(
                    &component.id,
                    &native.id,
                    None,
                    meta! {
                        "synthesizedBy" => "fabric-native-impl",
                        "viaSuffix" => if suffix.is_empty() { "(exact)" } else { suffix },
                        "componentName" => component.name.as_str(),
                    },
                ));
            }
        }
    }
    edges
}

/// MyBatis: link a Java mapper interface method to the XML statement that holds its SQL.
pub fn mybatis_java_xml_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Collect the XML side FIRST: if the project has none, return before paying the full
    // java-method stream below.
    let mut xml_methods: Vec<NodeRef> = Vec::new();
    each_node_in(ctx, &[NodeKind::Method], &[Language::Xml], |m| xml_methods.push(m));
    if xml_methods.is_empty() {
        return edges;
    }

    // Index Java methods by `<ClassName>::<methodName>` for O(1) lookup.
    let mut java_index: HashMap<String, Vec<NodeRef>> = HashMap::new();
    each_node_in(ctx, &[NodeKind::Method], &[Language::Java, Language::Kotlin], |m| {
        let parts: Vec<&str> = m.qualified_name.split("::").collect();
        let last = parts[parts.len() - 1];
        let Some(cls) = (parts.len() >= 2).then(|| parts[parts.len() - 2]) else { return };
        if last.is_empty() || cls.is_empty() {
            return;
        }
        java_index.entry(format!("{cls}::{last}")).or_default().push(Rc::clone(&m));
    });

    for xml in &xml_methods {
        // Qualified name: `<namespace>::<id>`. Extract the simple class name.
        let Some(colon_idx) = xml.qualified_name.rfind("::") else { continue };
        let namespace = &xml.qualified_name[..colon_idx];
        let id = &xml.qualified_name[colon_idx + 2..];
        if namespace.is_empty() || id.is_empty() {
            continue;
        }
        let class_name = match namespace.rfind('.') {
            Some(i) => &namespace[i + 1..],
            None => namespace,
        };
        let Some(candidates) = java_index.get(&format!("{class_name}::{id}")).filter(|c| !c.is_empty()) else { continue };
        // Drop ambiguous matches (multiple same-name classes).
        if candidates.len() > 1 {
            continue;
        }
        let java = &candidates[0];
        if !seen.insert(format!("{}>{}", java.id, xml.id)) {
            continue;
        }
        edges.push(calls_edge(
            &java.id,
            &xml.id,
            Some(java.start_line),
            meta! {
                "synthesizedBy" => "mybatis-java-xml", "via" => format!("{class_name}.{id}"),
                "registeredAt" => format!("{}:{}", xml.file_path, xml.start_line),
            },
        ));
    }
    edges
}

use std::rc::Rc;

/// Balanced `(...)` body starting at the '(' index; `None` if unbalanced.
fn go_balanced_args(s: &str, open_idx: usize) -> Option<&str> {
    let mut depth = 0;
    for (i, c) in s[open_idx..].char_indices() {
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth == 0 {
                return Some(&s[open_idx + 1..open_idx + i]);
            }
        }
    }
    None
}

/// Split a top-level comma list, respecting nested () [] {}.
fn go_split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for c in args.chars() {
        if c == '(' || c == '[' || c == '{' {
            depth += 1;
            cur.push(c);
        } else if c == ')' || c == ']' || c == '}' {
            depth -= 1;
            cur.push(c);
        } else if c == ',' && depth == 0 {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !js_trim(&cur).is_empty() {
        out.push(cur);
    }
    out
}

/// Tail ident of a handler arg: `gin.Logger()`→`Logger`, `mw`→`mw`; `None` for string paths /
/// closures.
fn go_handler_ident(expr: &str) -> Option<String> {
    let cleaned = re!(r"\(\s*\)$").replace_first(js_trim(expr), |_| String::new());
    if cleaned.is_empty() || cleaned.starts_with('"') || cleaned.starts_with('`') || cleaned.starts_with("func") {
        return None;
    }
    re!(r"(?:\.|^)([A-Za-z_]\w*)$").captures(&cleaned).and_then(|m| m.text(1).map(String::from))
}

/// Gin middleware chain: the chain DISPATCHER (a Go method whose body invokes a `handlers` slice by
/// index) → every HandlerFunc registered via a gin registration call.
pub fn gin_middleware_chain_edges(ctx: &Ctx) -> Vec<Edge> {
    // 1. Find the chain dispatcher(s): a Go method that invokes a `handlers` slice by index.
    let dispatch_re = re!(r"\.handlers\s*\[[^\]]*\]\s*\(");
    let mut dispatchers: Vec<NodeRef> = Vec::new();
    each_node_in(ctx, &[NodeKind::Method], &[Language::Go], |n| {
        if node_source(ctx, &n).is_some_and(|src| dispatch_re.is_match(&src)) {
            dispatchers.push(n);
        }
    });
    if dispatchers.is_empty() {
        return Vec::new(); // not a gin repo — bail
    }

    // 2. Collect handler identifiers registered via gin registration calls.
    let mut registered: IndexMap<String, String> = IndexMap::new(); // name → registeredAt (file:line)
    let reg_gate = re!(r"\.(?:GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD|Any|Handle)\(");
    let reg_re = re!(r"\.(?:Use|GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD|Any|Handle)\s*\(", "g");
    for file in ctx.get_all_files() {
        if !file.ends_with(".go") {
            continue;
        }
        let Some(content) = ctx.read_file(&file) else { continue };
        if content.is_empty() || (!content.contains(".Use(") && !reg_gate.is_match(&content)) {
            continue;
        }
        let safe = codegraph_text::strip_comments_for_regex(&content, "go");
        for m in reg_re.captures_iter(&safe) {
            let whole = m.get(0).unwrap();
            let paren_idx = whole.end() - 1;
            let Some(arg_str) = go_balanced_args(&safe, paren_idx).filter(|a| !a.is_empty()) else { continue };
            let line = line_of(&safe, whole.start());
            for arg in go_split_args(arg_str) {
                if let Some(name) = go_handler_ident(&arg) {
                    registered.entry(name).or_insert_with(|| format!("{file}:{line}"));
                }
            }
        }
    }
    if registered.is_empty() {
        return Vec::new();
    }

    // 3. Link each dispatcher → each registered handler node (dedup, capped).
    let mut edges = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for disp in &dispatchers {
        let mut added = 0;
        for (name, registered_at) in &registered {
            if added >= MAX_CALLBACKS_PER_CHANNEL {
                break;
            }
            let handler = ctx
                .get_nodes_by_name(name)
                .iter()
                .find(|n| (n.kind == NodeKind::Function || n.kind == NodeKind::Method) && n.language == Language::Go)
                .cloned();
            let Some(handler) = handler else { continue };
            if handler.id == disp.id {
                continue;
            }
            if !seen.insert(format!("{}>{}", disp.id, handler.id)) {
                continue;
            }
            edges.push(calls_edge(
                &disp.id,
                &handler.id,
                Some(disp.start_line),
                meta! { "synthesizedBy" => "gin-middleware-chain", "via" => name.as_str(), "registeredAt" => registered_at.as_str() },
            ));
            added += 1;
        }
    }
    edges
}

/// Delphi form code-behind: a form unit `UFRMAbout.pas` owns its visual form definition
/// `UFRMAbout.dfm` / `.fmx`, paired by basename in the same directory.
pub fn pascal_form_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let all_files: IndexSet<String> = ctx.get_all_files().into_iter().collect();
    let form_re = re!(r"\.(dfm|fmx)$", "i");
    for file in &all_files {
        if !form_re.is_match(file) {
            continue;
        }
        let pas_file = form_re.replace_first(file, |_| ".pas".to_string());
        if !all_files.contains(&pas_file) {
            continue;
        }
        let form_node = ctx.get_nodes_in_file(file).iter().find(|n| n.kind == NodeKind::File).cloned();
        let unit_node = ctx.get_nodes_in_file(&pas_file).iter().find(|n| n.kind == NodeKind::File).cloned();
        let (Some(form_node), Some(unit_node)) = (form_node, unit_node) else { continue };
        edges.push(heuristic_edge(
            &unit_node.id,
            &form_node.id,
            EdgeKind::References,
            Some(unit_node.start_line),
            meta! { "synthesizedBy" => "pascal-form", "registeredAt" => pas_file.as_str() },
        ));
    }
    edges
}

/// SvelteKit file-convention data flow: a route directory's `+page.svelte` receives its `data`
/// from the sibling loader's `load` / `actions`. Page → load.
pub fn svelte_kit_load_edges(ctx: &Ctx) -> Vec<Edge> {
    let mut edges = Vec::new();
    let all_files: IndexSet<String> = ctx.get_all_files().into_iter().collect();
    let page_re = re!(r"(.*\/)(\+(?:page|layout))\.svelte$");
    let is_hook_kind = |k: NodeKind| matches!(k, NodeKind::Function | NodeKind::Method | NodeKind::Constant | NodeKind::Variable);
    for file in &all_files {
        let Some(m) = page_re.captures(file) else { continue };
        let dir = m.text(1).unwrap_or("");
        let prefix = m.text(2).unwrap_or("");
        let Some(page) = ctx.get_nodes_in_file(file).iter().find(|n| n.kind == NodeKind::Component).cloned() else { continue };
        for ext in [".server.ts", ".server.js", ".ts", ".js"] {
            let loader_file = format!("{dir}{prefix}{ext}");
            if !all_files.contains(&loader_file) {
                continue;
            }
            for hook in ctx.get_nodes_in_file(&loader_file).iter() {
                // `load` and `actions` by name, and every function the loader file declares — a
                // form action is an arrow inside `actions`, and it is a node of its own.
                let named = is_hook_kind(hook.kind) && (hook.name == "load" || hook.name == "actions");
                if !named && hook.kind != NodeKind::Function && hook.kind != NodeKind::Method {
                    continue;
                }
                edges.push(calls_edge(
                    &page.id,
                    &hook.id,
                    Some(page.start_line),
                    meta! {
                        "synthesizedBy" => "sveltekit-load", "via" => hook.name.as_str(),
                        "registeredAt" => format!("{}:{}", loader_file, hook.start_line),
                    },
                ));
            }
        }
    }
    edges
}
