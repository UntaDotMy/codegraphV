//! Higher-level queries, ported from `src/graph/queries.ts`.

use crate::{GraphError, GraphTraverser, Result};
use codegraph_db::Store;
use codegraph_jsre::{escape_regex, Re};
use codegraph_types::{Context, EdgeKind, IndexMap, Node, NodeKind, Subgraph};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeMetrics {
    pub incoming_edge_count: usize,
    pub outgoing_edge_count: usize,
    pub call_count: usize,
    pub caller_count: usize,
    pub child_count: usize,
    pub depth: usize,
}

pub struct GraphQueryManager<'a> {
    store: &'a Store,
    traverser: GraphTraverser<'a>,
}

impl<'a> GraphQueryManager<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self {
            store,
            traverser: GraphTraverser::new(store),
        }
    }

    pub fn get_traverser(&self) -> GraphTraverser<'a> {
        self.traverser
    }

    pub fn get_context(&self, id: &str) -> Result<Context> {
        let focal = self
            .store
            .get_node_by_id(id)?
            .ok_or_else(|| GraphError::NodeNotFound(id.to_owned()))?;
        let ancestors = self.traverser.get_ancestors(id)?;
        let children = self.traverser.get_children(id)?;
        let incoming = self
            .store
            .get_incoming_edges(id, &[])?
            .into_iter()
            .filter(|e| e.kind != EdgeKind::Contains)
            .collect();
        let outgoing = self
            .store
            .get_outgoing_edges(id, &[], None)?
            .into_iter()
            .filter(|e| e.kind != EdgeKind::Contains)
            .collect();
        let incoming_refs = self.traverser.refs_for_edges(incoming, true)?;
        let outgoing_refs = self.traverser.refs_for_edges(outgoing, false)?;
        let mut types = Vec::new();
        let mut seen_types = HashSet::new();
        for kind in [EdgeKind::TypeOf, EdgeKind::Returns] {
            let edges = self.store.get_outgoing_edges(id, &[kind], None)?;
            for r in self.traverser.refs_for_edges(edges, false)? {
                if seen_types.insert(r.node.id.clone()) {
                    types.push(r.node);
                }
            }
        }
        let imports = if let Some(file) = ancestors.iter().find(|n| n.kind == NodeKind::File) {
            self.traverser
                .refs_for_edges(
                    self.store
                        .get_outgoing_edges(&file.id, &[EdgeKind::Imports], None)?,
                    false,
                )?
                .into_iter()
                .map(|r| r.node)
                .collect()
        } else {
            Vec::new()
        };
        Ok(Context {
            focal,
            ancestors,
            children,
            incoming_refs,
            outgoing_refs,
            types,
            imports,
        })
    }

    pub fn get_file_dependencies(&self, file_path: &str) -> Result<Vec<String>> {
        Ok(self.store.get_dependency_file_paths(file_path)?)
    }

    pub fn get_file_dependents(&self, file_path: &str) -> Result<Vec<String>> {
        Ok(self.store.get_dependent_file_paths(file_path)?)
    }

    pub fn get_exported_symbols(&self, file_path: &str) -> Result<Vec<Node>> {
        Ok(self
            .store
            .get_nodes_by_file(file_path)?
            .into_iter()
            .filter(|n| n.is_exported == Some(true))
            .collect())
    }

    pub fn find_by_qualified_name(&self, pattern: &str) -> Result<Vec<Node>> {
        // Keep ECMAScript's non-unicode wildcard behavior, including line
        // terminators and UTF-16 units, through the shared regex compatibility layer.
        let mut expression = String::from("^");
        for c in pattern.chars() {
            match c {
                '*' => expression.push_str(".*"),
                '?' => expression.push('.'),
                _ => expression.push_str(&escape_regex(&c.to_string())),
            }
        }
        expression.push('$');
        let re = Re::new(&expression, "");
        let mut out = Vec::new();
        for kind in [
            NodeKind::Class,
            NodeKind::Union,
            NodeKind::Function,
            NodeKind::Method,
            NodeKind::Interface,
            NodeKind::TypeAlias,
            NodeKind::Variable,
            NodeKind::Constant,
        ] {
            out.extend(
                self.store
                    .get_nodes_by_kind(kind)?
                    .into_iter()
                    .filter(|n| re.is_match(&n.qualified_name)),
            );
        }
        Ok(out)
    }

    pub fn get_module_structure(&self) -> Result<IndexMap<String, Vec<String>>> {
        let mut out = IndexMap::<String, Vec<String>>::new();
        for file in self.store.get_all_files()? {
            let dir = file
                .path
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .filter(|s| !s.is_empty())
                .unwrap_or(".")
                .to_owned();
            out.entry(dir).or_default().push(file.path);
        }
        Ok(out)
    }

    pub fn find_circular_dependencies(&self) -> Result<Vec<Vec<String>>> {
        enum Frame {
            Enter(String),
            Leave(String),
        }
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        let mut path = Vec::<String>::new();
        let mut active = HashMap::new();
        for file in self.store.get_all_files()? {
            if visited.contains(&file.path) {
                continue;
            }
            let mut stack = vec![Frame::Enter(file.path)];
            while let Some(frame) = stack.pop() {
                match frame {
                    Frame::Enter(file) => {
                        if let Some(&start) = active.get(&file) {
                            out.push(path[start..].to_vec());
                            continue;
                        }
                        if !visited.insert(file.clone()) {
                            continue;
                        }
                        active.insert(file.clone(), path.len());
                        path.push(file.clone());
                        stack.push(Frame::Leave(file.clone()));
                        for dep in self.get_file_dependencies(&file)?.into_iter().rev() {
                            stack.push(Frame::Enter(dep));
                        }
                    }
                    Frame::Leave(file) => {
                        active.remove(&file);
                        path.pop();
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn get_node_metrics(&self, id: &str) -> Result<NodeMetrics> {
        let incoming = self.store.get_incoming_edges(id, &[])?;
        let outgoing = self.store.get_outgoing_edges(id, &[], None)?;
        Ok(NodeMetrics {
            incoming_edge_count: incoming.len(),
            outgoing_edge_count: outgoing.len(),
            call_count: outgoing
                .iter()
                .filter(|e| e.kind == EdgeKind::Calls)
                .count(),
            caller_count: incoming
                .iter()
                .filter(|e| e.kind == EdgeKind::Calls)
                .count(),
            child_count: outgoing
                .iter()
                .filter(|e| e.kind == EdgeKind::Contains)
                .count(),
            depth: self.traverser.get_ancestors(id)?.len(),
        })
    }

    /// The legacy unreferenced-symbol API. The richer dead-code report ports
    /// separately; this method deliberately retains its original export rules.
    pub fn find_dead_code(&self, kinds: Option<&[NodeKind]>) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        for kind in kinds.unwrap_or(&[NodeKind::Function, NodeKind::Method, NodeKind::Class]) {
            for node in self.store.get_nodes_by_kind(*kind)? {
                if node.is_exported == Some(true) {
                    continue;
                }
                if self
                    .store
                    .get_incoming_edges(&node.id, &[])?
                    .iter()
                    .all(|e| e.kind == EdgeKind::Contains)
                {
                    out.push(node);
                }
            }
        }
        Ok(out)
    }

    pub fn get_filtered_subgraph(
        &self,
        filter: impl Fn(&Node) -> bool,
        include_edges: bool,
    ) -> Result<Subgraph> {
        let mut out = Subgraph::default();
        for kind in [
            NodeKind::File,
            NodeKind::Module,
            NodeKind::Class,
            NodeKind::Struct,
            NodeKind::Union,
            NodeKind::Interface,
            NodeKind::Trait,
            NodeKind::Function,
            NodeKind::Method,
            NodeKind::Variable,
            NodeKind::Constant,
            NodeKind::Enum,
            NodeKind::TypeAlias,
        ] {
            for node in self.store.get_nodes_by_kind(kind)? {
                if filter(&node) {
                    out.nodes.insert(node.id.clone(), node);
                }
            }
        }
        if include_edges {
            for id in out.nodes.keys() {
                out.edges.extend(
                    self.store
                        .get_outgoing_edges(id, &[], None)?
                        .into_iter()
                        .filter(|e| out.nodes.contains_key(&e.target)),
                );
            }
        }
        Ok(out)
    }
}
