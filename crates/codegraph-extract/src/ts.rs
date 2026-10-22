//! The tree-sitter surface the walkers use, with byte offsets that always index
//! the UTF-8 source.
//!
//! The TypeScript engine parses an erroring file with web-tree-sitter, which reads
//! UTF-16. Error *recovery* weighs skipped text in bytes against fixed per-recovery
//! costs, so the same grammar resolves an erroring file differently over UTF-8 than
//! over UTF-16 (proven on Qt: a class vanishes from the UTF-8 tree and comes back in
//! the UTF-16 one). A clean file parses identically either way. So [`parse`] reads
//! UTF-8 first and, only when that tree has errors, parses again as UTF-16 — the tree
//! the wasm engine would have walked — and [`Tree`] translates that tree's offsets
//! (two bytes per unit) back to the UTF-8 source the walkers slice.
//!
//! [`Node`] wraps `tree_sitter::Node` for that translation. It carries the tree's
//! [`Offsets`] so there is no per-thread state: two trees can be alive at once.

use std::ops::Range;

pub use tree_sitter::{Language, Point, TreeCursor};

/// How a tree's raw byte offsets map to offsets in the UTF-8 source.
pub enum Offsets {
    /// Parsed from the UTF-8 source: raw offsets are source offsets.
    Utf8,
    /// Parsed from the source's UTF-16 units, an ASCII source: unit `i` is byte `i`.
    Utf16Ascii,
    /// Parsed from UTF-16, a source with non-ASCII text.
    Utf16Map {
        /// Source byte offset of each UTF-16 unit boundary (`units + 1` entries; the
        /// second unit of a surrogate pair maps to the end of its character).
        to_byte: Vec<u32>,
        /// Source byte offset of the start of each line.
        line_starts: Vec<u32>,
    },
}

impl Offsets {
    #[inline]
    fn byte(&self, raw: usize) -> usize {
        match self {
            Offsets::Utf8 => raw,
            Offsets::Utf16Ascii => raw >> 1,
            Offsets::Utf16Map { to_byte, .. } => to_byte[raw >> 1] as usize,
        }
    }

    fn point(&self, raw_byte: usize, raw: Point) -> Point {
        match self {
            Offsets::Utf8 => raw,
            Offsets::Utf16Ascii => Point { row: raw.row, column: raw.column >> 1 },
            Offsets::Utf16Map { to_byte, line_starts } => Point {
                row: raw.row,
                column: to_byte[raw_byte >> 1] as usize - line_starts[raw.row] as usize,
            },
        }
    }

    fn for_utf16(source: &str) -> Offsets {
        if source.is_ascii() {
            return Offsets::Utf16Ascii;
        }
        let mut to_byte: Vec<u32> = Vec::with_capacity(source.len() + 1);
        for (at, c) in source.char_indices() {
            to_byte.push(at as u32);
            if c.len_utf16() == 2 {
                to_byte.push((at + c.len_utf8()) as u32);
            }
        }
        to_byte.push(source.len() as u32);
        let mut line_starts: Vec<u32> = vec![0];
        line_starts.extend(source.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i as u32 + 1));
        Offsets::Utf16Map { to_byte, line_starts }
    }
}

/// A parse tree plus the translation from its offsets to the source's.
pub struct Tree {
    inner: tree_sitter::Tree,
    offsets: Offsets,
}

impl Tree {
    pub fn root_node(&self) -> Node<'_> {
        Node { inner: self.inner.root_node(), off: &self.offsets }
    }
}

/// Parse `source` the way the wasm engine would see it. An erroring tree is refused
/// with a `defer:` message when `tolerate_errors` is off (the differential tests
/// compare clean files only); otherwise it is parsed again as UTF-16. The tree that
/// is returned is the one the walker uses, and `stack::note_tree_error` is told
/// whether *it* has errors.
pub fn parse(grammar: &Language, language: &str, source: &str) -> Result<Tree, String> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(grammar).map_err(|e| format!("set_language({language}) failed: {e}"))?;
    let utf8 = parser.parse(source, None).ok_or_else(|| "parser returned null tree".to_string())?;
    if !utf8.root_node().has_error() {
        return Ok(Tree { inner: utf8, offsets: Offsets::Utf8 });
    }
    if !crate::stack::tolerate_errors() {
        crate::stack::note_tree_error();
        return Err("defer: parse tree contains errors — wasm recovery is canonical".to_string());
    }
    let units: Vec<u16> = source.encode_utf16().collect();
    parser.reset();
    let (inner, offsets) = match parser.parse_utf16_le(&units, None) {
        Some(utf16) => (utf16, Offsets::for_utf16(source)),
        None => (utf8, Offsets::Utf8),
    };
    if inner.root_node().has_error() {
        crate::stack::note_tree_error();
    }
    Ok(Tree { inner, offsets })
}

/// `tree_sitter::Node` with source-relative offsets.
#[derive(Clone, Copy)]
pub struct Node<'t> {
    inner: tree_sitter::Node<'t>,
    off: &'t Offsets,
}

impl<'t> PartialEq for Node<'t> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}
impl<'t> Eq for Node<'t> {}

impl<'t> std::fmt::Debug for Node<'t> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

