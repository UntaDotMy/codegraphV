//! The graph-derived sections of a `codegraph_explore` answer — port of the
//! `ToolHandler` methods `handleExplore` calls (`src/mcp/tools.ts` 2661–2706,
//! 3363–4090): the Flow among named symbols, dynamic / interface dispatch
//! boundaries, the blast radius, the copybook list, graph relevance (RWR) and the
//! on-disk drift check. Each one swallows its errors the way the TypeScript does.
use super::helpers::{follow_up_name, is_low_value};
use codegraph_core::orchestrator::FileStat;
use codegraph_core::CodeGraph;
use codegraph_graph::branch_guards::{guard_label, guards_for_file, site_key, supports_branch_guards, CallSite};
use codegraph_graph::cobol_copybooks::{NamedCopybook, MAX_COPYBOOK_INCLUDES};
use codegraph_graph::dynamic_boundary_report::{find_dynamic_boundaries, BoundaryGraph, BoundarySite};
use codegraph_graph::go_package::GoPackageReader;
use codegraph_graph::named_symbol_flow::{self, NamedSymbolFlowOptions};
use codegraph_graph::symbol_lookup::SymbolLookupHost;
use codegraph_graph::synthesized_hop::describe_synthesized_hop;
use codegraph_graph::type_hierarchy::count_implementers;
use codegraph_jsre::re;
use codegraph_search::query_utils::is_test_file;
use codegraph_types::jscompat::utf16_len;
use codegraph_types::paths::{validate_path_within_root, ValidateOptions};
use codegraph_types::{Edge, EdgeKind, Node, Provenance, SearchOptions, Subgraph};
use indexmap::{IndexMap, IndexSet};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// The graph as `ToolHandler` sees it through `CodeGraph`: exact lookups on the
/// store, ranked search with the project's `deprioritize` paths applied.
pub struct CgHost<'a>(pub &'a CodeGraph);

impl CgHost<'_> {
    fn search(&self, query: &str, limit: usize) -> codegraph_graph::Result<Vec<Node>> {
        let options = SearchOptions { limit: Some(limit), ..Default::default() };
        let matcher = self.0.deprioritized_path_matcher();
        Ok(self
            .0
            .store()
            .search_nodes_with_deprioritizer(query, &options, Some(&matcher))?
            .into_iter()
            .map(|r| r.node)
            .collect())
    }
}

impl SymbolLookupHost for CgHost<'_> {
    fn get_nodes_by_name(&self, name: &str) -> codegraph_graph::Result<Vec<Node>> {
        Ok(self.0.store().get_nodes_by_name(name)?)
    }
    fn get_nodes_by_name_prefix(&self, prefix: &str, limit: usize) -> codegraph_graph::Result<Vec<Node>> {
        Ok(self.0.store().get_nodes_by_name_prefix(prefix, limit)?)
    }
    fn search_nodes(&self, query: &str, limit: usize) -> codegraph_graph::Result<Vec<Node>> {
        self.search(query, limit)
    }
    fn generated_paths(&self, paths: &[String]) -> codegraph_graph::Result<HashSet<String>> {
        SymbolLookupHost::generated_paths(self.0.store().as_ref(), paths)
    }
}

impl BoundaryGraph for CgHost<'_> {
    fn get_nodes_by_name(&self, name: &str) -> codegraph_graph::Result<Vec<Node>> {
        Ok(self.0.store().get_nodes_by_name(name)?)
    }
    fn search_nodes(&self, query: &str, limit: usize) -> codegraph_graph::Result<Vec<Node>> {
        self.search(query, limit)
    }
    fn get_node(&self, id: &str) -> codegraph_graph::Result<Option<Node>> {
        Ok(self.0.store().get_node_by_id(id)?)
    }
    fn get_outgoing_edges(&self, id: &str) -> codegraph_graph::Result<Vec<Edge>> {
        Ok(self.0.store().get_outgoing_edges(id, &[], None)?)
    }
}

/// `findAllSymbols(cg, symbol).nodes`, with the project's Go package reader.
pub fn find_all_symbols(cg: &CodeGraph, symbol: &str) -> Vec<Node> {
    let reader = GoPackageReader::new(cg.project_root());
    let go = |file: &str| reader.package_of(file);
    named_symbol_flow::find_all_symbols(&CgHost(cg), symbol, Some(&go)).map(|a| a.nodes).unwrap_or_default()
}

/// The surface `tools/list` shows: `CODEGRAPH_MCP_TOOLS` when set, else `explore` alone.
pub fn is_tool_served(name: &str) -> bool {
    let short = name.strip_prefix("codegraph_").unwrap_or(name);
    match tool_allowlist() {
        Some(allow) => allow.contains(short),
        None => short == "explore",
    }
}

