//! Drupal framework resolver. Port of `src/resolution/frameworks/drupal.ts`.
//!
//! Supports Drupal 8/9/10/11 (Composer-based projects). Drupal 7 is not supported.
//!
//! 1. **Detection** — reads composer.json and checks for any `drupal/*` dependency in `require`
//!    or `require-dev`.
//!
//! 2. **Route extraction** — parses `*.routing.yml` files and emits `route` nodes for each Drupal
//!    route, with `references` edges to the `_controller`, `_form`, or entity handler
//!    class/method.
//!
//! 3. **Hook detection** — scans `.module`, `.install`, `.theme`, and `.inc` files for Drupal hook
//!    implementations. Two strategies are used:
//!      a. Docblock: `@Implements hook_X()` → precise, no false positives.
//!      b. Name pattern: function `{moduleName}_{hookSuffix}()` → catches hooks without docblocks
//!         but may produce false positives on helper functions.
//!    Detected hooks emit an `UnresolvedRef` from the implementing function node to the canonical
//!    `hook_X` name, linking implementations to the hook when `codegraph_callers` is invoked.
//!
//!    A method marked `#[Hook('x')]`, the way Drupal 11 writes one, does the same, from the
//!    method; on a class, the attribute names the method or means `__invoke`.
//!
//! 4. **Services** — parses `*.services.yml`. Each service is a `component` node named
//!    `service:<id>` (the class in `signature`), with `references` edges to its class and to the
//!    services it is given. The prefix is load-bearing: an id is a free-form string, and a node
//!    named `Currency` would answer every `new Currency()` in the project. A call on what the
//!    container hands out (`\Drupal::service('x')->run()`, `$container->get('x')->run()`) links to
//!    the method on that service's class.
//!
//! 5. **Plugins** — a class under `src/Plugin/`, `src/Entity/` or `src/Element/` carrying an
//!    attribute or a docblock annotation with an `id` is a `component` node named `<Type>:<id>`
//!    with a `references` edge to the class.
//!
//! Twig symbol extraction and virtual `hook_*` nodes are out of scope here exactly as they are in
//! the TypeScript resolver.

use super::now_ms;
use super::object_literal::match_bracket;
use crate::ctx::Ctx;
use crate::synth_utils::make_line_at;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::jscompat::{is_js_space, js_trim, js_trim_end, js_trim_start};
use codegraph_types::node_id;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;
use std::collections::HashSet;

pub struct DrupalResolver;

const LANGUAGES: [Language; 2] = [Language::Php, Language::Yaml];

const HOOK_FILE_EXTENSIONS: [&str; 4] = [".module", ".install", ".theme", ".inc"];

fn is_drupal_hook_file(file_path: &str) -> bool {
    HOOK_FILE_EXTENSIONS.iter().any(|ext| file_path.ends_with(ext))
}

/// Parse the last PHP namespace segment from a FQCN like `\Drupal\mymodule\Controller\Foo`.
/// `None` for strings that don't look like a FQCN.
fn last_segment(fqcn: &str) -> Option<String> {
    let clean = js_trim(fqcn.trim_start_matches('\\'));
    if !clean.contains('\\') {
        return None;
    }
    clean.rsplit('\\').next().map(String::from)
}

/// Derive the Drupal module name from a file path.
/// e.g. `web/modules/custom/my_module/my_module.module` → `my_module`
fn module_name_from_path(file_path: &str) -> Option<String> {
    re!(r"/([^/]+)\.[^./]+$").captures(file_path).and_then(|m| m.text(1).map(String::from))
}

