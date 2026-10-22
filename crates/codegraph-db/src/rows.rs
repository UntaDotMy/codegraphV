//! Row ↔ model conversions.
//!
//! Every query selects an EXPLICIT column list (the `*_COLS` constants) and
//! reads by position. `SELECT *` is not safe to read positionally: a database
//! migrated from before v5 carries `nodes.return_type` at the END of the table,
//! a fresh one in the middle.

use codegraph_types::{
    Edge, EdgeKind, FileRecord, Language, Metadata, Node, NodeKind, Provenance, ReferenceKind,
    UnresolvedReference, Visibility,
};
use rusqlite::types::Type;
use rusqlite::Row;

pub const NODE_COLS: &str = "id, kind, name, qualified_name, file_path, language, start_line, end_line, \
start_column, end_column, docstring, signature, visibility, is_exported, is_async, is_static, \
is_abstract, decorators, type_parameters, return_type, updated_at";

pub const EDGE_COLS: &str = "source, target, kind, metadata, line, col, provenance";

pub const FILE_COLS: &str = "path, content_hash, language, size, modified_at, indexed_at, node_count, errors, generated";

pub const REF_COLS: &str = "id, from_node_id, reference_name, reference_kind, line, col, candidates, file_path, language";

/// `safeJsonParse(text, undefined)`.
fn json<T: serde::de::DeserializeOwned>(text: Option<String>) -> Option<T> {
    text.filter(|t| !t.is_empty()).and_then(|t| serde_json::from_str(&t).ok())
}

fn bad_value(idx: usize, what: &str, got: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        Type::Text,
        format!("unknown {what} {got:?}").into(),
    )
}

pub fn node_from_row(row: &Row<'_>) -> rusqlite::Result<Node> {
    let kind: String = row.get(1)?;
    let language: String = row.get(5)?;
    let visibility: Option<String> = row.get(12)?;
    Ok(Node {
        id: row.get(0)?,
        kind: NodeKind::parse(&kind).ok_or_else(|| bad_value(1, "node kind", &kind))?,
        name: row.get(2)?,
        qualified_name: row.get(3)?,
        file_path: row.get(4)?,
        language: Language::parse(&language).ok_or_else(|| bad_value(5, "language", &language))?,
        start_line: row.get(6)?,
        end_line: row.get(7)?,
        start_column: row.get(8)?,
        end_column: row.get(9)?,
        docstring: row.get(10)?,
        signature: row.get(11)?,
        visibility: visibility.as_deref().and_then(Visibility::parse),
        is_exported: Some(row.get::<_, i64>(13)? == 1),
        is_async: Some(row.get::<_, i64>(14)? == 1),
        is_static: Some(row.get::<_, i64>(15)? == 1),
        is_abstract: Some(row.get::<_, i64>(16)? == 1),
        decorators: json(row.get(17)?),
        type_parameters: json(row.get(18)?),
        return_type: row.get(19)?,
        updated_at: row.get(20)?,
    })
}

pub fn edge_from_row(row: &Row<'_>) -> rusqlite::Result<Edge> {
    let kind: String = row.get(2)?;
    let provenance: Option<String> = row.get(6)?;
    Ok(Edge {
        source: row.get(0)?,
        target: row.get(1)?,
        kind: EdgeKind::parse(&kind).ok_or_else(|| bad_value(2, "edge kind", &kind))?,
        metadata: json::<Metadata>(row.get(3)?),
        line: row.get(4)?,
        column: row.get(5)?,
        provenance: provenance.as_deref().and_then(Provenance::parse),
    })
}

pub fn file_from_row(row: &Row<'_>) -> rusqlite::Result<FileRecord> {
    let language: String = row.get(2)?;
    Ok(FileRecord {
        path: row.get(0)?,
        content_hash: row.get(1)?,
        language: Language::parse(&language).ok_or_else(|| bad_value(2, "language", &language))?,
        size: row.get(3)?,
        modified_at: row.get(4)?,
        indexed_at: row.get(5)?,
        node_count: row.get(6)?,
        errors: json(row.get(7)?),
        generated: Some(row.get::<_, Option<i64>>(8)? == Some(1)),
    })
}

pub fn ref_from_row(row: &Row<'_>) -> rusqlite::Result<UnresolvedReference> {
    let kind: String = row.get(3)?;
    let language: String = row.get(8)?;
    Ok(UnresolvedReference {
        row_id: Some(row.get(0)?),
        from_node_id: row.get(1)?,
        reference_name: row.get(2)?,
        reference_kind: ReferenceKind::parse(&kind).ok_or_else(|| bad_value(3, "reference kind", &kind))?,
        line: row.get(4)?,
        column: row.get(5)?,
        candidates: json(row.get(6)?),
        file_path: Some(row.get(7)?),
        // An unknown language string reads back as `unknown` rather than failing the whole batch.
        language: Some(Language::parse(&language).unwrap_or(Language::Unknown)),
    })
}
