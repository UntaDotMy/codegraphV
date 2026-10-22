//! The graph's data model — port of the interfaces in `src/types.ts`.
//!
//! JSON field names are the TypeScript camelCase ones, and absent optionals are
//! omitted rather than written as `null`, so a value serialized here is
//! interchangeable with what the TypeScript implementation emits (`--json`
//! output, the MCP tool payloads, `metadata` columns).

use crate::kinds::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// JSON object stored in an edge's `metadata` column.
pub type Metadata = Map<String, Value>;

/// A node in the knowledge graph representing a code symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    /// Unique identifier (hash of file path + qualified name).
    pub id: String,
    pub kind: NodeKind,
    /// Simple name (e.g. "calculateTotal").
    pub name: String,
    /// Fully qualified name (e.g. "src/utils.ts::MathHelper.calculateTotal").
    pub qualified_name: String,
    /// File path relative to the project root.
    pub file_path: String,
    pub language: Language,
    /// 1-indexed.
    pub start_line: u32,
    /// 1-indexed.
    pub end_line: u32,
    /// 0-indexed.
    pub start_column: u32,
    pub end_column: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docstring: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_exported: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_async: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_static: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_abstract: Option<bool>,
    /// Decorators/annotations applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decorators: Option<Vec<String>>,
    /// Generic type parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_parameters: Option<Vec<String>>,
    /// Normalized return/result type name for a function/method (the bare
    /// class name, smart-pointer pointee unwrapped). Captured for C/C++ so
    /// resolution can infer a chained receiver's type from what the inner call
    /// returns (issue #645).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_type: Option<String>,
    /// When the node was last updated (ms since the epoch).
    pub updated_at: i64,
}

impl Node {
    /// A node with only the required fields set; every optional is absent.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        kind: NodeKind,
        name: impl Into<String>,
        qualified_name: impl Into<String>,
        file_path: impl Into<String>,
        language: Language,
        start_line: u32,
        end_line: u32,
        start_column: u32,
        end_column: u32,
        updated_at: i64,
    ) -> Self {
        Node {
            id: id.into(),
            kind,
            name: name.into(),
            qualified_name: qualified_name.into(),
            file_path: file_path.into(),
            language,
            start_line,
            end_line,
            start_column,
            end_column,
            docstring: None,
            signature: None,
            visibility: None,
            is_exported: None,
            is_async: None,
            is_static: None,
            is_abstract: None,
            decorators: None,
            type_parameters: None,
            return_type: None,
            updated_at,
        }
    }
}

/// An edge representing a relationship between two nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub source: String,
    pub target: String,
    pub kind: EdgeKind,
    /// Additional context about the relationship.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Line where the relationship occurs (e.g. a call site).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Column where the relationship occurs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

impl Edge {
    pub fn new(source: impl Into<String>, target: impl Into<String>, kind: EdgeKind) -> Self {
        Edge {
            source: source.into(),
            target: target.into(),
            kind,
            metadata: None,
            line: None,
            column: None,
            provenance: None,
        }
    }
}

/// Error during code extraction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractionError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    pub severity: Severity,
    /// Error code for categorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// Metadata about a tracked file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRecord {
    /// Path relative to the project root.
    pub path: String,
    /// Content hash for change detection.
    pub content_hash: String,
    pub language: Language,
    pub size: u64,
    pub modified_at: i64,
    pub indexed_at: i64,
    pub node_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<ExtractionError>>,
    /// Tool-generated source, decided at index time from the filename
    /// convention OR a generation banner in the file's header. A relevance hint
    /// for ranking, not a hard filter. `None` on indexes built before schema
    /// v9 — treat as "content signal unknown, fall back to the path check".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<bool>,
}

/// Indexed files whose symbols are missing even though their content hash is
/// current — what a hash comparison alone calls "up to date" (#2336). Paths are
/// project-relative and sorted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexHealth {
    /// Files `sync` re-indexes: stored while their grammar could not load
    /// (#2335) or stored with no nodes and no recorded reason (#1541).
    pub needs_reindex: Vec<String>,
    /// Files with a recorded parse error and no symbols from it. Deterministic:
    /// they stay this way until the file (or the parser) changes.
    pub parse_errors: Vec<String>,
}

