use crate::formatter::{
    format_context_as_json, format_context_as_markdown_with_generated, slice_utf16,
};
use crate::LOW_CONFIDENCE_MARKER;
use codegraph_db::{Result, Store};
use codegraph_graph::synthesized_hop::describe_synthesized_hop;
use codegraph_types::{
    jscompat::utf16_len,
    paths::{validate_path_within_root, ValidateOptions},
    *,
};
use indexmap::{IndexMap, IndexSet};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
pub enum BuildOutput {
    Context(TaskContext),
    Text(String),
}

pub struct ContextBuilder<'a> {
    pub(crate) project_root: &'a str,
    pub(crate) store: &'a Store,
    pub(crate) deprioritized: Option<&'a dyn Fn(&str) -> bool>,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(project_root: &'a str, store: &'a Store) -> Self {
        Self {
            project_root,
            store,
            deprioritized: None,
        }
    }

    pub fn with_deprioritizer(mut self, predicate: &'a dyn Fn(&str) -> bool) -> Self {
        self.deprioritized = Some(predicate);
        self
    }

    pub fn build_context(
        &self,
        input: &TaskInput,
        opts: &BuildContextOptions,
    ) -> Result<BuildOutput> {
        let context = self.build_task_context(input, opts)?;
        match opts.format.unwrap_or(ContextFormat::Markdown) {
            ContextFormat::Markdown => {
                let paths: Vec<_> = context
                    .subgraph
                    .nodes
                    .values()
                    .map(|n| n.file_path.as_str())
                    .collect();
                let generated = self.store.get_generated_paths_among(&paths)?;
                let mut text = format_context_as_markdown_with_generated(&context, &|p| {
                    generated.contains(p) || codegraph_search::is_generated_file(p)
                });
                text.push_str(&build_call_paths_section(&context.subgraph));
                if context.subgraph.confidence == Some(Confidence::Low) {
                    text.push_str(&low_confidence_note(&context.entry_points));
                }
                Ok(BuildOutput::Text(text))
            }
            ContextFormat::Json => Ok(BuildOutput::Text(format_context_as_json(&context))),
        }
    }

    /// Structured counterpart to `build_context`, without an output-format conversion.
    pub fn build_task_context(
        &self,
        input: &TaskInput,
        opts: &BuildContextOptions,
    ) -> Result<TaskContext> {
        let query = match input {
            TaskInput::Text(s) => s.clone(),
            TaskInput::Titled { title, description } => {
                match description.as_ref().filter(|d| !d.is_empty()) {
                    Some(d) => format!("{title}: {d}"),
                    None => title.clone(),
                }
            }
        };
        let subgraph = self.find_relevant_context(
            &query,
            &FindRelevantContextOptions {
                search_limit: Some(opts.search_limit.unwrap_or(3)),
                traversal_depth: Some(opts.traversal_depth.unwrap_or(1)),
                max_nodes: Some(opts.max_nodes.unwrap_or(20)),
                min_score: Some(opts.min_score.unwrap_or(0.3)),
                ..Default::default()
            },
        )?;
        let entry_points: Vec<_> = subgraph
            .roots
            .iter()
            .filter_map(|id| subgraph.nodes.get(id).cloned())
            .collect();
        let code_blocks = if opts.include_code.unwrap_or(true) {
            self.extract_code_blocks(
                &subgraph,
                opts.max_code_blocks.unwrap_or(5),
                opts.max_code_block_size.unwrap_or(1500),
            )
        } else {
            Vec::new()
        };
        let mut related_files: Vec<_> = subgraph
            .nodes
            .values()
            .map(|n| n.file_path.clone())
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect();
        related_files.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let names = entry_points
            .iter()
            .take(3)
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let rest = if entry_points.len() > 3 {
            format!(" and {} more", entry_points.len() - 3)
        } else {
            String::new()
        };
        let summary = format!("Found {} relevant code symbols across {} files. Key entry points: {names}{rest}. {} relationships identified.", subgraph.nodes.len(), related_files.len(), subgraph.edges.len());
        let stats = TaskContextStats {
            node_count: subgraph.nodes.len(),
            edge_count: subgraph.edges.len(),
            file_count: related_files.len(),
            code_block_count: code_blocks.len(),
            total_code_size: code_blocks.iter().map(|b| utf16_len(&b.content)).sum(),
        };
        Ok(TaskContext {
            query,
            subgraph,
            entry_points,
            code_blocks,
            related_files,
            summary,
            stats,
        })
    }

    pub fn get_code(&self, node_id: &str) -> Result<Option<String>> {
        Ok(self
            .store
            .get_node_by_id(node_id)?
            .as_ref()
            .and_then(|n| self.extract_node_code(n)))
    }

    fn extract_node_code(&self, node: &Node) -> Option<String> {
        // Config leaves expose keys only, including when their file no longer exists.
        if node.kind == NodeKind::Constant
            && matches!(node.language, Language::Yaml | Language::Properties)
        {
            return Some(
                node.signature
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_else(|| {
                        if node.qualified_name.is_empty() {
                            node.name.clone()
                        } else {
                            node.qualified_name.clone()
                        }
                    }),
            );
        }
        let path = validate_path_within_root(
            self.project_root,
            &node.file_path,
            ValidateOptions::default(),
        )?;
        let content = String::from_utf8_lossy(&std::fs::read(path).ok()?).into_owned();
        let lines: Vec<_> = content.split('\n').collect();
        let start = node.start_line.saturating_sub(1) as usize;
        let end = (node.end_line as usize).min(lines.len());
        Some(if start >= end {
            String::new()
        } else {
            lines[start..end].join("\n")
        })
    }

    fn extract_code_blocks(&self, graph: &Subgraph, max: usize, size: usize) -> Vec<CodeBlock> {
        let roots: HashSet<_> = graph.roots.iter().collect();
        let priority = graph
            .roots
            .iter()
            .filter_map(|id| graph.nodes.get(id))
            .chain(graph.nodes.values().filter(|n| {
                !roots.contains(&n.id) && matches!(n.kind, NodeKind::Function | NodeKind::Method)
            }))
            .chain(
                graph
                    .nodes
                    .values()
                    .filter(|n| !roots.contains(&n.id) && n.kind == NodeKind::Class),
            );
        priority
            .filter_map(|n| {
                let code = self.extract_node_code(n).filter(|s| !s.is_empty())?;
                let content = if utf16_len(&code) > size {
                    format!("{}\n... (truncated) ...", slice_utf16(&code, size))
                } else {
                    code
                };
                Some(CodeBlock {
                    content,
                    file_path: n.file_path.clone(),
                    start_line: n.start_line,
                    end_line: n.end_line,
                    language: n.language,
                    node: Some(n.clone()),
                })
            })
            .take(max)
            .collect()
    }
}

