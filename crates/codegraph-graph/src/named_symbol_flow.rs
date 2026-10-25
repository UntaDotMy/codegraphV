//! Explore's named-symbol flow finder and the viewer's directed variant.
//! Both use the same overload selection and recorded (including synthesized)
//! edges; no language-specific guesses are made at query time.

use crate::{go_package::GoPackageOf, symbol_lookup::*, GraphTraverser, PathStep, Result};
use codegraph_db::Store;
use codegraph_jsre::re;
use codegraph_types::{
    jscompat::{js_trim, utf16_len},
    Edge, EdgeKind, IndexMap, Language, Node, NodeKind, Provenance,
};
use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const DEFAULT_MAX_HOPS: usize = 7;
pub const DIRECTED_MAX_HOPS: usize = 12;
pub const DEFAULT_MAX_BRIDGE: u32 = 1;
const MAX_SEEDS: usize = 8;
const MAX_TOKENS: usize = 16;
const MAX_NAMED: usize = 40;
const NAMED_VISIT_CAP: usize = 1500;
const DIRECTED_VISIT_CAP: usize = 12_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlowMode {
    #[default]
    Named,
    Directed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedSymbolFlowOptions {
    pub mode: Option<FlowMode>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub max_hops: Option<usize>,
    pub max_bridge: Option<u32>,
    pub max_chains: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowChain {
    pub steps: Vec<PathStep>,
    pub call_sites: IndexMap<String, u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedSymbolFlow {
    pub tokens: Vec<String>,
    pub named: IndexMap<String, Node>,
    pub dyn_named: IndexMap<String, Node>,
    pub token_nodes: IndexMap<String, Vec<String>>,
    pub token_family: IndexMap<String, Vec<Node>>,
    pub unique_named_node_ids: IndexSet<String>,
    pub precise_named_ids: IndexSet<String>,
    pub chains: Vec<FlowChain>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllSymbols {
    pub nodes: Vec<Node>,
    pub note: String,
}

pub fn find_all_symbols(
    host: &impl SymbolLookupHost,
    symbol: &str,
    go: Option<GoPackageOf<'_>>,
) -> Result<AllSymbols> {
    if re!(r"^[a-z][\w'-]*(?:\.[\w'-]+)+$").is_match(symbol) {
        let mut option_hits = host.get_nodes_by_name(&format!("options.{symbol}"))?;
        option_hits.extend(host.get_nodes_by_name(symbol)?);
        option_hits.extend(host.get_nodes_by_name_prefix(&format!("{symbol}."), 12)?);
        let mut seen = HashSet::new();
        let nodes: Vec<_> = option_hits
            .into_iter()
            .filter(|n| n.language == Language::Nix && seen.insert(n.id.clone()))
            .take(10)
            .collect();
        if !nodes.is_empty() {
            return Ok(AllSymbols {
                nodes,
                note: String::new(),
            });
        }
    }
    let mut nodes = if is_qualified_symbol(symbol) {
        qualified_symbol_matches(host, symbol, go)?
    } else {
        host.get_nodes_by_name(symbol)?
    };
    if nodes.is_empty() {
        let suggestions: IndexSet<_> = host
            .search_nodes(symbol, 5)?
            .into_iter()
            .map(|n| n.name)
            .filter(|n| n != symbol)
            .collect();
        let suggestions: Vec<_> = suggestions.into_iter().take(3).collect();
        let note = if suggestions.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n> **Note:** no symbol named \"{symbol}\". Did you mean: {}?",
                suggestions.join(", ")
            )
        };
        return Ok(AllSymbols { nodes, note });
    }
    if nodes.len() == 1 {
        return Ok(AllSymbols {
            nodes,
            note: String::new(),
        });
    }
    rank_generated_last(host, &mut nodes)?;
    let locations: Vec<_> = nodes
        .iter()
        .map(|n| format!("{} at {}:{}", n.kind, n.file_path, n.start_line))
        .collect();
    let note = format!(
        "\n\n> **Note:** Aggregated results across {} symbols named \"{symbol}\": {}",
        nodes.len(),
        locations.join(", ")
    );
    Ok(AllSymbols { nodes, note })
}

pub fn is_flow_callable(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Method | NodeKind::Function | NodeKind::Component | NodeKind::Route
    )
}
pub fn is_flow_edge(kind: EdgeKind) -> bool {
    matches!(kind, EdgeKind::Calls | EdgeKind::Navigates)
}
fn is_dynamic_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Constant | NodeKind::Variable | NodeKind::Field | NodeKind::Property
    )
}

