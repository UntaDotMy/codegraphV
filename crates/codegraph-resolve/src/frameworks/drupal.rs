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
//! Services / plugins, Twig symbol extraction and virtual `hook_*` nodes are out of scope here
//! exactly as they are in the TypeScript resolver.

use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::node_id;
use codegraph_types::jscompat::js_trim;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use indexmap::IndexMap;

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
        name.starts_with("hook_") || name.contains('\\') || re!(r"^[A-Za-z_]\w*::?\w+$").is_match(name)
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
            if let Some(class_name) = last_segment(name) {
                if let Some(cls) = ctx.get_nodes_by_name(&class_name).iter().find(|n| n.kind == NodeKind::Class) {
                    return Some(ResolvedRef::new(r, cls.id.clone(), 0.85, ResolvedBy::Framework));
                }
            }
        }

        // hook_X — find any function whose name ends in _{hookSuffix} in a hook file
        if let Some(hook_suffix) = name.strip_prefix("hook_") {
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

        if is_drupal_hook_file(file_path) || file_path.ends_with(".php") {
            return Some(extract_drupal_hooks(file_path, content));
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
