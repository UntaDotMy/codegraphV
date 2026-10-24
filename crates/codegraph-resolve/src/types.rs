//! Reference-resolution types — port of `src/resolution/types.ts`.

use crate::ctx::Ctx;
use codegraph_jsre::re;
use codegraph_types::{EdgeKind, Language, Metadata, Node, NodeKind, ReferenceKind};
use std::rc::Rc;

/// An unresolved reference from extraction.
#[derive(Debug, Clone, PartialEq)]
pub struct UnresolvedRef {
    /// ID of the source node containing the reference.
    pub from_node_id: String,
    /// The name being referenced.
    pub reference_name: String,
    pub reference_kind: ReferenceKind,
    pub line: u32,
    pub column: u32,
    pub file_path: String,
    pub language: Language,
    /// Possible qualified names it might resolve to.
    pub candidates: Option<Vec<String>>,
    /// `unresolved_refs.id` when loaded from the database — post-pass cleanup
    /// targets exactly this row instead of every same-key sibling (#1269).
    pub row_id: Option<i64>,
    /// The tail a ref that failed to resolve is parked under, when it is not the
    /// one its name gives (see `importBindingTail`).
    pub name_tail: Option<String>,
}

impl UnresolvedRef {
    pub fn new(
        from_node_id: impl Into<String>,
        reference_name: impl Into<String>,
        reference_kind: impl Into<ReferenceKind>,
        line: u32,
        column: u32,
        file_path: impl Into<String>,
        language: Language,
    ) -> Self {
        UnresolvedRef {
            from_node_id: from_node_id.into(),
            reference_name: reference_name.into(),
            reference_kind: reference_kind.into(),
            line,
            column,
            file_path: file_path.into(),
            language,
            candidates: None,
            row_id: None,
            name_tail: None,
        }
    }

    pub fn is_kind(&self, kind: EdgeKind) -> bool {
        self.reference_kind == ReferenceKind::Edge(kind)
    }
}

/// How a reference was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolvedBy {
    ExactMatch,
    Import,
    QualifiedName,
    Framework,
    Fuzzy,
    InstanceMethod,
    FilePath,
    FunctionRef,
}

impl ResolvedBy {
    pub const fn as_str(self) -> &'static str {
        match self {
            ResolvedBy::ExactMatch => "exact-match",
            ResolvedBy::Import => "import",
            ResolvedBy::QualifiedName => "qualified-name",
            ResolvedBy::Framework => "framework",
            ResolvedBy::Fuzzy => "fuzzy",
            ResolvedBy::InstanceMethod => "instance-method",
            ResolvedBy::FilePath => "file-path",
            ResolvedBy::FunctionRef => "function-ref",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlsoTarget {
    pub target_node_id: String,
    pub metadata: Option<Metadata>,
}

/// A resolved reference.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRef {
    /// Original unresolved reference.
    pub original: UnresolvedRef,
    /// ID of the target node.
    pub target_node_id: String,
    /// Confidence score (0-1).
    pub confidence: f64,
    pub resolved_by: ResolvedBy,
    /// Edge kind the edge should carry when it is NOT the ref's own kind — a
    /// framework that turns a `calls` ref into a `navigates` edge, for example.
    pub edge_kind: Option<EdgeKind>,
    /// Extra metadata the strategy wants persisted on the edge (`href`, …).
    pub metadata: Option<Metadata>,
    /// The OTHER targets, when one reference names several.
    pub also_targets: Option<Vec<AlsoTarget>>,
}

impl ResolvedRef {
    pub fn new(original: &UnresolvedRef, target_node_id: impl Into<String>, confidence: f64, resolved_by: ResolvedBy) -> Self {
        ResolvedRef {
            original: original.clone(),
            target_node_id: target_node_id.into(),
            confidence,
            resolved_by,
            edge_kind: None,
            metadata: None,
            also_targets: None,
        }
    }
}

/// Result of resolution attempt.
#[derive(Debug, Clone, Default)]
pub struct ResolutionResult {
    pub resolved: Vec<ResolvedRef>,
    pub unresolved: Vec<UnresolvedRef>,
    pub stats: ResolutionStats,
}

#[derive(Debug, Clone, Default)]
pub struct ResolutionStats {
    pub total: usize,
    pub resolved: usize,
    pub unresolved: usize,
    pub by_method: indexmap::IndexMap<String, usize>,
}

/// Result of framework-specific file extraction.
#[derive(Debug, Clone, Default)]
pub struct FrameworkExtractionResult {
    pub nodes: Vec<Node>,
    pub references: Vec<UnresolvedRef>,
}

/// Nodes a framework can name only from several files at once, with the
/// references that bind each — and how to recognise the ones an earlier run
/// produced, so a run can remove those it no longer wants.
pub struct CrossFileNodes {
    pub nodes: Vec<Node>,
    pub references: Vec<UnresolvedRef>,
    /// The kind every one of these nodes has.
    pub kind: NodeKind,
    /// True for a node this pass produces, and for no node extraction does.
    pub owns: Box<dyn Fn(&Node) -> bool>,
}

/// The navigation calls a router binds to its routes (see `FrameworkResolver::navigation`).
pub struct NavigationCalls {
    /// The name tails of the calls `claims_reference` accepts as navigation.
    pub tails: Vec<&'static str>,
    /// The files whose navigation calls can name `route`: the path prefixes of
    /// the apps whose table it is in (`""` for every file), or `None` when it is
    /// not one of this router's routes.
    pub scope: Box<dyn Fn(&Node, &Ctx) -> Option<Vec<String>>>,
}

/// Framework-specific resolver.
pub trait FrameworkResolver {
    /// Framework name.
    fn name(&self) -> &str;
    /// Languages this framework applies to: `extract()` runs only on files in
    /// them, and `resolve()` sees only references written in them (unless
    /// `resolve_languages` says otherwise). `None` applies to all languages.
    fn languages(&self) -> Option<&[Language]> {
        None
    }
    /// The languages whose references `resolve()` sees, when they are not `languages`.
    fn resolve_languages(&self) -> Option<&[Language]> {
        None
    }
    /// Packages an app declares when it is built on this framework.
    fn app_dependencies(&self) -> Option<&[&'static str]> {
        None
    }
    /// Detect if project uses this framework (project-level, called once at startup).
    fn detect(&self, ctx: &Ctx) -> bool;
    /// Resolve a reference using framework-specific patterns.
    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef>;
    /// Opt a reference NAME through the resolver's name-exists pre-filter.
    fn claims_reference(&self, _name: &str) -> bool {
        false
    }
    /// Extract framework-specific nodes and references from a file; `None` when
    /// the framework has no extraction hook.
    fn extract(&self, _file_path: &str, _content: &str) -> Option<FrameworkExtractionResult> {
        None
    }
    /// Cross-file finalization pass; `None` when the framework has none.
    fn post_extract(&self, _ctx: &Ctx) -> Option<Vec<Node>> {
        None
    }
    /// Nodes no single file's `extract()` can decide on; `None` when none.
    fn cross_file_nodes(&self, _ctx: &Ctx) -> Option<CrossFileNodes> {
        None
    }
    fn navigation(&self) -> Option<&NavigationCalls> {
        None
    }
    /// The modules whose content `resolve()` reads to answer `ref`, a route node's reference.
    fn lazy_modules(&self, _r: &UnresolvedRef, _ctx: &Ctx) -> Option<Vec<String>> {
        None
    }
}

/// Import mapping from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportMapping {
    /// Local name used in the file.
    pub local_name: String,
    /// Original exported name (may differ due to aliasing).
    pub exported_name: String,
    /// Source module/path.
    pub source: String,
    pub is_default: bool,
    pub is_namespace: bool,
    /// Resolved file path (if local).
    pub resolved_path: Option<String>,
}

