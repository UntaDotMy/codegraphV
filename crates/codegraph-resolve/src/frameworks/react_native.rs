//! React Native cross-language bridge resolver. Port of
//! `src/resolution/frameworks/react-native.ts`.
//!
//! Closes the JS ↔ native flow gap in React Native projects. Covers:
//!
//! **Legacy bridge**: ObjC `RCT_EXPORT_MODULE([opt_name])` / `RCT_EXPORT_METHOD(selector:(args))` /
//! `RCT_REMAP_METHOD(jsName, nativeSelector:(args))`; Java/Kotlin `@ReactMethod` methods on a
//! `ReactContextBaseJavaModule` subclass (the module name from `getName()`'s literal).
//!
//! **TurboModules**: a TS spec interface in a `Native<X>.ts` file exporting
//! `TurboModuleRegistry.getEnforcing<Spec>('<ModuleName>')`; the matching native implementation
//! is a class whose method names match.
//!
//! **Swift modules via `RCT_EXTERN_MODULE`**: a Swift class exposed through a thin `.m` shim; the
//! implementation is the Swift `@objc func` of that name on the class or one of its extensions —
//! a real node from the Swift extractor, so no synthetic node is minted.
//!
//! **Receiver evidence.** `captureView.finalizeCaptureSession()` where `const captureView =
//! NativeModules.CaptureView` names the module as surely as
//! `NativeModules.CaptureView.finalizeCaptureSession()` does: both resolve at 0.95 to that
//! module's method — ahead of the import resolver. A bare `.method()` with no module evidence
//! keeps the by-name match at 0.6.

use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, Metadata, Node, NodeKind};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const LANGUAGES: [Language; 5] = [Language::JavaScript, Language::TypeScript, Language::Tsx, Language::Jsx, Language::ObjC];

/// One native RN method known to the resolver. Indexed by JS-visible name.
struct NativeMethod {
    /// Module name as seen from JS (`Geolocation`, `RNSVGRenderableModule`, …).
    module_name: String,
    /// Native implementation node (ObjC method / Java method / Kotlin function).
    node: NodeRef,
}

struct RnMaps {
    by_js_name: HashMap<String, Vec<NativeMethod>>,
    aliases: HashMap<String, String>,
}

/// Per-context lazy map cache.
#[derive(Default)]
struct RnMapsMemo(RefCell<Option<Rc<RnMaps>>>);

// ─── Native-side extraction ─────────────────────────────────────────────────

/// Default ObjC module name when `RCT_EXPORT_MODULE()` has no argument: strip a leading `RCT`
/// prefix from the class name (Apple's convention) and treat the rest as the JS-visible module
/// name. `RCTGeolocation` → `Geolocation`.
fn default_objc_module_name(class_name: &str) -> &str {
    if class_name.starts_with("RCT") && class_name.len() > 3 {
        &class_name[3..]
    } else {
        class_name
    }
}

struct ObjcExport {
    module_name: String,
    js_name: String,
    native_selector_first_kw: String,
    line: u32,
}

fn line_of(source: &str, idx: usize) -> u32 {
    source[..idx.min(source.len())].matches('\n').count() as u32 + 1
}

