//! Grammar-free template extractors and Razor's delegated C# dependency pass.
//! The same native result types feed indexing; no JavaScript runtime is involved.
mod dfm;
mod liquid;
mod mybatis;
mod razor;

pub use dfm::extract as extract_dfm;
pub use liquid::extract as extract_liquid;
pub use mybatis::extract as extract_mybatis;
pub use razor::extract as extract_razor;

use crate::source::{now_ms, SourceIndex};
use codegraph_types::{
    jscompat::utf16_len, node_id, Edge, EdgeKind, ExtractionResult, Language, Node, NodeKind,
    UnresolvedReference,
};

struct Document<'a> {
    file: &'a str,
    lines: SourceIndex<'a>,
    language: Language,
    out: ExtractionResult,
    started: std::time::Instant,
}

impl<'a> Document<'a> {
    fn new(file: &'a str, source: &'a str, language: Language) -> Self {
        Self {
            file,
            lines: SourceIndex::new(source),
            language,
            out: ExtractionResult::default(),
            started: std::time::Instant::now(),
        }
    }
    fn file_node(&mut self) -> String {
        let id = node_id(self.file, NodeKind::File, self.file, 1);
        let name = self
            .file
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(self.file);
        self.out.nodes.push(Node::new(
            &id,
            NodeKind::File,
            name,
            self.file,
            self.file,
            self.language,
            1,
            self.lines.starts.len() as u32,
            0,
            utf16_len(self.lines.line(self.lines.starts.len() - 1)) as u32,
            now_ms(),
        ));
        id
    }
    fn contains(&mut self, parent: &str, node: Node) {
        self.out
            .edges
            .push(Edge::new(parent, &node.id, EdgeKind::Contains));
        self.out.nodes.push(node);
    }
    fn reference(
        &mut self,
        from: &str,
        name: impl Into<String>,
        line: u32,
        column: u32,
        with_context: bool,
    ) {
        self.out.unresolved_references.push(UnresolvedReference {
            from_node_id: from.to_owned(),
            reference_name: name.into(),
            reference_kind: EdgeKind::References.into(),
            line,
            column,
            file_path: with_context.then(|| self.file.to_owned()),
            language: with_context.then_some(self.language),
            candidates: None,
            row_id: None,
        });
    }
    fn finish(mut self) -> ExtractionResult {
        self.out.duration_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        self.out
    }
}
