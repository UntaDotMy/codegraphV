//! Port of `src/graph/traversal.ts`. Explicit stacks keep long dependency
//! chains safe on Windows while retaining the oracle's depth-first order.

use codegraph_db::{Result, Store};
use codegraph_types::{Direction, Edge, EdgeKind, Node, NodeKind, Ref, Subgraph, TraversalOptions};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

const CALL_KINDS: &[EdgeKind] = &[
    EdgeKind::Calls,
    EdgeKind::References,
    EdgeKind::Imports,
    EdgeKind::Instantiates,
    EdgeKind::Navigates,
];
const TYPE_KINDS: &[EdgeKind] = &[EdgeKind::Extends, EdgeKind::Implements];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathStep {
    pub node: Node,
    pub edge: Option<Edge>,
}

#[derive(Clone, Copy)]
pub struct GraphTraverser<'a> {
    store: &'a Store,
}

impl<'a> GraphTraverser<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store }
    }

    fn adjacent(&self, id: &str, direction: Direction, kinds: &[EdgeKind]) -> Result<Vec<Edge>> {
        match direction {
            Direction::Outgoing => self.store.get_outgoing_edges(id, kinds, None),
            Direction::Incoming => self.store.get_incoming_edges(id, kinds),
            Direction::Both => {
                let mut out = self.store.get_outgoing_edges(id, kinds, None)?;
                out.extend(self.store.get_incoming_edges(id, kinds)?);
                Ok(out)
            }
        }
    }

    pub fn traverse_bfs(&self, start_id: &str, opts: &TraversalOptions) -> Result<Subgraph> {
        let Some(start) = self.store.get_node_by_id(start_id)? else {
            return Ok(Subgraph::default());
        };
        let mut out = Subgraph {
            roots: vec![start_id.to_owned()],
            ..Subgraph::default()
        };
        let mut enqueued = HashSet::from([start.id.clone()]);
        let mut visited = HashSet::new();
        let mut seen_edges = HashSet::new();
        let mut queue = VecDeque::from([(start.clone(), 0u32)]);
        if opts.include_start.unwrap_or(true) {
            out.nodes.insert(start.id.clone(), start);
        }
        let limit = opts.limit.unwrap_or(1000);
        while !queue.is_empty() && out.nodes.len() < limit {
            let (node, depth) = queue.pop_front().unwrap();
            if !visited.insert(node.id.clone()) || depth >= opts.max_depth.unwrap_or(u32::MAX) {
                continue;
            }
            let mut edges = self.adjacent(
                &node.id,
                opts.direction.unwrap_or(Direction::Outgoing),
                opts.edge_kinds.as_deref().unwrap_or(&[]),
            )?;
            edges.sort_by_key(|e| match e.kind {
                EdgeKind::Contains => 0,
                EdgeKind::Calls => 1,
                _ => 2,
            });
            let want: Vec<_> = edges
                .iter()
                .map(|e| other(e, &node.id))
                .filter(|id| !visited.contains(*id) && !enqueued.contains(*id))
                .collect();
            let neighbors = self.store.get_nodes_by_ids(&want)?;
            for edge in edges {
                let id = other(&edge, &node.id);
                let Some(next) = neighbors.get(id).or_else(|| out.nodes.get(id)).cloned() else {
                    continue;
                };
                if !keeps_kind(opts, next.kind) {
                    continue;
                }
                if !visited.contains(id) && !enqueued.contains(id) {
                    if out.nodes.len() >= limit {
                        continue;
                    }
                    enqueued.insert(id.to_owned());
                    out.nodes.insert(next.id.clone(), next.clone());
                    queue.push_back((next, depth + 1));
                }
                if seen_edges.insert(edge_key(&edge)) {
                    out.edges.push(edge);
                }
            }
        }
        Ok(out)
    }

    pub fn traverse_dfs(&self, start_id: &str, opts: &TraversalOptions) -> Result<Subgraph> {
        let Some(start) = self.store.get_node_by_id(start_id)? else {
            return Ok(Subgraph::default());
        };
        let mut out = Subgraph {
            roots: vec![start_id.to_owned()],
            ..Subgraph::default()
        };
        if opts.include_start.unwrap_or(true) {
            out.nodes.insert(start.id.clone(), start.clone());
        }
        // Enter frames are checked when popped, like the recursive implementation.
        // Add frames also run then, rather than pre-adding all siblings.
        enum Frame {
            Enter(Node, u32),
            Add(Node, Edge, u32),
        }
        let mut stack = vec![Frame::Enter(start, 0)];
        let mut visited = HashSet::new();
        let limit = opts.limit.unwrap_or(1000);
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Add(node, edge, depth) => {
                    if out.nodes.len() >= limit
                        || visited.contains(&node.id)
                        || !keeps_kind(opts, node.kind)
                    {
                        continue;
                    }
                    out.nodes.insert(node.id.clone(), node.clone());
                    out.edges.push(edge);
                    stack.push(Frame::Enter(node, depth));
                }
                Frame::Enter(node, depth) => {
                    if visited.contains(&node.id)
                        || out.nodes.len() >= limit
                        || depth >= opts.max_depth.unwrap_or(u32::MAX)
                    {
                        continue;
                    }
                    visited.insert(node.id.clone());
                    let edges = self.adjacent(
                        &node.id,
                        opts.direction.unwrap_or(Direction::Outgoing),
                        opts.edge_kinds.as_deref().unwrap_or(&[]),
                    )?;
                    let want: Vec<_> = edges
                        .iter()
                        .map(|e| other(e, &node.id))
                        .filter(|id| !visited.contains(*id))
                        .collect();
                    let neighbors = self.store.get_nodes_by_ids(&want)?;
                    for edge in edges.into_iter().rev() {
                        if let Some(next) = neighbors.get(other(&edge, &node.id)) {
                            stack.push(Frame::Add(next.clone(), edge, depth + 1));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn get_callers(&self, id: &str, max_depth: u32) -> Result<Vec<Ref>> {
        self.call_walk(id, max_depth, Direction::Incoming)
    }

    pub fn get_callees(&self, id: &str, max_depth: u32) -> Result<Vec<Ref>> {
        self.call_walk(id, max_depth, Direction::Outgoing)
    }

    fn call_walk(&self, id: &str, max_depth: u32, direction: Direction) -> Result<Vec<Ref>> {
        let mut result = Vec::new();
        let mut visited = HashMap::<String, u32>::new();
        let mut reported = HashSet::from([id.to_owned()]);
        let mut stack = vec![(id.to_owned(), 0, None::<Ref>)];
        while let Some((id, depth, step)) = stack.pop() {
            if !nearer(&visited, &id, depth) {
                continue;
            }
            if let Some(step) = step {
                if reported.insert(step.node.id.clone()) {
                    result.push(step);
                }
            }
            visited.insert(id.clone(), depth);
            if depth >= max_depth {
                continue;
            }
            let edges = self.adjacent(&id, direction, CALL_KINDS)?;
            let ids: Vec<_> = edges.iter().map(|e| other(e, &id)).collect();
            let nodes = self.store.get_nodes_by_ids(&ids)?;
            for edge in edges.into_iter().rev() {
                if let Some(node) = nodes.get(other(&edge, &id)) {
                    stack.push((
                        node.id.clone(),
                        depth + 1,
                        Some(Ref {
                            node: node.clone(),
                            edge,
                        }),
                    ));
                }
            }
        }
        Ok(result)
    }

    pub fn get_call_graph(&self, id: &str, depth: u32) -> Result<Subgraph> {
        let Some(focal) = self.store.get_node_by_id(id)? else {
            return Ok(Subgraph::default());
        };
        let mut out = Subgraph {
            roots: vec![id.to_owned()],
            ..Subgraph::default()
        };
        out.nodes.insert(id.to_owned(), focal);
        for r in self
            .get_callers(id, depth)?
            .into_iter()
            .chain(self.get_callees(id, depth)?)
        {
            out.nodes.insert(r.node.id.clone(), r.node);
            out.edges.push(r.edge);
        }
        Ok(out)
    }

    /// Legacy traversal API, including its shared visited set in the upward
    /// and downward passes. For the complete subtype fan use `build_type_hierarchy`.
    pub fn get_type_hierarchy(&self, id: &str) -> Result<Subgraph> {
        let Some(focal) = self.store.get_node_by_id(id)? else {
            return Ok(Subgraph::default());
        };
        let mut out = Subgraph {
            roots: vec![id.to_owned()],
            ..Subgraph::default()
        };
        out.nodes.insert(id.to_owned(), focal);
        for direction in [Direction::Outgoing, Direction::Incoming] {
            // Each direction keeps its own visited set: the ancestor walk marks the focal node
            // first, and a shared set ended the descendant walk before it read one incoming
            // edge, so no type ever had subtypes (#2833).
            let mut visited = HashSet::new();
            let mut stack = vec![(id.to_owned(), None::<Ref>)];
            while let Some((id, step)) = stack.pop() {
                if let Some(step) = step {
                    if out.nodes.contains_key(&id) {
                        continue;
                    }
                    out.nodes.insert(id.clone(), step.node);
                    out.edges.push(step.edge);
                }
                if !visited.insert(id.clone()) {
                    continue;
                }
                let edges = self.adjacent(&id, direction, TYPE_KINDS)?;
                let ids: Vec<_> = edges.iter().map(|e| other(e, &id)).collect();
                let nodes = self.store.get_nodes_by_ids(&ids)?;
                for edge in edges.into_iter().rev() {
                    if let Some(node) = nodes.get(other(&edge, &id)) {
                        stack.push((
                            node.id.clone(),
                            Some(Ref {
                                node: node.clone(),
                                edge,
                            }),
                        ));
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn find_usages(&self, id: &str) -> Result<Vec<Ref>> {
        let edges = self.store.get_incoming_edges(id, &[])?;
        self.refs_for_edges(edges, true)
    }

    pub(crate) fn refs_for_edges(&self, edges: Vec<Edge>, incoming: bool) -> Result<Vec<Ref>> {
        let ids: Vec<_> = edges
            .iter()
            .map(|e| if incoming { &e.source } else { &e.target })
            .collect();
        let nodes = self.store.get_nodes_by_ids(&ids)?;
        Ok(edges
            .into_iter()
            .filter_map(|edge| {
                let node = nodes
                    .get(if incoming { &edge.source } else { &edge.target })?
                    .clone();
                Some(Ref { node, edge })
            })
            .collect())
    }

    pub fn get_impact_radius(&self, id: &str, max_depth: u32) -> Result<Subgraph> {
        let Some(focal) = self.store.get_node_by_id(id)? else {
            return Ok(Subgraph::default());
        };
        let mut out = Subgraph {
            roots: vec![id.to_owned()],
            ..Subgraph::default()
        };
        out.nodes.insert(id.to_owned(), focal);
        let mut visited = HashMap::<String, u32>::new();
        let mut expanded = HashSet::new();
        enum Frame {
            Enter(String, u32),
            Child(Node, Edge, u32),
            Incoming(String, u32, bool),
            Dependency(Node, Edge, u32, bool),
        }
        let mut stack = vec![Frame::Enter(id.to_owned(), 0)];
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Enter(id, depth) => {
                    if !nearer(&visited, &id, depth) {
                        continue;
                    }
                    visited.insert(id.clone(), depth);
                    if depth >= max_depth {
                        continue;
                    }
                    let first = expanded.insert(id.clone());
                    stack.push(Frame::Incoming(id.clone(), depth, first));
                    if self
                        .store
                        .get_node_by_id(&id)?
                        .is_some_and(|n| is_impact_container(n.kind))
                    {
                        let edges =
                            self.store
                                .get_outgoing_edges(&id, &[EdgeKind::Contains], None)?;
                        let ids: Vec<_> = edges.iter().map(|e| &e.target).collect();
                        let nodes = self.store.get_nodes_by_ids(&ids)?;
                        for edge in edges.into_iter().rev() {
                            if let Some(node) = nodes.get(&edge.target) {
                                stack.push(Frame::Child(node.clone(), edge, depth));
                            }
                        }
                    }
                }
                Frame::Child(node, edge, depth) => {
                    if !nearer(&visited, &node.id, depth) {
                        continue;
                    }
                    if !out.nodes.contains_key(&node.id) {
                        out.nodes.insert(node.id.clone(), node.clone());
                        out.edges.push(edge);
                    }
                    stack.push(Frame::Enter(node.id, depth));
                }
                Frame::Incoming(id, depth, first) => {
                    let edges: Vec<_> = self
                        .store
                        .get_incoming_edges(&id, &[])?
                        .into_iter()
                        .filter(|e| e.kind != EdgeKind::Contains)
                        .collect();
                    let ids: Vec<_> = edges.iter().map(|e| &e.source).collect();
                    let nodes = self.store.get_nodes_by_ids(&ids)?;
                    for edge in edges.into_iter().rev() {
                        if let Some(node) = nodes.get(&edge.source) {
                            stack.push(Frame::Dependency(node.clone(), edge, depth + 1, first));
                        }
                    }
                }
                Frame::Dependency(node, edge, depth, first) => {
                    if first {
                        out.edges.push(edge);
                    }
                    if nearer(&visited, &node.id, depth) {
                        out.nodes.insert(node.id.clone(), node.clone());
                        stack.push(Frame::Enter(node.id, depth));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Directed BFS, storing predecessors rather than copying each partial path.
    pub fn find_path(
        &self,
        from: &str,
        to: &str,
        kinds: &[EdgeKind],
    ) -> Result<Option<Vec<PathStep>>> {
        let Some(start) = self.store.get_node_by_id(from)? else {
            return Ok(None);
        };
        if self.store.get_node_by_id(to)?.is_none() {
            return Ok(None);
        }
        let mut steps = HashMap::from([(
            from.to_owned(),
            PathStep {
                node: start,
                edge: None,
            },
        )]);
        let mut queue = VecDeque::from([from.to_owned()]);
        while let Some(id) = queue.pop_front() {
            if id == to {
                let mut path = Vec::new();
                let mut current = to.to_owned();
                loop {
                    let step = steps.remove(&current).unwrap();
                    let parent = step.edge.as_ref().map(|e| e.source.clone());
                    path.push(step);
                    if let Some(p) = parent {
                        current = p;
                    } else {
                        break;
                    }
                }
                path.reverse();
                return Ok(Some(path));
            }
            let edges = self.store.get_outgoing_edges(&id, kinds, None)?;
            let ids: Vec<_> = edges
                .iter()
                .map(|e| &e.target)
                .filter(|id| !steps.contains_key(*id))
                .collect();
            let nodes = self.store.get_nodes_by_ids(&ids)?;
            for edge in edges {
                if steps.contains_key(&edge.target) {
                    continue;
                }
                if let Some(node) = nodes.get(&edge.target) {
                    queue.push_back(node.id.clone());
                    steps.insert(
                        node.id.clone(),
                        PathStep {
                            node: node.clone(),
                            edge: Some(edge),
                        },
                    );
                }
            }
        }
        Ok(None)
    }

    pub fn get_ancestors(&self, id: &str) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        let mut current = id.to_owned();
        let mut visited = HashSet::new();
        while visited.insert(current.clone()) {
            let edges = self
                .store
                .get_incoming_edges(&current, &[EdgeKind::Contains])?;
            let Some(edge) = edges.first() else {
                break;
            };
            let Some(parent) = self.store.get_node_by_id(&edge.source)? else {
                break;
            };
            current = parent.id.clone();
            out.push(parent);
        }
        Ok(out)
    }

    pub fn get_children(&self, id: &str) -> Result<Vec<Node>> {
        let edges = self
            .store
            .get_outgoing_edges(id, &[EdgeKind::Contains], None)?;
        Ok(self
            .refs_for_edges(edges, false)?
            .into_iter()
            .map(|r| r.node)
            .collect())
    }
}

fn other<'a>(edge: &'a Edge, id: &str) -> &'a str {
    if edge.source == id {
        &edge.target
    } else {
        &edge.source
    }
}

fn keeps_kind(opts: &TraversalOptions, kind: NodeKind) -> bool {
    opts.node_kinds
        .as_ref()
        .is_none_or(|k| k.is_empty() || k.contains(&kind))
}

fn nearer(visited: &HashMap<String, u32>, id: &str, depth: u32) -> bool {
    visited.get(id).is_none_or(|d| depth < *d)
}

fn edge_key(e: &Edge) -> (String, String, EdgeKind, Option<u32>, Option<u32>) {
    (e.source.clone(), e.target.clone(), e.kind, e.line, e.column)
}

fn is_impact_container(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Struct
            | NodeKind::Union
            | NodeKind::Trait
            | NodeKind::Protocol
            | NodeKind::Module
            | NodeKind::Enum
    )
}
