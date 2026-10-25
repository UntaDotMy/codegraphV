//! Text Delphi forms: object nesting and event handler bindings.
use super::Document;
use crate::source::now_ms;
use codegraph_jsre::re;
use codegraph_types::{
    jscompat::{is_js_space, utf16_len},
    node_id, Edge, EdgeKind, ExtractionResult, Language, Node, NodeKind,
};

pub fn extract(file: &str, source: &str) -> ExtractionResult {
    let mut doc = Document::new(file, source, Language::Pascal);
    doc.file_node();
    let mut stack = vec![0usize];
    let mut multi_line_end = None;
    for (row, line) in source.split('\n').enumerate() {
        let line_num = row as u32 + 1;
        if let Some(end) = multi_line_end {
            if line.trim_end_matches(is_js_space).ends_with(end) {
                multi_line_end = None;
            }
            continue;
        }
        if re!(r"=\s*\(\s*$").is_match(line) {
            multi_line_end = Some(')');
            continue;
        }
        if re!(r"=\s*<\s*$").is_match(line) {
            multi_line_end = Some('>');
            continue;
        }
        if let Some(c) = re!(r"^\s*(object|inherited|inline)\s+(\w+)\s*:\s*(\w+)").captures(line) {
            let name = c.text(2).unwrap();
            let id = node_id(file, NodeKind::Component, name, line_num);
            let mut node = Node::new(
                &id,
                NodeKind::Component,
                name,
                format!("{file}#{name}"),
                file,
                Language::Pascal,
                line_num,
                line_num,
                0,
                utf16_len(line) as u32,
                now_ms(),
            );
            node.signature = c.text(3).map(str::to_owned);
            doc.out.edges.push(Edge::new(
                &doc.out.nodes[*stack.last().unwrap()].id,
                &id,
                EdgeKind::Contains,
            ));
            stack.push(doc.out.nodes.len());
            doc.out.nodes.push(node);
            continue;
        }
        if let Some(c) = re!(r"^\s*(On\w+)\s*=\s*(\w+)\s*$").captures(line) {
            let owner = doc.out.nodes[*stack.last().unwrap()].id.clone();
            doc.reference(&owner, c.text(2).unwrap(), line_num, 0, false);
            continue;
        }
        if re!(r"^\s*end\s*$").is_match(line) && stack.len() > 1 {
            let node = &mut doc.out.nodes[stack.pop().unwrap()];
            node.end_line = line_num;
            node.end_column = utf16_len(line) as u32;
        }
    }
    doc.finish()
}