/// Parse an ObjC `.m`/`.mm` file's source for `RCT_EXPORT_MODULE` and `RCT_EXPORT_METHOD` /
/// `RCT_REMAP_METHOD` declarations, returning the inferred (moduleName, jsMethodName) pairs.
///
/// Regex-based scan is sufficient — these macros are highly stylized and appear at top level.
fn parse_objc_rn_exports(source: &str, class_name: Option<&str>) -> Vec<ObjcExport> {
    let mut results: Vec<ObjcExport> = Vec::new();

    // RCT_EXPORT_MODULE — one per file by convention. Capture the optional arg.
    let module_match = re!(r"RCT_EXPORT_MODULE\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)?\s*\)").captures(source);
    // Need a module name to attribute methods. Prefer the explicit macro arg, then the class
    // name, then bail (no module = nothing useful to register).
    let module_name = match module_match.as_ref().and_then(|m| m.text(1)) {
        Some(explicit) => Some(explicit.to_string()),
        None => class_name.filter(|c| !c.is_empty()).map(|c| default_objc_module_name(c).to_string()),
    };
    let Some(module_name) = module_name.filter(|m| !m.is_empty()) else { return results };

    // RCT_EXPORT_METHOD(selectorFirstKw:(args)…) — the first keyword (everything up to the first
    // `:` or open paren) is the JS-visible name.
    for m in re!(r"RCT_EXPORT_METHOD\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)", "g").captures_iter(source) {
        if let (Some(kw), Some(whole)) = (m.text(1), m.get(0)) {
            results.push(ObjcExport { module_name: module_name.clone(), js_name: kw.to_string(), native_selector_first_kw: kw.to_string(), line: line_of(source, whole.start()) });
        }
    }

    // RCT_REMAP_METHOD(jsName, nativeSelectorFirstKw:(args)…)
    for m in re!(r"RCT_REMAP_METHOD\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*,\s*([A-Za-z_][A-Za-z0-9_]*)", "g").captures_iter(source) {
        if let (Some(js_name), Some(native_kw), Some(whole)) = (m.text(1), m.text(2), m.get(0)) {
            results.push(ObjcExport { module_name: module_name.clone(), js_name: js_name.to_string(), native_selector_first_kw: native_kw.to_string(), line: line_of(source, whole.start()) });
        }
    }

    results
}

/// The `@implementation` class name in an ObjC file — the fallback module name when
/// `RCT_EXPORT_MODULE()` has no argument.
fn find_objc_class_name(source: &str) -> Option<String> {
    re!(r"@implementation\s+([A-Za-z_][A-Za-z0-9_]*)").captures(source).and_then(|m| m.text(1).map(String::from))
}

pub struct ObjcExtern {
    pub module_name: String,
    pub class_name: String,
    pub js_name: String,
    pub native_selector_first_kw: String,
    pub line: u32,
}

/// `RCT_EXTERN_MODULE` / `RCT_EXTERN_METHOD` — the `.m` shim that exposes a Swift class. One
/// entry per exposed method, each naming the Swift class the implementation lives on (the module
/// name is the class name unless `RCT_EXTERN_REMAP_MODULE` says otherwise).
pub fn parse_objc_rn_externs(source: &str) -> Vec<ObjcExtern> {
    let mut results: Vec<ObjcExtern> = Vec::new();
    let remap = re!(r"RCT_EXTERN_REMAP_MODULE\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*,\s*([A-Za-z_][A-Za-z0-9_]*)").captures(source);
    let plain = re!(r"RCT_EXTERN_MODULE\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)").captures(source);
    let module_name = remap.as_ref().and_then(|m| m.text(1)).or_else(|| plain.as_ref().and_then(|m| m.text(1)));
    let class_name = remap.as_ref().and_then(|m| m.text(2)).or_else(|| plain.as_ref().and_then(|m| m.text(1)));
    let (Some(module_name), Some(class_name)) = (module_name.filter(|m| !m.is_empty()), class_name.filter(|c| !c.is_empty())) else { return results };

    for m in re!(r"RCT_EXTERN_METHOD\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)", "g").captures_iter(source) {
        if let (Some(kw), Some(whole)) = (m.text(1), m.get(0)) {
            results.push(ObjcExtern {
                module_name: module_name.to_string(),
                class_name: class_name.to_string(),
                js_name: kw.to_string(),
                native_selector_first_kw: kw.to_string(),
                line: line_of(source, whole.start()),
            });
        }
    }
    for m in re!(r"RCT_EXTERN_REMAP_METHOD\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*,\s*([A-Za-z_][A-Za-z0-9_]*)", "g").captures_iter(source) {
        if let (Some(js_name), Some(native_kw), Some(whole)) = (m.text(1), m.text(2), m.get(0)) {
            results.push(ObjcExtern {
                module_name: module_name.to_string(),
                class_name: class_name.to_string(),
                js_name: js_name.to_string(),
                native_selector_first_kw: native_kw.to_string(),
                line: line_of(source, whole.start()),
            });
        }
    }
    results
}

