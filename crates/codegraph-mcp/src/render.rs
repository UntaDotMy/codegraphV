//! Retrieval presentation shared by the native CLI and MCP server.
use crate::tools::{number, required, ToolResult};
use codegraph_core::{
    utils::{validate_path_within_root, ValidateOptions, CONFIG_LEAF_LANGUAGES},
    CodeGraph,
};
use codegraph_graph::{
    go_package::GoPackageReader,
    named_symbol_flow::{resolve_named_symbol_flow, NamedSymbolFlowOptions},
    symbol_lookup::{group_definitions, lookup_symbol_nodes},
    synthesized_hop::describe_synthesized_hop,
};
use codegraph_types::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn lookup(cg: &CodeGraph, symbol: &str) -> Result<Vec<Node>> {
    let reader = GoPackageReader::new(cg.project_root());
    let go = |file: &str| reader.package_of(file);
    Ok(lookup_symbol_nodes(cg.store().as_ref(), symbol, Some(&go))?.nodes)
}
pub fn search(cg: &CodeGraph, args: &Value) -> Result<String> {
    let query = match required(args, "query") {
        Ok(s) => s,
        Err(e) => return Ok(e.message()),
    };
    let kinds = args["kind"]
        .as_str()
        .and_then(|s| NodeKind::parse(if s == "type" { "type_alias" } else { s }))
        .map(|k| vec![k]);
    let results = cg.search_nodes(
        query,
        &SearchOptions {
            limit: Some(number(args, "limit", 10, 1, 100)),
            kinds,
            ..Default::default()
        },
    )?;
    if results.is_empty() {
        return Ok(format!("No results found for \"{query}\""));
    }
    let mut out = format!("**Search Results ({} found)**\n\n", results.len());
    for r in results {
        out.push_str(&format!(
            "**{}** ({})\n{}:{}\n",
            r.node.name, r.node.kind, r.node.file_path, r.node.start_line
        ));
        if let Some(sig) = r.node.signature {
            out.push_str(&format!("`{sig}`\n"));
        }
        out.push('\n');
    }
    Ok(out)
}
fn location(n: &Node) -> String {
    format!("{} ({}) — {}:{}", n.name, n.kind, n.file_path, n.start_line)
}
fn hop(edge: &Edge) -> String {
    describe_synthesized_hop(Some(edge))
        .map(|h| format!(" — {}", h.summary))
        .unwrap_or_else(|| {
            if edge.kind == EdgeKind::Calls {
                String::new()
            } else if edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("fnRef"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                " — via callback registration".into()
            } else {
                format!(" — via {}", edge.kind)
            }
        })
}
pub fn relation_data(cg: &CodeGraph, args: &Value, callers: bool) -> Result<Value> {
    let symbol = args["symbol"].as_str().unwrap_or("");
    let matches = lookup(cg, symbol)?;
    let grouped = group_definitions(&matches, args["file"].as_str());
    let limit = number(args, "limit", 20, 1, 100);
    let mut union = IndexMap::<String, Node>::new();
    let mut edges = IndexMap::<String, Edge>::new();
    let mut definitions = vec![];
    for group in &grouped.groups {
        let mut refs = IndexMap::<String, Ref>::new();
        for n in group {
            for r in if callers {
                cg.get_callers(&n.id, 1)?
            } else {
                cg.get_callees(&n.id, 1)?
            } {
                union.entry(r.node.id.clone()).or_insert(r.node.clone());
                edges
                    .entry(format!(
                        "{}|{}|{}",
                        r.edge.source, r.edge.target, r.edge.kind
                    ))
                    .or_insert(r.edge.clone());
                refs.entry(r.node.id.clone()).or_insert(r);
            }
        }
        definitions.push(json!({"definition":group[0],"overloads":group,"references":refs.values().take(limit).collect::<Vec<_>>(),"total":refs.len(),"truncated":refs.len()>limit}));
    }
    let mut result = json!({"symbol":symbol,"targets":grouped.groups.iter().map(|g|&g[0]).collect::<Vec<_>>(),"ambiguous":grouped.groups.len()>1,"aggregation":if grouped.groups.len()>1{"union"}else{"definition"},"filteredOut":grouped.filtered_out,"definitions":definitions,"edges":edges.values().collect::<Vec<_>>(),"total":union.len(),"limit":limit,"truncated":union.len()>limit});
    result[if callers { "callers" } else { "callees" }] =
        json!(union.values().take(limit).collect::<Vec<_>>());
    Ok(result)
}
pub fn relations(cg: &CodeGraph, args: &Value, callers: bool) -> Result<String> {
    if let Err(e) = required(args, "symbol") {
        return Ok(e.message());
    }
    let data = relation_data(cg, args, callers)?;
    if data["targets"].as_array().is_none_or(|a| a.is_empty()) {
        return Ok(format!(
            "No symbol found: {}",
            args["symbol"].as_str().unwrap_or_default()
        ));
    }
    let direction = if callers { "Callers" } else { "Callees" };
    let mut out = format!(
        "**{direction} of {}**\n\n",
        args["symbol"].as_str().unwrap()
    );
    if data["filteredOut"] == true {
        out.push_str("> File hint matched no definition; showing all matches.\n\n");
    }
    for definition in data["definitions"].as_array().unwrap() {
        let target: Node = serde_json::from_value(definition["definition"].clone())?;
        out.push_str(&format!("**{}**\n", location(&target)));
        for r in definition["references"].as_array().unwrap() {
            let r: Ref = serde_json::from_value(r.clone())?;
            out.push_str(&format!("- {}{}\n", location(&r.node), hop(&r.edge)));
        }
        if definition["total"] == 0 {
            out.push_str("No recorded relationships.\n");
        }
        if definition["truncated"] == true {
            out.push_str("More relationships available; increase limit.\n");
        }
        out.push('\n');
    }
    Ok(out)
}
pub fn impact_data(cg: &CodeGraph, args: &Value) -> Result<Value> {
    let symbol = args["symbol"].as_str().unwrap_or_default();
    let nodes = lookup(cg, symbol)?;
    let grouped = group_definitions(&nodes, args["file"].as_str());
    let depth = number(args, "depth", 2, 1, 10) as u32;
    let mut all = IndexMap::new();
    let mut edges = IndexMap::new();
    let mut definitions = vec![];
    for group in &grouped.groups {
        let mut affected = IndexMap::new();
        let mut es = IndexMap::new();
        for n in group {
            let graph = cg.get_impact_radius(&n.id, depth)?;
            for (id, n) in graph.nodes {
                all.insert(id.clone(), n.clone());
                affected.insert(id, n);
            }
            for e in graph.edges {
                edges.insert(format!("{}|{}|{}", e.source, e.target, e.kind), e.clone());
                es.insert(format!("{}|{}|{}", e.source, e.target, e.kind), e);
            }
        }
        definitions.push(json!({"definition":group[0],"roots":group.iter().map(|n|&n.id).collect::<Vec<_>>(),"affected":affected.values().collect::<Vec<_>>(),"edges":es.values().collect::<Vec<_>>()}));
    }
    Ok(
        json!({"symbol":symbol,"depth":depth,"targets":grouped.groups.iter().flatten().collect::<Vec<_>>(),"ambiguous":grouped.groups.len()>1,"filteredOut":grouped.filtered_out,"aggregation":if grouped.groups.len()>1{"union"}else{"definition"},"definitions":definitions,"affected":all.values().collect::<Vec<_>>(),"edges":edges.values().collect::<Vec<_>>(),"nodeCount":all.len(),"edgeCount":edges.len()}),
    )
}
pub fn impact(cg: &CodeGraph, args: &Value) -> Result<String> {
    if let Err(e) = required(args, "symbol") {
        return Ok(e.message());
    }
    let data = impact_data(cg, args)?;
    if data["targets"].as_array().unwrap().is_empty() {
        return Ok(format!("No symbol found: {}", args["symbol"]));
    }
    let mut out = format!(
        "**Impact: {} affects {} symbols**\n\n",
        args["symbol"], data["nodeCount"]
    );
    for n in data["affected"].as_array().unwrap() {
        let n: Node = serde_json::from_value(n.clone())?;
        out.push_str(&format!("- {}\n", location(&n)));
    }
    Ok(out)
}
pub fn node(cg: &CodeGraph, args: &Value) -> Result<ToolResult> {
    let Some(symbol) = args["symbol"].as_str().filter(|s| !s.trim().is_empty()) else {
        return if let Some(file) = args["file"].as_str() {
            file_view(cg, file, args)
        } else {
            Ok(ToolResult::text(
                "Pass symbol, or file to read an indexed file.",
            ))
        };
    };
    let mut nodes = lookup(cg, symbol)?;
    if let Some(file) = args["file"].as_str() {
        let hint = file.replace('\\', "/").to_lowercase();
        let narrowed: Vec<_> = nodes
            .iter()
            .filter(|n| n.file_path.to_lowercase().contains(&hint))
            .cloned()
            .collect();
        if !narrowed.is_empty() {
            nodes = narrowed;
        }
    }
    if let Some(line) = args["line"].as_u64() {
        let containing: Vec<_> = nodes
            .iter()
            .filter(|n| n.start_line as u64 <= line && n.end_line as u64 >= line)
            .cloned()
            .collect();
        if !containing.is_empty() {
            nodes = containing;
        } else if let Some(n) = nodes
            .iter()
            .min_by_key(|n| (n.start_line as u64).abs_diff(line))
            .cloned()
        {
            nodes = vec![n];
        }
    }
    if nodes.is_empty() {
        return Ok(ToolResult::text(format!(
            "No symbol found: {symbol}. Try codegraphV_explore with nearby symbol names."
        )));
    }
    let mut out = String::new();
    if nodes.len() > 1 {
        out.push_str(&format!(
            "{} definitions match `{symbol}`; all are shown below.\n\n",
            nodes.len()
        ));
    }
    for n in nodes {
        out.push_str(&format!(
            "**{}** ({})\n\n**Location:** {}:{}\n",
            n.name, n.kind, n.file_path, n.start_line
        ));
        if let Some(sig) = &n.signature {
            out.push_str(&format!("**Signature:** `{sig}`\n"));
        }
        if let Some(doc) = &n.docstring {
            if doc.chars().count() < 200 {
                out.push_str(doc);
                out.push('\n');
            }
        }
        if args["includeCode"].as_bool() == Some(true) {
            if let Some(code) = cg.get_code(&n.id)? {
                out.push_str(&format!(
                    "\n```{}\n{}\n```\n",
                    n.language,
                    number_lines(&code, n.start_line)
                ));
            }
        } else {
            for child in cg
                .get_children(&n.id)?
                .into_iter()
                .filter(|c| !matches!(c.kind, NodeKind::Import | NodeKind::Export))
            {
                out.push_str(&format!("- {}\n", location(&child)));
            }
        }
        for (label, refs) in [
            ("Callers", cg.get_callers(&n.id, 1)?),
            ("Callees", cg.get_callees(&n.id, 1)?),
        ] {
            if !refs.is_empty() {
                out.push_str(&format!("\n**{label}:**\n"));
                for r in refs {
                    out.push_str(&format!("- {}{}\n", location(&r.node), hop(&r.edge)));
                }
            }
        }
        out.push('\n');
    }
    Ok(ToolResult::text(out))
}
fn number_lines(source: &str, start: u32) -> String {
    source
        .split('\n')
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", start as usize + i, line.trim_end_matches('\r')))
        .collect::<Vec<_>>()
        .join("\n")
}
fn read_source(cg: &CodeGraph, file: &str) -> Option<String> {
    let abs = validate_path_within_root(cg.project_root(), file, ValidateOptions::default())?;
    std::fs::read_to_string(abs).ok()
}
pub fn file_view(cg: &CodeGraph, file: &str, args: &Value) -> Result<ToolResult> {
    let all = cg.get_files()?;
    let want = file
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_lowercase();
    let resolve = |want: &str| -> Vec<&FileRecord> {
        if let Some(f) = all.iter().find(|f| f.path.to_lowercase() == want) {
            return vec![f];
        }
        let suffix: Vec<_> = all
            .iter()
            .filter(|f| f.path.to_lowercase().ends_with(&format!("/{want}")))
            .collect();
        if !suffix.is_empty() {
            suffix
        } else {
            all.iter()
                .filter(|f| f.path.to_lowercase().contains(want))
                .collect()
        }
    };
    let mut found = resolve(&want);
    let mut args = args.clone();
    if found.is_empty() {
        if let Some(cap) =
            codegraph_jsre::re!(r"(?::(\d+)(?:-(\d+))?|#L(\d+)(?:-L?(\d+))?)$").captures(&want)
        {
            let suffix = cap.text(0).unwrap();
            let stripped = &want[..want.len() - suffix.len()];
            found = resolve(stripped);
            if !found.is_empty() {
                let start = cap
                    .text(1)
                    .or_else(|| cap.text(3))
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1);
                if args.get("offset").is_none() {
                    args["offset"] = json!(start);
                }
                if args.get("limit").is_none() {
                    if let Some(end) = cap
                        .text(2)
                        .or_else(|| cap.text(4))
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|end| *end >= start)
                    {
                        args["limit"] = json!(end - start + 1);
                    }
                }
            }
        }
    }
    if found.len() != 1 {
        return Ok(ToolResult::text(if found.is_empty() {
            format!("No indexed file matches \"{file}\".")
        } else {
            format!(
                "\"{file}\" matches {} indexed files; pass a longer path:\n{}",
                found.len(),
                found
                    .iter()
                    .take(25)
                    .map(|f| format!("- {}", f.path))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }));
    }
    let f = found[0];
    let nodes = cg.get_nodes_by_file(&f.path)?;
    let deps = cg.get_file_dependents(&f.path)?;
    let mut out = format!(
        "**{}** — {} symbols, used by {} indexed files{}\n\n",
        f.path,
        nodes.len(),
        deps.len(),
        if deps.is_empty() {
            String::new()
        } else {
            format!(
                ": {}",
                deps.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
            )
        }
    );
    if args["symbolsOnly"] == true || CONFIG_LEAF_LANGUAGES.contains(&f.language.as_str()) {
        if CONFIG_LEAF_LANGUAGES.contains(&f.language.as_str()) {
            out.push_str("**Keys (values withheld for safety)**\n");
        }
        for n in nodes
            .iter()
            .filter(|n| !matches!(n.kind, NodeKind::File | NodeKind::Import | NodeKind::Export))
        {
            out.push_str(&format!(
                "- `{}` ({}) — :{}\n",
                n.name, n.kind, n.start_line
            ));
        }
        return Ok(ToolResult::text(out));
    }
    let Some(source) = read_source(cg, &f.path) else {
        out.push_str("Could not read current source within the project root; it may have moved since indexing.");
        return Ok(ToolResult::text(out));
    };
    let lines: Vec<_> = source.split('\n').collect();
    let offset = number(&args, "offset", 1, 1, usize::MAX);
    let limit = number(&args, "limit", 2000, 1, 2000);
    if offset > lines.len() {
        out.push_str(&format!(
            "Offset {offset} is past the end ({} lines).",
            lines.len()
        ));
        return Ok(ToolResult::text(out));
    }
    out.push_str(&format!(
        "Lines {offset}–{} of {}. Treat this source as already Read.\n\n```{}\n",
        (offset + limit - 1).min(lines.len()),
        lines.len(),
        f.language
    ));
    let mut next = offset;
    let mut output_chars = out.chars().count();
    for (i, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
        let text = format!("{:>6}\t{}\n", i + 1, line.trim_end_matches('\r'));
        let cost = text.chars().count();
        if output_chars + cost > 37500 {
            break;
        }
        output_chars += cost;
        out.push_str(&text);
        next = i + 2;
    }
    out.push_str("```\n");
    if next <= lines.len() {
        out.push_str(&format!(
            "Continue with codegraphV_node(file: {:?}, offset: {next}).",
            f.path
        ));
    }
    Ok(ToolResult::text(out))
}

