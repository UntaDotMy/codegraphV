//! What the parameter-flow pass reads and writes (see codegraph-resolve's `parameter_flow`). Port
//! of `src/db/parameter-flow-queries.ts` (upstream 483a0988), less the per-file facts a sync's
//! incremental record compares: codegraphV runs the whole pass on a sync that touches Ruby.

use crate::error::Result;
use crate::reference_tail::reference_name_tail;
use crate::store::Store;
use codegraph_types::{Edge, Language, NodeKind, UnresolvedReference};

/// How a failed row says a parameter flow ruled its guess out.
pub const FLOW_MARK: &str = r#"["parameter-flow"]"#;
/// Set once a pass has written to the index.
const FLOW_MARK_KEY: &str = "parameter_flows";

/// What the pass reads of a method, a function or a class.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeOutline {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub is_static: bool,
    pub language: Language,
}

/// A `calls` edge as the pass reads it: its id, ends, place and metadata JSON.
#[derive(Debug, Clone)]
pub struct CallEdgeRow {
    pub id: i64,
    pub source: String,
    pub target: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub metadata: Option<String>,
}

/// A failed `calls` reference as the pass reads it.
#[derive(Debug, Clone)]
pub struct FailedCallRow {
    pub id: i64,
    pub from_node_id: String,
    pub line: u32,
    pub column: u32,
    pub reference_name: String,
    pub file_path: String,
    /// The row is the failed form of a guess a flow ruled out.
    pub via_flow: bool,
}

