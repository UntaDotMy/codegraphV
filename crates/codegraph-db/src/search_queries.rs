//! Ranked symbol search from `QueryBuilder`: FTS5, LIKE and fuzzy fallbacks,
//! exact-name supplementation, field filters and the shared scoring helpers.
use crate::{
    rows::{node_from_row, NODE_COLS},
    Result, Store,
};
use codegraph_jsre::re;
use codegraph_search::query_utils::NameMatchQuery;
use codegraph_search::{bounded_edit_distance, kind_bonus, parse_query, score_path_relevance};
use codegraph_types::{
    jscompat::{is_js_space, utf16_len},
    Language, NodeKind, SearchOptions, SearchResult,
};
use rusqlite::{params_from_iter, types::Value};
use std::collections::{HashMap, HashSet};

pub const DEPRIORITIZED_NAME_BONUS_SCALE: f64 = 0.75;
pub type DeprioritizedPath<'a> = &'a dyn Fn(&str) -> bool;

impl Store {
    pub fn find_nodes_by_name_substring(
        &self,
        substring: &str,
        options: &SearchOptions,
        exclude_prefix: bool,
    ) -> Result<Vec<SearchResult>> {
        let mut sql = format!("SELECT {NODE_COLS}, 1.0 AS score FROM nodes WHERE name LIKE ?");
        let mut params = vec![Value::Text(format!("%{substring}%"))];
        if exclude_prefix {
            sql.push_str(" AND name NOT LIKE ?");
            params.push(Value::Text(format!("{substring}%")));
        }
        append_filters(
            &mut sql,
            &mut params,
            options.kinds.as_deref().unwrap_or(&[]),
            options.languages.as_deref().unwrap_or(&[]),
            "",
        );
        sql.push_str(" ORDER BY length(name) ASC LIMIT ?");
        params.push(integer(options.limit.unwrap_or(30)));
        self.search_rows(&sql, params)
    }

    pub fn search_nodes(&self, query: &str, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        self.search_nodes_with_deprioritizer(query, options, None)
    }

