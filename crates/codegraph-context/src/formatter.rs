//! Output deliberately preserves the reference engine's ordering and whitespace.
use codegraph_search::generated::is_generated_file;
use codegraph_types::{
    jscompat::{js_trim, utf16_len},
    Edge, EdgeKind, Node, Subgraph, TaskContext,
};
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::HashSet;

pub fn format_context_as_markdown(context: &TaskContext) -> String {
    format_context_as_markdown_with_generated(context, &is_generated_file)
}

pub fn format_context_as_markdown_with_generated(
    context: &TaskContext,
    generated: &dyn Fn(&str) -> bool,
) -> String {
    let mut lines = vec![
        "## Code Context\n".into(),
        format!("**Query:** {}\n", context.query),
    ];
    let mut entries: Vec<_> = context.entry_points.iter().collect();
    entries.sort_by_key(|n| generated(&n.file_path));
    if !entries.is_empty() {
        lines.push("### Entry Points\n".into());
        for n in entries {
            lines.push(format!(
                "- **{}** ({}) - {}{}",
                n.name,
                n.kind,
                n.file_path,
                location(n)
            ));
            if let Some(s) = n.signature.as_ref().filter(|s| !s.is_empty()) {
                lines.push(format!("  `{s}`"));
            }
        }
        lines.push(String::new());
    }
    let mut by_file: IndexMap<&str, Vec<&Node>> = IndexMap::new();
    for n in context
        .subgraph
        .nodes
        .values()
        .filter(|n| !context.entry_points.iter().any(|e| e.id == n.id))
        .filter(|n| !generated(&n.file_path))
        .take(10)
    {
        by_file.entry(&n.file_path).or_default().push(n);
    }
    if !by_file.is_empty() {
        lines.push("### Related Symbols\n".into());
        for (file, nodes) in by_file {
            lines.push(format!(
                "- {file}: {}",
                nodes
                    .iter()
                    .map(|n| format!("{}:{}", n.name, n.start_line))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines.push(String::new());
    }
    if !context.code_blocks.is_empty() {
        let mut blocks: Vec<_> = context.code_blocks.iter().collect();
        blocks.sort_by_key(|b| generated(&b.file_path));
        lines.push("### Code\n".into());
        for b in blocks {
            let name = b.node.as_ref().map_or("Unknown", |n| &n.name);
            lines.push(format!("#### {name} ({}:{})\n", b.file_path, b.start_line));
            lines.push(format!("```{}", b.language));
            lines.push(b.content.clone());
            lines.push("```\n".into());
        }
    }
    lines.join("\n")
}

// The formatter has a narrower wire shape than the stored node. Optional values
// present as null in DB rows remain null; absent values stay omitted.
fn serialize_node(n: &Node) -> Value {
    let mut out = serde_json::Map::new();
    let fields = serde_json::to_value(n).expect("node serialization");
    for key in [
        "id",
        "kind",
        "name",
        "qualifiedName",
        "filePath",
        "language",
        "startLine",
        "endLine",
        "signature",
        "docstring",
        "visibility",
        "isExported",
        "isAsync",
        "isStatic",
    ] {
        if let Some(v) = fields.get(key) {
            out.insert(key.to_string(), v.clone());
        } else if key == "visibility" {
            out.insert(key.to_string(), Value::Null);
        }
    }
    Value::Object(out)
}

pub fn context_json(context: &TaskContext) -> Value {
    let blocks: Vec<Value> = context.code_blocks.iter().map(|b| {
        let mut v = serde_json::json!({"filePath": b.file_path, "startLine": b.start_line, "endLine": b.end_line,
            "language": b.language, "content": b.content});
        if let Some(n) = &b.node {
            v["nodeName"] = Value::String(n.name.clone());
            v["nodeKind"] = serde_json::to_value(n.kind).unwrap();
        }
        v
    }).collect();
    let edges: Vec<Value> = context
        .subgraph
        .edges
        .iter()
        .map(|e| {
            let mut v = serde_json::to_value(e).unwrap();
            v.as_object_mut().unwrap().retain(|k, _| {
                matches!(k.as_str(), "source" | "target" | "kind" | "line" | "column")
            });
            v["line"] = serde_json::to_value(e.line).unwrap();
            v["column"] = serde_json::to_value(e.column).unwrap();
            v
        })
        .collect();
    serde_json::json!({"query": context.query, "summary": context.summary,
        "entryPoints": context.entry_points.iter().map(serialize_node).collect::<Vec<_>>(),
        "nodes": context.subgraph.nodes.values().map(serialize_node).collect::<Vec<_>>(), "edges": edges,
        "codeBlocks": blocks, "relatedFiles": context.related_files, "stats": context.stats})
}

pub fn format_context_as_json(context: &TaskContext) -> String {
    serde_json::to_string_pretty(&context_json(context)).expect("context serialization")
}

pub fn format_subgraph_tree(graph: &Subgraph, entries: &[Node]) -> String {
    let mut lines = Vec::new();
    let mut printed = HashSet::new();
    let mut outgoing: IndexMap<&str, Vec<&Edge>> = IndexMap::new();
    for e in &graph.edges {
        outgoing.entry(&e.source).or_default().push(e);
    }
    for n in entries {
        node_tree(n, graph, &outgoing, &mut printed, &mut lines, 0, "");
        lines.push(String::new());
    }
    let remaining: Vec<_> = graph
        .nodes
        .values()
        .filter(|n| !printed.contains(&n.id))
        .collect();
    if !remaining.is_empty() && remaining.len() <= 10 {
        lines.push("Other relevant symbols:".into());
        for n in remaining {
            lines.push(format!(
                "  {}: {} ({}{})",
                n.kind,
                n.name,
                n.file_path,
                location(n)
            ));
        }
    } else if remaining.len() > 10 {
        lines.push(format!("... and {} more related symbols", remaining.len()));
    }
    js_trim(&lines.join("\n")).to_string()
}

fn location(n: &Node) -> String {
    if n.start_line == 0 {
        String::new()
    } else {
        format!(":{}", n.start_line)
    }
}

fn node_tree(
    n: &Node,
    graph: &Subgraph,
    outgoing: &IndexMap<&str, Vec<&Edge>>,
    printed: &mut HashSet<String>,
    lines: &mut Vec<String>,
    depth: usize,
    prefix: &str,
) {
    if !printed.insert(n.id.clone()) {
        return;
    }
    let sig = n
        .signature
        .as_ref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            format!(
                " - {}",
                if utf16_len(s) > 50 {
                    format!("{}...", slice_utf16(s, 47))
                } else {
                    s.clone()
                }
            )
        })
        .unwrap_or_default();
    lines.push(format!(
        "{prefix}{}: {} ({}{}){sig}",
        n.kind,
        n.name,
        n.file_path,
        location(n)
    ));
    let significant: Vec<_> = outgoing
        .get(n.id.as_str())
        .into_iter()
        .flatten()
        .copied()
        .filter(|e| {
            matches!(
                e.kind,
                EdgeKind::Calls
                    | EdgeKind::Extends
                    | EdgeKind::Implements
                    | EdgeKind::Imports
                    | EdgeKind::References
            )
        })
        .collect();
    let mut by_kind: IndexMap<EdgeKind, Vec<&Edge>> = IndexMap::new();
    for e in &significant {
        by_kind.entry(e.kind).or_default().push(e);
    }
    let next_prefix = format!("{prefix}  ");
    let name = |e: &Edge| graph.nodes.get(&e.target).map_or("unknown", |n| &n.name);
    for (kind, edges) in by_kind {
        if edges.len() > 3 {
            lines.push(format!(
                "{next_prefix}├── {kind}: {} and {} more",
                edges
                    .iter()
                    .take(3)
                    .map(|e| name(e))
                    .collect::<Vec<_>>()
                    .join(", "),
                edges.len() - 3
            ));
        } else {
            for (i, e) in edges.iter().enumerate() {
                lines.push(format!(
                    "{next_prefix}{} {kind} → {}",
                    if i == edges.len() - 1 {
                        "└──"
                    } else {
                        "├──"
                    },
                    name(e)
                ));
            }
        }
    }
    if depth < 1 {
        for e in significant.iter().take(3) {
            if let Some(target) = graph.nodes.get(&e.target) {
                node_tree(
                    target,
                    graph,
                    outgoing,
                    printed,
                    lines,
                    depth + 1,
                    &next_prefix,
                );
            }
        }
    }
}

/// JavaScript slicing counts UTF-16 code units. A boundary through a surrogate
/// becomes U+FFFD in Rust's valid UTF-8 string representation.
pub(crate) fn slice_utf16(s: &str, length: usize) -> String {
    String::from_utf16_lossy(&s.encode_utf16().take(length).collect::<Vec<_>>())
}

pub fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        format!(
            "{} bytes",
            if bytes == 0.0 {
                "0".into()
            } else {
                bytes.to_string()
            }
        )
    } else if bytes < 1024.0 * 1024.0 {
        format!("{} KB", fixed_one(bytes / 1024.0))
    } else {
        format!("{} MB", fixed_one(bytes / (1024.0 * 1024.0)))
    }
}

