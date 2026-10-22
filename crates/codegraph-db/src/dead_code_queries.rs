//! Queries behind the dead-code report — port of the `dead-code` half of
//! `QueryBuilder` in `src/db/queries.ts`.
//!
//! Every method here is batched: the report asks about a whole candidate set at
//! once and the answers are keyed by node id or name. Nothing in this module
//! decides whether a symbol is dead — it only answers what the index holds.

use crate::error::Result;
use crate::reference_tail::{MEMBER_TAIL_SQL, MODULE_PARKED_SQL};
use crate::rows::{node_from_row, NODE_COLS};
use crate::store::{Store, SQLITE_PARAM_CHUNK_SIZE};
use codegraph_types::{EdgeKind, Node, NodeKind};
use rusqlite::params_from_iter;
use std::collections::{HashMap, HashSet};

/// One raw candidate: nothing points at it except `contains`.
#[derive(Debug, Clone, PartialEq)]
pub struct UnreferencedNode {
    pub node: Node,
    /// The file is flagged tool-generated.
    pub generated: bool,
}

fn placeholders(n: usize) -> String {
    std::iter::repeat("?").take(n).collect::<Vec<_>>().join(",")
}

fn unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if !item.is_empty() && seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

/// `NODE_COLS` qualified with the `nodes` alias `n`, for queries that join
/// `files` (which also has a `language` column).
fn node_cols_qualified() -> String {
    NODE_COLS
        .split(", ")
        .map(|c| format!("n.{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Store {
    /// Non-exported symbols of `kinds` with no incoming edge other than
    /// `contains`. A fact, not a claim: most rows are overrides, framework
    /// registrations or references the extractor never recorded.
    pub fn get_unreferenced_nodes(&self, kinds: &[NodeKind], limit: usize) -> Result<Vec<UnreferencedNode>> {
        if kinds.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let kind_strs: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        let sql = format!(
            "SELECT {}, COALESCE(f.generated, 0) AS file_generated
               FROM nodes n
               LEFT JOIN files f ON f.path = n.file_path
              WHERE n.kind IN ({})
                AND NOT EXISTS (
                      SELECT 1 FROM edges e
                       WHERE e.target = n.id
                         AND e.kind != 'contains'
                         AND e.source != n.id
                    )
           ORDER BY n.file_path, n.start_line, n.name
              LIMIT ?",
            node_cols_qualified(),
            placeholders(kinds.len())
        );
        let mut stmt = self.conn().prepare_cached(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = kind_strs.iter().map(|k| k as &dyn rusqlite::ToSql).collect();
        let limit_i = limit as i64;
        params.push(&limit_i);
        let rows = stmt.query_map(params_from_iter(params), |row| {
            let node = node_from_row(row)?;
            let generated: i64 = row.get(21)?;
            Ok(UnreferencedNode {
                node,
                generated: generated == 1,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Which of `languages` the index records any export marker for. The
    /// exported filter is the strongest exclusion the report has, and for a
    /// language with no recorded marker it would silently do nothing.
    pub fn get_languages_with_exports(&self, languages: &[String]) -> Result<HashSet<String>> {
        let unique = unique(languages.iter().cloned());
        let mut found = HashSet::new();
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let sql = format!(
                "SELECT language, MAX(is_exported) AS any_exported
                   FROM nodes
                  WHERE language IN ({})
               GROUP BY language",
                placeholders(chunk.len())
            );
            let mut stmt = self.conn().prepare_cached(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (language, any) = row?;
                if any == 1 {
                    found.insert(language);
                }
            }
        }
        Ok(found)
    }

    /// Which of `names` the index holds an unresolved reference to — by whole
    /// name, by the tail its name gives, and by the member of a module-parked
    /// reference.
    pub fn get_unresolved_names_among(&self, names: &[String]) -> Result<HashSet<String>> {
        let unique = unique(names.iter().cloned());
        let mut found = HashSet::new();
        if unique.is_empty() {
            return Ok(found);
        }
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE / 3) {
            let marks = placeholders(chunk.len());
            let sql = format!(
                "SELECT DISTINCT reference_name AS name FROM unresolved_refs
                  WHERE reference_name IN ({marks})
                  UNION
                 SELECT DISTINCT name_tail AS name FROM unresolved_refs
                  WHERE name_tail IN ({marks})
                  UNION
                 SELECT DISTINCT {MEMBER_TAIL_SQL} AS name FROM unresolved_refs
                  WHERE {MODULE_PARKED_SQL} AND {MEMBER_TAIL_SQL} IN ({marks})"
            );
            let mut stmt = self.conn().prepare_cached(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk
                .iter()
                .chain(chunk.iter())
                .chain(chunk.iter())
                .map(|s| s as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params_from_iter(params), |row| row.get::<_, String>(0))?;
            for row in rows {
                found.insert(row?);
            }
        }
        Ok(found)
    }

    /// Which of `names` are carried by MORE THAN ONE symbol, at least one of
    /// which something points at — the resolver may have linked the twin.
    pub fn get_ambiguous_referenced_names(&self, names: &[String]) -> Result<HashSet<String>> {
        let unique = unique(names.iter().cloned());
        let mut found = HashSet::new();
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let sql = format!(
                "SELECT name FROM (
                   SELECT n.name AS name,
                          EXISTS (
                            SELECT 1 FROM edges e
                             WHERE e.target = n.id AND e.kind != 'contains'
                          ) AS referenced
                     FROM nodes n
                    WHERE n.name IN ({})
                 )
                 GROUP BY name
                   HAVING COUNT(*) > 1 AND SUM(referenced) > 0",
                placeholders(chunk.len())
            );
            let mut stmt = self.conn().prepare_cached(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| row.get::<_, String>(0))?;
            for row in rows {
                found.insert(row?);
            }
        }
        Ok(found)
    }

    /// Distinct other files that reach into each of `file_paths`. Zero means
    /// nothing else in the index reaches the file at all — an island.
    pub fn get_file_dependent_counts(&self, file_paths: &[String]) -> Result<HashMap<String, i64>> {
        let mut out = HashMap::new();
        let unique = unique(file_paths.iter().cloned());
        if unique.is_empty() {
            return Ok(out);
        }
        let json = serde_json::to_string(&unique)?;
        let sql = "SELECT tn.file_path AS file_path, COUNT(DISTINCT sn.file_path) AS dependents
                     FROM edges e
                     JOIN nodes tn ON tn.id = e.target
                     JOIN nodes sn ON sn.id = e.source
                    WHERE e.kind != 'contains'
                      AND tn.file_path IN (SELECT value FROM json_each(?))
                      AND sn.file_path <> tn.file_path
                 GROUP BY tn.file_path";
        let mut stmt = self.conn().prepare_cached(sql)?;
        let rows = stmt.query_map([&json], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (path, count) = row?;
            out.insert(path, count);
        }
        Ok(out)
    }

    /// Distinct file paths that reach into `file_path` through a non-`contains`
    /// edge. An `imports` edge here points at the importing file's own import
    /// declaration, so the resolved symbol graph is the real dependency signal.
    pub fn get_file_dependents(&self, file_path: &str) -> Result<Vec<String>> {
        let sql = "SELECT DISTINCT src.file_path AS fp
                     FROM edges e
                     JOIN nodes tgt ON tgt.id = e.target
                     JOIN nodes src ON src.id = e.source
                    WHERE tgt.file_path = ?
                      AND e.kind != 'contains'
                      AND src.file_path != ?";
        let mut stmt = self.conn().prepare_cached(sql)?;
        let rows = stmt.query_map([file_path, file_path], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The names each of `node_ids` holds an unresolved reference of `kinds` to.
    pub fn get_unresolved_reference_names_from(
        &self,
        node_ids: &[String],
        kinds: &[EdgeKind],
    ) -> Result<HashMap<String, Vec<String>>> {
        let unique = unique(node_ids.iter().cloned());
        let mut found: HashMap<String, Vec<String>> = HashMap::new();
        if unique.is_empty() || kinds.is_empty() {
            return Ok(found);
        }
        let kind_strs: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        let kind_marks = placeholders(kinds.len());
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let sql = format!(
                "SELECT DISTINCT from_node_id AS id, reference_name AS name FROM unresolved_refs
                  WHERE from_node_id IN ({})
                    AND reference_kind IN ({kind_marks})",
                placeholders(chunk.len())
            );
            let mut stmt = self.conn().prepare_cached(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk
                .iter()
                .map(|s| s as &dyn rusqlite::ToSql)
                .chain(kind_strs.iter().map(|k| k as &dyn rusqlite::ToSql))
                .collect();
            let rows = stmt.query_map(params_from_iter(params), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, name) = row?;
                found.entry(id).or_default().push(name);
            }
        }
        Ok(found)
    }

    /// Every node holding an unresolved reference of `kinds` to each of
    /// `names` — the mirror, keyed by the name.
    pub fn get_unresolved_reference_sources_named(
        &self,
        names: &[String],
        kinds: &[EdgeKind],
    ) -> Result<HashMap<String, HashSet<String>>> {
        let unique = unique(names.iter().cloned());
        let mut found: HashMap<String, HashSet<String>> = HashMap::new();
        if unique.is_empty() || kinds.is_empty() {
            return Ok(found);
        }
        let kind_strs: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        let kind_marks = placeholders(kinds.len());
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let sql = format!(
                "SELECT DISTINCT reference_name AS name, from_node_id AS id FROM unresolved_refs
                  WHERE reference_name IN ({})
                    AND reference_kind IN ({kind_marks})",
                placeholders(chunk.len())
            );
            let mut stmt = self.conn().prepare_cached(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk
                .iter()
                .map(|s| s as &dyn rusqlite::ToSql)
                .chain(kind_strs.iter().map(|k| k as &dyn rusqlite::ToSql))
                .collect();
            let rows = stmt.query_map(params_from_iter(params), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (name, id) = row?;
                found.entry(name).or_default().insert(id);
            }
        }
        Ok(found)
    }

    /// Which of `node_ids` extend or implement a type the resolver could not
    /// follow. Such an ancestor leaves no edge, only an unresolved row.
    pub fn get_unresolved_supertype_sources_among(&self, node_ids: &[String]) -> Result<HashSet<String>> {
        Ok(self
            .get_unresolved_reference_names_from(node_ids, &[EdgeKind::Extends, EdgeKind::Implements])?
            .into_keys()
            .collect())
    }
}