/// A reference that couldn't be resolved during extraction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnresolvedReference {
    /// ID of the node containing the reference.
    pub from_node_id: String,
    /// Name being referenced.
    pub reference_name: String,
    pub reference_kind: ReferenceKind,
    pub line: u32,
    pub column: u32,
    /// File where the reference occurs (denormalized for performance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Language of the source file (denormalized for performance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// Possible qualified names it might resolve to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<String>>,
    /// `unresolved_refs.id` when this ref was loaded from the database.
    /// Post-pass cleanup (delete-on-resolve / park-as-failed) targets exactly
    /// this row; without it cleanup would also delete SIBLING rows (same
    /// caller, same callee, other lines) that a later batch hasn't attempted
    /// yet (#1269).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_id: Option<i64>,
}

/// Result from parsing a source file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractionResult {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// References that couldn't be resolved yet.
    pub unresolved_references: Vec<UnresolvedReference>,
    pub errors: Vec<ExtractionError>,
    /// Extraction duration in milliseconds.
    pub duration_ms: f64,
}

// =============================================================================
// Query types
// =============================================================================

/// A subgraph containing a subset of the knowledge graph. `nodes` keeps
/// insertion order, as the JavaScript `Map` it replaces did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subgraph {
    pub nodes: IndexMap<String, Node>,
    pub edges: Vec<Edge>,
    /// Root node IDs (entry points).
    pub roots: Vec<String>,
    /// Retrieval confidence for context-style queries. `Low` means the query
    /// resolved only to isolated common-word matches (no entry point
    /// corroborated by 2+ distinct query terms). `None` for graph traversals
    /// that don't run the search-ranking path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
}

/// Options for graph traversal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TraversalOptions {
    /// Maximum depth to traverse (default: unbounded).
    pub max_depth: Option<u32>,
    /// Edge types to follow (default: all).
    pub edge_kinds: Option<Vec<EdgeKind>>,
    /// Node types to include (default: all).
    pub node_kinds: Option<Vec<NodeKind>>,
    pub direction: Option<Direction>,
    /// Maximum nodes to return.
    pub limit: Option<usize>,
    /// Whether to include the starting node.
    pub include_start: Option<bool>,
}

/// Options for searching the graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchOptions {
    pub kinds: Option<Vec<NodeKind>>,
    pub languages: Option<Vec<Language>>,
    pub include_patterns: Option<Vec<String>>,
    pub exclude_patterns: Option<Vec<String>>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub case_sensitive: Option<bool>,
}

/// A search result with relevance scoring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub node: Node,
    /// Relevance score for relative ranking only — higher is more relevant.
    /// NOT normalized and NOT a 0-1 fraction: the FTS path returns an
    /// unbounded BM25 magnitude, while the fuzzy/exact paths return ~0-1.
    pub score: f64,
    /// Matched text snippets for highlighting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlights: Option<Vec<String>>,
}

/// A symbol whose name-segments match prose words from a prompt — the
/// graph-derived signal behind the front-load hook's medium tier. Always
/// verified to exist in `nodes` at the time it is returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentMatch {
    /// Symbol name as indexed (e.g. `OrderStateMachine`).
    pub name: String,
    pub kind: NodeKind,
    pub file_path: String,
    pub start_line: u32,
    /// The prompt words (normalized) that matched this name's segments.
    pub matched_words: Vec<String>,
}

// =============================================================================
// Context types
// =============================================================================

/// An incoming or outgoing reference: the node on the far end and the edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ref {
    pub node: Node,
    pub edge: Edge,
}

/// Context information for code understanding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    /// Primary node being examined.
    pub focal: Node,
    /// Nodes containing the focal node (file, class, etc.).
    pub ancestors: Vec<Node>,
    /// Nodes directly contained by the focal node.
    pub children: Vec<Node>,
    /// Incoming references (who calls/uses this).
    pub incoming_refs: Vec<Ref>,
    /// Outgoing references (what this calls/uses).
    pub outgoing_refs: Vec<Ref>,
    /// Related type information.
    pub types: Vec<Node>,
    /// Relevant imports.
    pub imports: Vec<Node>,
}

/// A block of code with context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeBlock {
    pub content: String,
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Language for syntax highlighting.
    pub language: Language,
    /// Associated node if extracted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<Node>,
}

// =============================================================================
// Database types
// =============================================================================

/// Database schema version info.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaVersion {
    pub version: u32,
    pub applied_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Statistics about the knowledge graph.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphStats {
    pub node_count: u64,
    pub edge_count: u64,
    pub file_count: u64,
    pub nodes_by_kind: IndexMap<NodeKind, u64>,
    pub edges_by_kind: IndexMap<EdgeKind, u64>,
    pub files_by_language: IndexMap<Language, u64>,
    /// Database size in bytes.
    pub db_size_bytes: u64,
    /// Size of the SQLite `-wal` sidecar in bytes (0 when absent). A WAL far
    /// larger than the DB at rest means killed sessions left it behind (#1431).
    pub wal_size_bytes: u64,
    /// Last update timestamp.
    pub last_updated: i64,
}

