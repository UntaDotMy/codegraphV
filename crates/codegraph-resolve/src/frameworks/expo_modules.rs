//! Expo Modules framework — close the JS → native flow for Expo SDK packages. Port of
//! `src/resolution/frameworks/expo-modules.ts`.
//!
//! Expo Modules use a Swift / Kotlin DSL distinct from the React Native legacy bridge. Each
//! native module is a class extending `Module` whose `definition()` body declares the JS surface
//! via literal `Name(...)`, `Function(...)`, `AsyncFunction(...)`, `Property(...)`. Tree-sitter
//! parses these as ordinary call expressions with trailing closures, so the JS-visible methods
//! don't exist as named symbol nodes by default.
//!
//! The framework extractor walks the file source for those declarative literals and emits method
//! nodes named `takePictureAsync` / `notificationAsync` / `width` / etc., attributed to the Swift
//! / Kotlin file. The standard name-matcher then resolves JS `Foo.takePictureAsync(...)` to them.
//!
//! Real-world shape (expo-haptics):
//!
//! ```swift
//! public class HapticsModule: Module {
//!   public func definition() -> ModuleDefinition {
//!     Name("ExpoHaptics")
//!     AsyncFunction("notificationAsync") { ... }
//!   }
//! }
//! ```

use super::now_ms;
use crate::ctx::Ctx;
use crate::import_resolver::resolve_import_path;
use crate::types::{AlsoTarget, FrameworkExtractionResult, FrameworkResolver, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::utf16_len;
use codegraph_types::{EdgeKind, Language, Node, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const SWIFT_KOTLIN: [Language; 2] = [Language::Swift, Language::Kotlin];
const JS_LANGUAGES: [Language; 4] = [Language::TypeScript, Language::Tsx, Language::JavaScript, Language::Jsx];

/// `Function("name")`, `AsyncFunction("name")`, `Property("name")` or `Constants("name")` — the
/// open paren on the same line as the keyword, which matches every real Expo Module declaration
/// style. The optional `<…>` covers Kotlin's GENERIC-typed declarations
/// (`AsyncFunction<Float>("getBatteryLevelAsync")`, `AsyncFunction<Int, String>(…)`) — without
/// it, every Android Expo Module method was silently dropped.
const EXPO_DECL_RE: &str = r#"\b(Function|AsyncFunction|Property|Constants)\s*(?:<[^(]*>)?\s*\(\s*["']([A-Za-z_][A-Za-z0-9_]*)["']"#;

/// The module name literal `Name("ExpoX")`. Enriches each emitted method's qualifiedName so the
/// same JS callsite to `Foo.fn` doesn't ambiguate across multiple Expo modules in a monorepo.
const EXPO_MODULE_NAME_RE: &str = r#"\bName\s*\(\s*["']([A-Za-z_][A-Za-z0-9_]*)["']"#;

/// Heuristic class-name match — a fallback if `Name(...)` literal isn't found. Detects
/// `class XxxModule: Module` (Swift) or `class XxxModule : Module` (Kotlin).
const EXPO_CLASS_RE: &str = r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)\s*:\s*Module\b";

/// Whether a file is plausibly an Expo Module — both the `: Module` inheritance and at least one
/// declarative `Function(...)` / `AsyncFunction(...)` / `Property(...)` literal. Any one of those
/// alone produces too many false positives.
fn is_expo_module_source(source: &str) -> bool {
    if !Re::cached(EXPO_CLASS_RE, "").is_match(source) {
        return false;
    }
    Re::cached(EXPO_DECL_RE, "g").is_match(source)
}

/// Each `Function("X") { … }` / `AsyncFunction("X") { … }` / `Property("X") { … }` literal of a
/// Swift / Kotlin source file becomes a method node named `X`, attributed to the file at the
/// line of the literal.
fn extract_expo_methods(file_path: &str, source: &str, language: Language) -> Vec<Node> {
    if !is_expo_module_source(source) {
        return Vec::new();
    }
    let mut nodes: Vec<Node> = Vec::new();

    let name_match = Re::cached(EXPO_MODULE_NAME_RE, "").captures(source);
    let class_match = Re::cached(EXPO_CLASS_RE, "").captures(source);
    // Prefer the explicit `Name("X")` literal — that's the JS-visible module name. Class name is
    // the fallback.
    let module_name = name_match.as_ref().and_then(|m| m.text(1)).or_else(|| class_match.as_ref().and_then(|m| m.text(1))).unwrap_or("ExpoModule").to_string();

    let now = now_ms();
    let mut seen_at_line: HashSet<String> = HashSet::new();
    let decl = Re::cached(EXPO_DECL_RE, "g");
    for m in decl.captures_iter(source) {
        let Some(whole) = m.get(0) else { continue };
        let kind = m.text(1).unwrap_or("");
        let method_name = m.text(2).unwrap_or("");
        // Compute line number from match index.
        let before = &source[..whole.start()];
        let start_line = before.matches('\n').count() as u32 + 1;
        // Avoid duplicates if the same method literal appears twice in one file (e.g., declared
        // and re-declared inside a `View {...}` block).
        if !seen_at_line.insert(format!("{method_name}:{start_line}")) {
            continue;
        }

        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let start_column = utf16_len(&before[line_start..]) as u32;
        // We don't extract the closure body's end-line — use the literal's line as a single-line
        // range. trace/explore still surfaces the declaration site, which is the main
        // user-visible signal.
        let mut node = Node::new(
            format!("expo-module:{file_path}:{module_name}:{method_name}:{start_line}"),
            NodeKind::Method,
            method_name,
            format!("{file_path}::{module_name}.{method_name}"),
            file_path,
            language,
            start_line,
            start_line,
            start_column,
            start_column + (utf16_len(kind) + 2 + utf16_len(method_name) + 2) as u32,
            now,
        );
        node.docstring = Some(format!("Expo Modules {kind}(\"{method_name}\") in {module_name}"));
        node.signature = Some(format!("{kind}(\"{method_name}\")"));
        node.is_exported = Some(true);
        nodes.push(node);
    }

    nodes
}

/// Detect Expo Modules by looking at the project's package.json or a small scan of source files
/// for the `: Module` + declarative-DSL markers. Either signal suffices.
fn detect_expo_modules(ctx: &Ctx) -> bool {
    if ctx.read_file("package.json").is_some_and(|p| !p.is_empty() && re!(r#"["']expo-modules-core["']\s*:"#).is_match(&p)) {
        return true;
    }
    for f in ctx.get_all_files().iter().take(200) {
        if f.is_empty() {
            continue;
        }
        if (f.ends_with(".swift") || f.ends_with(".kt")) && ctx.read_file(f).is_some_and(|src| !src.is_empty() && is_expo_module_source(&src)) {
            return true;
        }
    }
    false
}

pub struct ExpoModulesResolver;

impl FrameworkResolver for ExpoModulesResolver {
    fn name(&self) -> &str {
        "expo-modules"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&SWIFT_KOTLIN)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        detect_expo_modules(ctx)
    }

    /// Per-file extraction — the orchestrator invokes this for every `.swift` / `.kt` file in the
    /// project. We only emit nodes when the file looks like an Expo Module; otherwise return empty.
    fn extract(&self, file_path: &str, source: &str) -> Option<FrameworkExtractionResult> {
        let language = if file_path.ends_with(".kt") { Language::Kotlin } else { Language::Swift };
        Some(FrameworkExtractionResult { nodes: extract_expo_methods(file_path, source, language), references: Vec::new() })
    }

    /// No bespoke resolution needed — the synthetic method nodes emitted by `extract()` get
    /// picked up by the standard name-matcher when a JS callsite like
    /// `Foo.takePictureAsync(args)` resolves.
    fn resolve(&self, _r: &UnresolvedRef, _ctx: &Ctx) -> Option<ResolvedRef> {
        None
    }
}

// =============================================================================
// The JS side: `ExpoCamera.scanFromURLAsync(…)` → the module's function
// =============================================================================

/// `requireNativeModule('ExpoCamera')`, typed or not, optional or not.
const REQUIRE_NATIVE: &str = r#"\brequire(?:Optional)?NativeModule\s*(?:<\s*([A-Za-z_$][\w$]*)[^>]*>)?\s*\(\s*['"]([A-Za-z_$][\w$]*)['"]\s*\)"#;

struct NativeBinding {
    /// The `Name("…")` the module declares.
    module: String,
    /// The declared JS type, from `requireNativeModule<CameraNativeModule>(…)`.
    type_name: Option<String>,
}

#[derive(Default)]
struct BindingMemo(RefCell<HashMap<String, Option<Rc<NativeBinding>>>>);

/// What `name` is bound to in `file`: `const X = requireNativeModule('N')` here, or the import of
/// a module whose default export (or a named export) is one — expo-camera's
/// `import CameraManager from './ExpoCameraManager'`, whose whole body is
/// `export default requireNativeModule<CameraNativeModule>('ExpoCamera')`.
fn native_binding(name: &str, file: &str, ctx: &Ctx) -> Option<Rc<NativeBinding>> {
    let memo = ctx.memo::<BindingMemo>();
    let key = format!("{file}\0{name}");
    if let Some(hit) = memo.0.borrow().get(&key) {
        return hit.clone();
    }
    let mut found: Option<Rc<NativeBinding>> = None;
    if let Some(own) = ctx.read_file(file).filter(|o| o.contains("NativeModule")) {
        let pattern = format!(r"\b(?:const|let|var)\s+{}\s*(?::[^=]+)?=\s*{}", escape_regex(name), REQUIRE_NATIVE);
        let stripped = codegraph_text::strip_comments_for_regex(&own, "typescript");
        let local_re = Re::cached(&pattern, "");
        let local = local_re.captures(&stripped).map(|local| NativeBinding { type_name: local.text(1).map(String::from), module: local.text(2).unwrap_or("").to_string() });
        found = local.map(Rc::new);
    }
    if found.is_none() {
        let language = if re!(r"\.tsx$").is_match(file) {
            Language::Tsx
        } else if re!(r"\.[cm]?ts$").is_match(file) {
            Language::TypeScript
        } else if re!(r"\.jsx$").is_match(file) {
            Language::Jsx
        } else {
            Language::JavaScript
        };
        let mappings = ctx.get_import_mappings(file, language);
        let binding = mappings.iter().find(|m| m.local_name == name);
        let target = binding.filter(|b| b.source.starts_with('.')).and_then(|b| resolve_import_path(&b.source, file, language, ctx));
        let content = target.as_deref().and_then(|t| ctx.read_file(t));
        if let (Some(binding), Some(_), Some(content)) = (binding, &target, content.filter(|c| c.contains("NativeModule"))) {
            let safe = codegraph_text::strip_comments_for_regex(&content, "typescript");
            let pattern = if binding.is_default {
                format!(r"\bexport\s+default\s+{REQUIRE_NATIVE}")
            } else {
                format!(r"\bexport\s+(?:const|let)\s+{}\s*(?::[^=]+)?=\s*{}", escape_regex(&binding.exported_name), REQUIRE_NATIVE)
            };
            let export_re = Re::cached(&pattern, "");
            let exported = export_re.captures(&safe).map(|m| NativeBinding { type_name: m.text(1).map(String::from), module: m.text(2).unwrap_or("").to_string() });
            found = exported.map(Rc::new);
        }
    }
    memo.0.borrow_mut().insert(key, found.clone());
    found
}

/// JS calls into an Expo module, bound by the module's NAME, not by the method name alone:
/// expo-camera calls its module `CameraManager`, so `CameraManager.isAvailableAsync()` went to
/// `CameraView`'s own static `isAvailableAsync` in the same file. The native functions the module
/// declares are the targets — Swift first, Kotlin beside it — and when it declares none of that
/// name, the method on the type the binding is given (`requireNativeModule<CameraNativeModule>`).
pub struct ExpoModulesJsResolver;

impl FrameworkResolver for ExpoModulesJsResolver {
    fn name(&self) -> &str {
        "expo-modules-js"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&JS_LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        detect_expo_modules(ctx)
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        if !r.is_kind(EdgeKind::Calls) {
            return None;
        }
        let m = re!(r"^([A-Za-z_$][\w$]*)\.([A-Za-z_$][\w$]*)$").captures(&r.reference_name)?;
        let binding = native_binding(m.text(1)?, &r.file_path, ctx)?;
        let func = m.text(2)?;
        let by_name = ctx.get_nodes_by_name(func);
        let suffix = format!("::{}.{func}", binding.module);
        let mut declared: Vec<_> = by_name.iter().filter(|n| n.id.starts_with("expo-module:") && n.qualified_name.ends_with(&suffix)).collect();
        declared.sort_by_key(|n| if n.language == Language::Swift { 0 } else { 1 });
        if let Some(first) = declared.first() {
            let mut resolved = ResolvedRef::new(r, first.id.clone(), 0.95, ResolvedBy::Framework);
            if declared.len() > 1 {
                resolved.also_targets = Some(declared[1..].iter().map(|n| AlsoTarget { target_node_id: n.id.clone(), metadata: None }).collect());
            }
            return Some(resolved);
        }
        if let Some(type_name) = &binding.type_name {
            let typed_suffix = format!("{type_name}::{func}");
            let typed = by_name.iter().find(|n| matches!(n.kind, NodeKind::Method | NodeKind::Property | NodeKind::Field) && n.qualified_name.ends_with(&typed_suffix));
            if let Some(typed) = typed {
                return Some(ResolvedRef::new(r, typed.id.clone(), 0.9, ResolvedBy::Framework));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_become_method_nodes() {
        let src = "public class HapticsModule: Module {\n  public func definition() -> ModuleDefinition {\n    Name(\"ExpoHaptics\")\n    AsyncFunction(\"notificationAsync\") { }\n    AsyncFunction<Float>(\"level\") { }\n  }\n}\n";
        let result = ExpoModulesResolver.extract("ios/HapticsModule.swift", src).unwrap();
        let names: Vec<(&str, &str)> = result.nodes.iter().map(|n| (n.name.as_str(), n.qualified_name.as_str())).collect();
        assert_eq!(names, vec![("notificationAsync", "ios/HapticsModule.swift::ExpoHaptics.notificationAsync"), ("level", "ios/HapticsModule.swift::ExpoHaptics.level")]);
        assert_eq!(result.nodes[0].start_column, 4);
        assert_eq!(result.nodes[0].end_column, 4 + "AsyncFunction".len() as u32 + 2 + "notificationAsync".len() as u32 + 2);
        assert!(ExpoModulesResolver.extract("x.swift", "class X: Module {}").unwrap().nodes.is_empty());
    }
}
