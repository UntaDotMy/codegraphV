//! Queries only the reference resolver issues — the resolution half of `src/db/queries.ts`.

use crate::error::Result;
use crate::reference_tail::reference_name_tail;
use crate::rows::{ref_from_row, REF_COLS};
use crate::store::{Store, SQLITE_PARAM_CHUNK_SIZE};
use codegraph_types::{Language, UnresolvedReference};

/// A C / C++ `#include` import node, narrowed to what the include graph reads.
#[derive(Debug, Clone, PartialEq)]
pub struct CppIncludeNode {
    pub id: String,
    pub name: String,
    pub file_path: String,
    pub language: Language,
    pub start_line: u32,
    pub start_column: u32,
}

impl Store {
    /// Record the files synthesis read; a path the index does not hold as a file (a package
    /// directory) is not one (#2565).
    pub fn replace_synthesis_inputs(&self, files: &[String]) -> Result<()> {
        self.transaction(|| {
            self.conn().execute_batch("DELETE FROM synthesis_inputs")?;
            let mut stmt = self
                .conn()
                .prepare_cached("INSERT OR IGNORE INTO synthesis_inputs(file_path) SELECT path FROM files WHERE path = ?")?;
            for file in files {
                stmt.execute([file])?;
            }
            Ok(())
        })
    }

    /// Whether a file was read by the last synthesis run.
    pub fn was_synthesis_input(&self, file_path: &str) -> Result<bool> {
        let mut stmt = self.conn().prepare_cached("SELECT 1 FROM synthesis_inputs WHERE file_path = ?")?;
        Ok(stmt.exists([file_path])?)
    }

