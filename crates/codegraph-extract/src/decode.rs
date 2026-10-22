//! Decode a walker's flat tables into an [`ExtractionResult`] — the in-process
//! counterpart of `src/extraction/kernel/decode.ts`. See `buffers.rs` for the
//! byte layout the walkers write.

use crate::buffers::{
    EmitOut, EDGE_KINDS, EDGE_ROW_SIZE, FUNCTION_REF_CODE, KERNEL_ABI_VERSION, META_SIZE, NODE_KINDS, NODE_ROW_SIZE,
    NONE, REF_FLAG_FILE_PATH, REF_ROW_SIZE,
};
use codegraph_types::{
    Edge, EdgeKind, ExtractionError, ExtractionResult, Language, Node, NodeKind, Provenance, ReferenceKind,
    UnresolvedReference, Visibility,
};
use serde_json::Value;

const VISIBILITIES: [Option<Visibility>; 5] =
    [None, Some(Visibility::Public), Some(Visibility::Private), Some(Visibility::Protected), Some(Visibility::Internal)];
const PROVENANCES: [Option<Provenance>; 4] =
    [None, Some(Provenance::TreeSitter), Some(Provenance::Scip), Some(Provenance::Heuristic)];

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// An (offset, len) arena string; `None` when the field is absent.
fn str_at(arena: &[u8], row: &[u8], at: usize) -> Result<Option<String>, String> {
    let off = u32_at(row, at);
    if off == NONE {
        return Ok(None);
    }
    let len = u32_at(row, at + 4) as usize;
    let off = off as usize;
    let bytes = arena.get(off..off + len).ok_or_else(|| format!("arena string out of range ({off}+{len})"))?;
    // The walkers write valid UTF-8; a lossy read keeps a malformed byte from
    // taking the file down (Buffer#toString('utf8') replaces it the same way).
    Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
}

/// NUL-joined list field; `None` when absent.
fn list_at(arena: &[u8], row: &[u8], at: usize) -> Result<Option<Vec<String>>, String> {
    Ok(str_at(arena, row, at)?.map(|joined| joined.split('\0').map(str::to_string).collect()))
}

/// Tri-state boolean from a (present, value) bit pair.
fn flag(flags: u16, pair: u16) -> Option<bool> {
    if flags & (1 << (pair * 2)) == 0 {
        return None;
    }
    Some(flags & (1 << (pair * 2 + 1)) != 0)
}

fn opt_u32(row: &[u8], at: usize) -> Option<u32> {
    let v = u32_at(row, at);
    (v != NONE).then_some(v)
}

