//! The shared `sfc-script.ts` fold. Component-local values run as the
//! component; nested symbols keep their own containment and call ownership.
use codegraph_types::{
    nodepath, Edge, EdgeKind, ExtractionResult, Language, Node, NodeKind, ReferenceKind,
};
use std::collections::HashSet;

pub struct ScriptFold<'a> {
    pub file_path: &'a str,
    pub component_node_id: &'a str,
    pub line_offset: u32,
    pub language: Language,
    pub per_instance: bool,
}

pub fn sfc_file_node(file_path: &str, source: &str, language: Language) -> Node {
    let mut node = Node::new(
        format!("file:{file_path}"),
        NodeKind::File,
        nodepath::basename(file_path),
        file_path,
        file_path,
        language,
        1,
        source.split('\n').count() as u32,
        0,
        0,
        super::now_ms(),
    );
    node.is_exported = Some(false);
    node
}

pub fn fold_script_result(
    mut result: ExtractionResult,
    fold: &ScriptFold<'_>,
    sink: &mut ExtractionResult,
) {
    let block_file = format!("file:{}", fold.file_path);
    let parented: HashSet<_> = result
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Contains && e.source != block_file)
        .map(|e| e.target.clone())
        .collect();
    let instance_values: HashSet<_> = if fold.per_instance {
        result
            .nodes
            .iter()
            .filter(|n| {
                matches!(n.kind, NodeKind::Constant | NodeKind::Variable)
                    && !parented.contains(&n.id)
            })
            .map(|n| n.id.clone())
            .collect()
    } else {
        HashSet::new()
    };
    let runs_as_component = |id: &str| id == block_file || instance_values.contains(id);
    for mut node in result.nodes {
        if node.kind == NodeKind::File {
            continue;
        }
        node.start_line += fold.line_offset;
        node.end_line += fold.line_offset;
        node.language = fold.language;
        if !parented.contains(&node.id) {
            sink.edges.push(Edge::new(
                fold.component_node_id,
                &node.id,
                EdgeKind::Contains,
            ));
        }
        sink.nodes.push(node);
    }
    for mut edge in result.edges {
        if edge.kind == EdgeKind::Contains && edge.source == block_file {
            continue;
        }
        if let Some(line) = edge.line.as_mut().filter(|line| **line != 0) {
            *line += fold.line_offset;
        }
        if fold.per_instance
            && runs_as_component(&edge.source)
            && !matches!(edge.kind, EdgeKind::Imports | EdgeKind::Contains)
        {
            edge.source = fold.component_node_id.to_owned();
        }
        sink.edges.push(edge);
    }
    for r in &mut result.unresolved_references {
        r.line += fold.line_offset;
        r.file_path = Some(fold.file_path.to_owned());
        r.language = Some(fold.language);
        if fold.per_instance
            && runs_as_component(&r.from_node_id)
            && matches!(
                r.reference_kind,
                ReferenceKind::FunctionRef
                    | ReferenceKind::Edge(
                        EdgeKind::Calls | EdgeKind::References | EdgeKind::Instantiates
                    )
            )
        {
            r.from_node_id = fold.component_node_id.to_owned();
        }
    }
    sink.unresolved_references
        .extend(result.unresolved_references);
    for mut error in result.errors {
        if let Some(line) = error.line.as_mut().filter(|line| **line != 0) {
            *line += fold.line_offset;
        }
        sink.errors.push(error);
    }
}