fn tool_allowlist() -> Option<HashSet<String>> {
    let raw = std::env::var("CODEGRAPH_MCP_TOOLS").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let set: HashSet<String> = raw
        .split(',')
        .map(|s| {
            let t = s.trim();
            t.strip_prefix("codegraph_").unwrap_or(t).to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    (!set.is_empty()).then_some(set)
}

/// Floored `stat.mtimeMs` and size of a file, or `None` when it cannot be stat'ed.
fn stat_of(abs: &str) -> Option<FileStat> {
    std::fs::metadata(abs).ok().map(|m| FileStat::from_metadata(&m))
}

/// The branch condition guarding the call `edge` makes from `caller`, as a `when` label.
pub fn when_label(cg: &CodeGraph, caller: &Node, edge: &Edge) -> String {
    let Some(line) = edge.line.filter(|l| *l != 0) else { return String::new() };
    if !supports_branch_guards(caller.language.as_str()) {
        return String::new();
    }
    let Ok(Some(rec)) = cg.get_file(&caller.file_path) else { return String::new() };
    let Some(abs) = validate_path_within_root(cg.project_root(), &caller.file_path, ValidateOptions::default()) else {
        return String::new();
    };
    let Some(st) = stat_of(&abs) else { return String::new() };
    if st.size != rec.size || st.mtime_ms.floor() != (rec.modified_at as f64).floor() {
        return String::new();
    }
    let site = CallSite { line, column: edge.column, callee: None };
    guards_for_file(&abs, caller.language.as_str(), std::slice::from_ref(&site))
        .get(&site_key(&site))
        .map(|g| guard_label(g))
        .unwrap_or_default()
}

/// `dynamic: <summary> @<wiring site>` for a synthesized hop; `None` for a static edge.
pub fn synth_edge_note(edge: Option<&Edge>) -> Option<String> {
    let hop = describe_synthesized_hop(edge)?;
    Some(match hop.registered_at {
        Some(at) if !at.is_empty() => format!("dynamic: {} @{}", hop.summary, at),
        _ => format!("dynamic: {}", hop.summary),
    })
}

/// What the Flow section hands the source assembler.
#[derive(Debug, Clone, Default)]
pub struct FlowResult {
    pub text: String,
    pub path_node_ids: IndexSet<String>,
    pub named_node_ids: IndexSet<String>,
    pub unique_named_node_ids: IndexSet<String>,
    pub spine_call_sites: IndexMap<String, u32>,
}

fn is_heuristic(edge: &Edge) -> bool {
    edge.provenance == Some(Provenance::Heuristic)
}

fn edge_kind_label(edge: &Edge) -> String {
    synth_edge_note(Some(edge)).unwrap_or_else(|| edge.kind.as_str().to_string())
}

fn node_follow_up_name(n: &Node) -> String {
    follow_up_name(&n.name, n.kind.as_str(), Some(&n.qualified_name))
}

const DISPATCH_LINKS_HEADER: [&str; 2] = [
    "**Dynamic-dispatch links among your symbols**",
    "(synthesized — the indirect hops grep/Read would reconstruct; the `@file:line` is the wiring site)",
];

/// The call path among the symbols the query names, its dispatch links and boundaries.
pub fn build_flow_from_named_symbols(cg: &CodeGraph, query: &str) -> FlowResult {
    flow_inner(cg, query).unwrap_or_default()
}

fn flow_inner(cg: &CodeGraph, query: &str) -> Option<FlowResult> {
    let host = CgHost(cg);
    let reader = GoPackageReader::new(cg.project_root());
    let go = |file: &str| reader.package_of(file);
    let flow = named_symbol_flow::resolve_named_symbol_flow(
        cg.store().as_ref(),
        &host,
        query,
        &NamedSymbolFlowOptions::default(),
        Some(&go),
    )
    .ok()?;
    if flow.tokens.len() < 2 {
        return Some(FlowResult::default());
    }
    let named = &flow.named;
    let dyn_named = &flow.dyn_named;
    let unique = &flow.unique_named_node_ids;

    let collect_synth_links = |chain: Option<&[codegraph_graph::PathStep]>| -> Option<Vec<String>> {
        const MAX_SYNTH_LINKS: usize = 6;
        let on_chain: HashSet<&str> = chain.unwrap_or(&[]).iter().map(|s| s.node.id.as_str()).collect();
        let weight = |id: &str| -> i64 {
            if unique.contains(id) || dyn_named.contains_key(id) {
                3
            } else if named.contains_key(id) {
                1
            } else {
                0
            }
        };
        struct Link {
            edge: Edge,
            src: Node,
            tgt: Node,
            low_value: bool,
            weight: i64,
            turn: i64,
        }
        let mut links: Vec<Link> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut turns: HashMap<String, i64> = HashMap::new();
        for n in named.values().chain(dyn_named.values()) {
            let mut edges = cg.get_incoming_edges(&n.id).ok()?;
            edges.extend(cg.get_outgoing_edges(&n.id).ok()?);
            for edge in edges {
                if !is_heuristic(&edge) {
                    continue;
                }
                let other_id = if edge.source == n.id { edge.target.clone() } else { edge.source.clone() };
                let key = format!("{}>{}", edge.source, edge.target);
                if other_id == n.id || seen.contains(&key) {
                    continue;
                }
                seen.insert(key);
                if on_chain.contains(edge.source.as_str()) && on_chain.contains(edge.target.as_str()) {
                    continue;
                }
                let other = match named.get(&other_id).or_else(|| dyn_named.get(&other_id)) {
                    Some(o) => o.clone(),
                    None => match cg.get_node(&other_id).ok()? {
                        Some(o) => o,
                        None => continue,
                    },
                };
                let low_value = weight(&other_id) == 0 && is_low_value(&other.file_path);
                let link_weight = weight(&n.id) + weight(&other_id);
                let rank = format!("{}|{}|{}", n.id, low_value, link_weight);
                let turn = *turns.get(&rank).unwrap_or(&0);
                turns.insert(rank, turn + 1);
                let (src, tgt) = if edge.source == n.id { (n.clone(), other) } else { (other, n.clone()) };
                links.push(Link { edge, src, tgt, low_value, weight: link_weight, turn });
            }
        }
        links.sort_by(|a, b| {
            (a.low_value as i64 - b.low_value as i64)
                .cmp(&0)
                .then_with(|| (b.weight - a.weight).cmp(&0))
                .then_with(|| (a.turn - b.turn).cmp(&0))
        });
        let link_line = |edge: &Edge, src: &Node, tgt: &Node| -> String {
            let same = src.name == tgt.name;
            let s = if same { node_follow_up_name(src) } else { src.name.clone() };
            let t = if same { node_follow_up_name(tgt) } else { tgt.name.clone() };
            format!("- {s} → {t}   [{}]", edge_kind_label(edge))
        };
        let mut shown: HashSet<String> = HashSet::new();
        if let Some(chain) = chain {
            for i in 1..chain.len() {
                let prev = &chain[i - 1].node;
                let step = &chain[i];
                let Some(edge) = step.edge.as_ref().filter(|e| is_heuristic(e)) else { continue };
                shown.insert(if edge.source == prev.id {
                    link_line(edge, prev, &step.node)
                } else {
                    link_line(edge, &step.node, prev)
                });
            }
        }
        let mut synth_lines = Vec::new();
        for link in &links {
            if synth_lines.len() >= MAX_SYNTH_LINKS {
                break;
            }
            let line = link_line(&link.edge, &link.src, &link.tgt);
            if !shown.insert(line.clone()) {
                continue;
            }
            synth_lines.push(line);
        }
        Some(synth_lines)
    };

    let identity_only = || -> FlowResult {
        if flow.precise_named_ids.is_empty() {
            return FlowResult::default();
        }
        FlowResult {
            named_node_ids: flow.precise_named_ids.clone(),
            unique_named_node_ids: unique.iter().filter(|id| flow.precise_named_ids.contains(*id)).cloned().collect(),
            ..Default::default()
        }
    };
    let named_keys = || -> IndexSet<String> { named.keys().chain(dyn_named.keys()).cloned().collect() };

    if named.len() < 2 {
        let synth_lines = collect_synth_links(None)?;
        let boundaries = if named.is_empty() {
            String::new()
        } else {
            build_dynamic_boundaries(cg, &named.values().cloned().collect::<Vec<_>>(), named)
        };
        if synth_lines.is_empty() && boundaries.is_empty() {
            return Some(identity_only());
        }
        let mut out: Vec<String> = Vec::new();
        if !synth_lines.is_empty() {
            out.extend(DISPATCH_LINKS_HEADER.iter().map(|s| s.to_string()));
            out.push(String::new());
            out.extend(synth_lines);
            out.push(String::new());
        }
        if !boundaries.is_empty() {
            out.push(boundaries);
        }
        out.push("> Full source for these symbols is below.\n".to_string());
        return Some(FlowResult {
            text: out.join("\n"),
            named_node_ids: named_keys(),
            unique_named_node_ids: unique.clone(),
            ..Default::default()
        });
    }

    let best: Option<&[codegraph_graph::PathStep]> = flow.chains.first().map(|c| c.steps.as_slice());
    let has_main = best.is_some_and(|b| b.len() >= 3);
    let path_ids: IndexSet<String> = best.unwrap_or(&[]).iter().map(|s| s.node.id.clone()).collect();
    let spine_call_sites = flow.chains.first().map(|c| c.call_sites.clone()).unwrap_or_default();

    let mut boundary_text = String::new();
    {
        let mut uncovered: Vec<Node> = Vec::new();
        if !has_main {
            for n in named.values() {
                if !path_ids.contains(&n.id) {
                    uncovered.push(n.clone());
                }
            }
        } else {
            for ids in flow.token_nodes.values() {
                if ids.is_empty() || ids.iter().any(|id| path_ids.contains(id)) {
                    continue;
                }
                for id in ids {
                    if let Some(n) = named.get(id) {
                        uncovered.push(n.clone());
                    }
                }
            }
        }
        if !uncovered.is_empty() {
            let mut scan_list: Vec<Node> = Vec::new();
            if has_main {
                let b = best.unwrap();
                scan_list.push(b[b.len() - 1].node.clone());
            }
            uncovered.sort_by_key(|n| std::cmp::Reverse(unique.contains(&n.id)));
            scan_list.extend(uncovered);
            boundary_text = build_dynamic_boundaries(cg, &scan_list, named);
        }
    }

    let mut poly_text = String::new();
    {
        const POLY_MIN_FAMILY: usize = 8;
        let mut poly_cands: Vec<(String, Vec<Node>)> = Vec::new();
        for (t, fam) in &flow.token_family {
            if fam.len() < POLY_MIN_FAMILY {
                continue;
            }
            let covered = flow.token_nodes.get(t).is_some_and(|ids| ids.iter().any(|id| path_ids.contains(id)));
            if covered {
                continue;
            }
            poly_cands.push((t.clone(), fam.clone()));
        }
        if !poly_cands.is_empty() {
            poly_text = build_polymorphic_boundaries(cg, &poly_cands, named);
        }
    }

    let synth_lines = collect_synth_links(if has_main { best } else { None })?;
    if !has_main && synth_lines.is_empty() && boundary_text.is_empty() && poly_text.is_empty() {
        return Some(identity_only());
    }
    let mut out: Vec<String> = Vec::new();
    if has_main {
        let b = best.unwrap();
        out.push("**Flow (call path among the symbols you queried)**".to_string());
        out.push(String::new());
        for (i, step) in b.iter().enumerate() {
            if let Some(edge) = &step.edge {
                let when = if i > 0 { when_label(cg, &b[i - 1].node, edge) } else { String::new() };
                let when = if when.is_empty() { String::new() } else { format!(" (when {when})") };
                out.push(format!("   ↓ {}{when}", edge_kind_label(edge)));
            }
            out.push(format!("{}. {} ({}:{})", i + 1, step.node.name, step.node.file_path, step.node.start_line));
        }
        out.push(String::new());
    }
    if !synth_lines.is_empty() {
        out.extend(DISPATCH_LINKS_HEADER.iter().map(|s| s.to_string()));
        out.push(String::new());
        out.extend(synth_lines);
        out.push(String::new());
    }
    if !boundary_text.is_empty() {
        out.push(boundary_text);
    }
    if !poly_text.is_empty() {
        out.push(poly_text);
    }
    out.push("> Full source for these symbols is below — the call flow among them, followed by their bodies.".to_string());
    out.push(String::new());
    Some(FlowResult {
        text: out.join("\n"),
        path_node_ids: path_ids,
        named_node_ids: named_keys(),
        unique_named_node_ids: unique.clone(),
        spine_call_sites,
    })
}

/// Dynamic-boundary surfacing (#687): where a half-connected flow continues at runtime.
pub fn build_dynamic_boundaries(cg: &CodeGraph, scan_list: &[Node], named: &IndexMap<String, Node>) -> String {
    const MAX_NOTES: usize = 4;
    let named_map: HashMap<String, Node> = named.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let reports = find_dynamic_boundaries(&CgHost(cg), cg.project_root(), scan_list, MAX_NOTES, &named_map);
    let mut notes: Vec<String> = Vec::new();
    'reports: for report in &reports {
        if notes.len() >= MAX_NOTES {
            break;
        }
        for site in &report.sites {
            if notes.len() >= MAX_NOTES {
                continue 'reports;
            }
            let m = &site.matched;
            let more = if m.more_sites > 0 {
                format!(" (+{} more such site{} in this body)", m.more_sites, if m.more_sites > 1 { "s" } else { "" })
            } else {
                String::new()
            };
            notes.push(format!(
                "- `{}` ({}:{}) — {}: `{}`{more}",
                report.node.name, report.node.file_path, m.line, m.label, m.snippet
            ));
            let cand = boundary_candidates(site);
            if !cand.is_empty() {
                notes.push(format!("  {cand}"));
            }
        }
    }
    if notes.is_empty() {
        return String::new();
    }
    let node_hint = if is_tool_served("node") { " or codegraph_node" } else { "" };
    let mut out = vec!["**Dynamic boundaries (the static path ends at runtime dispatch)**".to_string(), String::new()];
    out.extend(notes);
    out.push(String::new());
    out.push(format!("> These sites choose their call target at runtime (registry / bus / reflection) — the site shown IS where the flow continues. To follow it, run codegraph_explore{node_hint} on a candidate; source for the sites above is included below."));
    out.push(String::new());
    out.join("\n")
}

const CLASSY: [&str; 6] = ["class", "struct", "interface", "trait", "protocol", "abstract"];

fn classy(n: &Node) -> bool {
    CLASSY.contains(&n.kind.as_str())
}

/// Interface/registry-dispatch announcement for a named method with a large family.
pub fn build_polymorphic_boundaries(cg: &CodeGraph, candidates: &[(String, Vec<Node>)], named: &IndexMap<String, Node>) -> String {
    const MIN_IMPL: usize = 8;
    const MIN_SUPPORT: usize = 2;
    const SAMPLE: usize = 40;
    const MAX_NOTES: usize = 3;
    let rel = |p: &str| p.replace('\\', "/");
    let container_of = |m: &Node| -> Option<Node> {
        let edges = cg.get_incoming_edges(&m.id).ok()?;
        let ce = edges.iter().find(|e| e.kind == EdgeKind::Contains)?;
        cg.get_node(&ce.source).ok().flatten()
    };
    #[derive(PartialEq)]
    enum Member {
        Declared,
        Absent,
        Unknown,
    }
    let member_of_supertype = |type_id: &str, name: &str, callable: bool| -> Member {
        let counts = |kind: &str| !callable || kind == "method" || kind == "function";
        let mut saw_members = false;
        let mut opaque = false;
        let mut seen: HashSet<String> = HashSet::new();
        fn visit(
            cg: &CodeGraph,
            id: &str,
            depth: u32,
            name: &str,
            counts: &dyn Fn(&str) -> bool,
            seen: &mut HashSet<String>,
            saw_members: &mut bool,
            opaque: &mut bool,
        ) -> bool {
            if !seen.insert(id.to_string()) {
                return false;
            }
            let (node, edges) = match (cg.get_node(id), cg.get_outgoing_edges(id)) {
                (Ok(n), Ok(e)) => (n, e),
                _ => return false,
            };
            let mut own = 0;
            for e in &edges {
                if e.kind != EdgeKind::Contains {
                    continue;
                }
                let Some(child) = cg.get_node(&e.target).ok().flatten() else { continue };
                if child.name == name {
                    return true;
                }
                if counts(child.kind.as_str()) {
                    own += 1;
                }
            }
            if own > 0 {
                *saw_members = true;
            } else if node.as_ref().is_some_and(|n| matches!(n.kind.as_str(), "interface" | "protocol" | "trait")) {
                *opaque = true;
            }
            if depth >= 3 {
                return false;
            }
            edges.iter().any(|e| {
                matches!(e.kind, EdgeKind::Extends | EdgeKind::Implements)
                    && visit(cg, &e.target, depth + 1, name, counts, seen, saw_members, opaque)
            })
        }
        let root = cg.get_node(type_id).ok().flatten();
        let namesakes: Vec<Node> = match &root {
            Some(r) => cg
                .get_nodes_by_name(&r.name)
                .map(|ns| ns.into_iter().filter(|n| n.language == r.language && classy(n)).collect())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let declared = visit(cg, type_id, 0, name, &counts, &mut seen, &mut saw_members, &mut opaque)
            || namesakes
                .iter()
                .any(|n| visit(cg, &n.id, 0, name, &counts, &mut seen, &mut saw_members, &mut opaque));
        if declared {
            Member::Declared
        } else if saw_members && !opaque {
            Member::Absent
        } else {
            Member::Unknown
        }
    };
    let mut notes: Vec<String> = Vec::new();
    let mut seen_super: HashSet<String> = HashSet::new();
    for (token, family) in candidates {
        if notes.len() >= MAX_NOTES {
            break;
        }
        let member_name = family.first().map(|n| n.name.clone()).unwrap_or_else(|| token.clone());
        let callable_family = family.first().is_some_and(|n| matches!(n.kind.as_str(), "method" | "function"));
        let mut supers: IndexMap<String, (Node, usize, Vec<Node>)> = IndexMap::new();
        for m in family.iter().take(SAMPLE) {
            let Some(container) = container_of(m) else { continue };
            if !classy(&container) {
                continue;
            }
            let sups: Vec<Node> = match cg.get_outgoing_edges(&container.id) {
                Ok(edges) => edges
                    .iter()
                    .filter(|e| matches!(e.kind, EdgeKind::Implements | EdgeKind::Extends))
                    .filter_map(|e| cg.get_node(&e.target).ok().flatten())
                    .filter(|n| classy(n) && utf16_len(&n.name) >= 3)
                    .collect(),
                Err(_) => Vec::new(),
            };
            for s in sups {
                let entry = supers.entry(s.id.clone()).or_insert_with(|| (s.clone(), 0, Vec::new()));
                entry.1 += 1;
                if entry.2.len() < 6 {
                    entry.2.push(m.clone());
                }
            }
        }
        let mut best: Option<(Node, usize, Vec<Node>)> = None;
        for (node, count, targets) in supers.values() {
            if *count < MIN_SUPPORT {
                continue;
            }
            let impl_count = count_implementers(cg.store(), &node.id).unwrap_or(0);
            if impl_count < MIN_IMPL {
                continue;
            }
            if best.as_ref().is_some_and(|b| impl_count <= b.1) {
                continue;
            }
            if member_of_supertype(&node.id, &member_name, callable_family) == Member::Absent {
                continue;
            }
            best = Some((node.clone(), impl_count, targets.clone()));
        }
        let Some((best_node, best_impl, best_targets)) = best else { continue };
        if !seen_super.insert(best_node.id.clone()) {
            continue;
        }
        let named_names: HashSet<&str> = named.values().map(|n| n.name.as_str()).collect();
        let eg: Vec<String> = best_targets
            .iter()
            .take(4)
            .map(|m| {
                let cont = container_of(m);
                let disp = match &cont {
                    Some(c) => format!("{}.{}", c.name, m.name),
                    None if !m.qualified_name.is_empty() => m.qualified_name.clone(),
                    None => m.name.clone(),
                };
                let mark = if cont.as_ref().is_some_and(|c| named_names.contains(c.name.as_str())) {
                    " ← you named this"
                } else {
                    ""
                };
                format!("`{disp}` ({}:{}){mark}", rel(&m.file_path), m.start_line)
            })
            .collect();
        let more = if best_impl > eg.len() { format!(" +{} more", best_impl - eg.len()) } else { String::new() };
        notes.push(format!(
            "- `{token}` → runtime dispatch to **{best_impl}** types implementing `{}` — the static path ends here, the target is chosen at runtime. e.g. {}{more}",
            best_node.name,
            eg.join(", ")
        ));
    }
    if notes.is_empty() {
        return String::new();
    }
    let mut out = vec!["**Interface dispatch (a named method has many implementations)**".to_string(), String::new()];
    out.extend(notes);
    out.push(String::new());
    out.push("> The method above is dispatched at runtime to one of the listed implementations (a registry / plugin / strategy interface) — there is no single static caller→callee edge; the implementations ARE the continuations. To follow one, run codegraph_explore on a listed target.".to_string());
    out.push(String::new());
    out.join("\n")
}

/// The candidate shortlist for a dispatch site, as one line.
pub fn boundary_candidates(site: &BoundarySite) -> String {
    if site.candidates.is_empty() {
        return site.candidate_note.clone().unwrap_or_default();
    }
    let list: Vec<String> = site
        .candidates
        .iter()
        .map(|c| {
            format!(
                "`{}` ({}:{}){}",
                c.display,
                c.node.file_path,
                c.node.start_line,
                if c.named { " ← you named this" } else { "" }
            )
        })
        .collect();
    format!("candidates for key `{}`: {}", site.matched.key.as_deref().unwrap_or("undefined"), list.join(", "))
}

/// Who depends on each targeted symbol, and which tests cover it — locations only.
pub fn build_blast_radius_section(cg: &CodeGraph, subgraph: &Subgraph, leading_ids: &[String]) -> String {
    const ROOT_CAP: usize = 5;
    const FILE_CAP: usize = 4;
    const MEANINGFUL: [&str; 15] = [
        "function", "method", "class", "interface", "struct", "union", "trait", "protocol", "enum", "type_alias",
        "component", "constant", "variable", "property", "field",
    ];
    let rel = |p: &str| p.replace('\\', "/");
    let ids: IndexSet<&String> = leading_ids.iter().chain(subgraph.roots.iter()).collect();
    let roots: Vec<&Node> = ids
        .into_iter()
        .filter_map(|id| subgraph.nodes.get(id))
        .filter(|n| MEANINGFUL.contains(&n.kind.as_str()))
        .take(ROOT_CAP)
        .collect();
    if roots.is_empty() {
        return String::new();
    }
    let mut entries: Vec<String> = Vec::new();
    for root in roots {
        let callers = cg.get_callers(&root.id, 1).unwrap_or_default();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut uniq: Vec<&Node> = Vec::new();
        for c in &callers {
            if seen.insert(&c.node.id) {
                uniq.push(&c.node);
            }
        }
        if uniq.is_empty() {
            continue;
        }
        let caller_files: Vec<String> = uniq.iter().map(|n| rel(&n.file_path)).collect::<IndexSet<_>>().into_iter().collect();
        let test_files: Vec<&String> = caller_files.iter().filter(|f| is_test_file(f)).collect();
        let non_test: Vec<&String> = caller_files.iter().filter(|f| !is_test_file(f)).collect();
        let shown = non_test.iter().take(FILE_CAP).map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ");
        let more = if non_test.len() > FILE_CAP { format!(" +{} more", non_test.len() - FILE_CAP) } else { String::new() };
        let where_ = if non_test.is_empty() { String::new() } else { format!(" in {shown}{more}") };
        let tests = if !test_files.is_empty() {
            let listed = test_files.iter().take(FILE_CAP).map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ");
            let more = if test_files.len() > FILE_CAP { format!(" +{}", test_files.len() - FILE_CAP) } else { String::new() };
            format!("; tests: {listed}{more}")
        } else {
            indirect_test_note(cg, &uniq)
        };
        entries.push(format!(
            "- `{}` ({}:{}) — {} caller{}{where_}{tests}",
            root.name,
            rel(&root.file_path),
            root.start_line,
            uniq.len(),
            if uniq.len() == 1 { "" } else { "s" }
        ));
    }
    if entries.is_empty() {
        return String::new();
    }
    let mut out = vec!["**Blast radius — what depends on these (update/verify before editing)**".to_string(), String::new()];
    out.extend(entries);
    out.push(String::new());
    out.join("\n")
}

/// The COBOL copybooks a query named: source, and every statement that includes it.
pub fn build_copybook_section(copybooks: &[NamedCopybook]) -> String {
    const MAX_LISTED_SITES: usize = 25;
    let mut blocks: Vec<String> = Vec::new();
    for copybook in copybooks {
        let name = copybook
            .includes
            .first()
            .map(|n| n.name.clone())
            .or_else(|| copybook.files.first().map(|f| re!(r"\.[^.]*$").replace_first(&f.name, |_| String::new())))
            .unwrap_or_else(|| copybook.member.clone());
        let source = if copybook.files.is_empty() {
            "no indexed source (typically a DB2 DCLGEN member, a compiler-supplied copybook such as SQLCA, or one kept outside this project)".to_string()
        } else {
            format!("source {}", copybook.files.iter().map(|f| format!("`{}`", f.file_path)).collect::<Vec<_>>().join(", "))
        };
        let count = copybook.includes.len();
        if count == 0 {
            blocks.push(format!("**COBOL copybook `{name}`** — {source}; nothing indexed includes it (no `COPY` or `EXEC SQL INCLUDE` of it)."));
            blocks.push(String::new());
            continue;
        }
        let sites = format!(
            "{count}{} site{}",
            if count >= MAX_COPYBOOK_INCLUDES as usize { "+" } else { "" },
            if count == 1 { "" } else { "s" }
        );
        blocks.push(format!("**COBOL copybook `{name}`** — {source}; included at {sites}:"));
        blocks.push(String::new());
        for include in copybook.includes.iter().take(MAX_LISTED_SITES) {
            let statement = if re!(r"^\s*EXEC\b", "i").is_match(include.signature.as_deref().unwrap_or("")) {
                "EXEC SQL INCLUDE"
            } else {
                "COPY"
            };
            blocks.push(format!("- `{}:{}` — {statement}", include.file_path, include.start_line));
        }
        if count > MAX_LISTED_SITES {
            let files = copybook.includes[MAX_LISTED_SITES..].iter().map(|n| n.file_path.as_str()).collect::<HashSet<_>>().len();
            let extra = count - MAX_LISTED_SITES;
            blocks.push(format!(
                "- … +{extra} more site{} in {files} file{}",
                if extra == 1 { "" } else { "s" },
                if files == 1 { "" } else { "s" }
            ));
        }
        blocks.push(String::new());
    }
    blocks.join("\n")
}

/// Test coverage up to two more caller hops for an entry no test calls directly.
pub fn indirect_test_note(cg: &CodeGraph, direct_callers: &[&Node]) -> String {
    const MAX_HOPS: u32 = 3;
    const FILE_CAP: usize = 2;
    let mut budget: i64 = 64;
    let mut visited: HashSet<String> = direct_callers.iter().map(|n| n.id.clone()).collect();
    let mut frontier: Vec<Node> = direct_callers.iter().map(|n| (*n).clone()).collect();
    let mut hop = 2;
    while hop <= MAX_HOPS && !frontier.is_empty() && budget > 0 {
        let mut next: Vec<Node> = Vec::new();
        let mut found: IndexSet<String> = IndexSet::new();
        for node in &frontier {
            let left = budget;
            budget -= 1;
            if left <= 0 {
                break;
            }
            let Ok(callers) = cg.get_callers(&node.id, 1) else { continue };
            for c in callers {
                let n = c.node;
                if !visited.insert(n.id.clone()) {
                    continue;
                }
                let f = n.file_path.replace('\\', "/");
                if is_test_file(&f) {
                    found.insert(f);
                } else {
                    next.push(n);
                }
            }
        }
        if !found.is_empty() {
            let files: Vec<&String> = found.iter().collect();
            let shown = files.iter().take(FILE_CAP).map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ");
            let more = if files.len() > FILE_CAP { format!(" +{}", files.len() - FILE_CAP) } else { String::new() };
            return format!("; tested via callers: {shown}{more}");
        }
        frontier = next;
        hop += 1;
    }
    if budget > 0 {
        format!("; no tests found within {MAX_HOPS} caller hops")
    } else {
        "; no test calls this directly".to_string()
    }
}

/// Random-walk-with-restart relevance from the seeds over the call/reference graph.
pub fn compute_graph_relevance(node_ids: &[String], edges: &[Edge], seed_ids: &IndexSet<String>) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    let n = node_ids.len();
    if n == 0 {
        return out;
    }
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for (i, id) in node_ids.iter().enumerate() {
        idx.insert(id, i);
    }
    let rank_edge = |k: EdgeKind| {
        matches!(
            k.as_str(),
            "calls" | "references" | "extends" | "implements" | "overrides" | "instantiates" | "returns" | "type_of"
                | "imports" | "navigates"
        )
    };
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in edges {
        if !rank_edge(e.kind) {
            continue;
        }
        let (Some(&i), Some(&j)) = (idx.get(e.source.as_str()), idx.get(e.target.as_str())) else { continue };
        if i == j {
            continue;
        }
        adj[i].push(j);
        adj[j].push(i);
    }
    let mut r = vec![0.0f64; n];
    let mut rsum = 0.0;
    for id in seed_ids {
        if let Some(&i) = idx.get(id.as_str()) {
            r[i] = 1.0;
            rsum += 1.0;
        }
    }
    if rsum == 0.0 {
        r.iter_mut().for_each(|x| *x = 1.0);
        rsum = n as f64;
    }
    r.iter_mut().for_each(|x| *x /= rsum);
    let alpha = 0.25;
    let mut s = r.clone();
    for _ in 0..25 {
        let mut next = vec![0.0f64; n];
        for i in 0..n {
            let si = s[i];
            if si == 0.0 {
                continue;
            }
            let d = adj[i].len();
            if d == 0 {
                next[i] += si;
                continue;
            }
            let share = si / d as f64;
            for &j in &adj[i] {
                next[j] += share;
            }
        }
        for i in 0..n {
            s[i] = (1.0 - alpha) * next[i] + alpha * r[i];
        }
    }
    for (i, id) in node_ids.iter().enumerate() {
        out.insert(id.clone(), s[i]);
    }
    out
}

/// True when the file on disk differs from what was indexed (its line ranges are untrustworthy).
pub fn is_file_stale_on_disk(cg: &CodeGraph, rel_path: &str, content: Option<&str>) -> bool {
    let Ok(Some(rec)) = cg.get_file(rel_path) else { return false };
    let Some(abs) = validate_path_within_root(cg.project_root(), rel_path, ValidateOptions::default()) else {
        return false;
    };
    let Some(st) = stat_of(&abs) else { return false };
    if st.size == rec.size && st.mtime_ms.floor() == (rec.modified_at as f64).floor() {
        return false;
    }
    // A read failure is not drift: the TypeScript catch reports "not stale".
    let mut unreadable = false;
    let data = codegraph_core::file_limits::indexed_hash_input(st.size, || match content {
        Some(c) => c.to_string(),
        None => match std::fs::read(&abs) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(_) => {
                unreadable = true;
                String::new()
            }
        },
    });
    if unreadable {
        return false;
    }
    let digest = Sha256::digest(data.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex != rec.content_hash
}
