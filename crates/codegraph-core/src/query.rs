//! Read operations of the public `CodeGraph` facade (`src/index.ts`).
use crate::{codegraph::CodeGraph, errors::Result};
use codegraph_context::{BuildOutput, ContextBuilder};
use codegraph_graph::{
    cobol_copybooks::{find_named_copybooks, NamedCopybook},
    NodeMetrics, PathStep,
};
use codegraph_types::*;
use std::collections::HashSet;

impl CodeGraph {
    pub fn get_node(&self, id: &str) -> Result<Option<Node>> {
        Ok(self.store().get_node_by_id(id)?)
    }
    pub fn get_nodes_by_name(&self, name: &str) -> Result<Vec<Node>> {
        Ok(self.store().get_nodes_by_name(name)?)
    }
    pub fn get_nodes_by_file(&self, path: &str) -> Result<Vec<Node>> {
        Ok(self.store().get_nodes_by_file(path)?)
    }
    pub fn get_nodes_by_kind(&self, kind: NodeKind) -> Result<Vec<Node>> {
        Ok(self.store().get_nodes_by_kind(kind)?)
    }
    pub fn get_all_nodes(&self) -> Result<Vec<Node>> {
        Ok(self.store().get_all_nodes()?)
    }
    pub fn get_stats(&self) -> Result<GraphStats> {
        Ok(self.store().get_stats()?)
    }
    pub fn search_nodes(&self, query: &str, options: &SearchOptions) -> Result<Vec<SearchResult>> {
        Ok(self.store().search_nodes_with_deprioritizer(
            query,
            options,
            Some(&self.deprioritized_path_matcher()),
        )?)
    }
    pub fn find_named_copybooks(&self, query: &str) -> Result<Vec<NamedCopybook>> {
        Ok(find_named_copybooks(self.store().as_ref(), query)?)
    }
    pub fn get_segment_matches(&self, words: &[String], limit: usize) -> Result<Vec<SegmentMatch>> {
        Ok(codegraph_context::segments::get_segment_matches(
            self.store(),
            words,
            limit,
        )?)
    }
    pub fn get_project_name_tokens(&self) -> HashSet<String> {
        self.store().project_name_tokens()
    }
    pub fn get_outgoing_edges(&self, id: &str) -> Result<Vec<Edge>> {
        Ok(self.store().get_outgoing_edges(id, &[], None)?)
    }
    pub fn get_incoming_edges(&self, id: &str) -> Result<Vec<Edge>> {
        Ok(self.store().get_incoming_edges(id, &[])?)
    }
    pub fn has_files_under(&self, dir: &str) -> Result<bool> {
        Ok(self.store().has_files_under(dir)?)
    }
    pub fn get_generated_file_count(&self) -> Result<i64> {
        Ok(self.store().count_generated_files()?)
    }
    /// An ambient-declaration test over a bounded candidate list (CG-28).
    pub fn ambient_declaration_file_predicate(&self, paths: &[String]) -> Result<impl Fn(&str) -> bool> {
        let flagged = self.store().get_ambient_declaration_paths_among(paths)?;
        Ok(move |p: &str| flagged.contains(p))
    }
    /// Lexical evidence for an empty explore result; does not alter retrieval.
    pub fn get_explore_miss_diagnostics(&self, query: &str) -> Result<codegraph_db::search_queries::ExploreMissDiagnostics> {
        Ok(self.store().get_explore_miss_diagnostics(query)?)
    }
    /// `getNodesByNameSubstring`: name-substring matches, shortest name first.
    pub fn get_nodes_by_name_substring(&self, substring: &str, kinds: &[NodeKind], limit: usize) -> Result<Vec<Node>> {
        let options = SearchOptions { kinds: Some(kinds.to_vec()), limit: Some(limit), ..Default::default() };
        Ok(self.store().find_nodes_by_name_substring(substring, &options, false)?.into_iter().map(|r| r.node).collect())
    }
    pub fn generated_file_predicate(&self, paths: &[String]) -> Result<impl Fn(&str) -> bool> {
        let generated = self.store().get_generated_paths_among(paths)?;
        Ok(move |p: &str| generated.contains(p) || codegraph_search::is_generated_file(p))
    }
    pub fn get_context(&self, id: &str) -> Result<Context> {
        Ok(self.graph_manager().get_context(id)?)
    }
    pub fn traverse(&self, id: &str, options: &TraversalOptions) -> Result<Subgraph> {
        Ok(self.traverser().traverse_bfs(id, options)?)
    }
    pub fn get_call_graph(&self, id: &str, depth: u32) -> Result<Subgraph> {
        Ok(self.traverser().get_call_graph(id, depth)?)
    }
    pub fn get_type_hierarchy(&self, id: &str) -> Result<Subgraph> {
        Ok(self.traverser().get_type_hierarchy(id)?)
    }
    pub fn find_usages(&self, id: &str) -> Result<Vec<Ref>> {
        Ok(self.traverser().find_usages(id)?)
    }
    pub fn get_callers(&self, id: &str, depth: u32) -> Result<Vec<Ref>> {
        Ok(self.traverser().get_callers(id, depth)?)
    }
    pub fn get_callees(&self, id: &str, depth: u32) -> Result<Vec<Ref>> {
        Ok(self.traverser().get_callees(id, depth)?)
    }
    pub fn get_impact_radius(&self, id: &str, depth: u32) -> Result<Subgraph> {
        Ok(self.traverser().get_impact_radius(id, depth)?)
    }
    pub fn find_path(
        &self,
        from: &str,
        to: &str,
        kinds: &[EdgeKind],
    ) -> Result<Option<Vec<PathStep>>> {
        Ok(self.traverser().find_path(from, to, kinds)?)
    }
    pub fn get_ancestors(&self, id: &str) -> Result<Vec<Node>> {
        Ok(self.traverser().get_ancestors(id)?)
    }
    pub fn get_children(&self, id: &str) -> Result<Vec<Node>> {
        Ok(self.traverser().get_children(id)?)
    }
    pub fn get_file_dependencies(&self, path: &str) -> Result<Vec<String>> {
        Ok(self.graph_manager().get_file_dependencies(path)?)
    }
    pub fn get_file_dependents(&self, path: &str) -> Result<Vec<String>> {
        Ok(self.graph_manager().get_file_dependents(path)?)
    }
    pub fn find_circular_dependencies(&self) -> Result<Vec<Vec<String>>> {
        Ok(self.graph_manager().find_circular_dependencies()?)
    }
    pub fn find_dead_code(&self, kinds: Option<&[NodeKind]>) -> Result<Vec<Node>> {
        Ok(self.graph_manager().find_dead_code(kinds)?)
    }
    pub fn get_node_metrics(&self, id: &str) -> Result<NodeMetrics> {
        Ok(self.graph_manager().get_node_metrics(id)?)
    }
    pub fn get_code(&self, id: &str) -> Result<Option<String>> {
        Ok(ContextBuilder::new(self.project_root(), self.store()).get_code(id)?)
    }
    pub fn find_relevant_context(
        &self,
        query: &str,
        options: &FindRelevantContextOptions,
    ) -> Result<Subgraph> {
        let mut options = options.clone();
        if options.seed_names.is_none() {
            options.seed_names = Some(
                self.get_segment_matches(&codegraph_search::extract_segment_search_words(query), 8)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| m.name)
                    .collect(),
            );
        }
        let deprioritized = self.deprioritized_path_matcher();
        Ok(ContextBuilder::new(self.project_root(), self.store())
            .with_deprioritizer(&deprioritized)
            .find_relevant_context(query, &options)?)
    }
    pub fn build_context(
        &self,
        input: &TaskInput,
        options: &BuildContextOptions,
    ) -> Result<BuildOutput> {
        let deprioritized = self.deprioritized_path_matcher();
        Ok(ContextBuilder::new(self.project_root(), self.store())
            .with_deprioritizer(&deprioritized)
            .build_context(input, options)?)
    }
    pub fn build_task_context(
        &self,
        input: &TaskInput,
        options: &BuildContextOptions,
    ) -> Result<TaskContext> {
        let deprioritized = self.deprioritized_path_matcher();
        Ok(ContextBuilder::new(self.project_root(), self.store())
            .with_deprioritizer(&deprioritized)
            .build_task_context(input, options)?)
    }
}