pub fn normalize_token(token: &str) -> String {
    js_trim(&re!(r"\.(?:java|kt|kts|ts|tsx|js|jsx|mjs|cjs|cs|py|go|rb|php|swift|rs|cpp|cc|cxx|c|h|hpp|scala|lua|dart|vue|svelte|astro|erl|hrl)$", "i").replace_first(token, |_| String::new())).to_owned()
}
fn precise_token(token: &str) -> bool {
    re!(r"[._$]|::|/|[a-z][A-Z]|^[A-Z]").is_match(token)
}

pub fn flow_tokens(query: &str) -> Vec<String> {
    let mut tokens = IndexSet::new();
    for token in re!(r"[\s,()[\]]+").split(query) {
        let token = normalize_token(token);
        if utf16_len(&token) >= 3 && re!(r"^[A-Za-z_$][\w$]*(?:(?:::|\.)[\w$]+)*$").is_match(&token)
        {
            tokens.insert(token);
        }
    }
    tokens.into_iter().take(MAX_TOKENS).collect()
}

pub fn resolve_named_tokens(
    store: &Store,
    host: &impl SymbolLookupHost,
    query: &str,
    opts: &NamedSymbolFlowOptions,
    go: Option<GoPackageOf<'_>>,
) -> Result<NamedSymbolFlow> {
    let directed = opts.mode == Some(FlowMode::Directed);
    let tokens = if directed
        && opts.from.as_ref().is_some_and(|s| !s.is_empty())
        && opts.to.as_ref().is_some_and(|s| !s.is_empty())
    {
        IndexSet::from([
            normalize_token(opts.from.as_ref().unwrap()),
            normalize_token(opts.to.as_ref().unwrap()),
        ])
        .into_iter()
        .collect()
    } else {
        flow_tokens(query)
    };
    let mut out = NamedSymbolFlow {
        tokens,
        ..NamedSymbolFlow::default()
    };
    if out.tokens.len() < 2 {
        return Ok(out);
    }
    let mut segments = HashSet::new();
    for token in &out.tokens {
        segments.extend(
            re!(r"::|\.")
                .split(&token.to_lowercase())
                .into_iter()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        );
    }
    for token in &out.tokens {
        let hits = find_all_symbols(host, token, go)?.nodes;
        let candidates: Vec<_> = hits
            .iter()
            .filter(|n| is_flow_callable(n.kind))
            .cloned()
            .collect();
        out.token_family.insert(token.clone(), candidates.clone());
        let specific = candidates.len() <= 3;
        let mut pick: Vec<_> = if specific || directed {
            candidates
        } else {
            candidates
                .into_iter()
                .filter(|n| {
                    let lower = n.qualified_name.to_lowercase();
                    let segs: Vec<_> = re!(r"::|\.")
                        .split(&lower)
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect();
                    segs.len() >= 2 && segments.contains(segs[segs.len() - 2])
                })
                .collect()
        };
        if directed {
            pick.sort_by_key(|n| codegraph_search::is_test_file(&n.file_path));
        }
        pick.truncate(if directed { 12 } else { 6 });
        out.token_nodes
            .insert(token.clone(), pick.iter().map(|n| n.id.clone()).collect());
        let precise = precise_token(token);
        for node in pick {
            if specific {
                out.unique_named_node_ids.insert(node.id.clone());
            }
            if precise {
                out.precise_named_ids.insert(node.id.clone());
            }
            out.named.insert(node.id.clone(), node);
        }
        if out.dyn_named.len() < 12 {
            let mut token_dyn = 0;
            for node in hits {
                if is_flow_callable(node.kind)
                    || !is_dynamic_kind(node.kind)
                    || out.dyn_named.contains_key(&node.id)
                {
                    continue;
                }
                let mut edges = store.get_incoming_edges(&node.id, &[])?;
                edges.extend(store.get_outgoing_edges(&node.id, &[], None)?);
                if edges
                    .iter()
                    .any(|e| e.provenance == Some(Provenance::Heuristic))
                {
                    if precise {
                        out.precise_named_ids.insert(node.id.clone());
                    }
                    out.dyn_named.insert(node.id.clone(), node);
                    token_dyn += 1;
                }
                if out.dyn_named.len() >= 12 || token_dyn >= 4 {
                    break;
                }
            }
        }
        if out.named.len() > MAX_NAMED {
            break;
        }
    }
    Ok(out)
}

#[derive(Clone)]
struct WalkStep {
    prev: Option<String>,
    edge: Option<Edge>,
    node: Node,
    depth: usize,
    streak: u32,
    named: usize,
}

