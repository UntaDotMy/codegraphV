//! Queries only the reference resolver issues — the resolution half of `src/db/queries.ts`.

use crate::error::Result;
use crate::store::Store;
use codegraph_types::Language;

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
}
