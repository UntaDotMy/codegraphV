//! Shared source coordinates: byte offsets for Rust scans, UTF-16 for graph IDs
//! and displayed positions. Lines borrow the source instead of allocating strings.
use codegraph_types::jscompat::utf16_len;

pub(crate) struct SourceIndex<'a> {
    pub source: &'a str,
    pub starts: Vec<usize>,
}

impl<'a> SourceIndex<'a> {
    pub fn new(source: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            source
                .bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { source, starts }
    }
    pub fn row(&self, byte: usize) -> usize {
        self.starts
            .partition_point(|p| *p <= byte)
            .saturating_sub(1)
    }
    pub fn line(&self, row: usize) -> &'a str {
        let start = self.starts[row];
        let end = self
            .starts
            .get(row + 1)
            .map(|p| p - 1)
            .unwrap_or(self.source.len());
        &self.source[start..end]
    }
    pub fn position(&self, byte: usize) -> (u32, u32) {
        let row = self.row(byte);
        (
            row as u32 + 1,
            utf16_len(&self.source[self.starts[row]..byte]) as u32,
        )
    }
}

/// Monotonic scans convert offsets in total O(source length), including when a
/// minified document puts thousands of statements on the same line.
pub(crate) struct Utf16Cursor<'a> {
    source: &'a str,
    byte: usize,
    units: usize,
}
impl<'a> Utf16Cursor<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            byte: 0,
            units: 0,
        }
    }
    pub fn at(&mut self, byte: usize) -> usize {
        debug_assert!(byte >= self.byte);
        self.units += utf16_len(&self.source[self.byte..byte]);
        self.byte = byte;
        self.units
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
