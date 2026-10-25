//! Shared hierarchy derivation from `src/graph/type-hierarchy.ts`.
//! Work is batched per level, including the complete direct-subtype count
//! when a wide hierarchy exceeds the materialization budget.

use codegraph_db::{Result, Store};
use codegraph_types::{Edge, EdgeKind, IndexMap, Node, NodeKind, Provenance};
use icu_collator::{options::CollatorOptions, Collator, CollatorBorrowed};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

pub const HIERARCHY_EDGE_KINDS: &[EdgeKind] = &[EdgeKind::Extends, EdgeKind::Implements];
pub const MAX_ANCESTOR_DEPTH: u32 = 8;
pub const MAX_DESCENDANT_DEPTH: u32 = 6;
pub const MAX_DESCENDANTS: usize = 400;
pub const DISPATCH_MIN_IMPLEMENTERS: usize = 8;
const MAX_OVERRIDE_ANCESTORS: usize = 12;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HierarchyEntry {
    pub node: Node,
    pub depth: u32,
    pub parent_id: String,
    pub relation: EdgeKind,
    pub edge: Edge,
    pub synthesized: bool,
    pub hidden_subtypes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverrideMatch {
    pub member_id: String,
    pub base_id: String,
    pub base_type_id: String,
    pub base_type_name: String,
    pub relation: EdgeKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeHierarchy {
    pub focus: Node,
    pub ancestors: Vec<HierarchyEntry>,
    pub descendants: Vec<HierarchyEntry>,
    pub direct_subtypes: usize,
    pub direct_implementers: usize,
    pub bounded: bool,
    pub polymorphic: bool,
    pub overrides: IndexMap<String, OverrideMatch>,
}

pub fn can_have_hierarchy(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Struct
            | NodeKind::Trait
            | NodeKind::Protocol
            | NodeKind::Enum
            | NodeKind::TypeAlias
            | NodeKind::Union
    )
}

/// `include_overrides` is false for surfaces that only need the subtype fan.
pub fn build_type_hierarchy(
    store: &Store,
    focus: &Node,
    include_overrides: bool,
) -> Result<Option<TypeHierarchy>> {
    if !can_have_hierarchy(focus) {
        return Ok(None);
    }
    let ancestors = walk_ancestors(store, focus)?;
    let (descendants, direct_subtypes, direct_implementers, bounded) =
        walk_descendants(store, focus)?;
    if ancestors.is_empty() && descendants.is_empty() {
        return Ok(None);
    }
    let overrides = if include_overrides {
        match_overrides(store, focus, &ancestors)?
    } else {
        IndexMap::new()
    };
    Ok(Some(TypeHierarchy {
        focus: focus.clone(),
        ancestors,
        descendants,
        direct_subtypes,
        direct_implementers,
        bounded,
        polymorphic: direct_implementers >= DISPATCH_MIN_IMPLEMENTERS,
        overrides,
    }))
}

fn walk_ancestors(store: &Store, focus: &Node) -> Result<Vec<HierarchyEntry>> {
    let mut out = Vec::new();
    let mut seen = HashSet::from([focus.id.clone()]);
    let mut frontier = vec![focus.id.clone()];
    for depth in 1..=MAX_ANCESTOR_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let edges = store.get_outgoing_edges_from(&frontier, HIERARCHY_EDGE_KINDS)?;
        let ids: Vec<_> = edges.iter().map(|e| &e.target).collect();
        let nodes = store.get_nodes_by_ids(&ids)?;
        let mut level = Vec::new();
        for edge in edges {
            let Some(node) = nodes.get(&edge.target) else {
                continue;
            };
            if seen.insert(node.id.clone()) {
                level.push(to_entry(node.clone(), depth, edge.source.clone(), edge));
            }
        }
        sort_level(&mut level);
        frontier = level.iter().map(|e| e.node.id.clone()).collect();
        out.extend(level);
    }
    Ok(out)
}

