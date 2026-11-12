//! `codegraphV implementations <symbol>` (upstream 08b187aa, #2833): the types that implement or
//! extend a trait, interface or class — the first question when a flow crosses a trait boundary,
//! and one `impact` can't answer, since it mixes implementors with dependents and tests. Renders
//! the shared type-hierarchy derivation explore reads, not a walk of its own.
use codegraph_core::CodeGraph;
use codegraph_graph::symbol_lookup::{describe_symbol_node, group_definitions};
use codegraph_graph::test_code::is_test_code;
use codegraph_graph::type_hierarchy::{build_type_hierarchy, can_have_hierarchy, MAX_DESCENDANTS, MAX_DESCENDANT_DEPTH};
use codegraph_types::{Node, SearchOptions};
use serde_json::{json, Value};
use std::collections::HashMap;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct Request<'a> {
    pub symbol: &'a str,
    pub file: Option<&'a str>,
    pub depth: u32,
    pub json: bool,
}

fn definition(node: &Node) -> Value {
    json!({"id":node.id,"name":node.name,"kind":node.kind,"filePath":node.file_path,"startLine":node.start_line,"qualifiedName":node.qualified_name,"language":node.language})
}

pub fn run(cg: &CodeGraph, request: &Request<'_>) -> Result<String> {
    let symbol = request.symbol;
    let depth = request.depth.clamp(1, MAX_DESCENDANT_DEPTH);
    let found = codegraph_mcp::render::lookup(cg, symbol)?;
    if found.is_empty() {
        let options = SearchOptions { limit: Some(5), ..Default::default() };
        let similar: Vec<String> = cg.store().search_nodes(symbol, &options)?.into_iter().map(|m| m.node.name).collect();
        let hint = if similar.is_empty() { String::new() } else { format!(" Did you mean: {}?", similar.join(", ")) };
        return Ok(format!("Symbol \"{symbol}\" not found in the index.{hint}"));
    }
    let targets: Vec<Node> = found.iter().filter(|n| can_have_hierarchy(n)).cloned().collect();
    if targets.is_empty() {
        return Ok(format!("\"{symbol}\" is {} — not a type something can implement or extend.", describe_symbol_node(&found[0])));
    }
    let grouped = group_definitions(&targets, request.file);
    let ambiguous = grouped.groups.len() > 1;
    let note = grouped.filtered_out.then(|| {
        format!("no definition of \"{symbol}\" matches file \"{}\" — showing all definitions instead.", request.file.unwrap_or_default())
    });
    // A Rust implementor is test code when it sits in a `#[cfg(test)]` module, which only its file
    // shows; read each file once, inside the project.
    let mut sources: HashMap<String, Option<String>> = HashMap::new();
    let mut collected = Vec::new();
    for group in &grouped.groups {
        let focus = &group[0];
        let hierarchy = build_type_hierarchy(cg.store(), focus, false)?;
        let entries: Vec<_> = hierarchy.as_ref().map(|h| h.descendants.iter().filter(|e| e.depth <= depth).collect()).unwrap_or_default();
        let mut name_of: HashMap<&str, &str> = HashMap::from([(focus.id.as_str(), focus.name.as_str())]);
        for e in &entries {
            name_of.insert(&e.node.id, &e.node.name);
        }
        let rows: Vec<Value> = entries
            .iter()
            .map(|e| {
                let source = if e.node.language.as_str() == "rust" {
                    sources
                        .entry(e.node.file_path.clone())
                        .or_insert_with(|| {
                            codegraph_types::paths::validate_path_within_root(cg.project_root(), &e.node.file_path, Default::default())
                                .and_then(|abs| std::fs::read_to_string(abs).ok())
                        })
                        .clone()
                } else {
                    None
                };
                let mut row = json!({"name":e.node.name,"kind":e.node.kind,"filePath":e.node.file_path,"startLine":e.node.start_line,"qualifiedName":e.node.qualified_name,"language":e.node.language,"relation":e.relation,"depth":e.depth,"test":is_test_code(&e.node, source.as_deref()),"synthesized":e.synthesized});
                if e.depth > 1 {
                    if let Some(via) = name_of.get(e.parent_id.as_str()) {
                        row["via"] = json!(via);
                    }
                }
                row
            })
            .collect();
        let direct = hierarchy.as_ref().map_or(0, |h| h.direct_subtypes);
        let bounded = hierarchy.as_ref().is_some_and(|h| h.bounded);
        collected.push((group, rows, direct, bounded));
    }

    if request.json {
        let mut out = json!({
            "symbol": symbol,
            "depth": depth,
            "targets": grouped.groups.iter().flatten().map(definition).collect::<Vec<_>>(),
            "ambiguous": ambiguous,
            "filteredOut": grouped.filtered_out,
            "definitions": collected.iter().map(|(group, rows, direct, bounded)| json!({
                "definition": definition(&group[0]),
                "directSubtypes": direct,
                "bounded": bounded,
                "implementations": rows,
            })).collect::<Vec<_>>(),
        });
        if let Some(file) = request.file {
            out["file"] = json!(file);
        }
        if let Some(note) = &note {
            out["note"] = json!(note);
        }
        return Ok(serde_json::to_string_pretty(&out)?);
    }

    let mut text = String::new();
    if let Some(note) = &note {
        text.push_str(&format!("Warning: {note}\n"));
    }
    for (group, rows, direct, bounded) in &collected {
        let focus = &group[0];
        let deeper = if depth > 1 { format!(", {} within {depth} levels", rows.len()) } else { String::new() };
        text.push_str(&format!("\nImplementations of {} — {direct} direct{deeper}:\n\n", describe_symbol_node(focus)));
        if rows.is_empty() {
            text.push_str("  (none in the index)\n");
        }
        for r in rows {
            let tags = [(r["test"] == true).then_some("[test]"), (r["synthesized"] == true).then_some("[synthesized]")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let indent = "  ".repeat(r["depth"].as_u64().unwrap_or(1) as usize - 1);
            let via = r["via"].as_str().map(|v| format!(" {v}")).unwrap_or_default();
            text.push_str(&format!(
                "  {indent}{:<10}{}  {}{via}  {}:{}{}\n",
                r["kind"].as_str().unwrap_or_default(),
                r["name"].as_str().unwrap_or_default(),
                r["relation"].as_str().unwrap_or_default(),
                r["filePath"].as_str().unwrap_or_default(),
                r["startLine"],
                if tags.is_empty() { String::new() } else { format!("  {tags}") }
            ));
        }
        if *bounded {
            text.push_str(&format!("\n  …the walk stops at {MAX_DESCENDANTS} subtypes or {MAX_DESCENDANT_DEPTH} levels; more exist.\n"));
        }
    }
    Ok(text)
}