impl FrameworkResolver for DrupalResolver {
    fn name(&self) -> &str {
        "drupal"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    // Drupal route handlers are FQCNs (`\Drupal\…\Class::method`, the single-colon
    // controller-service form `\Drupal\…\Class:method`, or a bare `\…\FormClass`) and hook refs
    // are canonical `hook_*` names — none match a declared symbol, so resolveOne's pre-filter
    // would drop them before resolve() runs. Claim the shapes resolve() handles (mirrors the
    // Rails `controller#action` claim).
    fn claims_reference(&self, name: &str) -> bool {
        name.starts_with("hook_")
            || name.starts_with("drupal:")
            || name.contains('\\')
            || re!(r"^[A-Za-z_]\w*::?\w+$").is_match(name)
            || container_call_re().is_match(name)
            || drupal_service_call_re().is_match(name)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Primary: composer.json identifies a Drupal project/module/theme/profile. A contrib
        // module often has an EMPTY `require` (no `drupal/*` dep) but still declares `"name":
        // "drupal/<module>"` and `"type": "drupal-module"`, so check those too — checking deps
        // alone misses every standalone contrib module.
        if let Some(composer) = ctx.read_file("composer.json").filter(|c| !c.is_empty()) {
            // Malformed composer.json — fall through to file-based detection.
            if let Ok(serde_json::Value::Object(json)) = serde_json::from_str::<serde_json::Value>(&composer) {
                if json.get("name").and_then(|v| v.as_str()).is_some_and(|n| n.starts_with("drupal/")) {
                    return true;
                }
                if json.get("type").and_then(|v| v.as_str()).is_some_and(|t| t.starts_with("drupal-")) {
                    return true;
                }
                let declares_drupal =
                    |key: &str| json.get(key).and_then(|v| v.as_object()).is_some_and(|deps| deps.keys().any(|k| k.starts_with("drupal/")));
                if declares_drupal("require") || declares_drupal("require-dev") {
                    return true;
                }
            }
        }

        // Fallback (composer-less module, or a non-Drupal composer.json): the unmistakable Drupal
        // signature is a `*.info.yml` manifest alongside a Drupal PHP/route file. Require both so
        // a stray `.info.yml` elsewhere doesn't trigger a false positive.
        let files = ctx.get_all_files();
        if !files.iter().any(|f| f.ends_with(".info.yml")) {
            return false;
        }
        files.iter().any(|f| f.ends_with(".routing.yml") || f.ends_with(".module") || f.ends_with(".install") || f.ends_with(".theme"))
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        let name = r.reference_name.as_str();
        let resolved = |target: Option<NodeRef>, confidence: f64| target.map(|t| ResolvedRef::new(r, t.id.clone(), confidence, ResolvedBy::Framework));

        if let Some(id) = name.strip_prefix(SERVICE_REF) {
            return resolved(service_node(id, ctx), 0.9);
        }
        if let Some(fqcn) = name.strip_prefix(FQCN_REF) {
            return resolved(class_by_fqcn(&fqcn.replacen(':', "", 1), ctx), 0.9);
        }
        if let Some(rest) = name.strip_prefix(CLASS_REF) {
            let (file, class_name) = rest.split_once('#').unwrap_or((rest, ""));
            return resolved(ctx.get_nodes_in_file(file).iter().find(|n| n.kind == NodeKind::Class && n.name == class_name).cloned(), 0.95);
        }

        let on_container = container_call_re().captures(name);
        let on_drupal = if on_container.is_some() { None } else { drupal_service_call_re().captures(name) };
        if on_container.is_some() || on_drupal.is_some() {
            let method = on_container.as_ref().and_then(|c| c.text(2)).or_else(|| on_drupal.as_ref().and_then(|c| c.text(1))).unwrap_or("");
            let id = match on_container.as_ref().and_then(|c| c.text(1)) {
                Some(id) => Some(id.to_string()),
                None => service_on_line(r, method, ctx),
            };
            let cls = id.and_then(|id| service_class(&id, ctx));
            return resolved(cls.and_then(|c| method_of(&c, method, ctx)), 0.9);
        }

        // _controller: '\Drupal\module\...\ClassName::methodName' (double colon) or the
        // single-colon controller-service form '\Drupal\...\ClassName:methodName'.
        if let Some(controller_match) = re!(r"^\\?(?:Drupal\\[^:]+\\)?([^\\:]+):{1,2}(\w+)$").captures(name) {
            let class_name = controller_match.text(1).unwrap_or("");
            let method_name = controller_match.text(2).unwrap_or("");
            for cls in ctx.get_nodes_by_name(class_name).iter() {
                if cls.kind != NodeKind::Class {
                    continue;
                }
                let file_nodes = ctx.get_nodes_in_file(&cls.file_path);
                if let Some(method) = file_nodes.iter().find(|n| n.kind == NodeKind::Method && n.name == method_name) {
                    return Some(ResolvedRef::new(r, method.id.clone(), 0.9, ResolvedBy::Framework));
                }
                return Some(ResolvedRef::new(r, cls.id.clone(), 0.7, ResolvedBy::Framework));
            }
        }

        // _form / _entity_form: '\Drupal\module\...\ClassName'  (bare FQCN, no method)
        if name.contains('\\') && !name.contains(':') {
            // The namespace first: one class name can sit in several, as a plugin's attribute and
            // annotation classes do.
            let cls = class_by_fqcn(name, ctx)
                .or_else(|| last_segment(name).and_then(|class_name| ctx.get_nodes_by_name(&class_name).iter().find(|n| n.kind == NodeKind::Class).cloned()));
            if let Some(cls) = cls {
                return Some(ResolvedRef::new(r, cls.id.clone(), 0.85, ResolvedBy::Framework));
            }
        }

        // hook_X — find any function whose name ends in _{hookSuffix} in a hook file. A method
        // only has #[Hook] to go by: another module's function is no answer to it.
        if let Some(hook_suffix) = name.strip_prefix("hook_").filter(|_| !r.from_node_id.starts_with("method:")) {
            let suffix = format!("_{hook_suffix}");
            let functions = ctx.get_nodes_by_kind(NodeKind::Function);
            if let Some(candidate) = functions.iter().find(|n| n.name.ends_with(&suffix) && is_drupal_hook_file(&n.file_path)) {
                return Some(ResolvedRef::new(r, candidate.id.clone(), 0.75, ResolvedBy::Framework));
            }
        }

        None
    }

    fn extract(&self, file_path: &str, content: &str) -> Option<FrameworkExtractionResult> {
        if file_path.ends_with(".routing.yml") {
            return Some(extract_drupal_routes(file_path, content));
        }

        if file_path.ends_with(".services.yml") {
            return Some(extract_drupal_services(file_path, content));
        }

        if is_drupal_hook_file(file_path) || file_path.ends_with(".php") {
            let hooks = extract_drupal_hooks(file_path, content);
            let declarations = if content.contains("#[") { attributed_declarations(content) } else { Vec::new() };
            let plugins = extract_drupal_plugins(file_path, content, &declarations);
            let mut references = hooks.references;
            references.extend(oop_hook_references(file_path, content, &declarations));
            references.extend(plugins.references);
            return Some(FrameworkExtractionResult { nodes: plugins.nodes, references });
        }

        Some(FrameworkExtractionResult::default())
    }
}

/// Extract route nodes and handler references from a Drupal `*.routing.yml` file.
///
/// Drupal routing YAML format:
///
/// ```text
/// route.name:
///   path: '/some/path'
///   defaults:
///     _controller: '\Drupal\module\Controller\MyController::method'
///     _form: '\Drupal\module\Form\MyForm'
///     _title: 'Page title'
///   requirements:
///     _permission: 'access content'
///   methods: [GET, POST]   # optional
/// ```
fn extract_drupal_routes(file_path: &str, content: &str) -> FrameworkExtractionResult {
    let mut nodes: Vec<Node> = Vec::new();
    let mut references: Vec<UnresolvedRef> = Vec::new();
    let now = now_ms();

    struct Pending {
        name: String,
        line_num: u32,
    }
    let mut pending: Option<Pending> = None;
    let mut current_path: Option<String> = None;
    let mut handler_refs: Vec<String> = Vec::new();
    let mut methods: Vec<String> = Vec::new();

    let flush_route = |pending: &Option<Pending>,
                       current_path: &Option<String>,
                       handler_refs: &[String],
                       methods: &[String],
                       nodes: &mut Vec<Node>,
                       references: &mut Vec<UnresolvedRef>| {
        let (Some(pending), Some(current_path)) = (pending, current_path) else { return };
        if current_path.is_empty() {
            return;
        }
        let method_tag = if methods.is_empty() { String::new() } else { format!(" [{}]", methods.join(",")) };
        let node = Node::new(
            format!("route:{file_path}:{}:{current_path}", pending.line_num),
            NodeKind::Route,
            format!("{current_path}{method_tag}"),
            format!("{file_path}::{}", pending.name),
            file_path,
            Language::Yaml,
            pending.line_num,
            pending.line_num,
            0,
            0,
            now,
        );
        let route_id = node.id.clone();
        nodes.push(node);

        for handler in handler_refs {
            references.push(UnresolvedRef::new(route_id.clone(), handler.as_str(), EdgeKind::References, pending.line_num, 0, file_path, Language::Yaml));
        }
    };

    for (i, line) in content.split('\n').enumerate() {
        let trimmed = js_trim(line);

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Top-level route name: no leading whitespace, ends with a colon (no value after)
        if re!(r"^\S.*:\s*$").is_match(line) && !re!(r"^\s").is_match(line) {
            flush_route(&pending, &current_path, &handler_refs, &methods, &mut nodes, &mut references);
            let name_end = trimmed.len().saturating_sub(1);
            pending = Some(Pending { name: js_trim(&trimmed[..name_end]).to_string(), line_num: i as u32 + 1 });
            current_path = None;
            handler_refs = Vec::new();
            methods = Vec::new();
            continue;
        }

        // path: '/some/path'
        if let Some(m) = re!(r##"^path:\s*['"]?([^'"#\n]+?)['"]?\s*(?:#.*)?$"##).captures(trimmed) {
            current_path = Some(js_trim(m.text(1).unwrap_or("")).to_string());
            continue;
        }

        // _controller: '\Drupal\...\Class::method'
        if let Some(m) = re!(r##"^_controller:\s*['"]?([^'"#\n]+?)['"]?\s*(?:#.*)?$"##).captures(trimmed) {
            handler_refs.push(js_trim(m.text(1).unwrap_or("")).to_string());
            continue;
        }

        // _form: '\Drupal\...\Form\MyForm'
        if let Some(m) = re!(r##"^_form:\s*['"]?([^'"#\n]+?)['"]?\s*(?:#.*)?$"##).captures(trimmed) {
            handler_refs.push(js_trim(m.text(1).unwrap_or("")).to_string());
            continue;
        }

        // _entity_form / _entity_list / _entity_view: entity.type
        if let Some(m) = re!(r##"^_(entity_form|entity_list|entity_view):\s*['"]?([^'"#\n]+?)['"]?\s*(?:#.*)?$"##).captures(trimmed) {
            handler_refs.push(js_trim(m.text(2).unwrap_or("")).to_string());
            continue;
        }

        // methods: [GET, POST]  or  methods: [GET]
        if let Some(m) = re!(r"^methods:\s*\[([^\]]+)\]").captures(trimmed) {
            methods = m.text(1).unwrap_or("").split(',').map(|s| js_trim(s).to_uppercase()).filter(|s| !s.is_empty()).collect();
            continue;
        }
    }

    flush_route(&pending, &current_path, &handler_refs, &methods, &mut nodes, &mut references);
    FrameworkExtractionResult { nodes, references }
}

/// Extract hook implementation references from a Drupal PHP file.
///
/// Strategy A (primary): look for docblocks containing `Implements hook_X().` followed immediately
/// by the function definition. This is the Drupal coding standard and is precise.
///
/// Strategy B (fallback): for functions whose name starts with `{moduleName}_`, treat the suffix
/// as the hook name. Catches hooks without docblocks but may produce false positives on non-hook
/// helper functions.
///
/// Each detected hook emits an UnresolvedRef from the implementing function node (identified by
/// computing the same ID tree-sitter would generate) to the canonical hook name, e.g.
/// `hook_form_alter`.
fn extract_drupal_hooks(file_path: &str, content: &str) -> FrameworkExtractionResult {
    let mut references: Vec<UnresolvedRef> = Vec::new();

    // Build a map of function name → 1-indexed line number for all top-level functions. This
    // mirrors tree-sitter's line numbering so we can reconstruct node IDs.
    let mut func_line_map: IndexMap<String, u32> = IndexMap::new();
    let func_def = re!(r"^function\s+(\w+)\s*\(", "gm");
    for fm in func_def.captures_iter(content) {
        let Some(whole) = fm.get(0) else { continue };
        let name = fm.text(1).unwrap_or("");
        if !func_line_map.contains_key(name) {
            // line = number of newlines before match start + 1
            func_line_map.insert(name.to_string(), content[..whole.start()].matches('\n').count() as u32 + 1);
        }
    }

    let mut emit_hook_ref = |hook_name: &str, func_name: &str, func_line_map: &IndexMap<String, u32>| {
        let Some(&line_num) = func_line_map.get(func_name) else { return };
        let from = node_id(file_path, NodeKind::Function, func_name, line_num);
        references.push(UnresolvedRef::new(from, hook_name, EdgeKind::References, line_num, 0, file_path, Language::Php));
    };

    // Strategy A: docblock `Implements hook_X().` followed by function definition. The docblock
    // and function may be separated by blank lines.
    let docblock_pattern =
        re!(r"/\*\*[\s\S]*?(?:@|\*\s+)Implements\s+(hook_\w+)\s*\(\)[\s\S]*?\*/\s*\n(?:\s*\n)*function\s+(\w+)\s*\(", "g");
    let mut docblock_matched: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in docblock_pattern.captures_iter(content) {
        let hook_name = m.text(1).unwrap_or("");
        let func_name = m.text(2).unwrap_or("");
        emit_hook_ref(hook_name, func_name, &func_line_map);
        docblock_matched.insert(func_name.to_string());
    }

    // Strategy B: fallback name-pattern matching for functions without docblocks. Only applies to
    // functions whose name starts with {moduleName}_ and that were not already matched by
    // Strategy A.
    if let Some(module_name) = module_name_from_path(file_path).filter(|m| !m.is_empty()) {
        let prefix = format!("{module_name}_");
        for func_name in func_line_map.keys() {
            if docblock_matched.contains(func_name) {
                continue;
            }
            let Some(hook_suffix) = func_name.strip_prefix(&prefix) else { continue };
            if hook_suffix.is_empty() {
                continue;
            }
            // Emit a reference to hook_{suffix} — the resolver will link it if the hook is
            // defined somewhere in the indexed graph (e.g. Drupal core).
            emit_hook_ref(&format!("hook_{hook_suffix}"), func_name, &func_line_map);
        }
    }

    FrameworkExtractionResult { nodes: Vec::new(), references }
}

// ---------------------------------------------------------------------------
// Services (*.services.yml) — upstream a2870563
// ---------------------------------------------------------------------------

/// A reference to a service by id, to a class by the file that declares it, and by its FQCN:
/// answered only here.
const SERVICE_REF: &str = "drupal:service:";
const CLASS_REF: &str = "drupal:class:";
const FQCN_REF: &str = "drupal:fqcn:";

/// `Drupal\x\:Y`: the colon parks a failed reference under the class's short name, which is what
/// a sync looks for.
fn fqcn_ref(fqcn: &str) -> String {
    let cut = fqcn.rfind('\\').map_or(0, |i| i + 1);
    format!("{FQCN_REF}{}:{}", &fqcn[..cut], &fqcn[cut..])
}

/// Cut a trailing comment from a YAML line: the first `#` outside quotes after a space.
fn without_yaml_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut prev: Option<char> = None;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '#' && prev.is_none_or(is_js_space) => return &line[..i],
            None => {}
        }
        prev = Some(c);
    }
    line
}

