//! Where the graph stops — the boundary report, as data.
//! Port of `src/graph/dynamic-boundary-report.ts`.
//!
//! When a flow does not connect, the honest answer is the dispatch site where
//! the static path ends. `dynamic_boundaries` finds those sites in a body; this
//! module is the graph-aware layer on top of it — it reads the bodies off disk,
//! shortlists the candidate runtime targets for a statically-visible dispatch
//! key, and collects the continuations out of the stopping symbol.
//!
//! The verdict lives here once so explore's prose and the viewer's end cap read
//! one answer. Everything is query-time and read-only: the graph is never
//! mutated, no edge is ever guessed, and a fully connected flow never reaches
//! this module — silence beats a wrong edge (#687).

use crate::dynamic_boundaries::{scan_dynamic_dispatch, BoundaryMatch};
use crate::Result;
use codegraph_db::Store;
use codegraph_jsre::{re, utf16_len};
use codegraph_types::paths::{is_within_dir, lexical_path_within_root, resolve_path};
use codegraph_types::{Edge, EdgeKind, Node, NodeKind};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Below this resolution confidence an edge is a name-only guess, not a call.
pub const UNCERTAIN_BELOW: f64 = 0.6;

/// Dispatch sites reported across one scan. Matches explore's bullet budget.
pub const MAX_BOUNDARY_SITES: usize = 4;

/// Bodies read off disk per scan, however many symbols were handed in.
const MAX_SCAN: usize = 8;

/// Total body characters read per scan — a god-function tail must not stall a request.
const MAX_TOTAL_CHARS: usize = 200_000;

/// Candidate runtime targets shortlisted for one dispatch key.
const MAX_CANDIDATES: usize = 4;

/// FTS rows inspected while shortlisting; also the "too generic" threshold.
const CANDIDATE_SEARCH_LIMIT: usize = 12;

/// One plausible runtime target of a keyed dispatch.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryCandidate {
    pub node: Node,
    /// How the candidate should be named. Usually `qualified_name`, but a
    /// typed-bus key resolves to a CLASS whose real target is its handler
    /// method, so the display names that method.
    pub display: String,
    /// The reader already named this symbol: "you were right, here's the wiring".
    pub named: bool,
}

/// A dispatch site: the detector's verdict plus what the graph knows about it.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundarySite {
    pub matched: BoundaryMatch,
    /// Runtime targets for the match's key. Empty when the key is a runtime value.
    pub candidates: Vec<BoundaryCandidate>,
    /// Why there is no shortlist, when a key was visible but nothing could be
    /// narrowed down: "key `id` is too generic to shortlist (12+ matches)".
    pub candidate_note: Option<String>,
}

/// Every dispatch site found in one symbol's body.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeBoundary {
    pub node: Node,
    pub sites: Vec<BoundarySite>,
}

/// One call out of the stopping symbol, and how sure the resolver was of it.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryContinuation {
    pub node: Node,
    pub line: Option<u32>,
    pub confidence: Option<f64>,
}

/// The calls recorded out of a symbol, split by whether the resolver believed
/// them. `uncertain` is the part a flow search deliberately does not follow.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoundaryContinuations {
    pub resolved: Vec<BoundaryContinuation>,
    pub uncertain: Vec<BoundaryContinuation>,
}

/// The queries this derivation needs — satisfied by `codegraph_db::Store`.
pub trait BoundaryGraph {
    fn get_nodes_by_name(&self, name: &str) -> Result<Vec<Node>>;
    fn search_nodes(&self, query: &str, limit: usize) -> Result<Vec<Node>>;
    fn get_node(&self, id: &str) -> Result<Option<Node>>;
    fn get_outgoing_edges(&self, id: &str) -> Result<Vec<Edge>>;
}

impl BoundaryGraph for Store {
    fn get_nodes_by_name(&self, name: &str) -> Result<Vec<Node>> {
        Ok(Store::get_nodes_by_name(self, name)?)
    }
    fn search_nodes(&self, query: &str, limit: usize) -> Result<Vec<Node>> {
        Ok(Store::search_nodes(
            self,
            query,
            &codegraph_types::SearchOptions {
                limit: Some(limit),
                ..Default::default()
            },
        )?
        .into_iter()
        .map(|r| r.node)
        .collect())
    }
    fn get_node(&self, id: &str) -> Result<Option<Node>> {
        Ok(Store::get_node_by_id(self, id)?)
    }
    fn get_outgoing_edges(&self, id: &str) -> Result<Vec<Edge>> {
        Ok(Store::get_outgoing_edges(self, id, &[], None)?)
    }
}

/// Kinds that can be the runtime target of a dispatch.
fn is_callable_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Method | NodeKind::Function | NodeKind::Component | NodeKind::Class
    )
}

/// A conventional handler method on a typed-bus target class — MediatR's
/// `Handle`, a consumer's `Consume`, PHP's `__invoke`.
fn is_handler_method(name: &str) -> bool {
    re!(
        r"^(handle|handleAsync|execute|executeAsync|consume|consumeAsync|run|__invoke)$",
        "i"
    )
    .is_match(name)
}

