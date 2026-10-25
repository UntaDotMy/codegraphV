//! Bounded source reads — port of `src/file-limits.ts`.

use std::io::Read;
use std::path::Path;

/// Largest source file CodeGraph will parse or read during resolution. Generated bundles,
/// minified sources, and dependency archives above this limit provide no useful symbols; 1 MB
/// covers essentially all hand-written source.
pub const MAX_SOURCE_FILE_SIZE_BYTES: u64 = 1024 * 1024;

/// What stands in for the content of a file over [`MAX_SOURCE_FILE_SIZE_BYTES`]. Such a file is
/// never parsed, so its bytes are never needed — reading them only to hash and discard cost
/// multi-GB RSS spikes on committed video/blob fixtures (#1910). The stamp is a function of size
/// alone: change detection compares it to the stored hash, so a same-size rewrite of an oversize
/// file is not a change (nothing about it is indexed), while crossing the limit in either
/// direction is.
pub fn oversize_stamp(size: u64) -> String {
    format!("codegraph:oversize:{size}")
}

/// The text whose hash the index stores for a file of `size` bytes: its content within the limit,
/// the size stamp over it. Anything that checks a file on disk against its stored `contentHash`
/// has to hash this, or every unchanged file over the limit reads as drifted. `content` is only
/// called within the limit.
pub fn indexed_hash_input(size: u64, content: impl FnOnce() -> String) -> String {
    if size > MAX_SOURCE_FILE_SIZE_BYTES {
        oversize_stamp(size)
    } else {
        content()
    }
}

/// A source file's stats, and its bytes when they are within the limit (`None` = oversize).
pub struct BoundedSource {
    pub metadata: std::fs::Metadata,
    /// The size to report: the larger of the stat and what the read saw (a file can grow
    /// between the two).
    pub size: u64,
    pub bytes: Option<Vec<u8>>,
}

const READ_CHUNK_BYTES: usize = 64 * 1024;

fn assert_regular_file(metadata: &std::fs::Metadata) -> std::io::Result<()> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(std::io::Error::other("Source path is not a regular file"))
    }
}

/// Read a source file without ever holding more than the limit plus one byte. A stat before the
/// read is not enough: the file can grow between the stat and the read (a log, a download, a
/// build output being written), so the descriptor is re-checked after opening and the read itself
/// stops one byte past the limit. Returns `bytes: None` for an oversize file.
pub fn read_bounded_source(file: &Path) -> std::io::Result<BoundedSource> {
    let initial = std::fs::metadata(file)?;
    assert_regular_file(&initial)?;
    if initial.len() > MAX_SOURCE_FILE_SIZE_BYTES {
        let size = initial.len();
        return Ok(BoundedSource { metadata: initial, size, bytes: None });
    }
    let mut handle = std::fs::File::open(file)?;
    let stats = handle.metadata()?;
    assert_regular_file(&stats)?;
    if stats.len() > MAX_SOURCE_FILE_SIZE_BYTES {
        let size = stats.len();
        return Ok(BoundedSource { metadata: stats, size, bytes: None });
    }
    let limit = MAX_SOURCE_FILE_SIZE_BYTES as usize;
    let mut bytes: Vec<u8> = Vec::with_capacity((stats.len() as usize).min(limit));
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    while bytes.len() <= limit {
        let want = READ_CHUNK_BYTES.min(limit + 1 - bytes.len());
        let n = handle.read(&mut chunk[..want])?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let after = handle.metadata()?;
    if bytes.len() > limit || after.len() > MAX_SOURCE_FILE_SIZE_BYTES {
        let size = (bytes.len() as u64).max(after.len());
        return Ok(BoundedSource { metadata: after, size, bytes: None });
    }
    let size = after.len();
    Ok(BoundedSource { metadata: after, size, bytes: Some(bytes) })
}