/// Decode one file's tables.
pub fn decode_tables(out: &EmitOut, file_path: &str, language: Language) -> Result<ExtractionResult, String> {
    let meta = &out.meta;
    if meta.len() < META_SIZE {
        return Err(format!("kernel meta too short: {}", meta.len()));
    }
    if meta[0] != KERNEL_ABI_VERSION {
        return Err(format!("kernel buffer ABI {} != expected {KERNEL_ABI_VERSION}", meta[0]));
    }
    let node_count = u32_at(meta, 4) as usize;
    let edge_count = u32_at(meta, 8) as usize;
    let ref_count = u32_at(meta, 12) as usize;
    let arena = &out.arena[..];
    let now = codegraph_now_ms();

    let mut nodes: Vec<Node> = Vec::with_capacity(node_count);
    let mut id_by_row: Vec<String> = Vec::with_capacity(node_count);
    for i in 0..node_count {
        let row = out.nodes.get(i * NODE_ROW_SIZE..(i + 1) * NODE_ROW_SIZE).ok_or("node table truncated")?;
        let id = str_at(arena, row, 36)?.ok_or("node without id")?;
        let flags = u16::from_le_bytes([row[2], row[3]]);
        let kind_idx = row[0] as usize;
        let kind = NODE_KINDS
            .get(kind_idx)
            .and_then(|k| NodeKind::parse(k))
            .ok_or_else(|| format!("bad node kind index {kind_idx}"))?;
        let mut node = Node::new(
            id.clone(),
            kind,
            str_at(arena, row, 20)?.ok_or("node without name")?,
            str_at(arena, row, 28)?.ok_or("node without qualified name")?,
            file_path,
            language,
            u32_at(row, 4),
            u32_at(row, 8),
            u32_at(row, 12),
            u32_at(row, 16),
            now,
        );
        node.docstring = str_at(arena, row, 44)?;
        node.signature = str_at(arena, row, 52)?;
        node.visibility = VISIBILITIES.get(row[1] as usize).copied().flatten();
        node.is_exported = flag(flags, 0);
        node.is_async = flag(flags, 1);
        node.is_static = flag(flags, 2);
        node.is_abstract = flag(flags, 3);
        node.decorators = list_at(arena, row, 60)?;
        node.type_parameters = list_at(arena, row, 68)?;
        node.return_type = str_at(arena, row, 76)?;
        if let Some(extra) = str_at(arena, row, 84)? {
            // Escape hatch: extra Node props as JSON, assigned over the decoded node.
            let extra: Value = serde_json::from_str(&extra).map_err(|e| format!("bad extraJson: {e}"))?;
            let mut base = serde_json::to_value(&node).map_err(|e| e.to_string())?;
            if let (Value::Object(base), Value::Object(extra)) = (&mut base, extra) {
                for (k, v) in extra {
                    base.insert(k, v);
                }
            }
            node = serde_json::from_value(base).map_err(|e| format!("bad extraJson node: {e}"))?;
        }
        id_by_row.push(id);
        nodes.push(node);
    }

    let mut edges: Vec<Edge> = Vec::with_capacity(edge_count);
    for i in 0..edge_count {
        let row = out.edges.get(i * EDGE_ROW_SIZE..(i + 1) * EDGE_ROW_SIZE).ok_or("edge table truncated")?;
        let endpoint = |idx_at: usize, str_field: usize| -> Result<String, String> {
            let idx = u32_at(row, idx_at);
            if idx == NONE {
                str_at(arena, row, str_field)?.ok_or_else(|| "edge endpoint without id".to_string())
            } else {
                id_by_row.get(idx as usize).cloned().ok_or_else(|| format!("edge endpoint row {idx} out of range"))
            }
        };
        let kind_idx = row[8] as usize;
        let kind = EDGE_KINDS
            .get(kind_idx)
            .and_then(|k| EdgeKind::parse(k))
            .ok_or_else(|| format!("bad edge kind index {kind_idx}"))?;
        // Row indices at 0 (source) and 4 (target); the string fallbacks sit at
        // 28 (sourceIdStr) and 36 (targetIdStr).
        let mut edge = Edge::new(endpoint(0, 28)?, endpoint(4, 36)?, kind);
        edge.line = opt_u32(row, 12);
        edge.column = opt_u32(row, 16);
        edge.provenance = PROVENANCES.get(row[9] as usize).copied().flatten();
        if let Some(meta_json) = str_at(arena, row, 20)? {
            match serde_json::from_str::<Value>(&meta_json) {
                Ok(Value::Object(m)) => edge.metadata = Some(m),
                Ok(_) => {}
                Err(e) => return Err(format!("bad edge metadata: {e}")),
            }
        }
        edges.push(edge);
    }

    let mut refs: Vec<UnresolvedReference> = Vec::with_capacity(ref_count);
    for i in 0..ref_count {
        let row = out.refs.get(i * REF_ROW_SIZE..(i + 1) * REF_ROW_SIZE).ok_or("ref table truncated")?;
        let from_idx = u32_at(row, 0);
        let from_node_id = if from_idx == NONE {
            str_at(arena, row, 32)?.ok_or("ref without source id")?
        } else {
            id_by_row.get(from_idx as usize).cloned().ok_or_else(|| format!("ref source row {from_idx} out of range"))?
        };
        let kind_byte = row[4];
        let reference_kind = if kind_byte == FUNCTION_REF_CODE {
            ReferenceKind::FunctionRef
        } else {
            let k = EDGE_KINDS.get(kind_byte as usize).and_then(|k| EdgeKind::parse(k));
            ReferenceKind::Edge(k.ok_or_else(|| format!("bad reference kind {kind_byte}"))?)
        };
        // Ordinary refs carry no filePath/language: the store fills `ref.filePath ??
        // filePath`. The one exception is flagged (ruby/php mixin/trait `implements`).
        refs.push(UnresolvedReference {
            from_node_id,
            reference_name: str_at(arena, row, 16)?.ok_or("ref without name")?,
            reference_kind,
            line: u32_at(row, 8),
            column: u32_at(row, 12),
            file_path: (row[5] & REF_FLAG_FILE_PATH != 0).then(|| file_path.to_string()),
            language: None,
            candidates: list_at(arena, row, 24)?,
            row_id: None,
        });
    }

    let errors_off = u32_at(meta, 20);
    let errors: Vec<ExtractionError> = if errors_off == NONE {
        Vec::new()
    } else {
        let len = u32_at(meta, 24) as usize;
        let off = errors_off as usize;
        let bytes = arena.get(off..off + len).ok_or("errors JSON out of range")?;
        serde_json::from_slice(bytes).map_err(|e| format!("bad errors JSON: {e}"))?
    };

    Ok(ExtractionResult { nodes, edges, unresolved_references: refs, errors, duration_ms: 0.0 })
}

pub(crate) fn codegraph_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