/// `validatePathWithinRoot`: lexical containment, then symlink-aware
/// containment. A missing file keeps the lexical path (nothing to follow);
/// any other resolution failure rejects it.
fn validate_within_root(project_root: &str, file_path: &str) -> Option<PathBuf> {
    let resolved = lexical_path_within_root(project_root, file_path)?;
    let root = resolve_path(project_root);
    match (
        std::fs::canonicalize(&root),
        std::fs::canonicalize(&resolved),
    ) {
        (Ok(real_root), Ok(real_resolved)) => is_within_dir(
            &real_resolved.to_string_lossy(),
            &real_root.to_string_lossy(),
        )
        .then_some(real_resolved),
        (Err(e), _) | (_, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            Some(PathBuf::from(resolved))
        }
        _ => None,
    }
}

/// Scan the given symbols' bodies for dynamic-dispatch sites, in order.
///
/// `scan_list` is a priority order, not a set: the caller puts the place the
/// flow actually stopped first, then the symbols that were asked for and never
/// reached. Scanning stops at the first of three budgets — sites found, bodies
/// read, characters read — so a question about a god function costs the same as
/// any other. Returns one entry per symbol that yielded at least one site.
pub fn find_dynamic_boundaries<G: BoundaryGraph>(
    graph: &G,
    project_root: &str,
    scan_list: &[Node],
    max_sites: usize,
    named: &HashMap<String, Node>,
) -> Vec<NodeBoundary> {
    let mut out = Vec::new();
    let mut seen_node: HashSet<&str> = HashSet::new();
    let mut seen_site: HashSet<String> = HashSet::new();
    let mut sites = 0usize;
    let mut scanned = 0usize;
    let mut chars_scanned = 0usize;

    for node in scan_list {
        if sites >= max_sites || scanned >= MAX_SCAN || chars_scanned > MAX_TOTAL_CHARS {
            break;
        }
        if seen_node.contains(node.id.as_str()) || node.start_line == 0 || node.end_line == 0 {
            continue;
        }
        seen_node.insert(&node.id);
        let Some(abs) = validate_within_root(project_root, &node.file_path) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let take = node.end_line.saturating_sub(node.start_line) as usize + 1;
        let body: String = content
            .split('\n')
            .skip(node.start_line as usize - 1)
            .take(take)
            .collect::<Vec<_>>()
            .join("\n");
        scanned += 1;
        chars_scanned += utf16_len(&body);

        let mut found = Vec::new();
        for matched in scan_dynamic_dispatch(&body, node.language.as_str(), node.start_line) {
            if sites >= max_sites {
                break;
            }
            let site_key = format!("{}:{}:{}", node.file_path, matched.line, matched.form);
            if !seen_site.insert(site_key) {
                continue;
            }
            let shortlist = match matched.key.as_deref() {
                Some(key) => shortlist_boundary_candidates(
                    graph,
                    key,
                    matched.key_is_type,
                    named,
                    &node.id,
                ),
                None => (Vec::new(), None),
            };
            found.push(BoundarySite {
                matched,
                candidates: shortlist.0,
                candidate_note: shortlist.1,
            });
            sites += 1;
        }
        if !found.is_empty() {
            out.push(NodeBoundary {
                node: node.clone(),
                sites: found,
            });
        }
    }
    out
}

