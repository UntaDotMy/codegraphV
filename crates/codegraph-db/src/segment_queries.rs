//! Bounded, indexed segment-vocabulary lookups for prose query seeds.
use crate::{Result, Store};
use codegraph_types::IndexMap;
use rusqlite::{params_from_iter, types::Value};

impl Store {
    pub fn get_segment_co_occurrence(
        &self,
        variants: &[(String, String)],
        min_words: usize,
        limit: usize,
    ) -> Result<Vec<(String, usize)>> {
        if variants.is_empty() {
            return Ok(Vec::new());
        }
        let whens = vec!["WHEN ? THEN ?"; variants.len()].join(" ");
        let placeholders = vec!["?"; variants.len()].join(", ");
        let sql = format!("SELECT name, COUNT(DISTINCT CASE segment {whens} END) AS matches FROM name_segment_vocab WHERE segment IN ({placeholders}) GROUP BY name HAVING matches >= ? ORDER BY matches DESC, length(name) ASC LIMIT ?");
        let mut params: Vec<Value> = variants
            .iter()
            .flat_map(|(s, w)| [Value::Text(s.clone()), Value::Text(w.clone())])
            .collect();
        params.extend(variants.iter().map(|(s, _)| Value::Text(s.clone())));
        params.extend([
            Value::Integer(min_words as i64),
            Value::Integer(limit as i64),
        ]);
        let mut stmt = self.conn().prepare_cached(&sql)?;
        let out = stmt
            .query_map(params_from_iter(params), |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? as usize))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub fn get_segment_name_counts(&self, segments: &[String]) -> Result<IndexMap<String, usize>> {
        if segments.is_empty() {
            return Ok(IndexMap::new());
        }
        let placeholders = vec!["?"; segments.len()].join(", ");
        let mut stmt = self.conn().prepare_cached(&format!("SELECT segment, COUNT(*) AS n FROM name_segment_vocab WHERE segment IN ({placeholders}) GROUP BY segment"))?;
        let out = stmt
            .query_map(params_from_iter(segments), |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? as usize))
            })?
            .collect::<std::result::Result<IndexMap<_, _>, _>>()?;
        Ok(out)
    }

    pub fn get_names_for_segment(&self, segment: &str, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare_cached("SELECT name FROM name_segment_vocab WHERE segment = ? ORDER BY length(name) ASC LIMIT ?")?;
        let out = stmt
            .query_map(rusqlite::params![segment, limit as i64], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }
}