/// Local names bound to a native module on the JS side — the receiver evidence `resolve()`
/// trusts:
///
/// ```text
/// const captureView = NativeModules.CaptureView
/// export const { CaptureEvents } = NativeModules
/// const { Geo: geolocation } = NativeModules
/// ```
///
/// Collected across the project by name: an alias is nearly always an exported constant imported
/// elsewhere under the same name. A name bound to two different modules is dropped as ambiguous
/// rather than guessed.
pub fn collect_native_module_aliases(source: &str, aliases: &mut HashMap<String, String>, ambiguous: &mut HashSet<String>) {
    let mut bind = |alias: &str, module_name: &str| {
        if ambiguous.contains(alias) {
            return;
        }
        if let Some(prior) = aliases.get(alias) {
            if prior != module_name {
                aliases.remove(alias);
                ambiguous.insert(alias.to_string());
                return;
            }
        }
        aliases.insert(alias.to_string(), module_name.to_string());
    };
    let direct = re!(r"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=;]+)?=\s*NativeModules\.([A-Z][\w$]*)", "g");
    for m in direct.captures_iter(source) {
        bind(m.text(1).unwrap_or(""), m.text(2).unwrap_or(""));
    }
    let destructured = re!(r"\b(?:const|let|var)\s*\{([^}]+)\}\s*=\s*NativeModules\b", "g");
    for m in destructured.captures_iter(source) {
        for part in m.text(1).unwrap_or("").split(',') {
            let part = codegraph_types::jscompat::js_trim(part);
            if let Some(entry) = re!(r"^([A-Za-z_$][\w$]*)(?:\s*:\s*([A-Za-z_$][\w$]*))?$").captures(part) {
                let first = entry.text(1).unwrap_or("");
                bind(entry.text(2).unwrap_or(first), first);
            }
        }
    }
}