fn normalize_name(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Shortlist the runtime targets a dispatch key could reach.
///
/// Exact conventional names first (`save` → `onSave` / `handleSave`;
/// `CreateCmd` → `CreateCmdHandler`), then FTS, with a normalized-containment
/// post-filter — a shortlist that is mostly wrong is worse than none. Symbols
/// the caller already named sort first and are marked. A key too short or too
/// common to narrow down returns no candidates and a note saying so.
pub fn shortlist_boundary_candidates<G: BoundaryGraph>(
    graph: &G,
    key: &str,
    key_is_type: bool,
    named: &HashMap<String, Node>,
    self_id: &str,
) -> (Vec<BoundaryCandidate>, Option<String>) {
    let key_norm = normalize_name(key);
    if key_norm.chars().count() < 3 {
        return (Vec::new(), None);
    }

    let mut cands: Vec<Node> = Vec::new();
    let mut consider = |n: Option<&Node>| {
        let Some(n) = n else { return };
        if n.id == self_id || !is_callable_kind(n.kind) || cands.iter().any(|c| c.id == n.id) {
            return;
        }
        let name_norm = normalize_name(&n.name);
        if name_norm.chars().count() < 3 {
            return;
        }
        if !name_norm.contains(&key_norm) && !key_norm.contains(&name_norm) {
            return;
        }
        cands.push(n.clone());
    };

    let mut chars = key.chars();
    let cap = match chars.next() {
        Some(c) => {
            let mut s: String = c.to_uppercase().collect();
            s.push_str(chars.as_str());
            s
        }
        None => String::new(),
    };
    let probes: Vec<String> = if key_is_type {
        vec![format!("{key}Handler"), key.to_owned()]
    } else {
        vec![
            key.to_owned(),
            format!("on{cap}"),
            format!("handle{cap}"),
            format!("{key}Handler"),
            format!("handle_{key}"),
        ]
    };
    for probe in &probes {
        if let Ok(nodes) = graph.get_nodes_by_name(probe) {
            for n in &nodes {
                consider(Some(n));
            }
        }
    }

    let mut raw = 0usize;
    if let Ok(results) = graph.search_nodes(key, CANDIDATE_SEARCH_LIMIT) {
        raw = results.len();
        for n in &results {
            consider(Some(n));
        }
    }

    if cands.is_empty() {
        let generic = raw >= CANDIDATE_SEARCH_LIMIT && key.chars().count() < 5;
        return (
            Vec::new(),
            generic.then(|| format!("key `{key}` is too generic to shortlist ({raw}+ matches)")),
        );
    }

    // A constructor candidate duplicates its class: extractors emit constructors
    // as METHOD nodes named like the class (C#/Java `Foo::Foo`) — keep the class.
    let class_keys: HashSet<String> = cands
        .iter()
        .filter(|n| n.kind == NodeKind::Class)
        .map(|n| format!("{}|{}", n.name, n.file_path))
        .collect();
    // The flow's named set holds callables only, so a class whose METHOD the
    // reader named still counts as named — transfer the mark by name.
    let named_names: HashSet<&str> = named.values().map(|n| n.name.as_str()).collect();
    let is_named = |n: &Node| named.contains_key(&n.id) || named_names.contains(n.name.as_str());

    cands.retain(|n| n.kind == NodeKind::Class || !class_keys.contains(&format!("{}|{}", n.name, n.file_path)));
    cands.sort_by(|a, b| is_named(b).cmp(&is_named(a)));
    cands.truncate(MAX_CANDIDATES);

    let candidates = cands
        .into_iter()
        .map(|n| {
            if key_is_type && n.kind == NodeKind::Class {
                if let Some(method) = handler_method_of(graph, &n) {
                    return BoundaryCandidate {
                        display: format!("{}.{}", n.name, method.name),
                        named: is_named(&n),
                        node: method,
                    };
                }
            }
            BoundaryCandidate {
                display: if n.qualified_name.is_empty() {
                    n.name.clone()
                } else {
                    n.qualified_name.clone()
                },
                named: is_named(&n),
                node: n,
            }
        })
        .collect();
    (candidates, None)
}

fn handler_method_of<G: BoundaryGraph>(graph: &G, cls: &Node) -> Option<Node> {
    let edges = graph.get_outgoing_edges(&cls.id).ok()?;
    edges
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Contains)
        .filter_map(|e| graph.get_node(&e.target).ok().flatten())
        .find(|c| c.kind == NodeKind::Method && is_handler_method(&c.name))
}

fn is_continuation_kind(kind: EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::Calls | EdgeKind::Instantiates | EdgeKind::Navigates
    )
}

/// The calls recorded out of a symbol, minus the ones already on the path.
///
/// This is the other half of an honest end cap: calls the resolver was sure of
/// and the path did not need, plus name-only matches under [`UNCERTAIN_BELOW`]
/// that the search deliberately refused to follow. Listing the second kind is
/// the point — an unfollowed guess that stays invisible reads as "there is
/// nothing here", which is the one thing it does not mean. Deduped by target,
/// keeping the first line each was recorded at.
pub fn continuations_from<G: BoundaryGraph>(
    graph: &G,
    node: &Node,
    exclude: &HashSet<String>,
) -> BoundaryContinuations {
    let Ok(edges) = graph.get_outgoing_edges(&node.id) else {
        return BoundaryContinuations::default();
    };
    let mut resolved: Vec<BoundaryContinuation> = Vec::new();
    let mut uncertain: Vec<BoundaryContinuation> = Vec::new();
    for edge in edges {
        if !is_continuation_kind(edge.kind) {
            continue;
        }
        if edge.target == node.id || exclude.contains(&edge.target) {
            continue;
        }
        let confidence = edge
            .metadata
            .as_ref()
            .and_then(|m| m.get("confidence"))
            .and_then(|v| v.as_f64());
        let is_uncertain = confidence.is_some_and(|c| c < UNCERTAIN_BELOW);
        let bucket = if is_uncertain { &mut uncertain } else { &mut resolved };
        if bucket.iter().any(|c| c.node.id == edge.target) {
            continue;
        }
        let Ok(Some(target)) = graph.get_node(&edge.target) else {
            continue;
        };
        bucket.push(BoundaryContinuation {
            node: target,
            line: edge.line,
            confidence,
        });
    }
    let by_line = |a: &BoundaryContinuation, b: &BoundaryContinuation| {
        a.line.unwrap_or(0).cmp(&b.line.unwrap_or(0))
    };
    resolved.sort_by(by_line);
    uncertain.sort_by(by_line);
    BoundaryContinuations { resolved, uncertain }
}
