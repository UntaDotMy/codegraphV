//! Small helpers the dynamic-edge synthesizers share — port of `src/resolution/synth-utils.ts`.

use crate::types::NodeRef;
use codegraph_types::NodeKind;
use std::cell::OnceCell;

/// The kinds a call site can be attributed to.
pub fn is_fn_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Function | NodeKind::Component)
}

/// Per-match line resolver over `src`, 1-based at `base_line`. Builds the newline index once —
/// lazily, since most sources never produce a match — then answers each call by binary search.
/// `idx` is a byte offset into `src`.
pub struct LineAt<'a> {
    src: &'a str,
    base_line: u32,
    nl: OnceCell<Vec<usize>>,
}

pub fn make_line_at(src: &str, base_line: u32) -> LineAt<'_> {
    LineAt { src, base_line, nl: OnceCell::new() }
}

impl LineAt<'_> {
    /// The line of byte offset `idx`.
    pub fn at(&self, idx: usize) -> u32 {
        let nl = self.nl.get_or_init(|| self.src.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i).collect());
        // Count newlines strictly before idx.
        let lo = nl.partition_point(|&p| p < idx);
        self.base_line + lo as u32
    }
}

/// Innermost function/method node whose line range contains `line`.
pub fn enclosing_fn(nodes_in_file: &[NodeRef], line: u32) -> Option<NodeRef> {
    let mut best: Option<&NodeRef> = None;
    for n in nodes_in_file {
        if !is_fn_kind(n.kind) {
            continue;
        }
        let end = n.end_line;
        if n.start_line <= line && end >= line && best.is_none_or(|b| n.start_line >= b.start_line) {
            best = Some(n); // prefer the tightest (latest-starting) encloser
        }
    }
    best.cloned()
}

/// The smallest `constant` / `variable` node whose lines contain `line` — the value a
/// top-level registration is written inside.
pub fn enclosing_value(nodes_in_file: &[NodeRef], line: u32) -> Option<NodeRef> {
    let mut best: Option<&NodeRef> = None;
    for n in nodes_in_file {
        if n.kind != NodeKind::Constant && n.kind != NodeKind::Variable {
            continue;
        }
        let end = n.end_line;
        if n.start_line <= line && end >= line && best.is_none_or(|b| n.start_line >= b.start_line) {
            best = Some(n);
        }
    }
    best.cloned()
}