// =============================================================================
// Task context (buildContext)
// =============================================================================

/// Input for building task context: a bare query, or a title and description.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskInput {
    Text(String),
    Titled { title: String, description: Option<String> },
}

/// Options for building task context. `None` means "use the documented default".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BuildContextOptions {
    /// Maximum number of nodes to include (default: 50).
    pub max_nodes: Option<usize>,
    /// Maximum number of code blocks to include (default: 10).
    pub max_code_blocks: Option<usize>,
    /// Maximum characters per code block (default: 2000).
    pub max_code_block_size: Option<usize>,
    /// Whether to include code blocks (default: true).
    pub include_code: Option<bool>,
    /// Output format (default: markdown).
    pub format: Option<ContextFormat>,
    /// Number of semantic search results (default: 5).
    pub search_limit: Option<usize>,
    /// Graph traversal depth from entry points (default: 2).
    pub traversal_depth: Option<u32>,
    /// Minimum semantic similarity score (default: 0.3).
    pub min_score: Option<f64>,
}

/// Counters describing a built context.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskContextStats {
    pub node_count: usize,
    pub edge_count: usize,
    pub file_count: usize,
    pub code_block_count: usize,
    /// Total characters in code blocks.
    pub total_code_size: usize,
}

/// Full context for a task, ready for Claude.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskContext {
    /// The original query/task.
    pub query: String,
    pub subgraph: Subgraph,
    /// Entry point nodes (from semantic search).
    pub entry_points: Vec<Node>,
    pub code_blocks: Vec<CodeBlock>,
    pub related_files: Vec<String>,
    /// Brief summary of the context.
    pub summary: String,
    pub stats: TaskContextStats,
}

/// Options for finding relevant context.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FindRelevantContextOptions {
    pub search_limit: Option<usize>,
    pub traversal_depth: Option<u32>,
    pub max_nodes: Option<usize>,
    pub min_score: Option<f64>,
    pub edge_kinds: Option<Vec<EdgeKind>>,
    pub node_kinds: Option<Vec<NodeKind>>,
    /// Extra symbol names to merge in as exact-name search candidates, at a
    /// dampened score. Fed by the segment-vocabulary supplement: word-level
    /// query terms can't reach camelCase names through FTS —
    /// `pinFeedIfNearBottom` is one FTS token — so names whose SEGMENTS the
    /// query's words name are seeded here instead.
    pub seed_names: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_serializes_like_the_typescript_interface() {
        let mut n = Node::new(
            "function:abc",
            NodeKind::Function,
            "foo",
            "src/a.ts::foo",
            "src/a.ts",
            Language::TypeScript,
            3,
            9,
            0,
            1,
            1_700_000_000_000,
        );
        n.is_exported = Some(true);
        n.decorators = Some(vec!["@Injectable".into()]);
        let json = serde_json::to_value(&n).unwrap();
        assert_eq!(json["qualifiedName"], "src/a.ts::foo");
        assert_eq!(json["isExported"], true);
        assert_eq!(json["kind"], "function");
        // Absent optionals are omitted, not null.
        assert!(json.get("docstring").is_none());
        assert!(json.get("returnType").is_none());
        let back: Node = serde_json::from_value(json).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn unresolved_reference_round_trips_with_function_ref_kind() {
        let r = UnresolvedReference {
            from_node_id: "function:x".into(),
            reference_name: "handler".into(),
            reference_kind: ReferenceKind::FunctionRef,
            line: 4,
            column: 2,
            file_path: Some("src/a.ts".into()),
            language: Some(Language::TypeScript),
            candidates: None,
            row_id: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"referenceKind\":\"function_ref\""));
        assert_eq!(serde_json::from_str::<UnresolvedReference>(&json).unwrap(), r);
    }

    #[test]
    fn subgraph_keeps_insertion_order() {
        let mut g = Subgraph::default();
        for id in ["z", "a", "m"] {
            g.nodes.insert(
                id.into(),
                Node::new(id, NodeKind::File, id, id, id, Language::Unknown, 1, 1, 0, 0, 0),
            );
        }
        let ids: Vec<&str> = g.nodes.keys().map(|s| s.as_str()).collect();
        assert_eq!(ids, ["z", "a", "m"]);
    }
}