fn walk_descendants(
    store: &Store,
    focus: &Node,
) -> Result<(Vec<HierarchyEntry>, usize, usize, bool)> {
    let mut entries: Vec<HierarchyEntry> = Vec::new();
    let mut by_id = HashMap::<String, usize>::new();
    let mut seen = HashSet::from([focus.id.clone()]);
    let mut frontier = vec![focus.id.clone()];
    let mut direct_total = 0;
    let mut direct_implementers = 0;
    let mut bounded = false;
    for depth in 1..=MAX_DESCENDANT_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let edges = store.get_incoming_edges_to(&frontier, HIERARCHY_EDGE_KINDS)?;
        if edges.is_empty() {
            break;
        }
        let ids: Vec<_> = edges.iter().map(|e| &e.source).collect();
        let nodes = store.get_nodes_by_ids(&ids)?;
        let mut level: Vec<HierarchyEntry> = Vec::new();
        let mut level_seen = HashSet::new();
        let mut overflow = HashMap::<String, usize>::new();
        for edge in edges {
            let Some(node) = nodes.get(&edge.source) else {
                continue;
            };
            if seen.contains(&node.id) {
                continue;
            }
            if level_seen.contains(&node.id) {
                if let Some(existing) = level.iter_mut().find(|e| e.node.id == node.id) {
                    if existing.relation == EdgeKind::Implements && edge.kind == EdgeKind::Extends {
                        existing.relation = EdgeKind::Extends;
                        existing.synthesized = edge.provenance == Some(Provenance::Heuristic);
                        existing.edge = edge;
                    }
                    continue;
                }
            }
            if depth == 1 {
                direct_total += 1;
                if edge.kind == EdgeKind::Implements {
                    direct_implementers += 1;
                }
            }
            level_seen.insert(node.id.clone());
            if entries.len() + level.len() >= MAX_DESCENDANTS {
                bounded = true;
                *overflow.entry(edge.target).or_default() += 1;
                continue;
            }
            level.push(to_entry(node.clone(), depth, edge.target.clone(), edge));
        }
        for entry in &level {
            seen.insert(entry.node.id.clone());
        }
        sort_level(&mut level);
        for entry in &level {
            by_id.insert(entry.node.id.clone(), entries.len());
            entries.push(entry.clone());
        }
        for (parent_id, count) in overflow {
            if let Some(&idx) = by_id.get(&parent_id) {
                entries[idx].hidden_subtypes += count;
            }
        }
        if bounded {
            break;
        }
        frontier = level.into_iter().map(|e| e.node.id).collect();
        if depth == MAX_DESCENDANT_DEPTH && !frontier.is_empty() {
            for edge in store.get_incoming_edges_to(&frontier, HIERARCHY_EDGE_KINDS)? {
                if seen.contains(&edge.source) {
                    continue;
                }
                bounded = true;
                if let Some(&idx) = by_id.get(&edge.target) {
                    entries[idx].hidden_subtypes += 1;
                }
            }
        }
    }
    Ok((entries, direct_total, direct_implementers, bounded))
}

fn to_entry(node: Node, depth: u32, parent_id: String, edge: Edge) -> HierarchyEntry {
    HierarchyEntry {
        node,
        depth,
        parent_id,
        relation: edge.kind,
        synthesized: edge.provenance == Some(Provenance::Heuristic),
        edge,
        hidden_subtypes: 0,
    }
}

fn sort_level(level: &mut [HierarchyEntry]) {
    level.sort_by(|a, b| {
        (a.relation == EdgeKind::Implements)
            .cmp(&(b.relation == EdgeKind::Implements))
            .then_with(|| locale_compare(&a.node.name, &b.node.name))
            .then_with(|| locale_compare(&a.node.file_path, &b.node.file_path))
            .then_with(|| a.node.start_line.cmp(&b.node.start_line))
    });
}

pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    static COLLATOR: LazyLock<CollatorBorrowed<'static>> = LazyLock::new(|| {
        Collator::try_new(Default::default(), CollatorOptions::default())
            .expect("compiled root collation data")
    });
    COLLATOR.compare(a, b)
}

fn members_of(store: &Store, ids: &[String]) -> Result<Vec<(Node, String)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let edges = store.get_outgoing_edges_from(ids, &[EdgeKind::Contains])?;
    let targets: Vec<_> = edges.iter().map(|e| &e.target).collect();
    let nodes = store.get_nodes_by_ids(&targets)?;
    let rank: HashMap<_, _> = ids.iter().enumerate().map(|(i, id)| (id, i)).collect();
    let mut out: Vec<_> = edges
        .into_iter()
        .filter_map(|e| nodes.get(&e.target).map(|n| (n.clone(), e.source)))
        .collect();
    out.sort_by_key(|(n, owner)| (rank.get(owner).copied().unwrap_or(0), n.start_line));
    Ok(out)
}

fn match_overrides(
    store: &Store,
    focus: &Node,
    ancestors: &[HierarchyEntry],
) -> Result<IndexMap<String, OverrideMatch>> {
    let mut out = IndexMap::new();
    if ancestors.is_empty() {
        return Ok(out);
    }
    let own = members_of(store, std::slice::from_ref(&focus.id))?;
    if own.is_empty() {
        return Ok(out);
    }
    let chain: Vec<_> = ancestors.iter().take(MAX_OVERRIDE_ANCESTORS).collect();
    let ids: Vec<_> = chain.iter().map(|e| e.node.id.clone()).collect();
    let bases = members_of(store, &ids)?;
    let owners: HashMap<_, _> = chain.into_iter().map(|e| (&e.node.id, e)).collect();
    let mut by_name = HashMap::new();
    for (member, owner) in &bases {
        by_name.entry(&member.name).or_insert((member, owner));
    }
    for (member, _) in own {
        if !matches!(
            member.kind,
            NodeKind::Method | NodeKind::Function | NodeKind::Property | NodeKind::Field
        ) {
            continue;
        }
        let Some((base, owner_id)) = by_name.get(&member.name) else {
            continue;
        };
        if base.id == member.id {
            continue;
        }
        let Some(owner) = owners.get(*owner_id) else {
            continue;
        };
        out.insert(
            member.id.clone(),
            OverrideMatch {
                member_id: member.id,
                base_id: base.id.clone(),
                base_type_id: owner.node.id.clone(),
                base_type_name: owner.node.name.clone(),
                relation: owner.relation,
            },
        );
    }
    Ok(out)
}

pub fn count_implementers(store: &Store, type_id: &str) -> Result<usize> {
    let edges = store.get_incoming_edges_to(&[type_id.to_owned()], HIERARCHY_EDGE_KINDS)?;
    Ok(edges
        .into_iter()
        .map(|e| e.source)
        .collect::<HashSet<_>>()
        .len())
}