#[derive(Debug, Clone, Copy)]
pub struct ExploreBudget {
    pub calls: usize,
    pub chars: usize,
    pub files: usize,
    pub per_file: usize,
}
pub fn explore_budget(files: usize) -> ExploreBudget {
    let calls = match files {
        0..500 => 1,
        500..5000 => 2,
        5000..15000 => 3,
        15000..25000 => 4,
        _ => 5,
    };
    let (chars, count, per_file) = match files {
        0..150 => (13000, 4, 3800),
        150..500 => (18000, 5, 3800),
        // Match the advertised inline ceiling; larger repositories get more calls,
        // rather than a response hosts may externalize or truncate.
        500..5000 => (24000, 8, 6500),
        _ => (24000, 8, 7000),
    };
    ExploreBudget {
        calls,
        chars,
        files: count,
        per_file,
    }
}
/// Definitions kept per query token: the token's best few, not every same-named symbol.
const NAMED_PER_TOKEN: usize = 3;

/// The definitions the query names, best first: one under a path the query also names
/// (`mpsc channel` → `sync/mpsc/…`), then one whose file defines more of the other named
/// symbols, then source over tests; a declaration beats a same-named member or variable.
/// Also returns the tokens nothing in the index defines.
fn named_definitions(cg: &CodeGraph, tokens: &[String], paths: &[String], pinned: &[String]) -> Result<(IndexMap<String, Node>, Vec<String>)> {
    // Exact file/range requests have no symbol tokens. Building every path's
    // scope segments here would do repository-wide work for a source continuation.
    if tokens.is_empty() {
        return Ok((IndexMap::new(), Vec::new()));
    }
    let segments: HashSet<String> = paths
        .iter()
        .flat_map(|p| p.split('/').map(|s| s.split('.').next().unwrap_or(s).to_lowercase()))
        .collect();
    let scopes: Vec<String> = tokens.iter().map(|t| t.to_lowercase()).filter(|t| segments.contains(t)).collect();
    let in_scope = |path: &str| {
        let lower = path.to_lowercase();
        scopes.iter().any(|s| lower.split('/').any(|seg| seg.split('.').next() == Some(s.as_str())))
    };
    let mut per_token: Vec<Vec<Node>> = Vec::new();
    for token in tokens {
        let mut hits: Vec<Node> = lookup(cg, token)?
            .into_iter()
            .filter(|n| !matches!(n.kind, NodeKind::File | NodeKind::Import | NodeKind::Export | NodeKind::Parameter))
            .collect();
        if hits.iter().any(|n| pinned.contains(&n.file_path)) {
            hits.retain(|n| pinned.contains(&n.file_path));
        }
        per_token.push(hits);
    }
    // Count co-named definitions once per file and token, rather than rescanning
    // every overload inside the sorting comparator.
    let mut file_tokens = HashMap::<&str, usize>::new();
    for hits in &per_token {
        for file in hits.iter().map(|n| n.file_path.as_str()).collect::<HashSet<_>>() {
            *file_tokens.entry(file).or_default() += 1;
        }
    }
    let mut out = IndexMap::new();
    let mut undefined = Vec::new();
    for (i, hits) in per_token.iter().enumerate() {
        if hits.is_empty() {
            // A bare path word (`mpsc`) is a scope, not a missing definition.
            if !segments.contains(&tokens[i].to_lowercase()) {
                undefined.push(tokens[i].clone());
            }
            continue;
        }
        let mut ranked: Vec<&Node> = hits.iter().collect();
        ranked.sort_by_key(|n| {
            (
                !pinned.is_empty() && !pinned.contains(&n.file_path),
                !scopes.is_empty() && !in_scope(&n.file_path),
                // A declaration before a member that shares its name (an enum variant, a field).
                matches!(n.kind, NodeKind::EnumMember | NodeKind::Field | NodeKind::Property | NodeKind::Variable),
                std::cmp::Reverse(file_tokens.get(n.file_path.as_str()).copied().unwrap_or(0)),
                codegraph_search::is_test_file(&n.file_path),
                n.file_path.len(),
                n.start_line,
            )
        });
        for n in ranked.into_iter().take(NAMED_PER_TOKEN) {
            out.entry(n.id.clone()).or_insert_with(|| n.clone());
        }
    }
    Ok((out, undefined))
}