impl ImportMapping {
    pub fn new(local_name: impl Into<String>, exported_name: impl Into<String>, source: impl Into<String>, is_default: bool, is_namespace: bool) -> Self {
        ImportMapping {
            local_name: local_name.into(),
            exported_name: exported_name.into(),
            source: source.into(),
            is_default,
            is_namespace,
            resolved_path: None,
        }
    }
}

/// Re-export from a file: `export { x } from './other'` or `export * from './other'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReExport {
    Named {
        /// Name as exported by THIS file.
        exported_name: String,
        /// Name in the upstream module (differs when renamed: `as`).
        original_name: String,
        source: String,
    },
    Wildcard {
        source: String,
    },
    /// `export * as ns from './other'`: only `ns` is exported, the module's members through it.
    Namespace {
        exported_name: String,
        source: String,
    },
}

impl ReExport {
    pub fn source(&self) -> &str {
        match self {
            ReExport::Named { source, .. } | ReExport::Wildcard { source } | ReExport::Namespace { source, .. } => source,
        }
    }
}

/// Node kinds an `extends`/`implements` edge may legally TARGET.
pub fn is_supertype_target_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Interface
            | NodeKind::Trait
            | NodeKind::Protocol
            | NodeKind::Enum
            | NodeKind::Union
            | NodeKind::TypeAlias
            | NodeKind::Component
            | NodeKind::Module
            | NodeKind::Namespace
    )
}

/// Scala singleton objects are values, unlike inheritable Ruby modules.
pub fn is_supertype_target(node: &Node) -> bool {
    is_supertype_target_kind(node.kind) && !(node.language == Language::Scala && node.kind == NodeKind::Module)
}

/// True for the reference kinds that assert an inheritance/conformance relation.
pub fn is_inheritance_ref(r: &UnresolvedRef) -> bool {
    r.is_kind(EdgeKind::Extends) || r.is_kind(EdgeKind::Implements)
}

/// Can an `imports` reference legally resolve to this node kind?
pub fn is_importable_kind(kind: NodeKind) -> bool {
    !matches!(kind, NodeKind::Property | NodeKind::Field | NodeKind::Method | NodeKind::EnumMember | NodeKind::Parameter)
}

/// Does `signature` mark a C/C++ function-like `#define` constant?
pub fn is_cpp_define_signature(signature: &str) -> bool {
    re!(r"^\s*#\s*define\b").is_match(signature)
}

pub type NodeRef = Rc<Node>;
pub type NodeList = Rc<Vec<NodeRef>>;

/// A TypeScript `T | null | undefined` result: `Undefined` means "this strategy has no opinion,
/// keep looking", `Null` means "decided: nothing", `Value` a hit.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Tri<T> {
    #[default]
    Undefined,
    Null,
    Value(T),
}

impl<T> Tri<T> {
    pub fn is_undefined(&self) -> bool {
        matches!(self, Tri::Undefined)
    }
    /// The value, collapsing `Undefined` and `Null` into `None` (`result ?? null`).
    pub fn value(self) -> Option<T> {
        match self {
            Tri::Value(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_value(&self) -> Option<&T> {
        match self {
            Tri::Value(v) => Some(v),
            _ => None,
        }
    }
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Tri<U> {
        match self {
            Tri::Undefined => Tri::Undefined,
            Tri::Null => Tri::Null,
            Tri::Value(v) => Tri::Value(f(v)),
        }
    }
}

impl<T> From<Option<T>> for Tri<T> {
    /// `None` is `Null` — a decided "nothing" — not `Undefined`.
    fn from(o: Option<T>) -> Self {
        match o {
            Some(v) => Tri::Value(v),
            None => Tri::Null,
        }
    }
}