/// Which calls edges to read.
pub enum CallEdgesOf<'a> {
    Target(&'a str),
    Source(&'a str),
    TargetName(&'a str),
}

/// Which failed calls to read.
pub enum FailedCallsOf<'a> {
    Tail(&'a str),
    Caller(&'a str),
}

const NODE_OUTLINE: &str = "id, kind, name, qualified_name, file_path, start_line, start_column, end_line, is_static, language";
const METHOD_KINDS: &str = "kind IN ('method', 'function')";
const EDGE_COLUMNS: &str = "id, source, target, line, col, metadata";
const FAILED_CALL_COLUMNS: &str = "SELECT id, from_node_id, line, col, reference_name, file_path, candidates FROM unresolved_refs";

fn outline(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<NodeOutline>> {
    let kind: String = row.get(1)?;
    let language: String = row.get(9)?;
    let (Some(kind), Some(language)) = (NodeKind::parse(&kind), Language::parse(&language)) else { return Ok(None) };
    Ok(Some(NodeOutline {
        id: row.get(0)?,
        kind,
        name: row.get(2)?,
        qualified_name: row.get(3)?,
        file_path: row.get(4)?,
        start_line: row.get(5)?,
        start_column: row.get::<_, Option<u32>>(6)?.unwrap_or(0),
        end_line: row.get(7)?,
        is_static: row.get::<_, Option<i64>>(8)?.unwrap_or(0) == 1,
        language,
    }))
}

fn edge_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallEdgeRow> {
    Ok(CallEdgeRow { id: row.get(0)?, source: row.get(1)?, target: row.get(2)?, line: row.get(3)?, column: row.get(4)?, metadata: row.get(5)? })
}

fn failed_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FailedCallRow> {
    let candidates: Option<String> = row.get(6)?;
    Ok(FailedCallRow {
        id: row.get(0)?,
        from_node_id: row.get(1)?,
        line: row.get(2)?,
        column: row.get(3)?,
        reference_name: row.get(4)?,
        file_path: row.get(5)?,
        via_flow: candidates.as_deref() == Some(FLOW_MARK),
    })
}

impl Store {
    fn outlines(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<NodeOutline>> {
        let mut stmt = self.conn().prepare_cached(sql)?;
        let rows = stmt.query_map(params, outline)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// The indexed files of a language.
    pub fn pf_file_paths_by_language(&self, language: Language) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare_cached("SELECT path FROM files WHERE language = ?")?;
        let rows = stmt.query_map([language.as_str()], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A language's methods that a call reaches.
    pub fn pf_reached_methods(&self, language: Language) -> Result<Vec<NodeOutline>> {
        self.outlines(
            &format!(
                "SELECT {NODE_OUTLINE} FROM (SELECT DISTINCT target FROM edges WHERE kind = 'calls') CROSS JOIN nodes ON id = target
                  WHERE {METHOD_KINDS} AND language = ?"
            ),
            [language.as_str()],
        )
    }

    /// The classes and modules a file declares.
    pub fn pf_scopes_in_file(&self, file_path: &str) -> Result<Vec<NodeOutline>> {
        self.outlines(&format!("SELECT {NODE_OUTLINE} FROM nodes WHERE file_path = ? AND kind IN ('class', 'module')"), [file_path])
    }

    /// One node's outline.
    pub fn pf_node_outline(&self, id: &str) -> Result<Option<NodeOutline>> {
        Ok(self.outlines(&format!("SELECT {NODE_OUTLINE} FROM nodes WHERE id = ?"), [id])?.into_iter().next())
    }

    /// Whether a language has a method of this name.
    pub fn pf_has_method_named(&self, name: &str, language: Language) -> Result<bool> {
        let mut stmt = self.conn().prepare_cached(&format!(
            "SELECT 1 FROM nodes INDEXED BY idx_nodes_name WHERE name = ? AND {METHOD_KINDS} AND language = ? LIMIT 1"
        ))?;
        Ok(stmt.exists([name, language.as_str()])?)
    }

    /// The calls edges into a node, out of one, or into any node of a name.
    pub fn pf_call_edges(&self, of: CallEdgesOf<'_>) -> Result<Vec<CallEdgeRow>> {
        let (sql, key) = match of {
            CallEdgesOf::Target(id) => (format!("SELECT {EDGE_COLUMNS} FROM edges WHERE target = ? AND kind = 'calls'"), id),
            CallEdgesOf::Source(id) => (format!("SELECT {EDGE_COLUMNS} FROM edges WHERE source = ? AND kind = 'calls'"), id),
            CallEdgesOf::TargetName(name) => (
                "SELECT e.id, e.source, e.target, e.line, e.col, e.metadata FROM nodes n CROSS JOIN edges e ON e.target = n.id
                  WHERE n.name = ? AND e.kind = 'calls'"
                    .to_string(),
                name,
            ),
        };
        let mut stmt = self.conn().prepare_cached(&sql)?;
        let rows = stmt.query_map([key], edge_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A language's failed calls of a method name, or made by one caller.
    pub fn pf_failed_call_sites(&self, language: Language, of: FailedCallsOf<'_>) -> Result<Vec<FailedCallRow>> {
        let (sql, a, b) = match of {
            FailedCallsOf::Tail(tail) => (
                format!("{FAILED_CALL_COLUMNS} WHERE status = 'failed' AND reference_kind = 'calls' AND language = ? AND name_tail = ?"),
                language.as_str(),
                tail,
            ),
            FailedCallsOf::Caller(id) => (
                format!(
                    "{FAILED_CALL_COLUMNS} INDEXED BY idx_unresolved_from_node
                      WHERE from_node_id = ? AND status = 'failed' AND reference_kind = 'calls' AND language = ?"
                ),
                id,
                language.as_str(),
            ),
        };
        let mut stmt = self.conn().prepare_cached(&sql)?;
        let rows = stmt.query_map([a, b], failed_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// What an earlier pass did and left in the index: its edges, and the failed rows of the
    /// guesses it ruled out. Nothing until a pass has written one: on a fresh index, looking is a
    /// scan of every edge.
    pub fn pf_flow_call_sites(&self, language: Language) -> Result<(Vec<CallEdgeRow>, Vec<FailedCallRow>)> {
        if self.get_metadata(FLOW_MARK_KEY)?.is_none() {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut stmt =
            self.conn().prepare_cached(&format!("SELECT {EDGE_COLUMNS} FROM edges WHERE kind = 'calls' AND metadata LIKE '%\"parameterFlow\"%'"))?;
        let edges = stmt.query_map([], edge_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        let mut stmt = self.conn().prepare_cached(&format!(
            "{FAILED_CALL_COLUMNS} WHERE status = 'failed' AND reference_kind = 'calls' AND language = ? AND candidates = ?"
        ))?;
        let rows = stmt.query_map([language.as_str(), FLOW_MARK], failed_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((edges, rows))
    }

    /// Remember that a pass wrote to this index.
    pub fn pf_note_parameter_flows(&self) -> Result<()> {
        self.set_metadata(FLOW_MARK_KEY, "1")
    }

    /// Swap what held each call a flow answers, an edge or a failed row, for its new edge, as one
    /// transaction: a call left with neither has nothing to bring it back.
    pub fn pf_retarget_calls(&self, edge_ids: &[i64], row_ids: &[i64], edges: &[Edge]) -> Result<()> {
        self.transaction(|| {
            self.delete_edges_by_ids(edge_ids)?;
            self.delete_references_by_row_ids(row_ids)?;
            self.insert_edges(edges)
        })
    }

    /// Put the calls no flow answers now back as references to resolve, as one transaction.
    pub fn pf_restore_calls(&self, edge_ids: &[i64], row_ids: &[i64], refs: &[UnresolvedReference]) -> Result<()> {
        self.transaction(|| {
            self.delete_references_by_row_ids(row_ids)?;
            self.replace_resolution_edges_with_unresolved_refs(edge_ids, refs)?;
            Ok(())
        })
    }

    /// Replace guesses a flow ruled out with failed rows that say so, as one transaction.
    pub fn pf_replace_edges_with_flow_marked_refs(&self, edge_ids: &[i64], refs: &[UnresolvedReference]) -> Result<()> {
        if refs.is_empty() {
            return Ok(());
        }
        self.transaction(|| {
            self.delete_edges_by_ids(edge_ids)?;
            let mut insert = self.conn().prepare_cached(
                "INSERT INTO unresolved_refs (from_node_id, reference_name, reference_kind, line, col, candidates, file_path, language, status, name_tail)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'failed', ?)",
            )?;
            for r in refs {
                let kind = r.reference_kind.as_str();
                insert.execute(rusqlite::params![
                    r.from_node_id,
                    r.reference_name,
                    kind,
                    r.line,
                    r.column,
                    FLOW_MARK,
                    r.file_path.as_deref().unwrap_or(""),
                    r.language.unwrap_or(Language::Unknown).as_str(),
                    reference_name_tail(&r.reference_name, Some(kind)),
                ])?;
            }
            Ok(())
        })
    }
}