/// Declaration lines for names the parser did not see (a type inside `pin_project! { … }`).
fn declaration_lines(lines: &[&str], names: &[String]) -> Vec<u32> {
    let mut found = Vec::new();
    for name in names {
        let re = codegraph_jsre::Re::cached(
            &format!(
                r"\b(?:struct|enum|union|trait|class|interface|type|fn|def|func|function|const|static|let|var|val|object|protocol|module|record)\s+{}\b",
                codegraph_jsre::escape_regex(name)
            ),
            "",
        );
        if let Some(i) = lines.iter().position(|l| re.is_match(l)) {
            found.push(i as u32 + 1);
        }
    }
    found
}

/// The query's symbol tokens: words and `a.b` / `a::b` paths, deduplicated in order.
fn query_tokens(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    query
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | ':' | '#')))
        .map(|t| t.trim_matches(|c| matches!(c, '.' | ':')))
        .filter(|t| t.chars().any(|c| c.is_alphabetic() || c == '_'))
        .filter(|t| seen.insert(t.to_string()))
        .map(str::to_string)
        .take(16)
        .collect()
}

/// Lines of each focus range taken before any range's full body.
const HEAD_LINES: u32 = 12;

/// A source range to show, most important first: a named definition, then an explicit line
/// anchor, then a step on the flow among the named symbols.
struct Focus {
    start: u32,
    end: u32,
}