fn low_confidence_note(entries: &[Node]) -> String {
    let mut dirs = IndexSet::new();
    for n in entries {
        let dir = n
            .file_path
            .rfind('/')
            .filter(|i| *i > 0)
            .map_or(n.file_path.as_str(), |i| &n.file_path[..i]);
        dirs.insert(dir);
        if dirs.len() >= 4 {
            break;
        }
    }
    let dir_line = if dirs.is_empty() {
        String::new()
    } else {
        format!(
            "\n- `codegraphV_files` a likely area: {}",
            dirs.iter()
                .map(|d| format!("`{d}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!("\n\n{LOW_CONFIDENCE_MARKER}\n\nThis query matched mostly on common words, so the entry points above may be off-target — treat them as a starting point, not a complete answer. For a reliable result:\n- `codegraphV_explore` with the **exact symbol names** you are after (class / function / method names), or\n- `codegraphV_search <name>` for one specific symbol{dir_line}\n\nDo not assume the list above is comprehensive.")
}

pub fn build_call_paths_section(graph: &Subgraph) -> String {
    let mut adj: IndexMap<&str, Vec<&str>> = IndexMap::new();
    for e in &graph.edges {
        if e.kind == EdgeKind::Calls
            && graph.nodes.contains_key(&e.source)
            && graph.nodes.contains_key(&e.target)
        {
            adj.entry(&e.source).or_default().push(&e.target);
        }
    }
    let starts: Vec<_> = if graph.roots.is_empty() {
        adj.keys().copied().take(5).collect()
    } else {
        graph
            .roots
            .iter()
            .map(String::as_str)
            .filter(|id| adj.contains_key(id))
            .take(5)
            .collect()
    };
    let mut chains = Vec::new();
    let mut budget = 2000i32;
    // Six-node bound preserves DFS order without risking an unbounded native stack.
    fn dfs<'a>(
        adj: &IndexMap<&'a str, Vec<&'a str>>,
        path: &mut Vec<&'a str>,
        chains: &mut Vec<Vec<&'a str>>,
        budget: &mut i32,
    ) {
        *budget -= 1;
        if *budget < 0 {
            return;
        }
        let next: Vec<_> = adj
            .get(path.last().unwrap())
            .into_iter()
            .flatten()
            .copied()
            .filter(|t| !path.contains(t))
            .collect();
        if next.is_empty() || path.len() >= 6 {
            if path.len() >= 3 {
                chains.push(path.clone());
            }
            return;
        }
        for t in next {
            path.push(t);
            dfs(adj, path, chains, budget);
            path.pop();
        }
    }
    for s in starts {
        dfs(&adj, &mut vec![s], &mut chains, &mut budget);
    }
    let roots: HashSet<_> = graph.roots.iter().map(String::as_str).collect();
    let count = |c: &Vec<&str>| c.iter().filter(|id| roots.contains(**id)).count();
    chains.retain(|c| count(c) >= 2);
    chains.sort_by(|a, b| count(b).cmp(&count(a)).then(b.len().cmp(&a.len())));
    let mut kept: Vec<Vec<&str>> = Vec::new();
    for c in chains {
        if kept.iter().any(|k| k.join(">").contains(&c.join(">"))) {
            continue;
        }
        kept.push(c);
        if kept.len() >= 3 {
            break;
        }
    }
    if kept.is_empty() {
        return String::new();
    }
    let mut synth = IndexMap::new();
    for e in graph.edges.iter().filter(|e| e.kind == EdgeKind::Calls) {
        if let Some(h) = describe_synthesized_hop(Some(e)) {
            let at = h
                .registered_at
                .map(|a| format!(" @{a}"))
                .unwrap_or_default();
            synth.insert(
                (e.source.as_str(), e.target.as_str()),
                format!("{}{}{at}", h.summary, h.detail),
            );
        }
    }
    let mut has_synth = false;
    let mut lines = vec![
        String::new(),
        "## Call paths".into(),
        String::new(),
        "Execution flow among the key symbols (traced through the call graph):".into(),
        String::new(),
    ];
    for c in kept {
        let mut line = format!("- {}", graph.nodes[c[0]].name);
        for pair in c.windows(2) {
            if let Some(h) = synth.get(&(pair[0], pair[1])) {
                has_synth = true;
                line.push_str(&format!(" →[{h}] {}", graph.nodes[pair[1]].name));
            } else {
                line.push_str(&format!(" → {}", graph.nodes[pair[1]].name));
            }
        }
        lines.push(line);
    }
    lines.push(String::new());
    lines.push(if has_synth { "_Hops marked `[…]` are dynamic dispatch bridged by codegraphV (with the registration site); the rest are direct calls. codegraphV_node any symbol for its body._" }
        else { "_codegraphV_node any symbol above for its source + its own callers/callees._" }.into());
    format!("\n{}\n", lines.join("\n"))
}
