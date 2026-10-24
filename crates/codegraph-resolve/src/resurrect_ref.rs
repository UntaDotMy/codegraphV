//! Rebuild an unresolved reference from an edge dropped for staleness — port of
//! `src/resolution/resurrect-ref.ts`.

use crate::types::UnresolvedRef;
use codegraph_types::{Edge, Language, ReferenceKind};

/// `e.metadata.refName` is the name the edge was resolved from; an edge without one cannot be
/// retried.
pub fn resurrect_ref_from_dropped_edge(e: &Edge, source_file_path: &str, source_language: Language) -> Option<UnresolvedRef> {
    let meta = e.metadata.as_ref();
    let ref_name = meta.and_then(|m| m.get("refName")).and_then(|v| v.as_str()).filter(|s| !s.is_empty())?;
    let ref_kind = meta
        .and_then(|m| m.get("refKind"))
        .and_then(|v| v.as_str())
        .and_then(ReferenceKind::parse)
        .unwrap_or(ReferenceKind::Edge(e.kind));
    Some(UnresolvedRef::new(
        e.source.clone(),
        ref_name.to_string(),
        ref_kind,
        e.line.unwrap_or(0),
        e.column.unwrap_or(0),
        source_file_path.to_string(),
        source_language,
    ))
}
