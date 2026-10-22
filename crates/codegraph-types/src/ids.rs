//! Node-ID generation — port of `generateNodeId` / `NodeIdAllocator` in
//! `src/extraction/tree-sitter-helpers.ts`, and of the file-node special case
//! in `TreeSitterExtractor.extract()`.
//!
//! Node identity is what lets every extractor, the store and the resolver agree
//! on the same graph; a drift here breaks every edge, so the vectors below are
//! pinned against the TypeScript output.
//!
//!   `${kind}:${sha256(`${filePath}:${kind}:${name}:${line}`).hex[0..32]}`
//!
//! `NodeIdAllocator` appends `:<UTF-16 column>` only for later, distinct source
//! positions that share the legacy ID within one extraction.

use crate::kinds::NodeKind;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// `kind:hash32` — the legacy node ID for a symbol at `line`.
pub fn node_id(file_path: &str, kind: NodeKind, name: &str, line: u32) -> String {
    node_id_str(file_path, kind.as_str(), name, line)
}

/// [`node_id`] with the kind spelled as a string, for callers (extractors that
/// compute kinds dynamically) that haven't parsed it into a [`NodeKind`].
pub fn node_id_str(file_path: &str, kind: &str, name: &str, line: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(file_path.as_bytes());
    hasher.update(b":");
    hasher.update(kind.as_bytes());
    hasher.update(b":");
    hasher.update(name.as_bytes());
    hasher.update(b":");
    hasher.update(line.to_string().as_bytes());
    let digest = hasher.finalize();

    // 32 hex chars = the first 16 bytes of the digest.
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(kind.len() + 1 + 32);
    id.push_str(kind);
    id.push(':');
    for b in &digest[..16] {
        id.push(HEX[(b >> 4) as usize] as char);
        id.push(HEX[(b & 0x0f) as usize] as char);
    }
    id
}

/// The file node's ID: `file:<path>`.
pub fn file_node_id(file_path: &str) -> String {
    format!("file:{file_path}")
}

/// Per-extraction collision handling: the first position keeps the legacy ID,
/// a later, distinct position that hashes to the same ID gets `:<column>`.
#[derive(Debug, Default)]
pub struct NodeIdAllocator {
    first_columns: HashMap<String, u32>,
}

impl NodeIdAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// `column` is the zero-based UTF-16 column, never tree-sitter's byte column.
    pub fn generate(
        &mut self,
        file_path: &str,
        kind: NodeKind,
        name: &str,
        line: u32,
        column: u32,
    ) -> String {
        self.generate_str(file_path, kind.as_str(), name, line, column)
    }

    pub fn generate_str(
        &mut self,
        file_path: &str,
        kind: &str,
        name: &str,
        line: u32,
        column: u32,
    ) -> String {
        let id = node_id_str(file_path, kind, name, line);
        let first_column = *self.first_columns.entry(id.clone()).or_insert(column);
        if first_column == column {
            id
        } else {
            format!("{id}:{column}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_typescript_output() {
        // node -e "crypto.createHash('sha256').update('src/a.ts:function:foo:3').digest('hex').substring(0,32)"
        assert_eq!(
            node_id("src/a.ts", NodeKind::Function, "foo", 3),
            "function:bfb15544fed707794274a5c61006ea7b"
        );
    }

    #[test]
    fn file_node_ids_are_path_based() {
        assert_eq!(file_node_id("src/a.ts"), "file:src/a.ts");
    }

    #[test]
    fn collision_only_identity_vectors() {
        let mut ids = NodeIdAllocator::new();
        let base = "function:bfb15544fed707794274a5c61006ea7b";
        for (column, expected) in [
            (2, base.to_string()),
            (24, format!("{base}:24")),
            (48, format!("{base}:48")),
            (24, format!("{base}:24")),
            (2, base.to_string()),
        ] {
            assert_eq!(ids.generate("src/a.ts", NodeKind::Function, "foo", 3, column), expected);
        }
        // A different file is a different identity entirely.
        assert_eq!(
            ids.generate("src/b.ts", NodeKind::Function, "foo", 3, 24),
            node_id("src/b.ts", NodeKind::Function, "foo", 3)
        );
        assert_eq!(
            NodeIdAllocator::new().generate("src/a.ts", NodeKind::Function, "foo", 3, 24),
            base
        );
    }
}
