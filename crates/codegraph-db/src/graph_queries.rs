//! File dependency projection and graph connectivity reads from `QueryBuilder`.

use crate::{
    rows::{edge_from_row, EDGE_COLS},
    Result, Store,
};
use codegraph_types::{Edge, EdgeKind};
use rusqlite::params_from_iter;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DominantFile {
    pub file_path: String,
    pub edge_count: u64,
    pub next_edge_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopRouteFile {
    pub file_path: String,
    pub route_count: u64,
    pub total_routes: u64,
}

pub(crate) type DominantMemo = ((i64, i64), Option<DominantFile>);

/// `project_metadata` key holding the last dominant-file verdict and the table fingerprint
/// it was computed against.
pub const DOMINANT_FILE_METADATA_KEY: &str = "dominant_file_v1";

/// Row counts and top row ids of the tables the verdict reads: a write to any of them moves it.
const DOMINANT_FINGERPRINT_SQL: &str = "SELECT (SELECT count(*) FROM edges), (SELECT max(rowid) FROM edges),     (SELECT count(*) FROM nodes), (SELECT max(rowid) FROM nodes), (SELECT count(*) FROM files), (SELECT max(rowid) FROM files)";

#[derive(Serialize, Deserialize)]
struct PersistedDominant {
    fingerprint: Vec<Option<i64>>,
    value: Option<DominantFile>,
}

pub const DOMINANT_FILE_SQL: &str = "SELECT n.file_path, COUNT(*) AS edge_count FROM edges e
    CROSS JOIN nodes n ON e.source = n.id CROSS JOIN nodes m ON e.target = m.id
    WHERE n.file_path = m.file_path GROUP BY n.file_path ORDER BY edge_count DESC, n.file_path";

impl Store {
    /// Memoization sees this connection's writes AND commits by another process.
    /// Transactional reads never populate the cache, so a rollback cannot leak
    /// a transient verdict into a subsequent read (#2578).
    pub fn get_dominant_file(&self) -> Result<Option<DominantFile>> {
        if !self.conn().is_autocommit() {
            self.dominant_file_memo.borrow_mut().take();
            return self.compute_dominant_file();
        }
        let stamp: (i64, i64) = self.conn().query_row(
            "SELECT total_changes(), (SELECT data_version FROM pragma_data_version)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if let Some((previous, value)) = self.dominant_file_memo.borrow().as_ref() {
            if *previous == stamp {
                return Ok(value.clone());
            }
        }
        let value = self.persisted_dominant_file()?;
        *self.dominant_file_memo.borrow_mut() = Some((stamp, value.clone()));
        Ok(value)
    }

    /// The aggregate reads every edge (~300 ms on a 170K-edge index), so the verdict is kept in
    /// `project_metadata` for the next connection — each CLI call opens a fresh one. Storing it
    /// is best-effort: a read-only connection just recomputes.
    fn persisted_dominant_file(&self) -> Result<Option<DominantFile>> {
        let fingerprint: Vec<Option<i64>> = self.conn().query_row(DOMINANT_FINGERPRINT_SQL, [], |r| {
            (0..6).map(|i| r.get::<_, Option<i64>>(i)).collect()
        })?;
        if let Some(stored) = self.get_metadata(DOMINANT_FILE_METADATA_KEY)? {
            if let Ok(p) = serde_json::from_str::<PersistedDominant>(&stored) {
                if p.fingerprint == fingerprint {
                    return Ok(p.value);
                }
            }
        }
        let value = self.compute_dominant_file()?;
        if let Ok(json) = serde_json::to_string(&PersistedDominant { fingerprint, value: value.clone() }) {
            // Never wait on another process's write lock for a cache entry.
            let conn = self.conn();
            let timeout: i64 = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0)).unwrap_or(5000);
            let _ = conn.busy_timeout(std::time::Duration::ZERO);
            let _ = self.set_metadata(DOMINANT_FILE_METADATA_KEY, &json);
            let _ = conn.busy_timeout(std::time::Duration::from_millis(timeout.max(0) as u64));
        }
        Ok(value)
    }

    fn compute_dominant_file(&self) -> Result<Option<DominantFile>> {
        let mut stmt = self.conn().prepare_cached(DOMINANT_FILE_SQL)?;
        let rows: Vec<(String, u64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut kept = Vec::new();
        for batch in rows.chunks(20) {
            let paths: Vec<_> = batch.iter().map(|r| &r.0).collect();
            let generated = self.get_generated_paths_among(&paths)?;
            for (path, count) in batch {
                if low_value_file(path, &generated) {
                    continue;
                }
                kept.push((path, *count));
                if kept.len() == 2 {
                    break;
                }
            }
            if kept.len() == 2 {
                break;
            }
        }
        let Some((path, count)) = kept.first().filter(|(_, n)| *n >= 20) else {
            return Ok(None);
        };
        Ok(Some(DominantFile {
            file_path: (*path).clone(),
            edge_count: *count,
            next_edge_count: kept.get(1).map(|(_, n)| *n).unwrap_or(0),
        }))
    }

    pub fn get_top_route_file(&self) -> Result<Option<TopRouteFile>> {
        let mut stmt = self.conn().prepare_cached("SELECT file_path, COUNT(*) AS cnt FROM nodes WHERE kind = 'route' GROUP BY file_path ORDER BY cnt DESC, file_path")?;
        let rows: Vec<(String, u64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let paths: Vec<_> = rows.iter().map(|r| &r.0).collect();
        let generated = self.get_generated_paths_among(&paths)?;
        let filtered: Vec<_> = rows
            .into_iter()
            .filter(|(p, _)| !low_value_file(p, &generated))
            .collect();
        let Some((path, count)) = filtered.first() else {
            return Ok(None);
        };
        let total: u64 = filtered.iter().map(|(_, count)| count).sum();
        if total < 3 || *count < 3 || (*count as f64 / total as f64) < 0.30 {
            return Ok(None);
        }
        Ok(Some(TopRouteFile {
            file_path: path.clone(),
            route_count: *count,
            total_routes: total,
        }))
    }
    /// All cross-file relations count as dependencies except containment.
    pub fn get_dependency_file_paths(&self, file_path: &str) -> Result<Vec<String>> {
        self.dependency_paths(file_path, false)
    }

    pub fn get_dependent_file_paths(&self, file_path: &str) -> Result<Vec<String>> {
        self.dependency_paths(file_path, true)
    }

    fn dependency_paths(&self, file_path: &str, incoming: bool) -> Result<Vec<String>> {
        let (from, to) = if incoming {
            ("tgt", "src")
        } else {
            ("src", "tgt")
        };
        let sql = format!(
            "SELECT DISTINCT {to}.file_path AS fp FROM edges e
             JOIN nodes src ON src.id = e.source JOIN nodes tgt ON tgt.id = e.target
             WHERE {from}.file_path = ? AND e.kind != 'contains' AND {to}.file_path != ?"
        );
        let mut stmt = self.conn().prepare_cached(&sql)?;
        let rows = stmt
            .query_map([file_path, file_path], |r| r.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    /// JSON membership avoids a quadratic number of SQL variables on large graphs.
    pub fn find_edges_between_nodes(
        &self,
        ids: &[String],
        kinds: &[EdgeKind],
    ) -> Result<Vec<Edge>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids_json = serde_json::to_string(ids).expect("string arrays serialize");
        let mut sql = format!("SELECT {EDGE_COLS} FROM edges WHERE source IN (SELECT value FROM json_each(?)) AND target IN (SELECT value FROM json_each(?))");
        let mut params = vec![ids_json.clone(), ids_json];
        if !kinds.is_empty() {
            sql.push_str(&format!(
                " AND kind IN ({})",
                vec!["?"; kinds.len()].join(",")
            ));
            params.extend(kinds.iter().map(|k| k.as_str().to_owned()));
        }
        let mut stmt = self.conn().prepare_cached(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(params), edge_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

fn low_value_file(path: &str, generated: &HashSet<String>) -> bool {
    generated.contains(path)
        || codegraph_search::is_test_file(path)
        || codegraph_jsre::re!(r"(?:^|/)__test__/", "i").is_match(path)
        || codegraph_search::is_generated_file(path)
}
