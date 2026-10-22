//! Vue SFC ownership, Options API members and template-to-method edges.
use super::{fold_script_result, shell, vue_options, vue_template, Lines, ScriptFold};
use codegraph_jsre::re;
use codegraph_types::{
    jscompat::utf16_len, node_id, Edge, EdgeKind, ExtractionError, ExtractionResult, Language,
    Node, NodeKind, Severity, UnresolvedReference,
};
use std::collections::{HashMap, HashSet};

pub(super) fn extract(file: &str, source: &str) -> ExtractionResult {
    let started = std::time::Instant::now();
    let lines = Lines::new(source);
    let (id, mut out) = shell::component_result(file, source, Language::Vue, &lines);
    let owner = out.nodes[1].name.clone();
    let result = (|| -> Result<(), String> {
        for block in shell::script_blocks(source, &lines, Language::Vue) {
            let mut script = crate::extract_file(file, block.content, block.script_language)
                .map_err(|e| e.to_string())?;
            if !block.per_instance {
                add_options_members(file, &owner, &block, &mut script);
            }
            fold_script_result(
                script,
                &ScriptFold {
                    file_path: file,
                    component_node_id: &id,
                    line_offset: block.offset,
                    language: Language::Vue,
                    per_instance: block.per_instance,
                },
                &mut out,
            );
        }
        let covered = shell::covered_lines(&lines);
        for (row, covered) in covered.into_iter().enumerate() {
            if covered {
                continue;
            }
            for c in re!(r"<([A-Za-z][A-Za-z0-9_-]*)\b", "g").captures_iter(lines.line(row)) {
                let raw = c.text(1).unwrap();
                let name = if raw.as_bytes()[0].is_ascii_uppercase() {
                    raw.to_owned()
                } else if raw.contains('-') {
                    raw.split('-')
                        .map(|p| {
                            let mut chars = p.chars();
                            chars
                                .next()
                                .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                                .unwrap_or_default()
                        })
                        .collect()
                } else {
                    continue;
                };
                if matches!(
                    name.as_str(),
                    "Transition"
                        | "TransitionGroup"
                        | "KeepAlive"
                        | "Suspense"
                        | "Teleport"
                        | "Component"
                        | "Slot"
                ) {
                    continue;
                }
                out.unresolved_references.push(reference(
                    file,
                    &id,
                    name,
                    EdgeKind::References,
                    row,
                    c.get(0).unwrap().utf16_start() + 1,
                ));
            }
        }
        let methods: HashMap<_, _> = out
            .nodes
            .iter()
            .filter(|n| {
                n.kind == NodeKind::Method && n.qualified_name == format!("{owner}::{}", n.name)
            })
            .map(|n| (n.name.clone(), n.id.clone()))
            .collect();
        for call in vue_template::template_calls(source)? {
            let row = lines.row(call.offset);
            let column = utf16_len(&source[lines.starts[row]..call.offset]);
            if let Some(target) = methods.get(&call.name).filter(|_| !call.name.contains('.')) {
                let mut edge = Edge::new(&id, target, EdgeKind::Calls);
                edge.line = Some(row as u32 + 1);
                edge.column = Some(column as u32);
                out.edges.push(edge);
            } else {
                out.unresolved_references.push(reference(
                    file,
                    &id,
                    call.name,
                    EdgeKind::Calls,
                    row,
                    column,
                ));
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        out.errors.push(ExtractionError {
            message: format!("Vue extraction error: {error}"),
            severity: Severity::Error,
            code: None,
            file_path: None,
            line: None,
            column: None,
        });
    }
    out.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    out
}

fn reference(
    file: &str,
    id: &str,
    name: String,
    kind: EdgeKind,
    row: usize,
    column: usize,
) -> UnresolvedReference {
    UnresolvedReference {
        from_node_id: id.to_owned(),
        reference_name: name,
        reference_kind: kind.into(),
        line: row as u32 + 1,
        column: column as u32,
        file_path: Some(file.to_owned()),
        language: Some(Language::Vue),
        candidates: None,
        row_id: None,
    }
}

fn add_options_members(
    file: &str,
    owner: &str,
    block: &shell::ScriptBlock<'_>,
    out: &mut ExtractionResult,
) {
    let lines = Lines::new(block.content);
    let created: Vec<_> = vue_options::options_members(block.content)
        .into_iter()
        .map(|m| {
            let row = lines.row(m.start);
            let end_row = lines.row(m.end);
            Node::new(
                node_id(
                    file,
                    NodeKind::Method,
                    &format!("{owner}.{}", m.name),
                    row as u32 + 1 + block.offset,
                ),
                NodeKind::Method,
                &m.name,
                format!("{owner}::{}", m.name),
                file,
                Language::Vue,
                row as u32 + 1,
                end_row as u32 + 1,
                utf16_len(&block.content[lines.starts[row]..m.start]) as u32,
                utf16_len(&block.content[lines.starts[end_row]..m.end]) as u32,
                super::now_ms(),
            )
        })
        .collect();
    let member_at = |line: u32| {
        created
            .iter()
            .filter(|n| n.start_line <= line && n.end_line >= line)
            .max_by_key(|n| n.start_line)
    };
    let file_id = out
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::File)
        .map(|n| n.id.as_str());
    let narrower: HashSet<_> = out
        .nodes
        .iter()
        .filter(|n| n.kind != NodeKind::File)
        .map(|n| n.id.as_str())
        .collect();
    let file_level = |id: &str| {
        file_id
            .map(|f| f == id)
            .unwrap_or_else(|| !narrower.contains(id))
    };
    for reference in &mut out.unresolved_references {
        if file_level(&reference.from_node_id) {
            if let Some(n) = member_at(reference.line) {
                reference.from_node_id = n.id.clone();
            }
        }
    }
    for edge in &mut out.edges {
        if edge.kind == EdgeKind::Contains || !file_level(&edge.source) {
            continue;
        }
        if let Some(n) = edge.line.filter(|line| *line != 0).and_then(member_at) {
            edge.source = n.id.clone();
        }
    }
    out.nodes.extend(created);
}