fn walk_calls(
    store: &Store,
    seed: &Node,
    named: &HashSet<String>,
    max_hops: usize,
    max_bridge: u32,
) -> Result<(IndexMap<String, WalkStep>, Vec<String>)> {
    let traverser = GraphTraverser::new(store);
    let mut parent = IndexMap::from([(
        seed.id.clone(),
        WalkStep {
            prev: None,
            edge: None,
            node: seed.clone(),
            depth: 0,
            streak: 0,
            named: usize::from(named.contains(&seed.id)),
        },
    )]);
    let mut queue = vec![seed.id.clone()];
    let mut reached = Vec::new();
    let mut head = 0;
    while head < queue.len() && parent.len() < NAMED_VISIT_CAP {
        let id = queue[head].clone();
        head += 1;
        let at = parent[&id].clone();
        if id != seed.id && named.contains(&id) {
            reached.push(id.clone());
        }
        if at.depth.saturating_add(1) >= max_hops {
            continue;
        }
        for c in traverser.get_callees(&id, 1)? {
            if !is_flow_edge(c.edge.kind) {
                continue;
            }
            let is_named = named.contains(&c.node.id);
            let streak = if is_named {
                0
            } else if c.node.kind == NodeKind::Route {
                at.streak
            } else {
                at.streak + 1
            };
            if streak > max_bridge {
                continue;
            }
            let depth = at.depth + 1;
            let named_on_route = at.named + usize::from(is_named);
            if let Some(prior) = parent.get(&c.node.id) {
                if prior.depth != depth
                    || streak > prior.streak
                    || (streak == prior.streak && named_on_route <= prior.named)
                {
                    continue;
                }
            } else {
                queue.push(c.node.id.clone());
            }
            parent.insert(
                c.node.id.clone(),
                WalkStep {
                    prev: Some(id.clone()),
                    edge: Some(c.edge),
                    node: c.node,
                    depth,
                    streak,
                    named: named_on_route,
                },
            );
        }
    }
    Ok((parent, reached))
}

fn chain_to(parent: &IndexMap<String, WalkStep>, target: &str) -> Vec<PathStep> {
    let mut steps = Vec::new();
    let mut current = Some(target);
    while let Some(id) = current.filter(|id| !id.is_empty()) {
        let Some(at) = parent.get(id) else {
            break;
        };
        steps.push(PathStep {
            node: at.node.clone(),
            edge: at.edge.clone(),
        });
        current = at.prev.as_deref();
    }
    steps.reverse();
    steps
}

fn walk_bidirectional(
    store: &Store,
    seed: &Node,
    sinks: &IndexSet<String>,
    max_hops: usize,
) -> Result<Option<Vec<PathStep>>> {
    if sinks.contains(&seed.id) {
        return Ok(None);
    }
    let traverser = GraphTraverser::new(store);
    let mut forward = IndexMap::from([(
        seed.id.clone(),
        WalkStep {
            prev: None,
            edge: None,
            node: seed.clone(),
            depth: 0,
            streak: 0,
            named: 0,
        },
    )]);
    let mut backward: HashMap<String, Option<(String, Edge)>> = HashMap::new();
    let mut back_nodes = HashMap::new();
    let mut front_f = vec![seed.clone()];
    let mut front_b = Vec::new();
    for id in sinks {
        let Some(node) = store.get_node_by_id(id)? else {
            continue;
        };
        backward.insert(id.clone(), None);
        back_nodes.insert(id.clone(), node.clone());
        front_b.push(node);
    }
    if front_b.is_empty() {
        return Ok(None);
    }
    for _ in 0..max_hops.saturating_sub(1).max(1) {
        if front_f.len() <= front_b.len() {
            if forward.len() > DIRECTED_VISIT_CAP {
                break;
            }
            let mut next = Vec::new();
            for node in front_f {
                for c in traverser.get_callees(&node.id, 1)? {
                    if !is_flow_edge(c.edge.kind) || forward.contains_key(&c.node.id) {
                        continue;
                    }
                    forward.insert(
                        c.node.id.clone(),
                        WalkStep {
                            prev: Some(node.id.clone()),
                            edge: Some(c.edge),
                            node: c.node.clone(),
                            depth: 0,
                            streak: 0,
                            named: 0,
                        },
                    );
                    next.push(c.node);
                }
            }
            if next.is_empty() {
                break;
            }
            front_f = next;
        } else {
            if backward.len() > DIRECTED_VISIT_CAP {
                break;
            }
            let mut next = Vec::new();
            for node in front_b {
                for c in traverser.get_callers(&node.id, 1)? {
                    if !is_flow_edge(c.edge.kind) || backward.contains_key(&c.node.id) {
                        continue;
                    }
                    backward.insert(c.node.id.clone(), Some((node.id.clone(), c.edge)));
                    back_nodes.insert(c.node.id.clone(), c.node.clone());
                    next.push(c.node);
                }
            }
            if next.is_empty() {
                break;
            }
            front_b = next;
        }
        let Some(meet) = forward.keys().find(|id| backward.contains_key(*id)) else {
            continue;
        };
        let mut steps = chain_to(&forward, meet);
        let mut link = backward.get(meet).and_then(Option::as_ref);
        while let Some((next, edge)) = link {
            let Some(node) = back_nodes.get(next) else {
                break;
            };
            steps.push(PathStep {
                node: node.clone(),
                edge: Some(edge.clone()),
            });
            link = backward.get(next).and_then(Option::as_ref);
        }
        if steps.len() < 2
            || steps.len() > max_hops
            || !steps.last().is_some_and(|s| sinks.contains(&s.node.id))
        {
            return Ok(None);
        }
        return Ok(Some(steps));
    }
    Ok(None)
}

