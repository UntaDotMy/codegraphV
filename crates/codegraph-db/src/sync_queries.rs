//! Queries the incremental sync and the resolver coordinator's reconcile passes issue —
//! the "re-open what a change may have moved" half of `src/db/queries.ts`.
//!
//! A resolution edge is the product of a reference the resolver once bound; when the graph
//! around it changes, the sync deletes the edge and puts its original reference back (see
//! `resurrect_ref_from_dropped_edge`). Everything here finds those edges or re-queues the
//! failed references parked beside them.

use crate::error::Result;
use crate::reference_tail::{is_module_key, MEMBER_TAIL_SQL, MODULE_PARKED_SQL};
use crate::rows::{edge_from_row, ref_from_row, REF_COLS};
use crate::store::{Store, SQLITE_PARAM_CHUNK_SIZE};
use codegraph_types::{Edge, IndexMap, Language, NodeKind, UnresolvedReference};
use rusqlite::types::Value;
use std::collections::HashSet;

/// How an import names a folder only through where it is written: `.`, or a folder up to
/// twelve levels above (`..`, `../..`, with or without the trailing `/`).
fn folder_specifiers() -> Vec<String> {
    let mut names = vec![".".to_string(), "./".to_string()];
    for levels in 1..=12 {
        let up = vec![".."; levels].join("/");
        names.push(up.clone());
        names.push(format!("{up}/"));
    }
    names
}

/// A failed python ref through one attribute (`self.c.send`) or a call receiver (`make().send`).
const FAILED_PY_ATTR_REF: &str = "status = 'failed' AND language = 'python'
  AND reference_name NOT GLOB '*.*.*.*'
  AND (reference_name GLOB '*.*.*' OR reference_name GLOB '*().*')";

/// A SQL predicate: is the reference name in `column` written through one of the names in
/// the JSON array bound to the next two parameters, as the name itself or as a member read
/// through it (`Order.objects` through `Order`)?
fn through_names(column: &str) -> String {
    format!(
        "(
  {column} IN (SELECT value FROM json_each(?))
  OR (instr({column}, '.') > 1 AND substr({column}, 1, instr({column}, '.') - 1) IN (SELECT value FROM json_each(?)))
)"
    )
}

/// Is this edge a synthesizer's? Ownership is independent of provenance.
pub const SYNTHESIZED_EDGE: &str = "CASE WHEN json_valid(metadata) THEN json_extract(metadata, '$.synthesizedBy') END IS NOT NULL";

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn language_of(s: &str) -> Language {
    Language::parse(s).unwrap_or(Language::Unknown)
}

/// A resolution edge together with the facts a re-resolution needs about its ends.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolutionEdge {
    pub edge: Edge,
    pub edge_id: i64,
    pub source_file_path: String,
    pub source_language: Language,
    /// The file of the edge's target, for the queries that join it.
    pub target_file_path: Option<String>,
}

/// One importing file of a node: its language and the module specifier it wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportInto {
    pub file_path: String,
    pub language: Language,
    pub ref_name: Option<String>,
}

/// An import node, narrowed to where it is and what it imports.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportNodeName {
    pub file_path: String,
    pub name: String,
    pub language: Language,
}

/// A failed reference narrowed to the columns a sync needs to choose among them.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedRefKey {
    pub id: i64,
    pub from_node_id: String,
    pub reference_name: String,
    pub file_path: String,
}

/// `e.source, e.target, e.kind, e.metadata, e.line, e.col, e.provenance, e.id, src.file_path, src.language`
const RES_EDGE_COLS: &str = "e.source, e.target, e.kind, e.metadata, e.line, e.col, e.provenance, e.id, src.file_path, src.language";

fn resolution_edge_from_row(row: &rusqlite::Row<'_>, target_file_column: Option<usize>) -> rusqlite::Result<ResolutionEdge> {
    let language: String = row.get(9)?;
    Ok(ResolutionEdge {
        edge: edge_from_row(row)?,
        edge_id: row.get(7)?,
        source_file_path: row.get(8)?,
        source_language: language_of(&language),
        target_file_path: match target_file_column {
            Some(i) => Some(row.get(i)?),
            None => None,
        },
    })
}

