//! React Native Fabric / Codegen view components. Port of `src/resolution/frameworks/fabric.ts`.
//!
//! In the new RN architecture, JS-visible view components are declared via Codegen TS spec files
//! of the shape:
//!
//! ```ts
//! // src/fabric/MyComponentNativeComponent.ts
//! export interface NativeProps extends ViewProps {
//!   color?: ColorValue;
//!   onTap?: CT.DirectEventHandler<TapEvent>;
//! }
//! export default codegenNativeComponent<NativeProps>('MyComponent');
//! ```
//!
//! Codegen then wires the JS component name to a native implementation class — by RN convention,
//! one of `MyComponent`, `MyComponentView`, `MyComponentComponentView`, `MyComponentManager`,
//! `MyComponentViewManager`. Without bridging, JSX `<MyComponent color="red"/>` has nothing in
//! the graph to land on — the JS-visible name `MyComponent` isn't a node anywhere (only
//! `MyComponentView` is, in the .mm), and the JSX synthesizer matches strictly by name.
//!
//! What this extractor does:
//!   1. Parse the spec file's `codegenNativeComponent<Props>('Name', ...)` literal — emit a
//!      `component` node named `Name`, attributed to the spec file.
//!   2. Parse the `NativeProps` interface and emit one `property` node per prop, attributed to
//!      the spec file.
//!
//! A companion synthesizer (`fabricNativeImplEdges` in callback-synthesizer) links the emitted
//! component node to its native implementation class via the convention-based name+suffix lookup.