impl<'t> Node<'t> {
    #[inline]
    fn wrap(&self, inner: tree_sitter::Node<'t>) -> Node<'t> {
        Node { inner, off: self.off }
    }

    #[inline]
    pub fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    #[inline]
    pub fn is_named(&self) -> bool {
        self.inner.is_named()
    }
    #[inline]
    pub fn has_error(&self) -> bool {
        self.inner.has_error()
    }
    #[inline]
    pub fn id(&self) -> usize {
        self.inner.id()
    }
    #[inline]
    pub fn child_count(&self) -> usize {
        self.inner.child_count()
    }
    #[inline]
    pub fn named_child_count(&self) -> usize {
        self.inner.named_child_count()
    }
    pub fn child(&self, i: usize) -> Option<Node<'t>> {
        self.inner.child(i).map(|n| self.wrap(n))
    }
    pub fn named_child(&self, i: usize) -> Option<Node<'t>> {
        self.inner.named_child(i).map(|n| self.wrap(n))
    }
    pub fn child_by_field_name(&self, name: impl AsRef<[u8]>) -> Option<Node<'t>> {
        self.inner.child_by_field_name(name).map(|n| self.wrap(n))
    }
    /// The field name of the `i`-th child, when the grammar labels it.
    pub fn field_name_for_child(&self, i: usize) -> Option<&'static str> {
        self.inner.field_name_for_child(i as u32)
    }
    pub fn is_named_child(&self, i: usize) -> bool {
        self.inner.child(i).is_some_and(|n| n.is_named())
    }
    pub fn parent(&self) -> Option<Node<'t>> {
        self.inner.parent().map(|n| self.wrap(n))
    }
    pub fn next_sibling(&self) -> Option<Node<'t>> {
        self.inner.next_sibling().map(|n| self.wrap(n))
    }
    pub fn prev_sibling(&self) -> Option<Node<'t>> {
        self.inner.prev_sibling().map(|n| self.wrap(n))
    }
    pub fn next_named_sibling(&self) -> Option<Node<'t>> {
        self.inner.next_named_sibling().map(|n| self.wrap(n))
    }
    pub fn prev_named_sibling(&self) -> Option<Node<'t>> {
        self.inner.prev_named_sibling().map(|n| self.wrap(n))
    }

    #[inline]
    pub fn start_byte(&self) -> usize {
        self.off.byte(self.inner.start_byte())
    }
    #[inline]
    pub fn end_byte(&self) -> usize {
        self.off.byte(self.inner.end_byte())
    }
    pub fn byte_range(&self) -> Range<usize> {
        self.start_byte()..self.end_byte()
    }
    pub fn start_position(&self) -> Point {
        self.off.point(self.inner.start_byte(), self.inner.start_position())
    }
    pub fn end_position(&self) -> Point {
        self.off.point(self.inner.end_byte(), self.inner.end_position())
    }

    pub fn walk(&self) -> TreeCursor<'t> {
        self.inner.walk()
    }
    pub fn children<'c>(&self, cursor: &'c mut TreeCursor<'t>) -> impl ExactSizeIterator<Item = Node<'t>> + 'c {
        let off = self.off;
        self.inner.children(cursor).map(move |inner| Node { inner, off })
    }
    pub fn named_children<'c>(
        &self,
        cursor: &'c mut TreeCursor<'t>,
    ) -> impl ExactSizeIterator<Item = Node<'t>> + 'c {
        let off = self.off;
        self.inner.named_children(cursor).map(move |inner| Node { inner, off })
    }
    pub fn children_by_field_name<'c>(
        &self,
        name: &str,
        cursor: &'c mut TreeCursor<'t>,
    ) -> impl Iterator<Item = Node<'t>> + 'c {
        let off = self.off;
        self.inner.children_by_field_name(name, cursor).map(move |inner| Node { inner, off })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grammar() -> Language {
        crate::langs::grammar_for("c").expect("c grammar")
    }

    fn function_text<'s>(tree: &Tree, src: &'s str) -> Vec<&'s str> {
        let root = tree.root_node();
        let mut cursor = root.walk();
        root.named_children(&mut cursor)
            .filter(|n| n.kind() == "function_definition")
            .map(|n| &src[n.byte_range()])
            .collect()
    }

    #[test]
    fn clean_files_are_read_as_utf8() {
        let src = "int f(void) { return 1; }\n";
        let tree = crate::stack::with_error_tolerance(true, || parse(&grammar(), "c", src)).unwrap();
        assert!(matches!(tree.offsets, Offsets::Utf8));
        assert_eq!(function_text(&tree, src), vec!["int f(void) { return 1; }"]);
    }

    #[test]
    fn erroring_files_are_reparsed_as_utf16_with_source_offsets() {
        // `é😀` before and inside the erroring region: unit offsets and byte offsets diverge.
        let src = "/* é😀 */ int ok(void) { return 1; }\nint @@@ ;\nint after(void) { /* é */ return 2; }\n";
        let tree = crate::stack::with_error_tolerance(true, || parse(&grammar(), "c", src)).unwrap();
        assert!(matches!(tree.offsets, Offsets::Utf16Map { .. }));
        for text in function_text(&tree, src) {
            assert!(text.starts_with("int ") && text.ends_with('}'), "{text:?}");
        }
        let root = tree.root_node();
        let end = root.end_position();
        assert_eq!(end.row, 3);
    }

    #[test]
    fn strict_mode_defers_erroring_files() {
        let err = crate::stack::with_error_tolerance(false, || parse(&grammar(), "c", "int @@@ ;")).err();
        assert!(err.unwrap().starts_with("defer:"));
    }
}