impl Store {
    fn query_resolution_edges(&self, sql: &str, params: Vec<Value>, target_file_column: Option<usize>) -> Result<Vec<ResolutionEdge>> {
        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |r| resolution_edge_from_row(r, target_file_column))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn query_references(&self, sql: &str, params: Vec<Value>) -> Result<Vec<UnresolvedReference>> {
        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), ref_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn update_pending_by_ids(&self, ids: &[i64]) -> Result<usize> {
        self.transaction(|| {
            let mut changed = 0;
            for chunk in ids.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let mut stmt = self
                    .conn()
                    .prepare(&format!("UPDATE unresolved_refs SET status = 'pending' WHERE id IN ({})", placeholders(chunk.len())))?;
                changed += stmt.execute(rusqlite::params_from_iter(chunk.iter()))?;
            }
            Ok(changed)
        })
    }

    // -- Unresolved references -------------------------------------------------------------

    /// Every unresolved reference recorded in one file, ordered by line.
    pub fn get_unresolved_references_in_file(&self, file_path: &str, limit: usize) -> Result<Vec<UnresolvedReference>> {
        self.query_references(
            &format!("SELECT {REF_COLS} FROM unresolved_refs WHERE file_path = ? ORDER BY line, col LIMIT ?"),
            vec![text(file_path), Value::Integer(limit as i64)],
        )
    }

    /// References recorded against a symbol that never resolved to a node.
    pub fn get_unresolved_references_from(&self, from_node_id: &str) -> Result<Vec<UnresolvedReference>> {
        self.query_references(&format!("SELECT {REF_COLS} FROM unresolved_refs WHERE from_node_id = ?"), vec![text(from_node_id)])
    }

    /// A batch of PENDING references by LIMIT/OFFSET.
    pub fn get_unresolved_references_batch(&self, offset: usize, limit: usize) -> Result<Vec<UnresolvedReference>> {
        self.query_references(
            &format!("SELECT {REF_COLS} FROM unresolved_refs WHERE status = 'pending' ORDER BY rowid LIMIT ? OFFSET ?"),
            vec![Value::Integer(limit as i64), Value::Integer(offset as i64)],
        )
    }

    /// Pending references scoped to specific file paths.
    pub fn get_unresolved_references_by_files(&self, file_paths: &[String]) -> Result<Vec<UnresolvedReference>> {
        let mut out = Vec::new();
        for chunk in file_paths.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            out.extend(self.query_references(
                &format!("SELECT {REF_COLS} FROM unresolved_refs WHERE status = 'pending' AND file_path IN ({})", placeholders(chunk.len())),
                chunk.iter().map(|s| text(s)).collect(),
            )?);
        }
        Ok(out)
    }

    /// Unresolved references by row id.
    pub fn get_unresolved_references_by_ids(&self, ids: &[i64]) -> Result<Vec<UnresolvedReference>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            out.extend(self.query_references(
                &format!("SELECT {REF_COLS} FROM unresolved_refs WHERE id IN ({})", placeholders(chunk.len())),
                chunk.iter().map(|i| Value::Integer(*i)).collect(),
            )?);
        }
        Ok(out)
    }

    /// Delete all unresolved references.
    pub fn clear_unresolved_references(&self) -> Result<()> {
        self.conn().execute_batch("DELETE FROM unresolved_refs")?;
        Ok(())
    }

    /// Delete the references written inside any of `from_node_ids`.
    pub fn delete_resolved_references(&self, from_node_ids: &[String]) -> Result<()> {
        for chunk in from_node_ids.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let mut stmt =
                self.conn().prepare(&format!("DELETE FROM unresolved_refs WHERE from_node_id IN ({})", placeholders(chunk.len())))?;
            stmt.execute(rusqlite::params_from_iter(chunk.iter()))?;
        }
        Ok(())
    }

    // -- Failed references ---------------------------------------------------------------------

    /// The row ids of the failed references of `file_path` that import one of `modules` or are
    /// written through one of `names`.
    pub fn get_failed_references_through(&self, file_path: &str, modules: &[String], names: &[String]) -> Result<Vec<i64>> {
        let sql = format!(
            "SELECT id FROM unresolved_refs
          WHERE file_path = ? AND status = 'failed'
            AND ((reference_kind = 'imports' AND reference_name IN (SELECT value FROM json_each(?)))
                 OR {})",
            through_names("reference_name")
        );
        let names_json = serde_json::to_string(names).unwrap_or_else(|_| "[]".into());
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params![file_path, serde_json::to_string(modules).unwrap_or_else(|_| "[]".into()), names_json, names_json],
                |r| r.get::<_, i64>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Put failed refs back in the pending set. Returns the number re-opened.
    pub fn reopen_failed_references(&self, row_ids: &[i64]) -> Result<usize> {
        if row_ids.is_empty() {
            return Ok(0);
        }
        self.transaction(|| {
            let mut changed = 0;
            for chunk in row_ids.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let mut stmt = self.conn().prepare(&format!(
                    "UPDATE unresolved_refs SET status = 'pending' WHERE status = 'failed' AND id IN ({})",
                    placeholders(chunk.len())
                ))?;
                changed += stmt.execute(rusqlite::params_from_iter(chunk.iter()))?;
            }
            Ok(changed)
        })
    }

    /// The keys failed references wait for a module under (`module:pages`), each once.
    pub fn get_failed_module_keys(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT DISTINCT name_tail AS key FROM unresolved_refs WHERE status = 'failed' AND name_tail GLOB 'module:*'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Failed refs whose name tail matches one of the given symbol names — the candidates a sync
    /// retries after files carrying those names changed (#1240). A name matching more than
    /// `per_name_ceiling` failed refs is skipped (external/builtin noise); a module key has the
    /// `per_module_ceiling` of its own.
    pub fn get_retryable_failed_references(
        &self,
        names: &[String],
        per_name_ceiling: i64,
        per_module_ceiling: i64,
    ) -> Result<Vec<UnresolvedReference>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let member_tail_where = format!("{MODULE_PARKED_SQL} AND instr(reference_name, '.') > 0");
        let lookups: [(String, String); 3] = [
            ("name_tail".to_string(), "status = 'failed'".to_string()),
            ("reference_name".to_string(), MODULE_PARKED_SQL.to_string()),
            (MEMBER_TAIL_SQL.to_string(), member_tail_where),
        ];

        // Pass 1: per-key counts, chunked under the SQLite parameter limit.
        let mut counts: Vec<IndexMap<String, i64>> = Vec::new();
        for (column, wher) in &lookups {
            let mut by_key: IndexMap<String, i64> = IndexMap::new();
            for chunk in names.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let sql = format!(
                    "SELECT {column} AS key, COUNT(*) AS count FROM unresolved_refs WHERE {wher} AND {column} IN ({}) GROUP BY {column}",
                    placeholders(chunk.len())
                );
                let mut stmt = self.conn().prepare(&sql)?;
                let found = stmt
                    .query_map(rusqlite::params_from_iter(chunk.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                for (key, count) in found {
                    by_key.insert(key, count);
                }
            }
            counts.push(by_key);
        }
        // A member reference parked under its module had its member's name as its tail before, so
        // the two lookups by that name share one ceiling.
        let population = |lookup: usize, key: &str, count: i64| -> i64 {
            if lookup == 1 {
                count
            } else {
                counts[0].get(key).copied().unwrap_or(0) + counts[2].get(key).copied().unwrap_or(0)
            }
        };

        let mut rows: IndexMap<i64, UnresolvedReference> = IndexMap::new();
        for (lookup, (column, wher)) in lookups.iter().enumerate() {
            let retry_keys: Vec<&String> = counts[lookup]
                .iter()
                .filter(|(key, count)| {
                    if lookup == 0 && is_module_key(key) {
                        **count <= per_module_ceiling
                    } else {
                        population(lookup, key, **count) <= per_name_ceiling
                    }
                })
                .map(|(key, _)| key)
                .collect();
            // Pass 2: load the surviving rows; a row two lookups find is kept once.
            for chunk in retry_keys.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let found = self.query_references(
                    &format!("SELECT {REF_COLS} FROM unresolved_refs WHERE {wher} AND {column} IN ({})", placeholders(chunk.len())),
                    chunk.iter().map(|s| text(s)).collect(),
                )?;
                for r in found {
                    rows.insert(r.row_id.unwrap_or(0), r);
                }
            }
        }

        // By receiver: one range of idx_unresolved_failed_module_name per name, `store.` up to
        // `store/` ('/' follows '.'), read one past the ceiling.
        for name in names {
            if !is_receiver_name(name) {
                continue;
            }
            let found = self.query_references(
                &format!(
                    "SELECT {REF_COLS} FROM unresolved_refs WHERE {MODULE_PARKED_SQL} AND reference_name >= ? AND reference_name < ? LIMIT ?"
                ),
                vec![text(&format!("{name}.")), text(&format!("{name}/")), Value::Integer(per_name_ceiling + 1)],
            )?;
            if found.len() as i64 > per_name_ceiling {
                continue;
            }
            for r in found {
                rows.insert(r.row_id.unwrap_or(0), r);
            }
        }
        Ok(rows.into_values().collect())
    }

    /// Failed `imports` refs a sync should retry once files appear: found by path keys (as a
    /// name tail or a whole name), by namespace/module names, and by module specifiers that now
    /// resolve to an added file. `could_name` rescues a path key past the ceiling for the imports
    /// that could name a file the sync added.
    #[allow(clippy::too_many_arguments)]
    pub fn get_retryable_failed_imports(
        &self,
        path_keys: &[String],
        names: &[String],
        per_name_ceiling: i64,
        resolved_modules: &[String],
        per_module_ceiling: i64,
        could_name: Option<&dyn Fn(&UnresolvedReference) -> bool>,
    ) -> Result<Vec<UnresolvedReference>> {
        let failed_imports = "status = 'failed' AND reference_kind = 'imports'";
        struct Lookup<'a> {
            keys: &'a [String],
            column: &'static str,
            wher: String,
            ceiling: i64,
            could_name: Option<&'a dyn Fn(&UnresolvedReference) -> bool>,
        }
        let lookups = [
            Lookup { keys: path_keys, column: "name_tail", wher: failed_imports.to_string(), ceiling: per_name_ceiling, could_name },
            Lookup { keys: path_keys, column: "reference_name", wher: failed_imports.to_string(), ceiling: per_name_ceiling, could_name },
            Lookup {
                keys: names,
                column: "reference_name",
                wher: format!("{failed_imports} AND name_tail != reference_name"),
                ceiling: per_name_ceiling,
                could_name: None,
            },
            Lookup { keys: resolved_modules, column: "reference_name", wher: failed_imports.to_string(), ceiling: per_module_ceiling, could_name: None },
        ];
        let mut rows: IndexMap<i64, UnresolvedReference> = IndexMap::new();
        for l in &lookups {
            let mut seen = HashSet::new();
            let unique: Vec<&String> = l.keys.iter().filter(|k| !k.is_empty() && seen.insert(k.as_str())).collect();
            // Pass 1: per-key counts.
            let mut retry_keys: Vec<String> = Vec::new();
            let mut crowded_keys: Vec<String> = Vec::new();
            for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let sql = format!(
                    "SELECT {col} AS key, COUNT(*) AS count FROM unresolved_refs WHERE {wher} AND {col} IN ({ph}) GROUP BY {col}",
                    col = l.column,
                    wher = l.wher,
                    ph = placeholders(chunk.len())
                );
                let mut stmt = self.conn().prepare(&sql)?;
                let counts = stmt
                    .query_map(rusqlite::params_from_iter(chunk.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                for (key, count) in counts {
                    if count <= l.ceiling {
                        retry_keys.push(key);
                    } else if l.could_name.is_some() {
                        crowded_keys.push(key);
                    }
                }
            }
            // Pass 2: load the surviving rows; a row two lookups find is kept once.
            for chunk in retry_keys.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let found = self.query_references(
                    &format!("SELECT {REF_COLS} FROM unresolved_refs WHERE {} AND {} IN ({})", l.wher, l.column, placeholders(chunk.len())),
                    chunk.iter().map(|s| text(s)).collect(),
                )?;
                for r in found {
                    rows.insert(r.row_id.unwrap_or(0), r);
                }
            }
            // A key past the ceiling: only the imports that could name an added file.
            if let Some(could_name) = l.could_name {
                for key in &crowded_keys {
                    let key_rows =
                        self.query_references(&format!("SELECT {REF_COLS} FROM unresolved_refs WHERE {} AND {} IN (?)", l.wher, l.column), vec![text(key)])?;
                    let named: Vec<UnresolvedReference> = key_rows.into_iter().filter(|r| could_name(r)).collect();
                    if named.len() as i64 > per_module_ceiling {
                        continue;
                    }
                    for r in named {
                        rows.insert(r.row_id.unwrap_or(0), r);
                    }
                }
            }
        }
        Ok(rows.into_values().collect())
    }

    /// The scoped package specifiers (`@excalidraw/common`) of failed imports, each with one file
    /// that wrote it.
    pub fn get_failed_scoped_imports(&self) -> Result<Vec<ImportNodeName>> {
        let mut stmt = self.conn().prepare(
            "SELECT reference_name AS name, MIN(file_path) AS file_path, language
           FROM unresolved_refs
          WHERE status = 'failed' AND reference_kind = 'imports'
            AND reference_name GLOB '@?*/?*' AND reference_name NOT GLOB '@*/*/*'
          GROUP BY reference_name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let language: String = r.get(2)?;
                Ok(ImportNodeName { name: r.get(0)?, file_path: r.get(1)?, language: language_of(&language) })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Failed imports of the folder the importing file is in or under (`.`, `..`, `../..`).
    pub fn get_failed_folder_imports(&self) -> Result<Vec<UnresolvedReference>> {
        let specifiers = folder_specifiers();
        self.query_references(
            &format!(
                "SELECT {REF_COLS} FROM unresolved_refs
          WHERE status = 'failed' AND reference_kind = 'imports' AND reference_name IN ({})",
                placeholders(specifiers.len())
            ),
            specifiers.iter().map(|s| text(s)).collect(),
        )
    }

    /// Failed `calls` refs whose name tail is one of `tails`: `(row id, reference name, file path)`.
    pub fn get_failed_calls_by_tail(&self, tails: &[String]) -> Result<Vec<(i64, String, String)>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let unique: Vec<&String> = tails.iter().filter(|t| seen.insert(t.as_str())).collect();
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let ph = placeholders(chunk.len());
            let sql = format!(
                "SELECT id, reference_name, file_path FROM unresolved_refs
            WHERE status = 'failed' AND reference_kind = 'calls' AND name_tail IN ({ph})
           UNION ALL
           SELECT id, reference_name, file_path FROM unresolved_refs
            WHERE {MODULE_PARKED_SQL} AND reference_kind = 'calls' AND {MEMBER_TAIL_SQL} IN ({ph})"
            );
            let mut stmt = self.conn().prepare(&sql)?;
            let params: Vec<Value> = chunk.iter().chain(chunk.iter()).map(|s| text(s)).collect();
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            out.extend(rows);
        }
        Ok(out)
    }

    /// Re-queue a language's parked failures in the files of `names` whose reference starts with
    /// one of the names bound there (`N`, `N.run`).
    pub fn requeue_failed_references_binding(&self, language: Language, names: &IndexMap<String, HashSet<String>>) -> Result<usize> {
        let files: Vec<&String> = names.keys().collect();
        let mut ids: Vec<i64> = Vec::new();
        for chunk in files.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let mut stmt = self.conn().prepare(&format!(
                "SELECT id, file_path, reference_name FROM unresolved_refs
            WHERE status = 'failed' AND language = ? AND file_path IN ({})",
                placeholders(chunk.len())
            ))?;
            let params: Vec<Value> = std::iter::once(text(language.as_str())).chain(chunk.iter().map(|s| text(s))).collect();
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (id, file_path, reference_name) in rows {
                let head = reference_name.split('.').next().unwrap_or("");
                if names.get(&file_path).is_some_and(|set| set.contains(head)) {
                    ids.push(id);
                }
            }
        }
        self.update_pending_by_ids(&ids)
    }

    /// Return a language's parked refs to the pending set — all of them, or only those naming one
    /// of `segments` as a dotted part (`x`, `a.x`, `x.b`, `.x`). The files of the re-queued rows
    /// are added to `files`.
    pub fn requeue_failed_references_by_language(
        &self,
        language: Language,
        segments: Option<&[String]>,
        files: Option<&mut HashSet<String>>,
    ) -> Result<usize> {
        let Some(segments) = segments else {
            let changed = self.conn().execute(
                "UPDATE unresolved_refs SET status = 'pending' WHERE status = 'failed' AND language = ?",
                [language.as_str()],
            )?;
            return Ok(changed);
        };
        if segments.is_empty() {
            return Ok(0);
        }
        let wanted: HashSet<&str> = segments.iter().map(String::as_str).collect();
        let mut stmt =
            self.conn().prepare("SELECT id, reference_name, file_path FROM unresolved_refs WHERE status = 'failed' AND language = ?")?;
        let rows = stmt
            .query_map([language.as_str()], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut ids = Vec::new();
        let mut files = files;
        for (id, reference_name, file_path) in rows {
            if reference_name.split('.').any(|seg| wanted.contains(seg)) {
                if let Some(f) = files.as_deref_mut() {
                    f.insert(file_path);
                }
                ids.push(id);
            }
        }
        self.update_pending_by_ids(&ids)
    }

    // -- Python attribute chains ------------------------------------------------------------

    /// The python files whose failed attribute-chain refs an edit to `file_paths` can affect: the
    /// files that import a touched file, directly or within `depth` hops, and the touched files.
    pub fn get_python_importer_closure(&self, file_paths: &[String], depth: usize) -> Result<Vec<String>> {
        let mut closure: IndexMap<String, ()> = file_paths.iter().map(|p| (p.clone(), ())).collect();
        let mut frontier: Vec<String> = file_paths.to_vec();
        for _ in 0..depth {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<String> = Vec::new();
            for chunk in frontier.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let mut stmt = self.conn().prepare(&format!(
                    "SELECT DISTINCT src.file_path AS file_path
               FROM edges e
               JOIN nodes tgt ON tgt.id = e.target
               JOIN nodes src ON src.id = e.source
              WHERE e.kind = 'imports' AND tgt.file_path IN ({}) AND src.language = 'python'",
                    placeholders(chunk.len())
                ))?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                for file_path in rows {
                    if !closure.contains_key(&file_path) {
                        closure.insert(file_path.clone(), ());
                        next.push(file_path);
                    }
                }
            }
            frontier = next;
        }
        Ok(closure.into_keys().collect())
    }

    /// The failed python attribute-chain refs in `file_paths`, as just the columns a sync needs.
    pub fn get_failed_python_attr_ref_keys(&self, file_paths: &[String]) -> Result<Vec<FailedRefKey>> {
        let wanted: HashSet<&str> = file_paths.iter().map(String::as_str).collect();
        let mut stmt =
            self.conn().prepare(&format!("SELECT id, from_node_id, reference_name, file_path FROM unresolved_refs WHERE {FAILED_PY_ATTR_REF}"))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(FailedRefKey { id: r.get(0)?, from_node_id: r.get(1)?, reference_name: r.get(2)?, file_path: r.get(3)? })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().filter(|r| wanted.contains(r.file_path.as_str())).collect())
    }

    /// The python files with an edge of one of `kinds` into a node of `file_path` whose kind is
    /// one of `target_kinds`.
    pub fn get_python_files_with_edges_into(&self, file_path: &str, kinds: &[String], target_kinds: &[String]) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT DISTINCT src.file_path AS file_path
           FROM edges e
           JOIN nodes tgt ON tgt.id = e.target
           JOIN nodes src ON src.id = e.source
          WHERE tgt.file_path = ? AND e.kind IN ({}) AND tgt.kind IN ({})
            AND src.language = 'python' AND src.file_path != ?",
            placeholders(kinds.len()),
            placeholders(target_kinds.len())
        );
        let params: Vec<Value> = std::iter::once(text(file_path))
            .chain(kinds.iter().map(|k| text(k)))
            .chain(target_kinds.iter().map(|k| text(k)))
            .chain(std::iter::once(text(file_path)))
            .collect();
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // -- Resolution edges ----------------------------------------------------------------------

    /// The highest edge id so far: edges inserted later have higher ids (AUTOINCREMENT).
    pub fn get_max_edge_id(&self) -> Result<i64> {
        Ok(self.conn().query_row("SELECT COALESCE(MAX(id), 0) AS id FROM edges", [], |r| r.get::<_, i64>(0))?)
    }

    /// Resolution edges whose TARGET symbol is named one of `names` — the edges a sync must
    /// re-resolve after `names` gained or lost a definition (CG-33). A name matching more than
    /// `per_name_ceiling` edges is skipped; with `counted_up_to_edge_id`, only edges up to that id
    /// count toward the ceiling.
    pub fn get_resolution_edges_by_target_name(
        &self,
        names: &[String],
        per_name_ceiling: i64,
        counted_up_to_edge_id: Option<i64>,
    ) -> Result<Vec<ResolutionEdge>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        // Pass 1: per-name edge counts.
        let mut keep: Vec<String> = Vec::new();
        let counted = if counted_up_to_edge_id.is_some() { "AND e.id <= ?" } else { "" };
        for chunk in names.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let sql = format!(
                "SELECT tgt.name AS name, COUNT(*) AS count
             FROM edges e
             JOIN nodes tgt ON tgt.id = e.target
            WHERE tgt.name IN ({})
              AND (e.provenance IS NULL OR e.provenance != 'heuristic')
              {counted}
            GROUP BY tgt.name",
                placeholders(chunk.len())
            );
            let mut params: Vec<Value> = chunk.iter().map(|s| text(s)).collect();
            if let Some(id) = counted_up_to_edge_id {
                params.push(Value::Integer(id));
            }
            let mut stmt = self.conn().prepare(&sql)?;
            let counts = stmt
                .query_map(rusqlite::params_from_iter(params), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (name, count) in counts {
                if count <= per_name_ceiling {
                    keep.push(name);
                }
            }
        }
        if keep.is_empty() {
            return Ok(Vec::new());
        }
        // Pass 2: load the surviving edges with the source file context a resurrection needs.
        let mut out = Vec::new();
        for chunk in keep.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            out.extend(self.query_resolution_edges(
                &format!(
                    "SELECT {RES_EDGE_COLS}
             FROM edges e
             JOIN nodes tgt ON tgt.id = e.target
             JOIN nodes src ON src.id = e.source
            WHERE tgt.name IN ({})
              AND (e.provenance IS NULL OR e.provenance != 'heuristic')",
                    placeholders(chunk.len())
                ),
                chunk.iter().map(|s| text(s)).collect(),
                None,
            )?);
        }
        Ok(out)
    }

    /// The resolution edges out of route nodes that a change to `file_paths` can move.
    pub fn get_route_edges_moved_by(&self, file_paths: &[String]) -> Result<Vec<ResolutionEdge>> {
        if file_paths.is_empty() {
            return Ok(Vec::new());
        }
        let files = serde_json::to_string(file_paths).unwrap_or_else(|_| "[]".into());
        self.query_resolution_edges(
            &format!(
                "SELECT {RES_EDGE_COLS}
           FROM nodes src
           JOIN edges e ON e.source = src.id
           JOIN nodes tgt ON tgt.id = e.target
          WHERE src.kind = 'route'
            AND e.kind != 'contains'
            AND (e.provenance IS NULL OR e.provenance != 'heuristic')
            AND src.file_path NOT IN (SELECT value FROM json_each(?))
            AND (tgt.file_path IN (SELECT value FROM json_each(?))
              OR (tgt.file_path = src.file_path AND tgt.kind IN ('constant', 'variable')))
          ORDER BY e.id"
            ),
            vec![text(&files), text(&files)],
            None,
        )
    }

    /// Every resolution edge leaving a node of `language`.
    pub fn get_resolution_edges_by_source_language(&self, language: Language) -> Result<Vec<ResolutionEdge>> {
        self.query_resolution_edges(
            &format!(
                "SELECT {RES_EDGE_COLS}
           FROM edges e
           JOIN nodes src ON src.id = e.source
          WHERE src.language = ?
            AND e.kind != 'contains'
            AND (e.provenance IS NULL OR e.provenance != 'heuristic')"
            ),
            vec![text(language.as_str())],
            None,
        )
    }

    /// Full rows for resolution edges by id, loaded edge-first.
    fn load_resolution_edges(&self, language: Language, ids: &[i64]) -> Result<Vec<ResolutionEdge>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let params: Vec<Value> = chunk.iter().map(|i| Value::Integer(*i)).chain(std::iter::once(text(language.as_str()))).collect();
            out.extend(self.query_resolution_edges(
                &format!(
                    "SELECT {RES_EDGE_COLS}
             FROM edges e
             CROSS JOIN nodes src ON src.id = e.source
             CROSS JOIN nodes tgt ON tgt.id = e.target
            WHERE e.id IN ({})
              AND src.language = ?
              AND e.kind != 'contains'
              AND (e.provenance IS NULL OR e.provenance != 'heuristic')",
                    placeholders(chunk.len())
                ),
                params,
                None,
            )?);
        }
        Ok(out)
    }

    /// Resolution edges leaving a `language` node whose TARGET is in one of `target_files`, or
    /// whose SOURCE is under one of `source_dirs`.
    pub fn get_resolution_edges_touching_files(
        &self,
        language: Language,
        target_files: &[String],
        source_dirs: &[String],
        import_node_segments: &[String],
    ) -> Result<Vec<ResolutionEdge>> {
        let mut candidates: IndexMap<i64, ()> = IndexMap::new();
        let ids_where = |column: &str, node_ids: &[String], candidates: &mut IndexMap<i64, ()>| -> Result<()> {
            for chunk in node_ids.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let mut stmt = self.conn().prepare(&format!("SELECT id FROM edges WHERE {column} IN ({})", placeholders(chunk.len())))?;
                let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| r.get::<_, i64>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
                for id in rows {
                    candidates.insert(id, ());
                }
            }
            Ok(())
        };
        // Edges into the affected module files.
        for chunk in target_files.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let mut stmt = self.conn().prepare(&format!("SELECT id FROM nodes WHERE file_path IN ({})", placeholders(chunk.len())))?;
            let ids = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
            ids_where("target", &ids, &mut candidates)?;
        }
        // Edges out of a changed package: its nodes by an index range on the path (`dir/` up to `dir0`).
        for dir in source_dirs {
            let ids: Vec<String> = if dir.is_empty() {
                let mut stmt = self.conn().prepare("SELECT id FROM nodes WHERE language = ?")?;
                let rows = stmt.query_map([language.as_str()], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            } else {
                let mut stmt = self.conn().prepare("SELECT id FROM nodes WHERE file_path >= ? AND file_path < ?")?;
                let rows = stmt
                    .query_map([format!("{dir}/"), format!("{dir}0")], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
            ids_where("source", &ids, &mut candidates)?;
        }
        // Imports the resolver couldn't place and bound to their own `import` node, matched by the
        // module name they spell: ONE read of the language's import nodes, filtered here.
        if !import_node_segments.is_empty() {
            let wanted: HashSet<&str> = import_node_segments.iter().map(String::as_str).collect();
            let mut stmt = self.conn().prepare("SELECT id, name FROM nodes WHERE kind = 'import' AND language = ?")?;
            let rows = stmt
                .query_map([language.as_str()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let ids: Vec<String> = rows.into_iter().filter(|(_, name)| name.split('.').any(|seg| wanted.contains(seg))).map(|(id, _)| id).collect();
            ids_where("target", &ids, &mut candidates)?;
        }
        let ids: Vec<i64> = candidates.into_keys().collect();
        self.load_resolution_edges(language, &ids)
    }

    /// Resolution edges leaving any node of `files`.
    pub fn get_resolution_edges_from_files(&self, language: Language, files: &[String]) -> Result<Vec<ResolutionEdge>> {
        let mut ids: Vec<i64> = Vec::new();
        for chunk in files.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let mut stmt = self.conn().prepare(&format!(
                "SELECT e.id AS id FROM nodes n CROSS JOIN edges e ON e.source = n.id
            WHERE n.file_path IN ({})",
                placeholders(chunk.len())
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| r.get::<_, i64>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
            ids.extend(rows);
        }
        self.load_resolution_edges(language, &ids)
    }

    /// Resolution edges whose answer was read from other files (`metadata.typeFrom`).
    pub fn get_edges_with_type_from(&self) -> Result<Vec<ResolutionEdge>> {
        self.query_resolution_edges(
            &format!(
                "SELECT {RES_EDGE_COLS}
           FROM edges e
           JOIN nodes src ON src.id = e.source
          WHERE e.metadata LIKE '%\"typeFrom\"%'"
            ),
            Vec::new(),
            None,
        )
    }

    /// The `navigates` edges a router's resolver made (not a synthesizer's).
    pub fn get_resolved_navigations(&self) -> Result<Vec<ResolutionEdge>> {
        self.query_resolution_edges(
            &format!(
                "SELECT {RES_EDGE_COLS}
           FROM edges e
           JOIN nodes src ON src.id = e.source
          WHERE e.kind = 'navigates' AND (e.provenance IS NULL OR e.provenance != 'heuristic')"
            ),
            Vec::new(),
            None,
        )
    }

    /// Resolution edges out of the nodes of `file_path` whose reference was written through one of
    /// `names`, with the file of each target.
    pub fn get_resolution_edges_through(&self, file_path: &str, names: &[String]) -> Result<Vec<ResolutionEdge>> {
        let names_json = serde_json::to_string(names).unwrap_or_else(|_| "[]".into());
        self.query_resolution_edges(
            &format!(
                "SELECT source, target, kind, metadata, line, col, provenance, id, source_file_path, source_language, target_file_path FROM (
           SELECT e.source AS source, e.target AS target, e.kind AS kind, e.metadata AS metadata, e.line AS line, e.col AS col,
                  e.provenance AS provenance, e.id AS id,
                  src.file_path AS source_file_path, src.language AS source_language, tgt.file_path AS target_file_path,
                  CASE WHEN json_valid(e.metadata) THEN json_extract(e.metadata, '$.refName') END AS ref_name
             FROM nodes src
             JOIN edges e ON e.source = src.id
             JOIN nodes tgt ON tgt.id = e.target
            WHERE src.file_path = ?
              AND e.kind != 'contains'
              AND (e.provenance IS NULL OR e.provenance != 'heuristic')
         ) WHERE {}",
                through_names("ref_name")
            ),
            vec![text(file_path), text(&names_json), text(&names_json)],
            Some(10),
        )
    }

    /// The import bindings of `file_paths` that resolved some other way than through their import.
    pub fn get_import_bindings_resolved_elsewhere(&self, file_paths: &[String]) -> Result<Vec<ResolutionEdge>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let unique: Vec<String> = file_paths.iter().filter(|p| seen.insert(p.as_str())).map(|p| format!("file:{p}")).collect();
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            out.extend(self.query_resolution_edges(
                &format!(
                    "SELECT {RES_EDGE_COLS}, tgt.file_path
             FROM edges e
             JOIN nodes src ON src.id = e.source
             JOIN nodes tgt ON tgt.id = e.target
            WHERE e.source IN ({})
              AND e.kind = 'imports'
              AND (e.provenance IS NULL OR e.provenance != 'heuristic')
              AND json_valid(e.metadata)
              AND json_extract(e.metadata, '$.resolvedBy') != 'import'",
                    placeholders(chunk.len())
                ),
                chunk.iter().map(|s| text(s)).collect(),
                Some(10),
            )?);
        }
        Ok(out)
    }

    /// The resolution edges into `target_ids`, with the file of each target.
    pub fn get_resolution_edges_into(&self, target_ids: &[String]) -> Result<Vec<ResolutionEdge>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let unique: Vec<&String> = target_ids.iter().filter(|p| seen.insert(p.as_str())).collect();
        for chunk in unique.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            out.extend(self.query_resolution_edges(
                &format!(
                    "SELECT {RES_EDGE_COLS}, tgt.file_path
             FROM edges e
             JOIN nodes src ON src.id = e.source
             JOIN nodes tgt ON tgt.id = e.target
            WHERE e.target IN ({})
              AND e.kind != 'contains'
              AND (e.provenance IS NULL OR e.provenance != 'heuristic')",
                    placeholders(chunk.len())
                ),
                chunk.iter().map(|s| text(s)).collect(),
                Some(10),
            )?);
        }
        Ok(out)
    }

    /// The module edges (`imports` edges into a file node) into the files an import of `dir/name`
    /// can find instead of a file added there.
    pub fn get_module_imports_named(&self, dir: &str, name: &str) -> Result<Vec<ResolutionEdge>> {
        let base = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
        // `x.` up to `x/` and `x/index.` up to `x/index/` ('/' is the byte after '.').
        let rows = self.query_resolution_edges(
            &format!(
                "SELECT {RES_EDGE_COLS}, tgt.file_path
           FROM nodes tgt
           JOIN edges e ON e.target = tgt.id AND e.kind = 'imports'
           JOIN nodes src ON src.id = e.source
          WHERE tgt.kind = 'file'
            AND ((tgt.file_path >= ? AND tgt.file_path < ?) OR (tgt.file_path >= ? AND tgt.file_path < ?) OR tgt.file_path = ?)"
            ),
            vec![
                text(&format!("{base}.")),
                text(&format!("{base}/")),
                text(&format!("{base}/index.")),
                text(&format!("{base}/index/")),
                text(&format!("{base}/__init__.py")),
            ],
            Some(10),
        )?;
        let index = format!("{base}/index.");
        let init = format!("{base}/__init__.py");
        Ok(rows
            .into_iter()
            .filter(|row| {
                let file = row.target_file_path.as_deref().unwrap_or("");
                if file == init {
                    return true;
                }
                // Not a deeper path that only shares the prefix (`dir/name.d/x.ts`).
                let rest = match file.strip_prefix(&index) {
                    Some(r) => r,
                    None => file.get(base.len() + 1..).unwrap_or(""),
                };
                !rest.contains('/')
            })
            .collect())
    }

    /// Delete edges by primary key.
    pub fn delete_edges_by_ids(&self, edge_ids: &[i64]) -> Result<usize> {
        if edge_ids.is_empty() {
            return Ok(0);
        }
        self.transaction(|| {
            let mut changed = 0;
            for chunk in edge_ids.chunks(SQLITE_PARAM_CHUNK_SIZE) {
                let mut stmt = self.conn().prepare(&format!("DELETE FROM edges WHERE id IN ({})", placeholders(chunk.len())))?;
                changed += stmt.execute(rusqlite::params_from_iter(chunk.iter()))?;
            }
            Ok(changed)
        })
    }

    /// Replace resolution edges with their original unresolved references as one transaction.
    pub fn replace_resolution_edges_with_unresolved_refs(&self, edge_ids: &[i64], refs: &[UnresolvedReference]) -> Result<usize> {
        self.transaction(|| {
            let changed = self.delete_edges_by_ids(edge_ids)?;
            self.insert_unresolved_refs_batch(refs)?;
            Ok(changed)
        })
    }

    // -- Imports ---------------------------------------------------------------------------------

    /// The targets, in other files, of the non-synthesized `imports` edges out of `file_path`.
    pub fn get_import_targets_from_file(&self, file_path: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT e.target AS target
           FROM nodes src
           JOIN edges e ON e.source = src.id
           JOIN nodes tgt ON tgt.id = e.target
          WHERE src.file_path = ?
            AND e.kind = 'imports'
            AND (e.provenance IS NULL OR e.provenance != 'heuristic')
            AND tgt.file_path != ?",
        )?;
        let rows = stmt.query_map([file_path, file_path], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The `imports` edges into the node `target_id` from files other than `except_file`.
    pub fn get_imports_into(&self, target_id: &str, except_file: &str) -> Result<Vec<ImportInto>> {
        let mut stmt = self.conn().prepare(
            "SELECT src.file_path AS file_path, src.language AS language,
                CASE WHEN json_valid(e.metadata) THEN json_extract(e.metadata, '$.refName') END AS ref_name
           FROM edges e
           JOIN nodes src ON src.id = e.source
          WHERE e.target = ? AND e.kind = 'imports' AND src.file_path != ?",
        )?;
        let rows = stmt
            .query_map([target_id, except_file], |r| {
                let language: String = r.get(1)?;
                let ref_name: Option<rusqlite::types::Value> = r.get(2)?;
                Ok(ImportInto {
                    file_path: r.get(0)?,
                    language: language_of(&language),
                    ref_name: match ref_name {
                        Some(Value::Text(s)) => Some(s),
                        _ => None,
                    },
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The files with a resolution edge into one of `target_ids`.
    pub fn get_files_with_edges_into(&self, target_ids: &[String]) -> Result<Vec<String>> {
        let mut files: IndexMap<String, ()> = IndexMap::new();
        for chunk in target_ids.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let mut stmt = self.conn().prepare(&format!(
                "SELECT DISTINCT src.file_path AS file_path
             FROM edges e
             JOIN nodes src ON src.id = e.source
            WHERE e.target IN ({})
              AND e.kind != 'contains'
              AND (e.provenance IS NULL OR e.provenance != 'heuristic')",
                placeholders(chunk.len())
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
            for f in rows {
                files.insert(f, ());
            }
        }
        Ok(files.into_keys().collect())
    }

    /// The import nodes named one of `names` (an import node is named by the module path it imports).
    pub fn get_import_nodes_by_names(&self, names: &[String]) -> Result<Vec<ImportNodeName>> {
        let mut out = Vec::new();
        for chunk in names.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let mut stmt = self.conn().prepare(&format!(
                "SELECT file_path, name, language FROM nodes WHERE name IN ({}) AND kind = 'import'",
                placeholders(chunk.len())
            ))?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                    let language: String = r.get(2)?;
                    Ok(ImportNodeName { file_path: r.get(0)?, name: r.get(1)?, language: language_of(&language) })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            out.extend(rows);
        }
        Ok(out)
    }

    // -- Definitions -------------------------------------------------------------------------------

    /// Distinct node names present in the given files; `kinds` narrows them to nodes of those kinds.
    pub fn get_node_names_by_files(&self, file_paths: &[String], kinds: Option<&[NodeKind]>) -> Result<Vec<String>> {
        if file_paths.is_empty() {
            return Ok(Vec::new());
        }
        let kind_filter = match kinds {
            Some(k) => format!(" AND kind IN ({})", placeholders(k.len())),
            None => String::new(),
        };
        let mut names: IndexMap<String, ()> = IndexMap::new();
        for chunk in file_paths.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let params: Vec<Value> = chunk.iter().map(|s| text(s)).chain(kinds.unwrap_or(&[]).iter().map(|k| text(k.as_str()))).collect();
            let mut stmt = self
                .conn()
                .prepare(&format!("SELECT DISTINCT name FROM nodes WHERE file_path IN ({}){kind_filter}", placeholders(chunk.len())))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
            for n in rows {
                names.insert(n, ());
            }
        }
        Ok(names.into_keys().collect())
    }

    /// Distinct `file\0name` pairs defined by the given files, in first-seen order — the shape
    /// sync's definition delta needs (CG-33). NUL-joined: a path or a symbol name can contain a
    /// space, never a NUL. Keyed by file so a name that moves between two changed files is one
    /// removal plus one addition instead of cancelling out.
    pub fn get_node_name_pairs_by_files(&self, file_paths: &[String]) -> Result<Vec<String>> {
        let mut pairs: IndexMap<String, ()> = IndexMap::new();
        for chunk in file_paths.chunks(SQLITE_PARAM_CHUNK_SIZE) {
            let mut stmt = self
                .conn()
                .prepare(&format!("SELECT DISTINCT file_path, name FROM nodes WHERE file_path IN ({})", placeholders(chunk.len())))?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (file_path, name) in rows {
                pairs.insert(format!("{file_path}\0{name}"), ());
            }
        }
        Ok(pairs.into_keys().collect())
    }

    /// Whether a synthesized edge touches `file_path`: an endpoint in it, or wiring registered in
    /// it. Must run before file replacement/deletion cascades the endpoint edges.
    pub fn has_synthesized_edges_touching_file(&self, file_path: &str) -> Result<bool> {
        let owned = "CASE WHEN json_valid(e.metadata) THEN json_extract(e.metadata, '$.synthesizedBy') END IS NOT NULL";
        for endpoint in ["source", "target"] {
            let mut stmt = self.conn().prepare_cached(&format!(
                "SELECT 1 FROM nodes n JOIN edges e ON e.{endpoint} = n.id
        WHERE n.file_path = ? AND {owned} LIMIT 1"
            ))?;
            if stmt.exists([file_path])? {
                return Ok(true);
            }
        }
        // Wiring often lives in a third file, with neither endpoint in it.
        let mut stmt = self.conn().prepare_cached(&format!(
            "SELECT 1 FROM edges e WHERE {owned}
      AND CASE WHEN json_valid(e.metadata) THEN json_extract(e.metadata, '$.registeredAt') END >= ?
      AND CASE WHEN json_valid(e.metadata) THEN json_extract(e.metadata, '$.registeredAt') END < ? LIMIT 1"
        ))?;
        Ok(stmt.exists([format!("{file_path}:"), format!("{file_path};")])?)
    }

    /// Delete every synthesized edge (ownership is `metadata.synthesizedBy`, independent of
    /// provenance: Go method containment is structural). Returns how many went.
    pub fn delete_synthesized_edges(&self) -> Result<usize> {
        let mut total = 0;
        let mut stmt = self.conn().prepare_cached(&format!(
            "DELETE FROM edges WHERE id IN (SELECT id FROM edges WHERE {SYNTHESIZED_EDGE} LIMIT 2000)"
        ))?;
        loop {
            let changed = stmt.execute([])?;
            if changed == 0 {
                return Ok(total);
            }
            total += changed;
        }
    }
}

/// `/^[A-Za-z_$][\w$]*$/` — a name a member reference's receiver can be: not a file's name, nor a
/// `module:` key.
fn is_receiver_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Group `(file, names)` pairs into the shape `requeue_failed_references_binding` takes.
pub fn bindings_from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> IndexMap<String, HashSet<String>> {
    let mut map: IndexMap<String, HashSet<String>> = IndexMap::new();
    for (file, name) in pairs {
        map.entry(file).or_default().insert(name);
    }
    map
}
