//! The shared pick behind the framework resolvers' name heuristics — "a `…Service` is a class
//! under `/services/`", "a PascalCase name is a model under `/models/`", "`router` is a FastAPI
//! router under `/routers/`". Port of `src/resolution/frameworks/name-heuristic.ts`.
//!
//! The pick considers only what the reference can see, and starts from its own scope.

use crate::ctx::Ctx;
use crate::name_matcher::reach::is_lexically_reachable;
use crate::name_matcher::visibility::is_visible_across_files;
use crate::types::UnresolvedRef;
use codegraph_types::{EdgeKind, Node, NodeKind, ReferenceKind};

/// Node kinds a nested type can be declared in.
const TYPE_OWNER_KINDS: [NodeKind; 6] = [NodeKind::Class, NodeKind::Struct, NodeKind::Interface, NodeKind::Enum, NodeKind::Trait, NodeKind::Protocol];

#[derive(Default)]
pub struct NamePickOptions<'a> {
    /// Whether a file in the reference's directory comes before the conventional folders — its
    /// package (JVM, Go, C#, Python). Not for Rust, where a sibling file is another module,
    /// reached only through a `use`. `Some(false)` turns it off.
    pub same_directory: Option<bool>,
    /// A language's own scope rule over the candidates (Rust's `use` / module paths).
    pub accept: Option<&'a dyn Fn(&Node) -> bool>,
}

/// The id of the node named `r.reference_name`, of one of `kinds`, that the reference can see:
/// never a declaration inside some function body, never a type nested in another file's type (a
/// nested type is reached by its bare name only from inside its owner), never one the language
/// keeps out of the reference's file (a `private` member, an unexported Go name, a C# namespace
/// the file does not use, a test suite from production code). Its own file's first, then (by
/// default) its directory's, then a conventional folder's.
pub fn pick_by_name_and_kind(
    r: &UnresolvedRef,
    kinds: &[NodeKind],
    in_preferred_dir: &dyn Fn(&str) -> bool,
    ctx: &Ctx,
    options: &NamePickOptions,
) -> Option<String> {
    let inheritance = matches!(r.reference_kind, ReferenceKind::Edge(EdgeKind::Extends) | ReferenceKind::Edge(EdgeKind::Implements));
    let candidates: Vec<_> = ctx
        .get_nodes_by_name(&r.reference_name)
        .iter()
        .filter(|n| {
            kinds.contains(&n.kind)
                // A type never inherits from itself.
                && !(inheritance && n.id == r.from_node_id)
                && is_lexically_reachable(n, r, ctx)
                && (n.file_path == r.file_path || (!is_nested_type(n, ctx) && is_visible_across_files(n, r, ctx)))
                && options.accept.is_none_or(|accept| accept(n))
        })
        .cloned()
        .collect();
    if candidates.is_empty() {
        return None;
    }
    if let Some(same_file) = candidates.iter().find(|n| n.file_path == r.file_path) {
        return Some(same_file.id.clone());
    }
    if options.same_directory != Some(false) {
        let dir = &r.file_path[..r.file_path.rfind('/').map(|i| i + 1).unwrap_or(0)];
        if let Some(same_dir) = candidates.iter().find(|n| n.file_path.starts_with(dir) && !n.file_path[dir.len()..].contains('/')) {
            return Some(same_dir.id.clone());
        }
    }
    Some(candidates.iter().find(|n| in_preferred_dir(&n.file_path)).unwrap_or(&candidates[0]).id.clone())
}

/// Whether a node is declared inside a type of its own file.
fn is_nested_type(n: &Node, ctx: &Ctx) -> bool {
    let Some(cut) = n.qualified_name.rfind("::") else { return false };
    let owner = &n.qualified_name[..cut];
    ctx.get_nodes_in_file(&n.file_path).iter().any(|p| p.qualified_name == owner && TYPE_OWNER_KINDS.contains(&p.kind))
}
