//! Single-file components. Script ranges borrow the original source and reuse
//! the native TS/JS walkers. A single fold preserves component ownership and
//! original file positions for every component format.

mod script;
mod shell;
mod vue;
mod vue_options;
mod vue_template;

pub use script::{fold_script_result, sfc_file_node, ScriptFold};
pub use shell::extract_component;

/// A source index shared by every scan, avoiding repeated prefix/newline scans.
pub(super) struct Lines<'a> {
    pub source: &'a str,
    pub starts: Vec<usize>,
}

impl<'a> Lines<'a> {
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
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