/// Metadata may be abbreviated; source lines must always remain verbatim.
fn metadata_preview(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

/// Explicit character pages for source lines that cannot fit as a whole. Pages
/// are independent of session deduplication and carry a content hash so an edit
/// cannot silently splice two different versions of a line together.
fn line_fragment(cg: &CodeGraph, file: &FileRecord, line_no: u32, args: &Value, budget: ExploreBudget) -> ToolResult {
    let Some(column) = args["column"].as_u64().and_then(|n| usize::try_from(n).ok()).filter(|n| *n > 0) else {
        return ToolResult::text("column must be a positive integer (1-based Unicode character position).");
    };
    if args.get("sourceHash").is_some_and(|v| !v.is_string()) {
        return ToolResult::text("sourceHash must be the string returned with the previous fragment.");
    }
    if CONFIG_LEAF_LANGUAGES.contains(&file.language.as_str()) {
        return ToolResult::text("Configuration keys only; values withheld.");
    }
    if file.path.chars().count() > 1000 {
        return ToolResult::text("This indexed path is too long to display within the response budget.");
    }
    let Some(source) = read_source(cg, &file.path) else {
        return ToolResult::text("Source unavailable on disk within the project root.");
    };
    use sha2::{Digest, Sha256};
    let hash = format!("{:x}", Sha256::digest(source.as_bytes()));
    if args["sourceHash"].as_str().is_some_and(|old| old != hash) {
        return ToolResult::text("Source changed since the previous fragment. Restart this line at column 1 without sourceHash.");
    }
    let Some(line) = source.split('\n').nth(line_no.saturating_sub(1) as usize) else {
        return ToolResult::text("Requested line is outside the current source.");
    };
    let line = line.trim_end_matches('\r');
    let total = line.chars().count();
    let start = column - 1;
    if start >= total && !(start == 0 && total == 0) {
        return ToolResult::text(format!("Column {column} is past the end of this line ({total} characters)."));
    }
    let fragment: String = line.chars().skip(start).take(budget.per_file - 100).collect();
    let end = start + fragment.chars().count();
    // Source can itself contain Markdown fences. Choose a fence that cannot
    // terminate early, preferring the character with the shorter longest run.
    let run = |mark: char| fragment.split(|c| c != mark).map(str::len).max().unwrap_or(0);
    let (mark, longest) = if run('`') <= run('~') { ('`', run('`')) } else { ('~', run('~')) };
    let fence = mark.to_string().repeat(3.max(longest + 1));
    let mut out = format!(
        "**{}:{line_no} — columns {column}–{end} of {total}**\n\nVerbatim source fragment; positions count Unicode characters. Only these columns are shown.\n\n{fence}{}\n{fragment}\n{fence}\n",
        file.path, file.language
    );
    if end < total {
        let next = json!({"query":format!("{}:{line_no}-{line_no}", file.path), "column":end + 1, "sourceHash":hash});
        out.push_str(&format!("\nCall codegraphV_explore with these arguments. Next fragment: {next}\n"));
    } else {
        out.push_str("\nEnd of source line.\n");
    }
    ToolResult::text(out)
}

pub fn explore(
    cg: &CodeGraph,
    args: &Value,
    shown: &mut HashMap<String, HashSet<u32>>,
) -> Result<ToolResult> {
    let query = match required(args, "query") {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let files = cg.get_files()?;
    let paths: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
    let budget = explore_budget(files.len());
    let extracted =
        codegraph_search::query_paths::extract_query_paths(query, &paths, &Default::default());
    if args.get("column").is_some() || args.get("sourceHash").is_some() {
        if extracted.line_anchors.len() != 1 || extracted.pinned_files.len() != 1
            || !extracted.stripped_query.trim().is_empty()
            || extracted.line_anchors[0].start != extracted.line_anchors[0].end {
            return Ok(ToolResult::text("For a source fragment, use query: \"path/to/file:LINE-LINE\" and column: 1 (or the returned next column)."));
        }
        let anchor = &extracted.line_anchors[0];
        if let Some(file) = files.iter().find(|f| f.path == anchor.file) {
            return Ok(line_fragment(cg, file, anchor.start, args, budget));
        }
    }
    let go_reader = GoPackageReader::new(cg.project_root());
    let go = |p: &str| go_reader.package_of(p);
    let flow = resolve_named_symbol_flow(
        cg.store().as_ref(),
        cg.store().as_ref(),
        &extracted.stripped_query,
        &NamedSymbolFlowOptions {
            preferred_files: extracted.pinned_files.clone(),
            ..Default::default()
        },
        Some(&go),
    )?;
    let (named, undefined) = named_definitions(cg, &query_tokens(&extracted.stripped_query), &paths, &extracted.pinned_files)?;
    // Relevance search widens a query whose words are not all symbol names; when every token
    // names a definition it would only add listed neighbours, for most of the call's time.
    let graph = if !extracted.stripped_query.trim().is_empty() && (named.is_empty() || !undefined.is_empty()) {
        cg.find_relevant_context(
            &extracted.stripped_query,
            &FindRelevantContextOptions {
                max_nodes: Some(60),
                search_limit: Some(12),
                ..Default::default()
            },
        )?
    } else {
        Default::default()
    };
    // Files in order of importance; each with the symbols to list and the ranges to show.
    let mut relevant = IndexMap::<String, (Vec<Node>, Vec<Focus>)>::new();
    let add = |relevant: &mut IndexMap<String, (Vec<Node>, Vec<Focus>)>, n: &Node, focus: bool| {
        let (list, ranges) = relevant.entry(n.file_path.clone()).or_default();
        if !list.iter().any(|v| v.id == n.id) {
            list.push(n.clone());
        }
        if focus {
            ranges.push(Focus { start: n.start_line, end: n.end_line });
        }
    };
    // Explicit files and ranges must not lose their place to unrelated symbols.
    for file in &extracted.pinned_files {
        relevant.entry(file.clone()).or_default();
    }
    for n in named.values() {
        add(&mut relevant, n, true);
    }
    let mut out = format!("**Explore: {}**\n\n", metadata_preview(query, 240));
    if !extracted.unresolved_path_spans.is_empty() {
        let missing = extracted.unresolved_path_spans.iter()
            .map(|path| format!("`{}`", metadata_preview(path, 160)))
            .collect::<Vec<_>>().join(", ");
        out.push_str(&format!("No indexed file matches requested path(s): {missing}. Check the indexed path; any results below match the remaining query terms.\n\n"));
    }
    if !flow.chains.is_empty() {
        out.push_str("**Flow among named symbols**\n\n");
        let mut omitted_chains = 0;
        for chain in &flow.chains {
            let mut chain_text = String::new();
            for (i, step) in chain.steps.iter().enumerate() {
                chain_text.push_str(&format!(
                    "{}{}{}\n",
                    if i == 0 { "- " } else { "  → " },
                    location(&step.node),
                    step.edge.as_ref().map(hop).unwrap_or_default()
                ));
                add(&mut relevant, &step.node, true);
            }
            // Never cut a chain mid-path: an omitted hop could imply a false connection.
            chain_text.push('\n');
            if out.chars().count() + chain_text.chars().count() <= budget.chars / 4 {
                out.push_str(&chain_text);
            } else {
                omitted_chains += 1;
            }
        }
        if omitted_chains > 0 {
            out.push_str(&format!("{omitted_chains} further flow paths omitted; query their endpoints separately.\n\n"));
        }
    }
    for anchor in &extracted.line_anchors {
        relevant.entry(anchor.file.clone()).or_default();
    }
    for n in flow.named.values().chain(graph.nodes.values()) {
        add(&mut relevant, n, false);
    }
    if relevant.is_empty() {
        if !extracted.unresolved_path_spans.is_empty() {
            return Ok(ToolResult::text(out));
        }
        return Ok(ToolResult::text(format!("No indexed symbols or files match \"{}\". Try exact symbol names or indexed paths.", metadata_preview(query, 240))));
    }
    let max_files = number(args, "maxFiles", budget.files, 1, 20);
    let mut omitted = vec![];
    for (index, (file, (mut nodes, focus))) in relevant.into_iter().enumerate() {
        // Reserve complete metadata, a continuation containing the path twice,
        // and the response footer before admitting a file. Source has its own budget.
        let notice_reserve = file.chars().count() * 3 + 800;
        if index >= max_files || out.chars().count() + 1200 + notice_reserve + 1000 > budget.chars {
            omitted.push(file);
            continue;
        }
        let mut header = format!("**{file}**\n");
        if header.chars().count() > 1000 {
            omitted.push(file);
            continue;
        }
        nodes.sort_by_key(|n| n.start_line);
        let mut listed = 0;
        for n in nodes.iter().take(12) {
            let row = format!(
                "- {} ({}) :{}–{}{}\n",
                n.name,
                n.kind,
                n.start_line,
                n.end_line,
                n.signature
                    .as_deref()
                    .map(|s| format!(" `{}`", metadata_preview(s.lines().next().unwrap_or(""), 160)))
                    .unwrap_or_default()
            );
            if header.chars().count() + row.chars().count() > 1000 {
                break;
            }
            header.push_str(&row);
            listed += 1;
        }
        if nodes.len() > listed {
            header.push_str(&format!("{} more symbols; query this file for source.\n", nodes.len() - listed));
        }
        // A precise continuation needs source, not a repeated dependency listing.
        // Symbol/whole-file queries still return the relationship summary.
        let source_only = extracted.stripped_query.trim().is_empty()
            && extracted.line_anchors.iter().any(|a| a.file == file);
        let deps = if source_only { Vec::new() } else { cg.get_file_dependents(&file)? };
        if !deps.is_empty() {
            header.push_str(&format!("Used by {} files", deps.len()));
            for (i, dep) in deps.iter().take(8).enumerate() {
                let part = format!("{}{dep}", if i == 0 { ": " } else { ", " });
                if header.chars().count() + part.chars().count() > 1180 {
                    break;
                }
                header.push_str(&part);
            }
            header.push('\n');
        }
        out.push_str(&header);
        out.push('\n');
        let Some(record) = files.iter().find(|f| f.path == file) else {
            continue;
        };
        if CONFIG_LEAF_LANGUAGES.contains(&record.language.as_str()) {
            out.push_str("Configuration keys only; values withheld.\n\n");
            continue;
        }
        // A file named only as context is listed, not printed; a pinned file with nothing
        // named in it is printed from the top.
        let pinned = extracted.pinned_files.contains(&file);
        if focus.is_empty() && !pinned && undefined.is_empty() {
            out.push('\n');
            continue;
        }
        let Some(source) = read_source(cg, &file) else {
            out.push_str("Source unavailable on disk.\n\n");
            continue;
        };
        let lines: Vec<_> = source.split('\n').collect();
        let last = lines.len() as u32;
        let mut focus = focus;
        for line in declaration_lines(&lines, &undefined) {
            focus.push(Focus { start: line, end: line + HEAD_LINES });
        }
        if focus.is_empty() && !pinned {
            out.push('\n');
            continue;
        }
        let anchored: Vec<_> = extracted.line_anchors.iter().filter(|a| a.file == file).collect();
        let explicit_range = !anchored.is_empty();
        // A continuation is an exact range. Adding leading context here can spend
        // every page on the same lines and prevent a fresh CLI call from advancing.
        let ranges: Vec<(u32, u32)> = if explicit_range {
            anchored.iter().map(|a| (a.start.max(1), a.end.min(last))).collect()
        } else if focus.is_empty() {
            vec![(1, last)]
        } else {
            focus.iter().map(|f| (f.start.saturating_sub(2).max(1), f.end.saturating_add(1).min(last))).collect()
        };
        // Scope emission state to project, file, and current content. An edit must be shown again.
        use sha2::{Digest, Sha256};
        let key = format!(
            "{}:{file}:{:x}",
            cg.project_root(),
            Sha256::digest(source.as_bytes())
        );
        let prefix = format!("{}:{file}:", cg.project_root());
        shown.retain(|old, _| !old.starts_with(&prefix) || old == &key);
        let seen = shown.entry(key).or_default();
        // Take ranges in priority order (each from its first line) until the budgets run out,
        // then print what was taken in line order.
        let mut taken = std::collections::BTreeMap::<u32, String>::new();
        let mut used = 0;
        let framing = 32 + record.language.as_str().len();
        let room = (budget.chars - 1000).saturating_sub(out.chars().count() + framing + notice_reserve);
        let mut next_missing: Option<u32> = None;
        let mut oversized = std::collections::BTreeMap::new();
        // Two passes: every range's head first, so one long body cannot crowd out the rest.
        for (pass, &(start, end)) in [0, 1].iter().flat_map(|p| ranges.iter().map(move |r| (*p, r))) {
            let stop = if pass == 0 { end.min(start + HEAD_LINES) } else { end };
            for line_no in start..=stop {
                if (!explicit_range && seen.contains(&line_no)) || taken.contains_key(&line_no) {
                    continue;
                }
                let Some(line) = lines.get(line_no.saturating_sub(1) as usize) else {
                    continue;
                };
                let text = format!("{line_no:>6}\t{}\n", line.trim_end_matches('\r'));
                let cost = text.chars().count();
                if cost > budget.per_file {
                    // Retrying cannot fit this line either. Keep it out of `seen`,
                    // report it explicitly, and allow the other definitions through.
                    oversized.insert(line_no, cost);
                    continue;
                }
                // Account for gap markers in the final line-sorted output. Inserting
                // between two windows can remove a marker as well as add one.
                let before = taken.range(..line_no).next_back().map(|(n, _)| *n);
                let after = taken.range(line_no..).next().map(|(n, _)| *n);
                let new_gaps = usize::from(before.is_some_and(|n| line_no > n + 1))
                    + usize::from(after.is_some_and(|n| n > line_no + 1));
                let old_gap = usize::from(before.zip(after).is_some_and(|(a, b)| b > a + 1));
                let next_used = used + cost + new_gaps * 7 - old_gap * 7;
                if next_used > budget.per_file || next_used > room {
                    next_missing = Some(next_missing.map_or(line_no, |old| old.min(line_no)));
                    break;
                }
                used = next_used;
                taken.insert(line_no, text);
            }
        }
        if !taken.is_empty() {
            out.push_str(&format!("\n```{}\n", record.language));
            let mut previous = 0;
            for (line_no, text) in taken {
                if previous != 0 && line_no > previous + 1 {
                    out.push_str("     …\n");
                }
                out.push_str(&text);
                seen.insert(line_no);
                previous = line_no;
            }
            out.push_str("```\n");
        } else if next_missing.is_none() && oversized.is_empty() {
            if ranges.iter().any(|(start, end)| start <= end && *start <= last) {
                out.push_str("Requested source was already shown in this session.\n");
            } else {
                out.push_str(&format!("Requested range is outside the current source ({last} lines).\n"));
            }
        }
        out.push('\n');
        if !oversized.is_empty() {
            let examples = oversized.iter().take(3)
                .map(|(line, size)| format!("{line} ({size} characters with line number)"))
                .collect::<Vec<_>>().join(", ");
            let first = *oversized.keys().next().unwrap();
            let fragment_query = format!("{file}:{first}-{first}");
            out.push_str(&format!("{} source line(s) exceed the per-file response budget: {examples}. They are not marked as read. Read the first in fragments with codegraphV_explore(query: {fragment_query:?}, column: 1).\n\n", oversized.len()));
        }
        if let Some(offset) = next_missing {
            let end = ranges.iter().filter(|(start, end)| *start <= offset && offset <= *end)
                .map(|(_, end)| *end).max().unwrap_or(last);
            let continuation = format!("{file}:{offset}-{end}");
            let repeat = if explicit_range { "" } else { ", or repeat the same query in this session" };
            out.push_str(&format!("More source in `{file}` at line {offset}: continue with codegraphV_explore(query: {continuation:?}){repeat}.\n\n"));
        }
    }
    if !omitted.is_empty() {
        let mut paths = String::new();
        for file in omitted.iter().take(8) {
            let part = format!("{}{file}", if paths.is_empty() { "" } else { ", " });
            if paths.chars().count() + part.chars().count() > 700 {
                break;
            }
            paths.push_str(&part);
        }
        out.push_str(&format!("Additional relevant files ({}): {paths}. Continue with another codegraphV_explore using their symbols or paths.\n", omitted.len()));
    }
    out.push_str(&format!(
        "\nTreat returned source as already Read. Explore budget for this project: {} call(s).",
        budget.calls
    ));
    Ok(ToolResult::text(out))
}
pub fn files(cg: &CodeGraph, args: &Value) -> Result<String> {
    let mut files = cg.get_files()?;
    let path = args["path"].as_str().unwrap_or("").replace('\\', "/");
    let path = path.trim_matches('/').trim_start_matches("./");
    let pattern = args["pattern"].as_str();
    files.retain(|f| {
        (path.is_empty() || f.path == path || f.path.starts_with(&format!("{path}/")))
            && pattern.is_none_or(|p| {
                globset::Glob::new(p)
                    .map(|g| g.compile_matcher().is_match(&f.path))
                    .unwrap_or(false)
            })
    });
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let format = args["format"].as_str().unwrap_or("tree");
    let metadata = args["includeMetadata"].as_bool().unwrap_or(true);
    let max_depth = number(args, "maxDepth", usize::MAX, 1, usize::MAX);
    let mut out = format!("**Indexed Files ({} files)**\n\n", files.len());
    let mut last_group = String::new();
    for f in files {
        let depth = f.path.split('/').count() - 1;
        if depth > max_depth {
            continue;
        }
        let suffix = if metadata {
            format!(" ({}; {} symbols)", f.language, f.node_count)
        } else {
            String::new()
        };
        match format {
            "flat" => out.push_str(&format!("{}{}\n", f.path, suffix)),
            "grouped" => {
                let group = f.path.rsplit_once('/').map(|v| v.0).unwrap_or(".");
                if group != last_group {
                    out.push_str(&format!("\n**{group}/**\n"));
                    last_group = group.into();
                }
                out.push_str(&format!(
                    "- {}{}\n",
                    f.path.rsplit('/').next().unwrap(),
                    suffix
                ));
            }
            _ => out.push_str(&format!("{}{}{}\n", "  ".repeat(depth), f.path, suffix)),
        }
    }
    Ok(out)
}
pub fn status(cg: &CodeGraph) -> Result<Value> {
    let mut data = serde_json::to_value(cg.get_stats()?)?;
    let build = cg.get_index_build_info();
    let health = cg.get_index_health()?;
    data["initialized"] = json!(true);
    data["version"] = json!(env!("CARGO_PKG_VERSION"));
    data["projectPath"] = json!(cg.project_root());
    data["indexPath"] = json!(codegraph_core::directory::get_code_graph_dir(cg.project_root()));
    data["backend"] = json!(cg.store().backend());
    data["journalMode"] = json!(cg.store().journal_mode()?);
    // A newer release exists: status is where users and agents look when something seems off.
    if let Some(notice) = codegraph_installer::upgrade::get_update_notice(env!("CARGO_PKG_VERSION")) {
        data["updateAvailable"] = json!(notice);
    }
    // Frameworks a sync detected since the last full index (#2516): files indexed before then were
    // extracted without the change, so a re-index is recommended while they differ.
    // While another process rebuilds the index, the advice meant for an index at rest does not
    // fit: the interrupted-run state, the re-index and the framework note all describe the live run.
    let rebuilding = cg.is_rebuild_in_progress();
    let framework_changes = if rebuilding { None } else { cg.get_framework_changes_since_index() };
    if rebuilding {
        data["rebuildInProgress"] = json!(true);
    }
    data["index"] = json!({"builtWithVersion":build.version,"builtWithExtractionVersion":build.extraction_version,"currentExtractionVersion":codegraph_core::codegraph::EXTRACTION_VERSION,"reindexRecommended":!rebuilding && (cg.is_index_stale() || framework_changes.is_some()),"frameworksChangedSinceIndex":framework_changes.map(|c| json!({"added":c.added,"removed":c.removed})),"state":cg.get_index_state().map(|s|s.as_str()),"pendingRefs":cg.get_pending_reference_count()?,"filesNeedingReindex":health.needs_reindex,"filesWithParseErrors":health.parse_errors});
    Ok(data)
}