// ECMAScript toFixed picks the larger integer at a tie. Rust's formatter
// rounds ties to even. Round the binary rational itself so multiplying by ten
// cannot introduce another rounding (1.15 is below the decimal midpoint).
fn fixed_one(value: f64) -> String {
    if !value.is_finite() {
        return if value.is_nan() { "NaN" } else { "Infinity" }.into();
    }
    if value >= 1e21 {
        return format!("{value:e}").replace('e', "e+");
    }
    let bits = value.to_bits();
    let mantissa = ((bits & ((1u64 << 52) - 1)) | (1u64 << 52)) as u128;
    let exponent = ((bits >> 52) & 0x7ff) as i32 - 1074;
    let scaled = mantissa * 5;
    let integer = if exponent >= 0 {
        scaled << exponent
    } else {
        let shift = (-exponent) as u32;
        let quotient = scaled >> shift;
        let remainder = scaled & ((1u128 << shift) - 1);
        quotient + u128::from(remainder >= (1u128 << (shift - 1)))
    };
    format!("{}.{}", integer / 10, integer % 10)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_rounding_matches_javascript_at_decimal_midpoints() {
        assert_eq!(format_bytes(1280.0), "1.3 KB");
        assert_eq!(format_bytes(2355.2), "2.3 KB");
        assert_eq!(format_bytes(1177.6), "1.1 KB");
        assert_eq!(format_bytes(1310720.0), "1.3 MB");
        assert_eq!(format_bytes(-0.0), "0 bytes");
    }
}