    /// Every indexed file path, in path order.
    pub fn get_all_file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare_cached("SELECT path FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every distinct node name (lightweight — just the strings, for pre-filtering).
    pub fn get_all_node_names(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare_cached("SELECT DISTINCT name FROM nodes")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The import node of every C / C++ `#include`, narrowed to the path it
    /// spells and where it is written.
    pub fn get_cpp_include_nodes(&self) -> Result<Vec<CppIncludeNode>> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, name, file_path, language, start_line, start_column
               FROM nodes WHERE kind = 'import' AND language IN ('c', 'cpp')",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let language: String = r.get(3)?;
                Ok(CppIncludeNode {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    file_path: r.get(2)?,
                    language: Language::parse(&language).unwrap_or(Language::Unknown),
                    start_line: r.get(4)?,
                    start_column: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
    /// Every unresolved reference of these kinds written inside any of `node_ids` — one indexed
    /// probe per chunk (`getUnresolvedReferencesFromNodes`).
    pub fn get_unresolved_references_from_nodes(
        &self,
        node_ids: &[String],
        kinds: &[codegraph_types::EdgeKind],
    ) -> Result<Vec<UnresolvedReference>> {
        let mut unique: Vec<&String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in node_ids {
            if seen.insert(id) {
                unique.push(id);
            }
        }
        let mut found = Vec::new();
        if unique.is_empty() || kinds.is_empty() {
            return Ok(found);
        }
        let kind_placeholders = vec!["?"; kinds.len()].join(",");
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut stmt = self.conn().prepare(&format!(
                "SELECT {REF_COLS} FROM unresolved_refs
                  WHERE from_node_id IN ({placeholders})
                    AND reference_kind IN ({kind_placeholders})"
            ))?;
            let params: Vec<&str> = chunk.iter().map(|s| s.as_str()).chain(kinds.iter().map(|k| k.as_str())).collect();
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), ref_from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            found.extend(rows);
        }
        Ok(found)
    }

    /// How many references are still pending.
    pub fn get_unresolved_references_count(&self) -> Result<i64> {
        let n = self
            .conn()
            .prepare_cached("SELECT COUNT(*) FROM unresolved_refs WHERE status = 'pending'")?
            .query_row([], |r| r.get::<_, i64>(0))?;
        Ok(n)
    }

    /// The next pending references after `after_row_id`, in row-id order. `prerequisites`
    /// narrows to the wiring kinds (`imports`/`extends`/`implements`) or to everything else.
    pub fn get_unresolved_references_batch_after(
        &self,
        after_row_id: i64,
        limit: usize,
        prerequisites: Option<bool>,
    ) -> Result<Vec<UnresolvedReference>> {
        let filter = match prerequisites {
            None => "",
            Some(true) => " AND reference_kind IN ('imports', 'extends', 'implements')",
            Some(false) => " AND reference_kind NOT IN ('imports', 'extends', 'implements')",
        };
        let mut stmt = self.conn().prepare_cached(&format!(
            "SELECT {REF_COLS} FROM unresolved_refs WHERE status = 'pending' AND id > ?{filter} ORDER BY id LIMIT ?"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![after_row_id, limit as i64], ref_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete the rows with these ids; returns how many went.
    pub fn delete_references_by_row_ids(&self, row_ids: &[i64]) -> Result<usize> {
        if row_ids.is_empty() {
            return Ok(0);
        }
        self.transaction(|| {
            let mut changed = 0;
            for chunk in row_ids.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                let mut stmt =
                    self.conn().prepare_cached(&format!("DELETE FROM unresolved_refs WHERE id IN ({placeholders})"))?;
                changed += stmt.execute(rusqlite::params_from_iter(chunk.iter()))?;
            }
            Ok(changed)
        })
    }

    /// Delete by the (caller, name, kind) key — the fallback for a ref with no row id.
    pub fn delete_specific_resolved_references(&self, refs: &[(String, String, String)]) -> Result<usize> {
        if refs.is_empty() {
            return Ok(0);
        }
        self.transaction(|| {
            let mut stmt = self.conn().prepare_cached(
                "DELETE FROM unresolved_refs WHERE from_node_id = ? AND reference_name = ? AND reference_kind = ?",
            )?;
            let mut changed = 0;
            for (from, name, kind) in refs {
                changed += stmt.execute(rusqlite::params![from, name, kind])?;
            }
            Ok(changed)
        })
    }

    /// Park references as `failed` by the (caller, name, kind) key. The optional tail
    /// overrides the one the name gives.
    pub fn mark_references_failed(&self, refs: &[(String, String, String, Option<String>)]) -> Result<usize> {
        if refs.is_empty() {
            return Ok(0);
        }
        self.transaction(|| {
            let mut stmt = self.conn().prepare_cached(
                "UPDATE unresolved_refs SET status = 'failed', name_tail = ? WHERE from_node_id = ? AND reference_name = ? AND reference_kind = ?",
            )?;
            let mut changed = 0;
            for (from, name, kind, tail) in refs {
                let tail = tail.clone().unwrap_or_else(|| reference_name_tail(name, Some(kind)));
                changed += stmt.execute(rusqlite::params![tail, from, name, kind])?;
            }
            Ok(changed)
        })
    }

    /// Park references as `failed` by row id: (row id, name, kind, optional tail).
    pub fn mark_references_failed_by_row_ids(&self, refs: &[(i64, String, String, Option<String>)]) -> Result<usize> {
        if refs.is_empty() {
            return Ok(0);
        }
        self.transaction(|| {
            let mut stmt =
                self.conn().prepare_cached("UPDATE unresolved_refs SET status = 'failed', name_tail = ? WHERE id = ?")?;
            let mut changed = 0;
            for (row_id, name, kind, tail) in refs {
                let tail = tail.clone().unwrap_or_else(|| reference_name_tail(name, Some(kind)));
                changed += stmt.execute(rusqlite::params![tail, row_id])?;
            }
            Ok(changed)
        })
    }
}

/// An edge into a file's nodes together with the facts a re-resolution needs about both ends.
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingEdge {
    pub edge: codegraph_types::Edge,
    pub edge_id: i64,
    pub target_name: String,
    pub target_kind: codegraph_types::NodeKind,
    pub source_file_path: String,
    pub source_language: Language,
}

impl Store {
    /// Edges from OTHER files into the nodes of `file_path` (everything but `contains`), with
    /// the target's name and kind and the source's file and language.
    pub fn get_cross_file_incoming_edges_with_target(&self, file_path: &str) -> Result<Vec<IncomingEdge>> {
        use crate::rows::edge_from_row;
        let mut stmt = self.conn().prepare_cached(
            "SELECT e.source, e.target, e.kind, e.metadata, e.line, e.col, e.provenance, e.id, \
                    tgt.name, tgt.kind, src.file_path, src.language \
               FROM edges e \
               JOIN nodes tgt ON tgt.id = e.target \
               JOIN nodes src ON src.id = e.source \
              WHERE tgt.file_path = ? AND e.kind != 'contains' AND src.file_path != ?",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![file_path, file_path], |row| {
                let target_kind: String = row.get(9)?;
                let source_language: String = row.get(11)?;
                Ok(IncomingEdge {
                    edge: edge_from_row(row)?,
                    edge_id: row.get(7)?,
                    target_name: row.get(8)?,
                    target_kind: codegraph_types::NodeKind::parse(&target_kind).unwrap_or(codegraph_types::NodeKind::File),
                    source_file_path: row.get(10)?,
                    source_language: Language::parse(&source_language).unwrap_or(Language::Unknown),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
