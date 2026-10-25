//! Swift ↔ Objective-C bridge resolver. Port of `src/resolution/frameworks/swift-objc.ts`.
//!
//! Closes the cross-language flow gap in mixed iOS codebases. The pure bridging name math lives
//! in `crate::swift_objc_bridge`; this file wires it into the resolution pipeline.
//!
//! **Two directions to close:**
//!
//! 1. **Swift call → ObjC method** — a Swift caller writes `imageDownloader.download(url:completion:)`.
//!    Tree-sitter-swift parses this as a call whose callee identifier is `download` (parameter
//!    labels live in the argument list, not the callee), while the ObjC implementation is
//!    `-downloadURL:completion:`. From the bare Swift name, look up ObjC methods whose bridged
//!    Swift base name would be `download` (the reverse map, built once per session).
//! 2. **ObjC call → Swift method** — an ObjC caller writes `[swiftThing fooWithBar:42]`, a
//!    selector no Swift node is named. Derive candidate Swift base names from the selector and
//!    look up Swift methods named those.
//!
//! Every edge produced here is a framework-resolved reference at `confidence: 0.6`.

use crate::ctx::Ctx;
use crate::swift_objc_bridge::{is_objc_exposed, swift_base_names_for_objc_selector};
use crate::types::{FrameworkResolver, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::re;
use codegraph_types::{Language, Node, NodeKind};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const LANGUAGES: [Language; 2] = [Language::Swift, Language::ObjC];

/// Names that are too generic to bridge with any precision. These are common Cocoa / NSObject
/// conventions that almost every ObjC class implements; if a Swift caller writes `init()` or
/// `description`, mapping it to an arbitrary project-local ObjC method of the same name produces
/// noise, not signal.
///
/// Critically, refs of these names virtually always resolve via the regular name-matcher (every
/// project has many `init` nodes) — skipping them here just keeps the bridge from competing with
/// name-match on already-handled refs.
const GENERIC_NAMES: &[&str] = &[
    "init",
    "description",
    "debugDescription",
    "hash",
    "isEqual",
    "isEqualTo",
    "copy",
    "mutableCopy",
    "class",
    "self",
    "count",
    "length",
    "value",
    "name",
    "data",
    "string",
    "object",
    "add",
    "remove",
    "update",
    "load",
    "save",
    "reload",
    "cancel",
    "start",
    "stop",
    "pause",
    "resume",
    "close",
    "open",
    "show",
    "hide",
    "toString",
    "dealloc",
    "release",
    "retain",
    "autorelease",
];

fn is_generic(name: &str) -> bool {
    GENERIC_NAMES.contains(&name)
}

/// Window of source text around a Swift declaration used by `is_objc_exposed` to spot `@objc` /
/// `@nonobjc` annotations. Read line above + the declaration line — Swift attributes typically
/// sit on the preceding line (`@objc` on a line of its own) or inline.
const SOURCE_PROBE_LINES: usize = 3;

type ObjcMap = HashMap<String, Vec<NodeRef>>;

/// Memoized "Swift base name → ObjC method nodes" map, per resolution context.
#[derive(Default)]
struct ObjcMapMemo(RefCell<Option<Rc<ObjcMap>>>);

/// Build the reverse-bridge map: for every ObjC method node in the graph, compute the Swift base
/// names that would auto-bridge to its selector and record the node under each.
fn build_objc_map(ctx: &Ctx) -> Rc<ObjcMap> {
    let memo = ctx.memo::<ObjcMapMemo>();
    if let Some(cached) = memo.0.borrow().as_ref() {
        return Rc::clone(cached);
    }

    let mut map: ObjcMap = HashMap::new();
    for node in ctx.get_nodes_by_kind(NodeKind::Method).iter().filter(|n| n.language == Language::ObjC) {
        for c in swift_base_names_for_objc_selector(&node.name) {
            // Skip the trivial case where the Swift base name equals the ObjC method name
            // verbatim (no colons) — the regular name-matcher already handles that and our map
            // would just duplicate the work.
            if c == node.name && !node.name.contains(':') {
                continue;
            }
            // Skip generic Cocoa names (init, description, etc.) — they would false-positive
            // against any project-local ObjC method of the same name. The regular name-matcher
            // handles them.
            if is_generic(&c) {
                continue;
            }
            map.entry(c).or_default().push(Rc::clone(node));
        }
    }
    let map = Rc::new(map);
    *memo.0.borrow_mut() = Some(Rc::clone(&map));
    map
}

/// A small window of source ending at `node.start_line`, used to inspect Swift attribute
/// annotations attached to a declaration. Empty if the source can't be read.
fn declaration_source_window(node: &Node, ctx: &Ctx) -> String {
    let Some(content) = ctx.read_file(&node.file_path).filter(|c| !c.is_empty()) else { return String::new() };
    let lines = re!(r"\r?\n").split(&content);
    let start_idx = (node.start_line as usize).saturating_sub(1 + SOURCE_PROBE_LINES);
    let end_idx = lines.len().min(node.start_line as usize);
    if start_idx >= end_idx {
        return String::new();
    }
    lines[start_idx..end_idx].join("\n")
}

/// The part of a possibly receiver-qualified name after its last `.`.
fn after_last_dot(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// Try to resolve a Swift caller's bare reference to an ObjC implementation: look up the ObjC
/// reverse-bridge map for nodes whose Swift base name would match, and return the first.
fn resolve_swift_call_to_objc(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    // Swift call sites of `obj.foo(bar:)` reach the resolver as either bare name `foo`
    // (tree-sitter-swift) or qualified `obj.foo` — strip prefix.
    let raw_name = after_last_dot(&r.reference_name);

    let map = build_objc_map(ctx);
    let target = map.get(raw_name)?.first()?;
    // A candidate reaching us is a legitimate cross-language hit: this resolver runs AFTER
    // exact-match, so any matching Swift node would already have won.
    Some(ResolvedRef::new(r, target.id.clone(), 0.6, ResolvedBy::Framework))
}

/// Try to resolve an ObjC caller's selector reference to a Swift `@objc` implementation: derive
/// candidate Swift base names from the selector, look up Swift methods named that, and verify
/// with a source-window check that the declaration is `@objc`-exposed.
fn resolve_objc_call_to_swift(r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
    // ObjC call sites get receiver-prefixed when the receiver isn't self/super:
    // `[obj foo:bar:]` becomes `obj.foo:bar:`. Strip the receiver prefix to recover the raw
    // selector for the bridge math.
    let raw_selector = after_last_dot(&r.reference_name);

    // Bridge math only applies to selector-shape names (contain `:`).
    if !raw_selector.contains(':') {
        return None;
    }

    for candidate in swift_base_names_for_objc_selector(raw_selector) {
        for m in ctx.get_nodes_by_name(&candidate).iter().filter(|n| n.language == Language::Swift && matches!(n.kind, NodeKind::Method | NodeKind::Function)) {
            let window = declaration_source_window(m, ctx);
            if is_objc_exposed(&window) {
                return Some(ResolvedRef::new(r, m.id.clone(), 0.6, ResolvedBy::Framework));
            }
        }
    }
    None
}

pub struct SwiftObjcBridgeResolver;

impl FrameworkResolver for SwiftObjcBridgeResolver {
    fn name(&self) -> &str {
        "swift-objc-bridge"
    }

    // Applies to both languages — bridging crosses the boundary.
    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    /// This resolver is relevant when the project has both Swift and Objective-C source.
    /// Either-side-only projects don't need bridging (and the empty reverse-map would be a no-op
    /// anyway).
    fn detect(&self, ctx: &Ctx) -> bool {
        let mut has_swift = false;
        let mut has_objc = false;
        for f in ctx.get_all_files() {
            if f.ends_with(".swift") {
                has_swift = true;
            } else if f.ends_with(".m") || f.ends_with(".mm") {
                has_objc = true;
            }
            if has_swift && has_objc {
                return true;
            }
        }
        false
    }

    /// Let selector-shape references (anything containing a `:`) through the resolver's
    /// name-exists pre-filter — no Swift node has a colon in its name, so without this opt-in
    /// those refs would be dropped before `resolve()` sees them. Bare names without colons are
    /// handled by the regular name-exists pre-filter.
    fn claims_reference(&self, name: &str) -> bool {
        name.contains(':')
    }

    /// Route based on which language the caller is in.
    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        match r.language {
            Language::Swift => resolve_swift_call_to_objc(r, ctx),
            Language::ObjC => resolve_objc_call_to_swift(r, ctx),
            _ => None,
        }
    }
}