use super::now_ms;
use crate::ctx::Ctx;
use crate::types::{FrameworkExtractionResult, FrameworkResolver, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::jscompat::{utf16_len, utf16_prefix};
use codegraph_types::{Language, Node, NodeKind};
use std::collections::HashSet;

const LANGUAGES: [Language; 7] = [Language::TypeScript, Language::Tsx, Language::JavaScript, Language::Jsx, Language::ObjC, Language::Java, Language::Kotlin];

/// Derive the JS-visible component name from a native ViewManager class. Strip a trailing
/// `Manager` (and optionally `ViewManager`) — RN's view registry maps `XXXManager` ↔ JS `<XXX/>`
/// by this convention. The leading `RCT` prefix is also stripped.
fn derive_component_name_from_manager(class_name: &str) -> String {
    let mut name = class_name.strip_prefix("RCT").unwrap_or(class_name);
    // Trim ViewManager > Manager > View, in order.
    if let Some(stem) = name.strip_suffix("ViewManager") {
        name = stem;
    } else if let Some(stem) = name.strip_suffix("Manager") {
        name = stem;
    }
    name.to_string()
}

/// Cheap source-level detector. A Paper-era module names its native view the same way:
/// segmented-control's `module.exports = requireNativeComponent('RNCSegmentedControl')`.
fn is_fabric_spec(source: &str) -> bool {
    source.contains("codegenNativeComponent") || source.contains("requireNativeComponent")
}

/// The `NativeProps` interface body out of a Fabric spec source; `None` when the interface isn't
/// declared in the expected shape.
fn find_native_props_body(source: &str) -> Option<&str> {
    // Permissive: `export interface NativeProps [extends X, Y] { … }`.
    re!(r"export\s+interface\s+NativeProps\b[^{]*\{([\s\S]*?)\n\}").captures(source)?.text(1)
}

/// Prop names of the NativeProps interface body. Each prop is `name?: Type;` or `name: Type;` on
/// its own line. We don't care about types — just the JS-visible name.
fn extract_prop_names(body: &str) -> Vec<String> {
    let mut props: Vec<String> = Vec::new();
    // Anchor to start-of-line (after optional whitespace), then capture an identifier, then
    // optional `?`, then `:`.
    for m in re!(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\??\s*:", "gm").captures_iter(body) {
        let Some(whole) = m.get(0) else { continue };
        let name = m.text(1).unwrap_or("");
        // Exclude any line that immediately turns into a function-shape (a literal `name(arg: T): R`
        // is a method declaration).
        let after = utf16_prefix(&body[whole.end()..], 80);
        if re!(r"^\s*\(").is_match(after) {
            continue; // method-shape, skip
        }
        props.push(name.to_string());
    }
    props
}

fn line_at(source: &str, idx: usize) -> u32 {
    source[..idx].matches('\n').count() as u32 + 1
}

#[allow(clippy::too_many_arguments)]
fn exported_node(
    id: String,
    kind: NodeKind,
    name: &str,
    qualified_name: String,
    file_path: &str,
    language: Language,
    start_line: u32,
    start_column: u32,
    end_column: u32,
    docstring: String,
    signature: Option<String>,
    now: i64,
) -> Node {
    let mut node = Node::new(id, kind, name, qualified_name, file_path, language, start_line, start_line, start_column, end_column, now);
    node.docstring = Some(docstring);
    node.signature = signature;
    node.is_exported = Some(true);
    node
}

/// Legacy Paper view-manager declarations from a .m/.mm file: a `component` node named after the
/// JS-visible name (derived from the @implementation class) plus a `property` node per
/// `RCT_EXPORT_VIEW_PROPERTY(name, ...)` macro. Empty if the file doesn't look like a
/// ViewManager (no RCT_EXPORT_VIEW_PROPERTY macros).
fn extract_legacy_view_manager_nodes(file_path: &str, source: &str) -> Vec<Node> {
    // Cheap gate: no view-property macros at all → not a view manager.
    if !source.contains("RCT_EXPORT_VIEW_PROPERTY") && !source.contains("RCT_CUSTOM_VIEW_PROPERTY") && !source.contains("RCT_REMAP_VIEW_PROPERTY") {
        return Vec::new();
    }
    let Some(impl_match) = re!(r"@implementation\s+([A-Za-z_][A-Za-z0-9_]*)").captures(source) else { return Vec::new() };
    let Some(class_name) = impl_match.text(1).filter(|c| !c.is_empty()) else { return Vec::new() };
    // Only process actual ViewManagers — classes ending in Manager or (legacy) ViewManager.
    // Classes with view-property macros that don't follow the naming convention are unusual;
    // skip to keep precision.
    if !class_name.ends_with("Manager") && !class_name.ends_with("ViewManager") {
        return Vec::new();
    }
    let component_name = derive_component_name_from_manager(class_name);
    if component_name.is_empty() {
        return Vec::new();
    }

    let now = now_ms();
    let mut nodes: Vec<Node> = Vec::new();

    // Component node — same shape as Codegen Fabric's, so the fabricNativeImplEdges synthesizer
    // linking component → native class works for legacy too. The native class IS the manager
    // itself in this case; the convention-based suffix lookup in the synthesizer (`Manager`,
    // `ViewManager`) will find it.
    let start_line = line_at(source, impl_match.get(0).map(|w| w.start()).unwrap_or(0));
    nodes.push(exported_node(
        format!("fabric-component:{file_path}:{component_name}:{start_line}"),
        NodeKind::Component,
        &component_name,
        format!("{file_path}::{component_name}"),
        file_path,
        Language::ObjC,
        start_line,
        0,
        utf16_len(&component_name) as u32,
        format!("Legacy Paper ViewManager component '{component_name}' (from @implementation {class_name})"),
        Some(format!("RCT_EXPORT_MODULE() // ViewManager: {class_name}")),
        now,
    ));

    // Property nodes per RCT_EXPORT_VIEW_PROPERTY macro.
    let mut seen: HashSet<String> = HashSet::new();
    for m in re!(r"\bRCT_(?:EXPORT|CUSTOM|REMAP)_VIEW_PROPERTY\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)", "g").captures_iter(source) {
        let Some(whole) = m.get(0) else { continue };
        let prop_name = m.text(1).unwrap_or("");
        if !seen.insert(prop_name.to_string()) {
            continue;
        }
        let prop_line = line_at(source, whole.start());
        nodes.push(exported_node(
            format!("fabric-prop:{file_path}:{prop_name}:{prop_line}"),
            NodeKind::Property,
            prop_name,
            format!("{file_path}::{component_name}.{prop_name}"),
            file_path,
            Language::ObjC,
            prop_line,
            0,
            utf16_len(prop_name) as u32,
            format!("Legacy Paper view prop '{prop_name}' on {component_name}"),
            None,
            now,
        ));
    }
    nodes
}

/// Java/Kotlin `@ReactProp("name")` extraction. The annotation precedes a setter method on a
/// class that extends `ViewManager` / `SimpleViewManager` (or in Kotlin, `:` syntax). Empty if no
/// @ReactProp annotations are found.
fn extract_jvm_view_manager_nodes(file_path: &str, source: &str) -> Vec<Node> {
    if !source.contains("@ReactProp") {
        return Vec::new();
    }

    // Class name — looking for `class FooManager [extends ViewManager...]` (Java) or
    // `class FooManager : ViewManager...` (Kotlin). Either gates us into a ViewManager file;
    // non-Manager classes with @ReactProp are unusual.
    let Some(class_match) = re!(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)\b").captures(source) else { return Vec::new() };
    let Some(class_name) = class_match.text(1).filter(|c| !c.is_empty()) else { return Vec::new() };
    if !class_name.ends_with("Manager") && !class_name.ends_with("ViewManager") {
        return Vec::new();
    }
    let component_name = derive_component_name_from_manager(class_name);
    if component_name.is_empty() {
        return Vec::new();
    }

    let language = if file_path.ends_with(".kt") { Language::Kotlin } else { Language::Java };
    let now = now_ms();
    let mut nodes: Vec<Node> = Vec::new();

    let start_line = line_at(source, class_match.get(0).map(|w| w.start()).unwrap_or(0));
    nodes.push(exported_node(
        format!("fabric-component:{file_path}:{component_name}:{start_line}"),
        NodeKind::Component,
        &component_name,
        format!("{file_path}::{component_name}"),
        file_path,
        language,
        start_line,
        0,
        utf16_len(&component_name) as u32,
        format!("Android view-manager component '{component_name}' (from class {class_name})"),
        Some(format!("class {class_name} : ViewManager")),
        now,
    ));

    // @ReactProp("name") followed (after optional modifiers / args) by a setter declaration. The
    // annotation argument is the JS-visible prop name. Permissive about the rest — we only need
    // the literal.
    let mut seen: HashSet<String> = HashSet::new();
    for m in re!(r#"@ReactProp\s*\(\s*(?:name\s*=\s*)?"([^"]+)""#, "g").captures_iter(source) {
        let Some(whole) = m.get(0) else { continue };
        let prop_name = m.text(1).unwrap_or("");
        if !seen.insert(prop_name.to_string()) {
            continue;
        }
        let prop_line = line_at(source, whole.start());
        nodes.push(exported_node(
            format!("fabric-prop:{file_path}:{prop_name}:{prop_line}"),
            NodeKind::Property,
            prop_name,
            format!("{file_path}::{component_name}.{prop_name}"),
            file_path,
            language,
            prop_line,
            0,
            utf16_len(prop_name) as u32,
            format!("Android @ReactProp prop '{prop_name}' on {component_name}"),
            None,
            now,
        ));
    }
    nodes
}

fn extract_fabric_nodes(file_path: &str, source: &str) -> Vec<Node> {
    if !is_fabric_spec(source) {
        return Vec::new();
    }

    let now = now_ms();
    let mut nodes: Vec<Node> = Vec::new();

    // The spec file's language by extension (a Paper module is often Flow `.js`).
    let spec_language = if file_path.ends_with(".tsx") {
        Language::Tsx
    } else if re!(r"\.[cm]?ts$").is_match(file_path) {
        Language::TypeScript
    } else if file_path.ends_with(".jsx") {
        Language::Jsx
    } else {
        Language::JavaScript
    };

    for m in re!(r#"\b(codegenNativeComponent|requireNativeComponent)\s*(?:<[^>]+>)?\s*\(\s*['"]([A-Za-z_][A-Za-z0-9_]*)['"]"#, "g").captures_iter(source) {
        let Some(whole) = m.get(0) else { continue };
        let declared_with = m.text(1).unwrap_or("");
        let component_name = m.text(2).unwrap_or("");
        let before = &source[..whole.start()];
        let start_line = before.matches('\n').count() as u32 + 1;
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let start_column = utf16_len(&before[line_start..]) as u32;

        // The component itself — kind: 'component' so the existing reactJsxChildEdges
        // synthesizer matches `<MyComponent>` JSX tags to it (its name+kind filter is the gate).
        let codegen = declared_with == "codegenNativeComponent";
        nodes.push(exported_node(
            format!("fabric-component:{file_path}:{component_name}:{start_line}"),
            NodeKind::Component,
            component_name,
            format!("{file_path}::{component_name}"),
            file_path,
            spec_language,
            start_line,
            start_column,
            start_column + utf16_len(declared_with) as u32,
            if codegen { format!("Fabric/Codegen native component '{component_name}'") } else { format!("Native component '{component_name}' (requireNativeComponent)") },
            Some(if codegen { format!("codegenNativeComponent<NativeProps>('{component_name}')") } else { format!("requireNativeComponent('{component_name}')") }),
            now,
        ));
    }

    // Props from the NativeProps interface. These are not "method" semantic — they're JS-visible
    // bindings the consumer sets via JSX attributes — so use `property` kind.
    if let Some(body) = find_native_props_body(source).filter(|b| !b.is_empty()) {
        let body_start = source.find(body).unwrap_or(0);
        for prop_name in extract_prop_names(body) {
            let prop_line = match source[body_start..].find(&prop_name) {
                Some(rel) => line_at(source, body_start + rel),
                None => 1,
            };
            nodes.push(exported_node(
                format!("fabric-prop:{file_path}:{prop_name}:{prop_line}"),
                NodeKind::Property,
                &prop_name,
                format!("{file_path}::NativeProps.{prop_name}"),
                file_path,
                if file_path.ends_with(".tsx") { Language::Tsx } else { Language::TypeScript },
                prop_line,
                0,
                utf16_len(&prop_name) as u32,
                format!("Fabric NativeProps prop '{prop_name}'"),
                None,
                now,
            ));
        }
    }

    nodes
}

pub struct FabricViewResolver;

impl FrameworkResolver for FabricViewResolver {
    fn name(&self) -> &str {
        "fabric-view"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        // Root package.json is the common case. The indexer only tracks SOURCE files in
        // getAllFiles(), so package.jsons in subpackages aren't enumerable that way — we have to
        // probe them explicitly via listDirectories() for monorepos.
        let check_pkg = |relative_path: &str| ctx.read_file(relative_path).is_some_and(|pkg| !pkg.is_empty() && re!(r#"["']react-native["']\s*:"#).is_match(&pkg));
        if check_pkg("package.json") {
            return true;
        }
        // Monorepo escape hatch — react-native-skia and similar workspace repos have the RN dep
        // only in `packages/<sub>/package.json`. Walk the common workspace roots one level deep.
        for root in ["packages", "apps", "modules", "libraries"] {
            for sub in ctx.list_directories(root) {
                if check_pkg(&format!("{root}/{sub}/package.json")) {
                    return true;
                }
            }
        }
        false
    }

    fn extract(&self, file_path: &str, source: &str) -> Option<FrameworkExtractionResult> {
        // Pick the right extractor by file language. The framework registry already filters by
        // `languages` so we only see relevant files.
        let nodes = if re!(r"\.(?:[cm]?[jt]s|[jt]sx)$").is_match(file_path) {
            extract_fabric_nodes(file_path, source)
        } else if file_path.ends_with(".m") || file_path.ends_with(".mm") {
            extract_legacy_view_manager_nodes(file_path, source)
        } else if file_path.ends_with(".java") || file_path.ends_with(".kt") {
            extract_jvm_view_manager_nodes(file_path, source)
        } else {
            Vec::new()
        };
        Some(FrameworkExtractionResult { nodes, references: Vec::new() })
    }

    // The companion synthesizer (`fabricNativeImplEdges`) handles cross-language edges; standard
    // name resolution handles <MyComponent> → component-node via the JSX synthesizer.
    fn resolve(&self, _r: &UnresolvedRef, _ctx: &Ctx) -> Option<ResolvedRef> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codegen_spec_yields_component_and_props() {
        let src = "import { codegenNativeComponent } from 'react-native';\nexport interface NativeProps extends ViewProps {\n  color?: ColorValue;\n  onTap?: CT.DirectEventHandler<TapEvent>;\n  doIt(arg: string): void;\n}\nexport default codegenNativeComponent<NativeProps>('MyComponent');\n";
        let result = FabricViewResolver.extract("src/MyComponentNativeComponent.ts", src).unwrap();
        let summary: Vec<(NodeKind, &str, u32)> = result.nodes.iter().map(|n| (n.kind, n.name.as_str(), n.start_line)).collect();
        assert_eq!(summary, vec![(NodeKind::Component, "MyComponent", 7), (NodeKind::Property, "color", 3), (NodeKind::Property, "onTap", 4)]);
    }

    #[test]
    fn view_managers_derive_component_names() {
        assert_eq!(derive_component_name_from_manager("RCTMapViewManager"), "Map");
        assert_eq!(derive_component_name_from_manager("CameraManager"), "Camera");
        let m = "@implementation RCTFooManager\nRCT_EXPORT_VIEW_PROPERTY(bar, NSString)\n@end\n";
        let nodes = extract_legacy_view_manager_nodes("ios/Foo.m", m);
        assert_eq!(nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), vec!["Foo", "bar"]);
    }
}