/// A Java/Kotlin source file's `@ReactMethod` annotated methods and the surrounding class's
/// `getName()` return value (the JS-visible module name), falling back to the class name with a
/// `Module` suffix stripped when the literal isn't present.
fn parse_jvm_rn_exports(source: &str) -> Vec<(String, String)> {
    // getName() literal — Java + Kotlin both look something like:
    //   public String getName() { return "Geolocation"; }
    //   fun getName(): String = "Geolocation"
    //   fun getName() = "Geolocation"
    let get_name = re!(r#"\bgetName\s*\([^)]*\)\s*(?::\s*String)?\s*(?:=\s*|\{[^}]*return\s*)"([^"]+)""#).captures(source);
    // Class name fallback.
    let class_match = re!(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)\b[^{]*ReactContextBaseJavaModule")
        .captures(source)
        .or_else(|| re!(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)\b[^{]*ReactPackage").captures(source));
    let module_name = match get_name.as_ref().and_then(|m| m.text(1)) {
        Some(n) => Some(n.to_string()),
        None => class_match.as_ref().and_then(|m| m.text(1)).filter(|c| !c.is_empty()).map(|c| c.strip_suffix("Module").unwrap_or(c).to_string()),
    };
    let Some(module_name) = module_name.filter(|m| !m.is_empty()) else { return Vec::new() };

    // @ReactMethod annotations — followed (after optional modifiers / args / newlines) by either
    // `void <name>(` (Java) or `fun <name>(` (Kotlin).
    let method_regex = re!(r"@ReactMethod\b[^{]*?(?:\bfun\s+|\bvoid\s+|\bpublic\s+\w[\w<>\[\]]*\s+)([A-Za-z_][A-Za-z0-9_]*)\s*\(", "g");
    method_regex.captures_iter(source).filter_map(|m| m.text(1).map(|js| (module_name.clone(), js.to_string()))).collect()
}

/// A TS file's TurboModule spec declaration: its interface lists every JS-visible method, and a
/// `TurboModuleRegistry.get*<Spec>(...)` default export pins the module name. `None` when the file
/// isn't a TurboModule spec.
fn parse_turbo_module_spec(source: &str) -> Option<(String, Vec<String>)> {
    // `TurboModuleRegistry.getEnforcing<Spec>('ModuleName')` or `TurboModuleRegistry.get<Spec>('ModuleName')`.
    let reg_match = re!(r#"TurboModuleRegistry\.(?:getEnforcing|get)\s*<[^>]*>\s*\(\s*['"]([^'"]+)['"]\s*\)"#).captures(source)?;
    let module_name = reg_match.text(1).filter(|m| !m.is_empty())?.to_string();

    // Find `export interface Spec extends TurboModule { … }` and pull each method declaration's
    // name. We don't need types — just names.
    let iface_match = re!(r"export\s+interface\s+Spec\b[^{]*\{([\s\S]*?)\n\}").captures(source)?;
    let body = iface_match.text(1).filter(|b| !b.is_empty())?;

    // Method shape: `name(args): ReturnType;` or `name(): void;`. Skip properties (no parens
    // before colon).
    let methods: Vec<String> = re!(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\(", "gm").captures_iter(body).filter_map(|m| m.text(1).map(String::from)).collect();
    Some((module_name, methods))
}

// ─── Map building ───────────────────────────────────────────────────────────

/// RCTEventEmitter built-ins that every emitter subclass inherits. JS code doesn't directly call
/// these — they're internal plumbing for the `NativeEventEmitter` abstraction. If we leave them
/// in the bridge map, every JS `addListener` / `remove` call gets mis-bridged to whichever
/// emitter happens to define them. Skip during map building.
const RN_EMITTER_BUILTINS: [&str; 6] = ["addListener", "removeListeners", "remove", "invalidate", "startObserving", "stopObserving"];

fn add_entry(by_js_name: &mut HashMap<String, Vec<NativeMethod>>, js_name: &str, entry: NativeMethod) {
    by_js_name.entry(js_name.to_string()).or_default().push(entry);
}

fn build_rn_maps(ctx: &Ctx) -> Rc<RnMaps> {
    let memo = ctx.memo::<RnMapsMemo>();
    if let Some(cached) = memo.0.borrow().as_ref() {
        return Rc::clone(cached);
    }

    let mut by_js_name: HashMap<String, Vec<NativeMethod>> = HashMap::new();
    let mut aliases: HashMap<String, String> = HashMap::new();
    let mut ambiguous_aliases: HashSet<String> = HashSet::new();
    let all_files = ctx.get_all_files();
    // Pre-index native methods by name for fast lookup when matching to their bridge exports.
    let mut objc_methods_by_first_kw: HashMap<String, Vec<NodeRef>> = HashMap::new();
    let mut jvm_methods_by_name: HashMap<String, Vec<NodeRef>> = HashMap::new();
    let mut swift_methods_by_name: HashMap<String, Vec<NodeRef>> = HashMap::new();
    for node in ctx.get_nodes_by_kind(NodeKind::Method).iter() {
        match node.language {
            Language::ObjC => {
                let first_kw = if node.name.contains(':') { node.name.split(':').next().unwrap_or("") } else { node.name.as_str() };
                if !first_kw.is_empty() {
                    objc_methods_by_first_kw.entry(first_kw.to_string()).or_default().push(Rc::clone(node));
                }
            }
            Language::Java | Language::Kotlin => jvm_methods_by_name.entry(node.name.clone()).or_default().push(Rc::clone(node)),
            Language::Swift => swift_methods_by_name.entry(node.name.clone()).or_default().push(Rc::clone(node)),
            _ => {}
        }
    }
    let empty: Vec<NodeRef> = Vec::new();

    for file in &all_files {
        // Legacy bridge — ObjC side.
        if file.ends_with(".m") || file.ends_with(".mm") {
            let Some(source) = ctx.read_file(file).filter(|s| !s.is_empty()) else { continue };
            let class_name = find_objc_class_name(&source);
            for exp in parse_objc_rn_exports(&source, class_name.as_deref()) {
                if RN_EMITTER_BUILTINS.contains(&exp.js_name.as_str()) {
                    continue;
                }
                // Resolve to the native node by selector first-keyword. Multiple ObjC methods may
                // share a first keyword across modules; filter by file path to attribute the
                // export to this module's implementation file.
                let candidates = objc_methods_by_first_kw.get(&exp.native_selector_first_kw).unwrap_or(&empty);
                let Some(node) = candidates.iter().find(|c| c.file_path == *file).or_else(|| candidates.first()) else { continue };
                add_entry(&mut by_js_name, &exp.js_name, NativeMethod { module_name: exp.module_name, node: Rc::clone(node) });
            }
            // Swift-backed module: the shim names the class; the implementation is the Swift
            // method of that name on the class or one of its extensions. Class-scoped, so a
            // same-named method on another Swift type (a `syncSettings` on `CaptureSettings`
            // beside the one on `CaptureView`) is never the answer.
            if re!(r"RCT_EXTERN_(?:REMAP_)?MODULE\b").is_match(&source) {
                for ext in parse_objc_rn_externs(&source) {
                    if RN_EMITTER_BUILTINS.contains(&ext.js_name.as_str()) {
                        continue;
                    }
                    let mut candidates: Vec<&NodeRef> =
                        swift_methods_by_name.get(&ext.native_selector_first_kw).unwrap_or(&empty).iter().filter(|c| c.qualified_name.split("::").any(|seg| seg == ext.class_name)).collect();
                    // By file in code-unit order: collation depends on the host's locale.
                    candidates.sort_by(|a, b| a.file_path.encode_utf16().cmp(b.file_path.encode_utf16()).then(a.start_line.cmp(&b.start_line)));
                    let Some(node) = candidates.first() else { continue };
                    add_entry(&mut by_js_name, &ext.js_name, NativeMethod { module_name: ext.module_name, node: Rc::clone(node) });
                }
            }
        }

        // JS side: the local names bound to `NativeModules.<Module>`.
        if re!(r"\.(?:[cm]?[jt]sx?)$").is_match(file) {
            if let Some(source) = ctx.read_file(file).filter(|s| s.contains("NativeModules")) {
                collect_native_module_aliases(&source, &mut aliases, &mut ambiguous_aliases);
            }
        }

        // Legacy bridge — Java/Kotlin side.
        if file.ends_with(".java") || file.ends_with(".kt") {
            let Some(source) = ctx.read_file(file).filter(|s| !s.is_empty()) else { continue };
            for (module_name, js_name) in parse_jvm_rn_exports(&source) {
                if RN_EMITTER_BUILTINS.contains(&js_name.as_str()) {
                    continue;
                }
                let candidates = jvm_methods_by_name.get(&js_name).unwrap_or(&empty);
                let Some(node) = candidates.iter().find(|c| c.file_path == *file).or_else(|| candidates.first()) else { continue };
                add_entry(&mut by_js_name, &js_name, NativeMethod { module_name, node: Rc::clone(node) });
            }
        }

        // TurboModule spec — TS side.
        if file.ends_with(".ts") || file.ends_with(".tsx") {
            let Some(source) = ctx.read_file(file).filter(|s| !s.is_empty()) else { continue };
            let Some((module_name, methods)) = parse_turbo_module_spec(&source) else { continue };
            // For each spec method, find a matching native implementation by name. The spec's
            // module name doesn't determine the native file path (Codegen wires it via name
            // convention), so we match across all native methods of the right name.
            for method_name in methods {
                if RN_EMITTER_BUILTINS.contains(&method_name.as_str()) {
                    continue;
                }
                // ObjC first-keyword match, then JVM bare-name match. Don't require module-name
                // match for ObjC because the native side may have stripped a prefix.
                let objc_cands = objc_methods_by_first_kw.get(&method_name).unwrap_or(&empty);
                let jvm_cands = jvm_methods_by_name.get(&method_name).unwrap_or(&empty);
                for node in objc_cands.iter().chain(jvm_cands.iter()) {
                    add_entry(&mut by_js_name, &method_name, NativeMethod { module_name: module_name.clone(), node: Rc::clone(node) });
                }
            }
        }
    }

    let result = Rc::new(RnMaps { by_js_name, aliases });
    *memo.0.borrow_mut() = Some(Rc::clone(&result));
    result
}

// ─── Resolver ───────────────────────────────────────────────────────────────

pub struct ReactNativeBridgeResolver;

impl FrameworkResolver for ReactNativeBridgeResolver {
    fn name(&self) -> &str {
        "react-native-bridge"
    }

    // objc/mm included so `extract()` sees the native files — `resolve()` still only redirects JS
    // callers (it returns null for native languages).
    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    /// Extract `RCT_EXPORT_METHOD` / `RCT_REMAP_METHOD` declarations as method nodes. These
    /// macros parse as a macro-expression (an ERROR node), NOT a `method_definition`, so the ObjC
    /// extractor never made a node for them — the iOS half of a native module was invisible, so
    /// a JS call couldn't resolve to it and the cross-platform pairing had nothing to pair. The
    /// node is named by the JS-visible name (the selector's first keyword, or the explicit
    /// `RCT_REMAP_METHOD` JS name) so it matches the Android `@ReactMethod` method.
    fn extract(&self, file_path: &str, source: &str) -> Option<FrameworkExtractionResult> {
        if !file_path.ends_with(".m") && !file_path.ends_with(".mm") {
            return Some(FrameworkExtractionResult::default());
        }
        if !re!(r"RCT_EXPORT_MODULE\b").is_match(source) {
            return Some(FrameworkExtractionResult::default());
        }
        let exports = parse_objc_rn_exports(source, find_objc_class_name(source).as_deref());
        let now = now_ms();
        let mut nodes: Vec<Node> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for e in exports {
            if !seen.insert(e.js_name.clone()) {
                continue;
            }
            let mut node = Node::new(
                format!("rn-export:{file_path}:{}.{}", e.module_name, e.js_name),
                NodeKind::Method,
                e.js_name.as_str(),
                format!("{file_path}::{}.{}", e.module_name, e.js_name),
                file_path,
                Language::ObjC,
                e.line,
                e.line,
                0,
                0,
                now,
            );
            node.is_exported = Some(true);
            node.docstring = Some(format!("RCT_EXPORT_METHOD {} (module {})", e.native_selector_first_kw, e.module_name));
            node.signature = Some(format!("RCT_EXPORT_METHOD({}:…)", e.native_selector_first_kw));
            nodes.push(node);
        }
        Some(FrameworkExtractionResult { nodes, references: Vec::new() })
    }

    /// Detect: package.json depends on `react-native`, OR any source file uses the
    /// `RCT_EXPORT_MODULE` / `RCT_EXTERN_MODULE` / `TurboModuleRegistry` markers. Either signal
    /// is enough — different libraries split the JS package from the native code, so we don't
    /// require both.
    fn detect(&self, ctx: &Ctx) -> bool {
        if ctx.read_file("package.json").is_some_and(|p| !p.is_empty() && re!(r#"["']react-native["']\s*:"#).is_match(&p)) {
            return true;
        }
        // Fallback: scan a small number of files for the macro markers — only looking at the
        // first ones returned by getAllFiles to keep detect() fast on huge repos.
        for f in ctx.get_all_files().iter().take(200) {
            if f.is_empty() {
                continue;
            }
            if f.ends_with(".mm") || f.ends_with(".m") {
                if ctx.read_file(f).is_some_and(|src| !src.is_empty() && re!(r"RCT_EXPORT_MODULE\b|RCT_EXTERN_(?:REMAP_)?MODULE\b").is_match(&src)) {
                    return true;
                }
            }
            if f.ends_with(".ts") || f.ends_with(".tsx") {
                if ctx.read_file(f).is_some_and(|src| !src.is_empty() && re!(r"TurboModuleRegistry\.(?:get|getEnforcing)\s*<").is_match(&src)) {
                    return true;
                }
            }
        }
        false
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        // We only redirect JS callers — native callers don't need this resolver.
        if !matches!(r.language, Language::JavaScript | Language::TypeScript | Language::Tsx | Language::Jsx) {
            return None;
        }

        // JS callsites of `obj.method()` reach the resolver as either `obj.method` (qualified) or
        // `method` (bare). Strip the receiver to get the JS-visible method name — and keep it,
        // as evidence.
        let raw = r.reference_name.as_str();
        let last_dot = raw.rfind('.');
        let name = last_dot.map(|i| &raw[i + 1..]).unwrap_or(raw);

        let maps = build_rn_maps(ctx);
        let entries = maps.by_js_name.get(name).filter(|e| !e.is_empty())?;

        // iOS first — the conventional first-class platform for RN library docs and most graph
        // queries; one edge is recorded either way.
        let pick = |list: &[&NativeMethod]| -> Option<usize> {
            list.iter()
                .position(|e| e.node.language == Language::ObjC)
                .or_else(|| list.iter().position(|e| e.node.language == Language::Swift))
                .or(if list.is_empty() { None } else { Some(0) })
        };
        let metadata = |module: &str| -> Metadata {
            let mut m = Metadata::new();
            m.insert("bridge".into(), serde_json::Value::String("react-native".into()));
            m.insert("module".into(), serde_json::Value::String(module.to_string()));
            m
        };

        // Receiver evidence: `NativeModules.Mod.method`, or an alias the project bound to
        // `NativeModules.Mod`. The module is named, so the match is to THAT module's method at a
        // confidence the import resolver cannot beat — and a module that has no such method is
        // not ours to guess at.
        if let Some(dot) = last_dot {
            let receiver = &raw[..dot];
            let direct = re!(r"^NativeModules\.([A-Z][\w$]*)$").captures(receiver).and_then(|m| m.text(1).map(String::from));
            let module_name = direct.or_else(|| maps.aliases.get(receiver).cloned());
            if let Some(module_name) = module_name {
                let in_module: Vec<&NativeMethod> = entries.iter().filter(|e| e.module_name == module_name).collect();
                let exact = pick(&in_module)?;
                let mut resolved = ResolvedRef::new(r, in_module[exact].node.id.clone(), 0.95, ResolvedBy::Framework);
                resolved.metadata = Some(metadata(&module_name));
                return Some(resolved);
            }
        }

        let all: Vec<&NativeMethod> = entries.iter().collect();
        let target = &all[pick(&all)?];
        let mut resolved = ResolvedRef::new(r, target.node.id.clone(), 0.6, ResolvedBy::Framework);
        resolved.metadata = Some(metadata(&target.module_name));
        Some(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objc_exports_and_remaps() {
        let src = "@implementation RCTGeolocation\nRCT_EXPORT_MODULE()\nRCT_EXPORT_METHOD(getPosition:(RCTResponseSenderBlock)cb) {}\nRCT_REMAP_METHOD(watch, startWatching:(NSString *)x) {}\n@end\n";
        let result = ReactNativeBridgeResolver.extract("ios/Geo.m", src).unwrap();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["getPosition", "watch"]);
        assert_eq!(result.nodes[0].qualified_name, "ios/Geo.m::Geolocation.getPosition");
        assert_eq!(result.nodes[0].start_line, 3);
    }

    #[test]
    fn externs_and_aliases() {
        let src = "@interface RCT_EXTERN_MODULE(CaptureView, RCTViewManager)\nRCT_EXTERN_METHOD(finalize:(NSString *)x)\nRCT_EXTERN_REMAP_METHOD(js, native:(id)y)\n@end\n";
        let ext = parse_objc_rn_externs(src);
        assert_eq!(ext.iter().map(|e| (e.js_name.as_str(), e.native_selector_first_kw.as_str())).collect::<Vec<_>>(), vec![("finalize", "finalize"), ("js", "native")]);
        let mut aliases = HashMap::new();
        let mut ambiguous = HashSet::new();
        collect_native_module_aliases("const a = NativeModules.A;\nconst { B, C: cc } = NativeModules;\nconst a = NativeModules.Other;", &mut aliases, &mut ambiguous);
        assert_eq!(aliases.get("B").map(String::as_str), Some("B"));
        assert_eq!(aliases.get("cc").map(String::as_str), Some("C"));
        assert!(!aliases.contains_key("a") && ambiguous.contains("a"));
    }

    #[test]
    fn jvm_and_turbo_specs() {
        let jvm = "class GeoModule extends ReactContextBaseJavaModule {\n  public String getName() { return \"Geo\"; }\n  @ReactMethod\n  public void getPosition(Callback cb) {}\n}\n";
        assert_eq!(parse_jvm_rn_exports(jvm), vec![("Geo".to_string(), "getPosition".to_string())]);
        let spec = "export interface Spec extends TurboModule {\n  getPosition(cb: () => void): void;\n  readonly x: number;\n}\nexport default TurboModuleRegistry.getEnforcing<Spec>('Geo');\n";
        assert_eq!(parse_turbo_module_spec(spec), Some(("Geo".to_string(), vec!["getPosition".to_string()])));
    }
}