pub fn resolve_named_symbol_flow(
    store: &Store,
    host: &impl SymbolLookupHost,
    query: &str,
    opts: &NamedSymbolFlowOptions,
    go: Option<GoPackageOf<'_>>,
) -> Result<NamedSymbolFlow> {
    let directed = opts.mode == Some(FlowMode::Directed);
    let mut flow = resolve_named_tokens(store, host, query, opts, go)?;
    if flow.named.len() < 2 {
        return Ok(flow);
    }
    let max_hops = opts.max_hops.unwrap_or(if directed {
        DIRECTED_MAX_HOPS
    } else {
        DEFAULT_MAX_HOPS
    });
    let max_bridge = opts.max_bridge.unwrap_or(if directed {
        u32::MAX
    } else {
        DEFAULT_MAX_BRIDGE
    });
    let max_chains = opts.max_chains.unwrap_or(1).max(1);
    let named_ids: HashSet<_> = flow.named.keys().cloned().collect();
    let mut found = Vec::new();
    if directed {
        let from = normalize_token(opts.from.as_deref().unwrap_or(""));
        let to = normalize_token(opts.to.as_deref().unwrap_or(""));
        let from_ids = flow.token_nodes.get(&from).cloned().unwrap_or_default();
        let sinks: IndexSet<_> = flow
            .token_nodes
            .get(&to)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        if from_ids.is_empty() || sinks.is_empty() {
            return Ok(flow);
        }
        for id in from_ids {
            if let Some(seed) = flow.named.get(&id) {
                if let Some(steps) = walk_bidirectional(store, seed, &sinks, max_hops)? {
                    found.push(steps);
                }
            }
        }
    } else {
        for seed in flow.named.values().take(MAX_SEEDS) {
            let (parent, reached) = walk_calls(store, seed, &named_ids, max_hops, max_bridge)?;
            let mut deepest: Option<&WalkStep> = None;
            for id in reached {
                let at = &parent[&id];
                if deepest.is_none_or(|d| {
                    at.depth > d.depth || (at.depth == d.depth && at.named > d.named)
                }) {
                    deepest = Some(at);
                }
            }
            if let Some(deepest) = deepest {
                found.push(chain_to(&parent, &deepest.node.id));
            }
        }
    }
    found.sort_by(|a, b| {
        if directed {
            a.len().cmp(&b.len())
        } else {
            b.len().cmp(&a.len())
        }
    });
    let mut kept: Vec<String> = Vec::new();
    for steps in found {
        let key = steps
            .iter()
            .map(|s| s.node.id.as_str())
            .collect::<Vec<_>>()
            .join(">");
        if kept
            .iter()
            .any(|other| other == &key || other.contains(&key))
        {
            continue;
        }
        kept.push(key);
        let mut call_sites = IndexMap::new();
        for pair in steps.windows(2) {
            if let Some(line) = pair[1]
                .edge
                .as_ref()
                .and_then(|e| e.line)
                .filter(|n| *n > 0)
            {
                if !pair[0].node.id.is_empty() {
                    call_sites.entry(pair[0].node.id.clone()).or_insert(line);
                }
            }
        }
        flow.chains.push(FlowChain { steps, call_sites });
        if flow.chains.len() >= max_chains {
            break;
        }
    }
    Ok(flow)
}