/// Service definitions from a `*.services.yml` file. Line by line, as the routes are: ids sit at
/// one indent under the top-level `services:`, and what a service is given sits below its id, in
/// any of the places Symfony takes one (`arguments`, `calls`, `factory`, `parent`, `decorates`, an
/// alias).
fn extract_drupal_services(file_path: &str, content: &str) -> FrameworkExtractionResult {
    struct Current {
        index: usize,
        given: HashSet<String>,
    }
    let mut out = FrameworkExtractionResult::default();
    let now = now_ms();
    let mut in_services = false;
    let mut id_indent: Option<usize> = None;
    let mut current: Option<Current> = None;

    fn reference(out: &mut FrameworkExtractionResult, index: usize, file_path: &str, name: String) {
        let from = &out.nodes[index];
        let r = UnresolvedRef::new(from.id.clone(), name, EdgeKind::References, from.start_line, 0, file_path, Language::Yaml);
        out.references.push(r);
    }
    fn give(out: &mut FrameworkExtractionResult, current: &mut Option<Current>, file_path: &str, id: &str) {
        let Some(cur) = current.as_mut() else { return };
        if cur.given.contains(id) || out.nodes[cur.index].name == format!("service:{id}") {
            return;
        }
        cur.given.insert(id.to_string());
        reference(out, cur.index, file_path, format!("{SERVICE_REF}{id}"));
    }
    fn give_all(out: &mut FrameworkExtractionResult, current: &mut Option<Current>, file_path: &str, text: &str) {
        // `'@a'`, `'@?a'` and `"@a"`, each a service; `'@@a'` is a literal string.
        for m in re!(r#"(?:^|[\s\[,'"])@\??([A-Za-z_\\][\w.\\-]*)"#, "g").captures_iter(text) {
            give(out, current, file_path, m.text(1).unwrap_or(""));
        }
    }
    fn set_class(out: &mut FrameworkExtractionResult, current: &Option<Current>, file_path: &str, fqcn: &str) {
        let Some(cur) = current.as_ref() else { return };
        if out.nodes[cur.index].signature.is_some() {
            return;
        }
        out.nodes[cur.index].signature = Some(fqcn.to_string());
        reference(out, cur.index, file_path, fqcn_ref(fqcn));
    }

    for (i, raw) in content.split('\n').enumerate() {
        let text = js_trim_end(without_yaml_comment(raw));
        let trimmed = js_trim(text);
        if trimmed.is_empty() {
            continue;
        }
        let indent = text.len() - js_trim_start(text).len();
        if indent == 0 {
            in_services = re!(r"^services\s*:").is_match(trimmed);
            id_indent = None;
            current = None;
            continue;
        }
        if !in_services {
            continue;
        }
        let id_at = *id_indent.get_or_insert(indent);

        if indent == id_at {
            current = None;
            let Some(id) = re!(r#"^(?:'([^']+)'|"([^"]+)"|([A-Za-z_\\][\w.\\-]*))\s*:\s*(.*)$"#).captures(trimmed) else { continue };
            let name = id.text(1).or(id.text(2)).or(id.text(3)).unwrap_or("").to_string();
            // `_defaults` and `_instanceof` say how services are built, not what they are.
            if name.starts_with('_') {
                continue;
            }
            let fqcn = name.trim_start_matches('\\').to_string();
            let line = i as u32 + 1;
            out.nodes.push(Node::new(
                format!("component:{file_path}:{line}:service:{name}"),
                NodeKind::Component,
                format!("service:{name}"),
                format!("{file_path}::service:{name}"),
                file_path,
                Language::Yaml,
                line,
                line,
                0,
                0,
                now,
            ));
            current = Some(Current { index: out.nodes.len() - 1, given: HashSet::new() });
            let value = id.text(4).unwrap_or("").to_string();
            if let Some(inline) = re!(r#"\bclass\s*:\s*['"]?([\w\\]+)"#).captures(&value).and_then(|c| c.text(1).map(String::from)) {
                set_class(&mut out, &current, file_path, inline.trim_start_matches('\\'));
            } else if fqcn.contains('\\') && !re!(r#"^['"]?@"#).is_match(&value) {
                set_class(&mut out, &current, file_path, &fqcn);
            }
            give_all(&mut out, &mut current, file_path, &value);
            continue;
        }
        if current.is_none() {
            continue;
        }
        if let Some(field) = re!(r#"^(class|parent|alias|decorates)\s*:\s*['"]?([^'"\s]+)"#).captures(trimmed) {
            let value = field.text(2).unwrap_or("");
            if field.text(1) == Some("class") {
                set_class(&mut out, &current, file_path, value.trim_start_matches('\\'));
            } else {
                give(&mut out, &mut current, file_path, value.strip_prefix('@').unwrap_or(value));
            }
            continue;
        }
        give_all(&mut out, &mut current, file_path, trimmed);
    }
    out
}

// ---------------------------------------------------------------------------
// Attributes: OOP hooks and plugin definitions
// ---------------------------------------------------------------------------

/// Attributes PHP and PHPUnit put on a class, which a string in them doesn't make a plugin.
const NOT_PLUGIN_ATTRIBUTES: [&str; 5] = ["Attribute", "AllowDynamicProperties", "Deprecated", "Group", "Hook"];

struct Attribute {
    /// The last segment of its name: `Hook`, `Block`.
    name: String,
    /// What is between its parentheses.
    args: String,
}

struct AttributedDeclaration {
    attributes: Vec<Attribute>,
    /// The line the attributes start on: tree-sitter counts them as part of what they mark, so a
    /// node's id does too.
    line: u32,
    offset: usize,
    method: Option<String>,
    class_name: Option<String>,
}

/// The offset of the next code after `at`: past whitespace and comments.
fn skip_trivia(s: &str, at: usize) -> usize {
    let mut i = at;
    loop {
        while let Some(c) = s[i..].chars().next().filter(|c| is_js_space(*c)) {
            i += c.len_utf8();
        }
        let rest = &s[i..];
        if rest.starts_with("//") || (rest.starts_with('#') && !rest.starts_with("#[")) {
            match rest.find('\n') {
                Some(end) => i += end,
                None => return s.len(),
            }
        } else if rest.starts_with("/*") {
            match s[i + 2..].find("*/") {
                Some(end) => i = i + 2 + end + 2,
                None => return s.len(),
            }
        } else {
            return i;
        }
    }
}

/// Every declaration that carries attributes. Read from the source, not from
/// `strip_comments_for_regex`, to which a `#[` is a comment like any other `#`.
fn attributed_declarations(content: &str) -> Vec<AttributedDeclaration> {
    let mut out = Vec::new();
    let line_of = make_line_at(content, 1);
    let attribute_start = re!(r"^[ \t]*#\[", "gm");
    // What a run of attributes sits on, once the modifiers are past it.
    let declaration = re!(r"(?:(?:abstract|final|readonly|public|protected|private|static)\s+)*(?:function\s+&?\s*([A-Za-z_]\w*)\s*\(|class\s+([A-Za-z_]\w*))", "y");
    let mut search_from = 0;
    while let Some(m) = attribute_start.captures_at(content, search_from) {
        let whole = m.get(0).unwrap();
        let start = whole.end() - 2;
        let mut attributes = Vec::new();
        let mut at = start;
        while content[at..].starts_with("#[") {
            let Some(close) = match_bracket(content, at + 1) else { break };
            if let Some(body) = re!(r"^\s*\\?([\w\\]+)\s*(\()?").captures(&content[at + 2..close]) {
                let open = at + 2 + body.get(0).unwrap().end() - 1;
                let end = if body.get(2).is_some() { match_bracket(content, open) } else { None };
                attributes.push(Attribute {
                    name: body.text(1).unwrap_or("").rsplit('\\').next().unwrap_or("").to_string(),
                    args: end.filter(|&e| e > 0).map_or(String::new(), |e| content[open + 1..e].to_string()),
                });
            }
            at = skip_trivia(content, close + 1);
        }
        search_from = at.max(whole.end());
        let Some(decl) = declaration.captures_at(content, at) else { continue };
        out.push(AttributedDeclaration {
            attributes,
            line: line_of.at(start),
            offset: at,
            method: decl.text(1).map(String::from),
            class_name: decl.text(2).map(String::from),
        });
    }
    out
}

/// The first string an attribute's arguments open with, by position or as `name:`.
fn first_argument(args: &str, name: &str) -> Option<String> {
    Re::cached(&format!(r#"^\s*(?:{name}\s*:\s*)?['"]([^'"]+)['"]"#), "").captures(args).and_then(|c| c.text(1).map(String::from))
}

/// `#[Hook('x')]` on a method, or on a class, where it names the method or means `__invoke`.
fn oop_hook_references(file_path: &str, content: &str, declarations: &[AttributedDeclaration]) -> Vec<UnresolvedRef> {
    let mut references = Vec::new();
    let mut emit = |hook: &str, method: &str, line: u32| {
        let from = node_id(file_path, NodeKind::Method, method, line);
        references.push(UnresolvedRef::new(from, format!("hook_{hook}"), EdgeKind::References, line, 0, file_path, Language::Php));
    };
    let line_of = make_line_at(content, 1);
    for decl in declarations {
        for attribute in &decl.attributes {
            if attribute.name != "Hook" {
                continue;
            }
            let Some(hook) = first_argument(&attribute.args, "hook") else { continue };
            if let Some(method) = &decl.method {
                emit(&hook, method, decl.line);
                continue;
            }
            let method = re!(r#"\bmethod\s*:\s*['"](\w+)['"]"#).captures(&attribute.args).and_then(|c| c.text(1).map(String::from)).unwrap_or_else(|| "__invoke".to_string());
            let Some(found) = Re::cached(&format!(r"\bfunction\s+&?\s*{method}\s*\("), "").find(&content[decl.offset..]) else { continue };
            let offset = decl.offset + found.start();
            let marked = declarations.iter().find(|other| other.method.as_deref() == Some(method.as_str()) && other.offset <= offset && offset - other.offset < 200);
            emit(&hook, &method, marked.map_or_else(|| line_of.at(offset), |m| m.line));
        }
    }
    references
}

struct PluginDefinition {
    ty: String,
    id: String,
    class_name: String,
    line: u32,
}

/// A class's plugin definitions: its attributes and its docblock's annotations, each with an `id`.
fn plugin_definitions(content: &str, declarations: &[AttributedDeclaration]) -> Vec<PluginDefinition> {
    let mut out: Vec<PluginDefinition> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |ty: &str, id: &str, class_name: &str, line: u32| {
        if seen.insert(format!("{ty}:{id}:{class_name}")) {
            out.push(PluginDefinition { ty: ty.to_string(), id: id.to_string(), class_name: class_name.to_string(), line });
        }
    };
    for decl in declarations {
        let Some(class_name) = &decl.class_name else { continue };
        for attribute in &decl.attributes {
            if NOT_PLUGIN_ATTRIBUTES.contains(&attribute.name.as_str()) {
                continue;
            }
            let id = first_argument(&attribute.args, "id")
                .or_else(|| re!(r#"\bid\s*:\s*['"]([^'"]+)['"]"#).captures(&attribute.args).and_then(|c| c.text(1).map(String::from)));
            if let Some(id) = id {
                add(&attribute.name, &id, class_name, decl.line);
            }
        }
    }
    let line_of = make_line_at(content, 1);
    let doc = re!(r"/\*\*((?:(?!\*/)[\s\S])*)\*/\s*(?:#\[[^\n]*\n\s*)*(?:(?:abstract|final|readonly)\s+)*class\s+([A-Za-z_]\w*)", "g");
    for m in doc.captures_iter(content) {
        let body_at = m.get(0).unwrap().start() + 3;
        let text = m.text(1).unwrap_or("");
        let class_name = m.text(2).unwrap_or("");
        let annotation = re!(r"^[ \t]*\*[ \t]*@([A-Z]\w*)\s*\(", "gm");
        let mut from = 0;
        while let Some(a) = annotation.captures_at(text, from) {
            let whole = a.get(0).unwrap();
            let open = whole.end() - 1;
            let Some(close) = match_bracket(text, open) else {
                from = whole.end();
                continue;
            };
            let body = &text[open..close];
            let id = re!(r#"(?:^|[\s,(])id\s*=\s*"([^"]+)""#)
                .captures(body)
                .or_else(|| re!(r#"^\(\s*"([^"]+)""#).captures(body))
                .and_then(|c| c.text(1).map(String::from));
            if let Some(id) = id {
                let at_sign = whole.as_str().find('@').unwrap_or(0);
                add(a.text(1).unwrap_or(""), &id, class_name, line_of.at(body_at + whole.start() + at_sign));
            }
            from = close;
        }
    }
    out
}

fn extract_drupal_plugins(file_path: &str, content: &str, declarations: &[AttributedDeclaration]) -> FrameworkExtractionResult {
    let mut out = FrameworkExtractionResult::default();
    // Where Drupal's plugin discovery and entity types look, by module-relative folder.
    if !re!(r"(?:^|/)src/(?:Plugin|Entity|Element)/").is_match(file_path) || re!(r"(?:^|/)tests?/").is_match(file_path) {
        return out;
    }
    let now = now_ms();
    for PluginDefinition { ty, id, class_name, line } in plugin_definitions(content, declarations) {
        let node = Node::new(
            format!("component:{file_path}:{line}:plugin:{ty}:{id}"),
            NodeKind::Component,
            format!("{ty}:{id}"),
            format!("{file_path}::plugin:{ty}:{id}"),
            file_path,
            Language::Php,
            line,
            line,
            0,
            0,
            now,
        );
        out.references.push(UnresolvedRef::new(node.id.clone(), format!("{CLASS_REF}{file_path}#{class_name}"), EdgeKind::References, line, 0, file_path, Language::Php));
        out.nodes.push(node);
    }
    out
}

// ---------------------------------------------------------------------------
// Resolving through the service container
// ---------------------------------------------------------------------------

/// `$container->get('x')->run()`, where extraction keeps the id in the name.
fn container_call_re() -> &'static Re {
    re!(r"^(?:this->)?container->get\('([\w.\\-]+)'\)\.(\w+)$")
}
/// `\Drupal::service('x')->run()`, where it doesn't: the id is read from the line.
fn drupal_service_call_re() -> &'static Re {
    re!(r"^\\?Drupal::service\(\)\.(\w+)$")
}

/// The class a FQCN names, by its namespace.
fn class_by_fqcn(fqcn: &str, ctx: &Ctx) -> Option<NodeRef> {
    let clean = fqcn.trim_start_matches('\\');
    let slash = clean.rfind('\\').filter(|&s| s > 0)?;
    let qualified = format!("{}::{}", &clean[..slash], &clean[slash + 1..]);
    ctx.get_nodes_by_qualified_name(&qualified).iter().find(|n| n.kind == NodeKind::Class).cloned()
}

fn service_node(id: &str, ctx: &Ctx) -> Option<NodeRef> {
    ctx.get_nodes_by_name(&format!("service:{id}")).iter().find(|n| n.kind == NodeKind::Component).cloned()
}

fn service_class(id: &str, ctx: &Ctx) -> Option<NodeRef> {
    let service = service_node(id, ctx)?;
    class_by_fqcn(service.signature.as_deref()?, ctx)
}

/// A method by name on a class, its own or an inherited one.
fn method_of(cls: &NodeRef, name: &str, ctx: &Ctx) -> Option<NodeRef> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut level: Vec<NodeRef> = vec![cls.clone()];
    let mut depth = 0;
    while depth < 6 && !level.is_empty() {
        let mut next: Vec<NodeRef> = Vec::new();
        for ty in &level {
            if !seen.insert(ty.id.clone()) {
                continue;
            }
            let prefix = format!("{}::", ty.qualified_name);
            if let Some(own) = ctx.get_nodes_in_file(&ty.file_path).iter().find(|n| n.kind == NodeKind::Method && n.name == name && n.qualified_name.starts_with(&prefix)) {
                return Some(own.clone());
            }
            next.extend(ctx.get_supertype_nodes(&ty.id).iter().cloned());
        }
        level = next;
        depth += 1;
    }
    None
}

/// The service the `\Drupal::service('x')->method()` on this reference's line asks for.
fn service_on_line(r: &UnresolvedRef, method: &str, ctx: &Ctx) -> Option<String> {
    let lines = ctx.get_file_lines(&r.file_path)?;
    let line = lines.get((r.line as usize).checked_sub(1)?)?;
    re!(r#"Drupal::service\(\s*['"]([\w.\\-]+)['"]\s*\)\s*->\s*(\w+)"#, "g")
        .captures_iter(line)
        .find(|m| m.text(2) == Some(method))
        .and_then(|m| m.text(1).map(String::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_yml_becomes_routes_with_handlers() {
        let src = "my_module.hello:\n  path: '/hello'\n  defaults:\n    _controller: '\\Drupal\\my_module\\Controller\\HelloController::hello'\n    _title: 'Hello'\n  methods: [get, post]\n  requirements:\n    _permission: 'access content'\nmy_module.form:\n  path: /form\n  defaults:\n    _form: '\\Drupal\\my_module\\Form\\MyForm'\n";
        let result = DrupalResolver.extract("web/modules/my_module/my_module.routing.yml", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["/hello [GET,POST]", "/form"]);
        let refs: Vec<&str> = result.references.iter().map(|r| r.reference_name.as_str()).collect();
        assert_eq!(refs, vec!["\\Drupal\\my_module\\Controller\\HelloController::hello", "\\Drupal\\my_module\\Form\\MyForm"]);
        assert_eq!(result.nodes[0].qualified_name, "web/modules/my_module/my_module.routing.yml::my_module.hello");
    }

    #[test]
    fn hooks_by_docblock_and_by_name() {
        let src = "<?php\n\n/**\n * Implements hook_form_alter().\n */\nfunction anything_goes(&$form) {}\n\nfunction my_module_cron() {}\n";
        let result = DrupalResolver.extract("web/modules/my_module/my_module.module", src).unwrap();
        let refs: Vec<(&str, u32)> = result.references.iter().map(|r| (r.reference_name.as_str(), r.line)).collect();
        assert_eq!(refs, vec![("hook_form_alter", 6), ("hook_cron", 8)]);
        assert!(result.references[0].from_node_id.starts_with("function:"));
    }

    #[test]
    fn fqcn_last_segment() {
        assert_eq!(last_segment("\\Drupal\\x\\Form\\MyForm").as_deref(), Some("MyForm"));
        assert_eq!(last_segment("NoNamespace"), None);
    }
}

/// Services, plugins and `#[Hook]` (upstream a2870563; port of drupal-services.test.ts).
#[cfg(test)]
mod services_tests {
    use super::*;

    fn extract(file: &str, source: &str) -> FrameworkExtractionResult {
        DrupalResolver.extract(file, source).unwrap()
    }

    const FILE: &str = "modules/shop/shop.services.yml";

    fn refs(source: &str, service: &str) -> Vec<String> {
        let out = extract(FILE, source);
        let node = out.nodes.iter().find(|n| n.name == format!("service:{service}")).unwrap();
        out.references.iter().filter(|r| r.from_node_id == node.id).map(|r| r.reference_name.clone()).collect()
    }

    #[test]
    fn a_node_per_service_named_by_its_id_with_its_class() {
        let out = extract(FILE, "services:\n  shop.cart:\n    class: Drupal\\shop\\Cart\n  shop.quoted:\n    class: '\\Drupal\\shop\\Quoted'\n");
        let got: Vec<(NodeKind, String, String, Option<String>, u32)> =
            out.nodes.into_iter().map(|n| (n.kind, n.name, n.qualified_name, n.signature, n.start_line)).collect();
        assert_eq!(
            got,
            [
                (NodeKind::Component, "service:shop.cart".into(), format!("{FILE}::service:shop.cart"), Some("Drupal\\shop\\Cart".into()), 2),
                (NodeKind::Component, "service:shop.quoted".into(), format!("{FILE}::service:shop.quoted"), Some("Drupal\\shop\\Quoted".into()), 4),
            ]
        );
    }

    #[test]
    fn links_the_class_and_every_service_a_definition_is_given() {
        let source = "services:\n  shop.cart:\n    class: Drupal\\shop\\Cart\n    parent: shop.base\n    decorates: shop.old_cart\n    arguments: ['@database', '@?logger.channel.shop', '%shop.setting%', '@@literal']\n    factory: ['@shop.factory', create]\n    calls:\n      - [setLogger, ['@logger.factory']]\n    tags:\n      - { name: event_subscriber }\n";
        assert_eq!(
            refs(source, "shop.cart"),
            [
                "drupal:fqcn:Drupal\\shop\\:Cart",
                "drupal:service:shop.base",
                "drupal:service:shop.old_cart",
                "drupal:service:database",
                "drupal:service:logger.channel.shop",
                "drupal:service:shop.factory",
                "drupal:service:logger.factory",
            ]
        );
    }

    #[test]
    fn reads_a_block_list_of_arguments_and_leaves_a_comment_out() {
        let source = "services:\n  shop.cart:\n    class: Drupal\\shop\\Cart\n    arguments:\n      - '@database' # not '@logger.factory'\n      # - '@cache.default'\n      - \"@current_user\"\n";
        assert_eq!(refs(source, "shop.cart"), ["drupal:fqcn:Drupal\\shop\\:Cart", "drupal:service:database", "drupal:service:current_user"]);
    }

    #[test]
    fn reads_an_alias_in_both_forms_and_a_class_named_service() {
        let source = "services:\n  shop.alias: '@shop.cart'\n  shop.long_alias:\n    alias: shop.cart\n  Drupal\\shop\\Autowired: ~\n  Drupal\\shop\\CartInterface: '@shop.cart'\n  shop.inline: { class: Drupal\\shop\\Inline, arguments: ['@database'] }\n";
        assert_eq!(refs(source, "shop.alias"), ["drupal:service:shop.cart"]);
        assert_eq!(refs(source, "shop.long_alias"), ["drupal:service:shop.cart"]);
        assert_eq!(refs(source, "Drupal\\shop\\Autowired"), ["drupal:fqcn:Drupal\\shop\\:Autowired"]);
        assert_eq!(refs(source, "Drupal\\shop\\CartInterface"), ["drupal:service:shop.cart"]);
        assert_eq!(refs(source, "shop.inline"), ["drupal:fqcn:Drupal\\shop\\:Inline", "drupal:service:database"]);
    }

    #[test]
    fn skips_what_isnt_a_service() {
        let source = "parameters:\n  shop.setting: true\nservices:\n  _defaults:\n    autowire: true\n  _instanceof:\n    Drupal\\shop\\Thing:\n      tags: [thing]\n  shop.cart:\n    class: Drupal\\shop\\Cart\n";
        assert_eq!(extract(FILE, source).nodes.into_iter().map(|n| n.name).collect::<Vec<_>>(), ["service:shop.cart"]);
        assert!(extract(FILE, "").nodes.is_empty());
    }

    fn plugins(file: &str, source: &str) -> Vec<(String, String)> {
        extract(file, source).nodes.into_iter().map(|n| (n.name, n.qualified_name.replace(&format!("{file}::"), ""))).collect()
    }

    fn one(name: &str) -> Vec<(String, String)> {
        let (ty, id) = name.split_once(':').unwrap();
        vec![(name.to_string(), format!("plugin:{ty}:{id}"))]
    }

    #[test]
    fn a_plugin_from_an_attribute_multi_line_or_not_with_the_id_named_or_first() {
        let a = "<?php\nnamespace Drupal\\shop\\Plugin\\Block;\n\n#[Block(\n  admin_label: new TranslatableMarkup(\"Don't\"),\n  id: 'shop_a',\n)]\nclass A extends BlockBase {}\n";
        assert_eq!(plugins("src/Plugin/Block/A.php", a), one("Block:shop_a"));
        assert_eq!(plugins("src/Plugin/views/field/B.php", "<?php\n#[ViewsField(\"shop_b\")]\nclass B extends FieldPluginBase {}\n"), one("ViewsField:shop_b"));
        assert_eq!(plugins("src/Element/C.php", "<?php\n#[\\Drupal\\Core\\Render\\Attribute\\RenderElement('shop_c')]\nclass C {}\n"), one("RenderElement:shop_c"));
    }

    #[test]
    fn a_plugin_from_a_docblock() {
        let d = "<?php\n/**\n * The block.\n *\n * @Block(\n *   admin_label = @Translation(\"D (legacy)\"),\n *   id = \"shop_d\",\n * )\n */\nclass D extends BlockBase {}\n";
        assert_eq!(plugins("src/Plugin/Block/D.php", d), one("Block:shop_d"));
        assert_eq!(plugins("src/Element/E.php", "<?php\n/**\n * @FormElement(\"shop_e\")\n */\nclass E {}\n"), one("FormElement:shop_e"));
    }

    #[test]
    fn a_contrib_modules_own_plugin_type_by_folder() {
        assert_eq!(
            plugins("modules/log/src/Plugin/Log/LogType/Harvest.php", "<?php\n#[LogType(id: 'harvest', label: new TranslatableMarkup('Harvest'))]\nfinal class Harvest extends FarmLogType {}\n"),
            one("LogType:harvest")
        );
        assert_eq!(plugins("src/Entity/Order.php", "<?php\n#[ContentEntityType(id: 'shop_order')]\nclass Order extends ContentEntityBase {}\n"), one("ContentEntityType:shop_order"));
    }

    #[test]
    fn a_plugin_links_the_class_it_marks() {
        let out = extract("src/Plugin/Block/A.php", "<?php\n#[Block(id: 'a')]\nclass A {}\n");
        assert_eq!(out.references.len(), 1);
        assert_eq!(out.references[0].from_node_id, out.nodes[0].id);
        assert_eq!(out.references[0].reference_name, "drupal:class:src/Plugin/Block/A.php#A");
        assert!(out.references[0].is_kind(EdgeKind::References));
    }

    #[test]
    fn not_a_plugin_outside_those_folders_in_a_test_or_with_nothing_to_go_by() {
        assert!(plugins("src/Service/A.php", "<?php\n#[Block(id: 'a')]\nclass A {}\n").is_empty());
        assert!(plugins("tests/src/Plugin/ATest.php", "<?php\n#[Group('shop')]\nclass ATest extends TestCase {}\n").is_empty());
        assert!(plugins("src/Plugin/Base.php", "<?php\n#[\\AllowDynamicProperties]\nclass Base {}\n").is_empty());
        assert!(plugins("src/Plugin/Marker.php", "<?php\n#[\\Attribute(\\Attribute::TARGET_CLASS)]\nclass Marker {}\n").is_empty());
        assert!(plugins("src/Plugin/Old.php", "<?php\n#[Deprecated('use New')]\nclass Old {}\n").is_empty());
        assert!(plugins("src/Plugin/Doc.php", "<?php\n/**\n * @see Foo(id = \"x\")\n * @param string $x\n */\nclass Doc {}\n").is_empty());
    }

    fn hooks(source: &str) -> Vec<String> {
        extract("src/Hook/ShopHooks.php", source).references.into_iter().map(|r| format!("{}@{}", r.reference_name, r.line)).collect()
    }

    #[test]
    fn a_hook_attribute_links_the_method_from_the_attribute_line() {
        assert_eq!(hooks("<?php\nclass ShopHooks {\n  /**\n   * Implements hook_cron().\n   */\n  #[Hook('cron')]\n  public function cron(): void {}\n}\n"), ["hook_cron@6"]);
    }

    #[test]
    fn stacked_hook_attributes_and_others_among_them() {
        let source = "<?php\nclass ShopHooks {\n  #[Hook('node_insert')]\n  #[Hook('node_update', order: new OrderBefore(['entity']))]\n  #[LegacyHook]\n  public function save(): void {}\n}\n";
        assert_eq!(hooks(source), ["hook_node_insert@3", "hook_node_update@3"]);
    }

    #[test]
    fn a_hook_on_a_class_names_the_method_or_means_invoke() {
        let source = "<?php\n#[Hook('form_alter', method: 'alter')]\n#[Hook('cron')]\nclass ShopHooks {\n  public function alter(): void {}\n  public function __invoke(): void {}\n}\n";
        assert_eq!(hooks(source), ["hook_form_alter@5", "hook_cron@6"]);
    }

    #[test]
    fn skips_a_hook_in_a_comment_and_one_with_no_method_after_it() {
        assert!(hooks("<?php\nclass ShopHooks {\n  // #[Hook('old')]\n  /* #[Hook('older')] */\n  public function kept(): void {}\n}\n").is_empty());
        assert!(hooks("<?php\nclass ShopHooks {\n  #[Hook('alone')]\n}\n").is_empty());
    }
}