    pub fn search_nodes_with_deprioritizer(
        &self,
        query: &str,
        options: &SearchOptions,
        deprioritized: Option<DeprioritizedPath<'_>>,
    ) -> Result<Vec<SearchResult>> {
        let limit = options.limit.unwrap_or(100);
        let offset = options.offset.unwrap_or(0);
        let parsed = parse_query(query);
        let mut kinds = options.kinds.clone().unwrap_or_default();
        let mut languages = options.languages.clone().unwrap_or_default();
        for kind in parsed.kinds {
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        for language in parsed.languages {
            if !languages.contains(&language) {
                languages.push(language);
            }
        }
        let mut results = if !parsed.text.is_empty() {
            if self.fts5_available() {
                self.search_fts(&parsed.text, &kinds, &languages, limit, offset)
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            self.search_all_filters(&kinds, &languages, limit.saturating_mul(5))?
        };
        if results.is_empty() && utf16_len(&parsed.text) >= 2 {
            results = self.search_like(&parsed.text, &kinds, &languages, limit, offset)?;
        }
        if results.is_empty() && utf16_len(&parsed.text) >= 3 {
            results = self.search_fuzzy(&parsed.text, &kinds, &languages, limit)?;
        }
        if !results.is_empty() && !query.is_empty() {
            let mut seen: HashSet<_> = results.iter().map(|r| r.node.id.clone()).collect();
            let max_score = results
                .iter()
                .map(|r| r.score)
                .fold(f64::NEG_INFINITY, f64::max);
            for term in query.split(is_js_space).filter(|t| utf16_len(t) >= 2) {
                let mut sql = format!(
                    "SELECT {NODE_COLS}, 1.0 AS score FROM nodes WHERE lower(name) = lower(?)"
                );
                let mut params = vec![Value::Text(term.to_owned())];
                append_filters(&mut sql, &mut params, &kinds, &languages, "");
                sql.push_str(" LIMIT 20");
                for mut r in self.search_rows(&sql, params)? {
                    if seen.insert(r.node.id.clone()) {
                        r.score = max_score;
                        results.push(r);
                    }
                }
            }
        }
        if !results.is_empty() && (!parsed.text.is_empty() || !query.is_empty()) {
            let scoring_query = if parsed.text.is_empty() {
                query
            } else {
                &parsed.text
            };
            let project_tokens = self.project_name_tokens();
            let name_scoring = NameMatchQuery::new(scoring_query);
            // Per-query only: identical paths receive identical scores, while
            // a project's deprioritization callback is still evaluated once
            // per candidate and its verdict is part of the memo key.
            let mut path_scores = HashMap::new();
            for r in &mut results {
                let deprioritized = deprioritized.is_some_and(|test| test(&r.node.file_path));
                let name_bonus = name_scoring.score_name(&r.node.name);
                r.score += f64::from(kind_bonus(r.node.kind));
                let path_score = *path_scores
                    .entry((r.node.file_path.as_str(), deprioritized))
                    .or_insert_with(|| {
                        score_path_relevance(
                            &r.node.file_path,
                            scoring_query,
                            Some(&project_tokens),
                            deprioritized,
                        )
                    });
                r.score += f64::from(path_score);
                r.score += if deprioritized {
                    (f64::from(name_bonus) * DEPRIORITIZED_NAME_BONUS_SCALE + 0.5).floor()
                } else {
                    f64::from(name_bonus)
                };
            }
            results.sort_by(|a, b| b.score.total_cmp(&a.score));
            results.truncate(limit);
        }
        if !parsed.path_filters.is_empty() {
            let filters: Vec<_> = parsed
                .path_filters
                .iter()
                .map(|p| p.to_lowercase())
                .collect();
            results.retain(|r| {
                filters
                    .iter()
                    .any(|f| r.node.file_path.to_lowercase().contains(f))
            });
        }
        if !parsed.name_filters.is_empty() {
            let filters: Vec<_> = parsed
                .name_filters
                .iter()
                .map(|p| p.to_lowercase())
                .collect();
            results.retain(|r| {
                filters
                    .iter()
                    .any(|f| r.node.name.to_lowercase().contains(f))
            });
        }
        Ok(results)
    }

    fn search_rows(&self, sql: &str, params: Vec<Value>) -> Result<Vec<SearchResult>> {
        let mut stmt = self.conn().prepare_cached(sql)?;
        let rows = stmt
            .query_map(params_from_iter(params), |row| {
                Ok(SearchResult {
                    node: node_from_row(row)?,
                    score: row.get(21)?,
                    highlights: None,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn search_all_filters(
        &self,
        kinds: &[NodeKind],
        languages: &[Language],
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let mut sql = format!("SELECT {NODE_COLS}, 1.0 AS score FROM nodes WHERE 1=1");
        let mut params = Vec::new();
        append_filters(&mut sql, &mut params, kinds, languages, "");
        sql.push_str(" ORDER BY name LIMIT ?");
        params.push(integer(limit));
        self.search_rows(&sql, params)
    }

    fn search_fts(
        &self,
        query: &str,
        kinds: &[NodeKind],
        languages: &[Language],
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchResult>> {
        let stripped = query.replace("::", " ");
        let stripped = re!(r#"['"*():^]"#).replace_all_str(&stripped, "");
        let terms: Vec<_> = stripped
            .split(is_js_space)
            .filter(|t| !t.is_empty() && !re!(r"^(AND|OR|NOT|NEAR)$", "i").is_match(t))
            .map(|t| format!("\"{t}\"*"))
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let columns = NODE_COLS
            .split(',')
            .map(|c| format!("nodes.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!("SELECT {columns}, bm25(nodes_fts, 0, 20, 5, 1, 2) AS score FROM nodes_fts JOIN nodes ON nodes_fts.id = nodes.id WHERE nodes_fts MATCH ?");
        let mut params = vec![Value::Text(terms.join(" OR "))];
        append_filters(&mut sql, &mut params, kinds, languages, "nodes.");
        sql.push_str(" ORDER BY score LIMIT ? OFFSET ?");
        params.extend([integer(limit.saturating_mul(5).max(100)), integer(offset)]);
        let mut results = self.search_rows(&sql, params)?;
        for r in &mut results {
            r.score = r.score.abs();
        }
        Ok(results)
    }

    fn search_like(
        &self,
        query: &str,
        kinds: &[NodeKind],
        languages: &[Language],
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchResult>> {
        let mut sql = format!("SELECT {NODE_COLS}, CASE WHEN name = ? THEN 1.0 WHEN name LIKE ? THEN 0.9 WHEN name LIKE ? THEN 0.8 WHEN qualified_name LIKE ? THEN 0.7 ELSE 0.5 END AS score FROM nodes WHERE (name LIKE ? OR qualified_name LIKE ? OR name LIKE ?)");
        let starts = format!("{query}%");
        let contains = format!("%{query}%");
        let mut params: Vec<_> = [
            query, &starts, &contains, &contains, &contains, &contains, &starts,
        ]
        .into_iter()
        .map(|p| Value::Text(p.to_owned()))
        .collect();
        append_filters(&mut sql, &mut params, kinds, languages, "");
        sql.push_str(" ORDER BY score DESC, length(name) ASC LIMIT ? OFFSET ?");
        params.extend([integer(limit), integer(offset)]);
        self.search_rows(&sql, params)
    }

    fn search_fuzzy(
        &self,
        text: &str,
        kinds: &[NodeKind],
        languages: &[Language],
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let lowered = text.to_lowercase();
        let max_dist = if utf16_len(&lowered) <= 4 { 1 } else { 2 };
        let mut candidates = Vec::new();
        for name in self.get_all_node_names()? {
            let dist = bounded_edit_distance(&name.to_lowercase(), &lowered, max_dist);
            if dist <= max_dist {
                candidates.push((name, dist));
            }
        }
        candidates.sort_by_key(|(_, dist)| *dist);
        candidates.truncate(limit.saturating_mul(2).max(50));
        let mut results = Vec::new();
        let mut seen = HashSet::new();
        for (name, dist) in candidates {
            if results.len() >= limit {
                break;
            }
            let mut sql = format!("SELECT {NODE_COLS}, 1.0 AS score FROM nodes WHERE name = ?");
            let mut params = vec![Value::Text(name)];
            append_filters(&mut sql, &mut params, kinds, languages, "");
            sql.push_str(" LIMIT 5");
            for mut r in self.search_rows(&sql, params)? {
                if !seen.insert(r.node.id.clone()) {
                    continue;
                }
                r.score = 1.0 / (1 + dist) as f64;
                results.push(r);
                if results.len() >= limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    /// Co-location-aware exact-name seeds used by context retrieval.
    pub fn find_nodes_by_exact_name(
        &self,
        names: &[String],
        options: &SearchOptions,
    ) -> Result<Vec<SearchResult>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let limit = options.limit.unwrap_or(50);
        let kinds = options.kinds.as_deref().unwrap_or(&[]);
        let languages = options.languages.as_deref().unwrap_or(&[]);
        let mut name_to_files = std::collections::HashMap::new();
        for name in names {
            let mut sql =
                String::from("SELECT DISTINCT file_path FROM nodes WHERE lower(name) = lower(?)");
            let mut params = vec![Value::Text(name.clone())];
            append_filters(&mut sql, &mut params, kinds, &[], "");
            sql.push_str(" LIMIT 100");
            let mut stmt = self.conn().prepare_cached(&sql)?;
            let files = stmt
                .query_map(params_from_iter(params), |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<HashSet<_>, _>>()?;
            name_to_files.insert(name.to_lowercase(), files);
        }
        let distinctive: HashSet<_> = name_to_files
            .into_values()
            .filter(|files| !files.is_empty() && files.len() < 10)
            .flatten()
            .collect();
        let per_name = limit.div_ceil(names.len()).max(8);
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            let mut sql =
                format!("SELECT {NODE_COLS}, 1.0 AS score FROM nodes WHERE lower(name) = lower(?)");
            let mut params = vec![Value::Text(name.clone())];
            append_filters(&mut sql, &mut params, kinds, languages, "");
            sql.push_str(" LIMIT ?");
            params.push(integer(per_name.saturating_mul(3).max(50)));
            let mut results = self.search_rows(&sql, params)?;
            results.retain(|r| !seen.contains(&r.node.id));
            for r in &mut results {
                if distinctive.contains(&r.node.file_path) {
                    r.score += 20.0;
                }
            }
            results.sort_by(|a, b| b.score.total_cmp(&a.score));
            for r in results.into_iter().take(per_name) {
                seen.insert(r.node.id.clone());
                out.push(r);
            }
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score));
        out.truncate(limit);
        Ok(out)
    }
}

fn integer(n: usize) -> Value {
    Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

fn append_filters(
    sql: &mut String,
    params: &mut Vec<Value>,
    kinds: &[NodeKind],
    languages: &[Language],
    prefix: &str,
) {
    if !kinds.is_empty() {
        sql.push_str(&format!(
            " AND {prefix}kind IN ({})",
            vec!["?"; kinds.len()].join(",")
        ));
        params.extend(kinds.iter().map(|k| Value::Text(k.as_str().to_owned())));
    }
    if !languages.is_empty() {
        sql.push_str(&format!(
            " AND {prefix}language IN ({})",
            vec!["?"; languages.len()].join(",")
        ));
        params.extend(languages.iter().map(|l| Value::Text(l.as_str().to_owned())));
    }
}
