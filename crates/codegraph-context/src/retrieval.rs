//! Hybrid symbol/text retrieval. Channel order and stable ties are part of parity.
use crate::ContextBuilder;
use codegraph_db::{Result, Store};
use codegraph_graph::{
    cobol_copybooks::{find_named_copybooks, is_copybook_include},
    GraphTraverser,
};
use codegraph_jsre::re;
use codegraph_search::{
    is_test_file,
    query_utils::{
        extract_search_terms, get_stem_variants, is_distinctive_identifier, score_path_relevance,
    },
};
use codegraph_types::{
    jscompat::{js_trim, utf16_len},
    nodepath, *,
};
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};

const HIGH_VALUE: &[NodeKind] = &[
    NodeKind::Function,
    NodeKind::Method,
    NodeKind::Class,
    NodeKind::Interface,
    NodeKind::TypeAlias,
    NodeKind::Struct,
    NodeKind::Union,
    NodeKind::Trait,
    NodeKind::Component,
    NodeKind::Route,
    NodeKind::Variable,
    NodeKind::Constant,
    NodeKind::Enum,
    NodeKind::Module,
    NodeKind::Namespace,
];
const DEFINITIONS: &[NodeKind] = &[
    NodeKind::Class,
    NodeKind::Interface,
    NodeKind::Struct,
    NodeKind::Union,
    NodeKind::Trait,
    NodeKind::Protocol,
    NodeKind::Enum,
    NodeKind::TypeAlias,
];
const CALLABLES: &[NodeKind] = &[NodeKind::Function, NodeKind::Method, NodeKind::Component];

pub fn extract_symbols_from_query(query: &str) -> Vec<String> {
    let mut symbols = IndexSet::new();
    for (pattern, flags, minimum) in [
        (
            r"\b([A-Z][a-z]+(?:[A-Z][a-z]*)*|[a-z]+(?:[A-Z][a-z]*)+)\b",
            "g",
            2,
        ),
        (r"\b([a-z][a-z0-9]*(?:_[a-z0-9]+)+)\b", "gi", 3),
        (r"\b([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)\b", "g", 0),
        (r"\b([A-Z]{2,})\b", "g", 0),
    ] {
        for m in codegraph_jsre::Re::cached(pattern, flags).captures_iter(query) {
            if let Some(s) = m.text(1).filter(|s| utf16_len(s) >= minimum) {
                symbols.insert(s.to_string());
            }
        }
    }
    for m in re!(
        r"\b([a-zA-Z][a-zA-Z0-9]*(?:\.[a-zA-Z][a-zA-Z0-9]*)+)\b",
        "g"
    )
    .captures_iter(query)
    {
        if let Some(s) = m.text(1) {
            symbols.insert(s.to_string());
            for part in s.split('.').filter(|s| utf16_len(s) >= 2) {
                symbols.insert(part.to_string());
            }
        }
    }
    for m in re!(r"\b([a-z][a-z0-9]{2,})\b", "g").captures_iter(query) {
        if let Some(s) = m.text(1) {
            symbols.insert(s.to_string());
        }
    }
    const COMMON: &str = "the and for with from this that have been will would could should does done make made use used using work works find found show call called calling get set add all any how what when where which who why not but are was were has had its can did may also into than then them each other some such only same about after before between through during without again further once here there both just more most very being having doing system need needs want wants like look change changes changed changing layer handle handles handling incoming outgoing data flow flows level levels request requests response responses implement implements implementation interface interfaces class classes method methods trigger triggers affected affect affects else code failing failed silently decide decides return returns returned take takes taken check checks checked create creates created read reads write writes written start starts stop stops run runs running";
    let common: HashSet<_> = COMMON.split(' ').collect();
    symbols
        .into_iter()
        .filter(|s| !common.contains(s.to_lowercase().as_str()))
        .collect()
}

fn options(limit: usize, kinds: &[NodeKind]) -> SearchOptions {
    SearchOptions {
        limit: Some(limit),
        kinds: Some(kinds.to_vec()),
        ..Default::default()
    }
}
fn rank(results: &mut [SearchResult]) {
    results.sort_by(|a, b| b.score.total_cmp(&a.score));
}
fn title(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}
fn hierarchy_kind(k: NodeKind) -> bool {
    matches!(
        k,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Struct
            | NodeKind::Union
            | NodeKind::Trait
            | NodeKind::Protocol
    )
}
fn merge_edges(to: &mut Vec<Edge>, edges: Vec<Edge>) {
    let mut keys: HashSet<_> = to
        .iter()
        .map(|e| (e.source.clone(), e.target.clone(), e.kind))
        .collect();
    for e in edges {
        if keys.insert((e.source.clone(), e.target.clone(), e.kind)) {
            to.push(e);
        }
    }
}

impl ContextBuilder<'_> {
    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>> {
        self.store
            .search_nodes_with_deprioritizer(query, opts, self.deprioritized)
    }
    fn path_score(&self, file: &str, query: &str) -> f64 {
        score_path_relevance(
            file,
            query,
            None,
            self.deprioritized.is_some_and(|p| p(file)),
        ) as f64
    }

    pub fn find_relevant_context(
        &self,
        query: &str,
        opts: &FindRelevantContextOptions,
    ) -> Result<Subgraph> {
        let mut graph = Subgraph::default();
        if js_trim(query).is_empty() {
            return Ok(graph);
        }
        let limit = opts.search_limit.unwrap_or(3);
        let max_nodes = opts.max_nodes.unwrap_or(20);
        let kinds = opts.node_kinds.as_deref().unwrap_or(HIGH_VALUE);
        let symbols = extract_symbols_from_query(query);
        let mut exact = Vec::new();
        // Expected search failures degrade to the remaining channels, as in the oracle.
        if !symbols.is_empty() {
            exact = self
                .store
                .find_nodes_by_exact_name(&symbols, &options(limit.saturating_mul(5), kinds))
                .unwrap_or_default();
        }
        if let Some(seeds) = &opts.seed_names {
            let mut known: HashSet<_> = exact.iter().map(|r| r.node.id.clone()).collect();
            for mut r in self
                .store
                .find_nodes_by_exact_name(seeds, &options(limit.saturating_mul(3), kinds))
                .unwrap_or_default()
            {
                if known.insert(r.node.id.clone()) {
                    r.score *= 0.6;
                    exact.push(r);
                }
            }
        }
        if exact.len() > 1 {
            let mut counts: HashMap<String, HashSet<String>> = HashMap::new();
            for r in &exact {
                counts
                    .entry(r.node.file_path.clone())
                    .or_default()
                    .insert(r.node.name.to_lowercase());
            }
            for r in &mut exact {
                r.score += counts[r.node.file_path.as_str()].len().saturating_sub(1) as f64 * 20.0;
            }
            rank(&mut exact);
        }
        exact.truncate(limit.saturating_mul(2));
        let mut expanded: IndexSet<_> = symbols.iter().cloned().collect();
        for sym in &symbols {
            expanded.extend(get_stem_variants(sym));
        }
        for sym in expanded {
            let t = title(&sym);
            if t == sym {
                continue;
            }
            let mut matched = self
                .search(&t, &options(30, DEFINITIONS))?
                .into_iter()
                .filter(|r| r.node.name.to_lowercase().starts_with(&t.to_lowercase()))
                .map(|mut r| {
                    r.score += 15.0
                        + (10.0 - (utf16_len(&r.node.name) as f64 - utf16_len(&t) as f64) / 3.0)
                            .max(0.0);
                    r
                })
                .collect::<Vec<_>>();
            rank(&mut matched);
            for r in matched.into_iter().take(limit) {
                if !exact.iter().any(|e| e.node.id == r.node.id) {
                    exact.push(r);
                }
            }
        }
        if !symbols.is_empty() {
            rank(&mut exact);
            exact.truncate(limit.saturating_mul(3));
        }

        let mut exact_files = Vec::new();
        let fq = js_trim(query);
        if opts
            .node_kinds
            .as_ref()
            .is_none_or(|k| k.is_empty() || k.contains(&NodeKind::File))
            && has_han(fq)
        {
            let mut candidates: Vec<_> = self
                .store
                .get_nodes_by_name(fq)?
                .into_iter()
                .filter(|n| n.kind == NodeKind::File)
                .collect();
            candidates.extend(
                self.store
                    .get_file_nodes_by_name_prefix(&format!("{fq}."))?,
            );
            for node in candidates {
                let base = nodepath::basename(&node.file_path);
                let ext = nodepath::extname(&base);
                let stem = &base[..base.len() - ext.len()];
                if base == fq || stem == fq {
                    exact_files.push(SearchResult {
                        node,
                        score: 1.0,
                        highlights: None,
                    });
                }
            }
        }
        let (copybooks, members) = if opts
            .node_kinds
            .as_ref()
            .is_none_or(|k| k.is_empty() || k.contains(&NodeKind::Import))
        {
            copybook_entries(self.store, query)?
        } else {
            (Vec::new(), HashSet::new())
        };

        let text_results = (|| -> Result<Vec<SearchResult>> {
            let mut hits: IndexMap<String, (SearchResult, usize)> = IndexMap::new();
            let fallback_kinds: Vec<_> = NodeKind::ALL
                .iter()
                .copied()
                .filter(|k| !matches!(k, NodeKind::Import | NodeKind::Parameter))
                .collect();
            let search_kinds = if kinds.is_empty() {
                &fallback_kinds
            } else {
                kinds
            };
            for term in extract_search_terms(query, true)
                .into_iter()
                .filter(|t| !members.contains(&t.to_uppercase()))
            {
                for r in self.search(&term, &options(limit.saturating_mul(2), search_kinds))? {
                    if let Some((existing, n)) = hits.get_mut(&r.node.id) {
                        *n += 1;
                        existing.score = existing.score.max(r.score);
                    } else {
                        hits.insert(r.node.id.clone(), (r, 1));
                    }
                }
            }
            let mut results: Vec<_> = hits
                .into_values()
                .map(|(mut r, n)| {
                    r.score += n.saturating_sub(1) as f64 * 5.0;
                    r
                })
                .collect();
            rank(&mut results);
            results.truncate(limit.saturating_mul(2));
            Ok(results)
        })()
        .unwrap_or_default();
        let mut merged: IndexMap<String, SearchResult> = IndexMap::new();
        for r in exact.iter().cloned().chain(text_results) {
            if let Some(e) = merged.get_mut(&r.node.id) {
                e.score = e.score.max(r.score);
            } else {
                merged.insert(r.node.id.clone(), r);
            }
        }
        let mut results: Vec<_> = merged.into_values().collect();
        let lower = query.to_lowercase();
        let test_query = lower.contains("test") || lower.contains("spec");
        if !test_query {
            for r in &mut results {
                if is_test_file(&r.node.file_path) {
                    r.score *= 0.3;
                }
            }
        }
        if let Ok(Some(d)) = self.store.get_dominant_file() {
            if d.edge_count >= 3 * d.next_edge_count {
                if let Some(slash) = d.file_path.rfind('/').filter(|i| *i > 0) {
                    for r in &mut results {
                        if r.node.file_path.starts_with(&d.file_path[..slash + 1]) {
                            r.score += 25.0;
                        }
                    }
                }
            }
        }
        let boost_terms = extract_search_terms(query, true);
        if boost_terms.len() >= 2 {
            let mut sorted = boost_terms.clone();
            sorted.sort_by_key(|s| std::cmp::Reverse(utf16_len(s)));
            let mut assigned = HashSet::new();
            let mut groups = Vec::new();
            for term in &sorted {
                if !assigned.insert(term.clone()) {
                    continue;
                }
                let mut group = vec![term.clone()];
                for other in &sorted {
                    if !assigned.contains(other) && (term.contains(other) || other.contains(term)) {
                        assigned.insert(other.clone());
                        group.push(other.clone());
                    }
                }
                groups.push(group);
            }
            let exact_ids: HashSet<_> = exact.iter().map(|r| r.node.id.as_str()).collect();
            let distinctive: HashSet<_> = symbols
                .iter()
                .filter(|s| is_distinctive_identifier(s))
                .map(|s| s.to_lowercase())
                .collect();
            let distinctive_ids: HashSet<_> = exact
                .iter()
                .filter(|r| distinctive.contains(&r.node.name.to_lowercase()))
                .map(|r| r.node.id.as_str())
                .collect();
            for r in &mut results {
                let name = r.node.name.to_lowercase();
                let dir = nodepath::dirname(&r.node.file_path).to_lowercase();
                let segments: Vec<_> = dir.split('/').collect();
                let hits = groups
                    .iter()
                    .filter(|g| {
                        g.iter()
                            .any(|t| name.contains(t) || segments.contains(&t.as_str()))
                    })
                    .count();
                if hits >= 2 {
                    r.score *= 1.0 + hits as f64 * 0.5;
                } else if distinctive_ids.contains(r.node.id.as_str()) {
                } else {
                    r.score *= if exact_ids.contains(r.node.id.as_str()) {
                        0.3
                    } else {
                        0.6
                    };
                }
            }
            rank(&mut results);
        }
        self.append_infix_results(query, &symbols, limit, test_query, &mut results)?;
        rank(&mut results);
        results.truncate(limit.saturating_mul(3));
        let mut filtered = resolve_imports(
            self.store,
            results
                .into_iter()
                .filter(|r| r.score >= opts.min_score.unwrap_or(0.3))
                .collect(),
        )?;
        if !exact_files.is_empty() {
            let ids: HashSet<_> = exact_files.iter().map(|r| r.node.id.clone()).collect();
            exact_files.extend(filtered.into_iter().filter(|r| !ids.contains(&r.node.id)));
            filtered = exact_files;
        }
        let copybook_ids: HashSet<_> = copybooks.iter().map(|r| r.node.id.clone()).collect();
        let mut leaf_ids = HashSet::new();
        if !copybooks.is_empty() {
            let matched: HashSet<_> = filtered.iter().map(|r| r.node.id.clone()).collect();
            let others: Vec<_> = filtered
                .into_iter()
                .filter(|r| !copybook_ids.contains(&r.node.id))
                .collect();
            let other_files: HashSet<_> = others.iter().map(|r| &r.node.file_path).collect();
            let mut ordered = copybooks;
            ordered.sort_by_key(|r| {
                if !is_copybook_include(&r.node) {
                    0
                } else if other_files.contains(&r.node.file_path) {
                    1
                } else {
                    2
                }
            });
            if !others.is_empty() {
                ordered.truncate(limit.div_ceil(2));
            }
            for r in &ordered {
                if !matched.contains(&r.node.id) {
                    leaf_ids.insert(r.node.id.clone());
                }
            }
            ordered.extend(others);
            filtered = ordered;
        }
        filtered.truncate(limit);
        let conf_terms: Vec<_> = extract_search_terms(query, false)
            .into_iter()
            .filter(|s| utf16_len(s) >= 3)
            .collect();
        graph.confidence = Some(Confidence::High);
        if conf_terms.len() >= 2 && !filtered.is_empty() {
            let distinctive: HashSet<_> = symbols
                .iter()
                .filter(|s| is_distinctive_identifier(s))
                .map(|s| s.to_lowercase())
                .collect();
            let strong = filtered.iter().any(|r| {
                if copybook_ids.contains(&r.node.id)
                    || distinctive.contains(&r.node.name.to_lowercase())
                {
                    return true;
                }
                let name = r.node.name.to_lowercase();
                let dir = nodepath::dirname(&r.node.file_path).to_lowercase();
                let dirs: Vec<_> = dir.split('/').collect();
                conf_terms
                    .iter()
                    .filter(|t| name.contains(t.as_str()) || dirs.contains(&t.as_str()))
                    .count()
                    >= 2
            });
            if !strong {
                graph.confidence = Some(Confidence::Low);
            }
        }
        for r in &filtered {
            graph.nodes.insert(r.node.id.clone(), r.node.clone());
            graph.roots.push(r.node.id.clone());
        }
        let traverser = GraphTraverser::new(self.store);
        let max_hierarchy = max_nodes.div_ceil(4);
        let mut added = 0;
        for r in &filtered {
            if added >= max_hierarchy {
                break;
            }
            if hierarchy_kind(r.node.kind) {
                let hierarchy = traverser.get_type_hierarchy(&r.node.id)?;
                // The budget holds per node: a widely implemented interface brings its whole
                // descendant tree (IDisposable: hundreds of types).
                for (id, n) in hierarchy.nodes {
                    if !graph.nodes.contains_key(&id) && added < max_hierarchy {
                        graph.nodes.insert(id, n);
                        added += 1;
                    }
                }
                merge_edges(
                    &mut graph.edges,
                    hierarchy.edges.into_iter().filter(|e| graph.nodes.contains_key(&e.source) && graph.nodes.contains_key(&e.target)).collect(),
                );
            }
        }
        if added > 0 {
            let candidates: Vec<_> = graph
                .nodes
                .values()
                .filter(|n| hierarchy_kind(n.kind) && !graph.roots.contains(&n.id))
                .map(|n| n.id.clone())
                .collect();
            for id in candidates {
                if added >= max_hierarchy {
                    break;
                }
                let hierarchy = traverser.get_type_hierarchy(&id)?;
                for (id, n) in hierarchy.nodes {
                    if !graph.nodes.contains_key(&id) && added < max_hierarchy {
                        graph.nodes.insert(id, n);
                        added += 1;
                    }
                }
                merge_edges(
                    &mut graph.edges,
                    hierarchy
                        .edges
                        .into_iter()
                        .filter(|e| {
                            graph.nodes.contains_key(&e.source)
                                && graph.nodes.contains_key(&e.target)
                        })
                        .collect(),
                );
            }
        }
        for r in &filtered {
            if leaf_ids.contains(&r.node.id) {
                continue;
            }
            let walk = traverser.traverse_bfs(
                &r.node.id,
                &TraversalOptions {
                    max_depth: Some(opts.traversal_depth.unwrap_or(1)),
                    direction: Some(Direction::Both),
                    edge_kinds: opts.edge_kinds.clone(),
                    node_kinds: Some(kinds.to_vec()),
                    limit: Some(max_nodes.div_ceil(filtered.len().max(1))),
                    ..Default::default()
                },
            )?;
            for (id, n) in walk.nodes {
                graph.nodes.entry(id).or_insert(n);
            }
            merge_edges(&mut graph.edges, walk.edges);
        }
        trim_graph(&mut graph, max_nodes, test_query);
        let recovered = self.store.find_edges_between_nodes(
            &graph.nodes.keys().cloned().collect::<Vec<_>>(),
            &[
                EdgeKind::Calls,
                EdgeKind::Extends,
                EdgeKind::Implements,
                EdgeKind::References,
                EdgeKind::Overrides,
                EdgeKind::Navigates,
            ],
        )?;
        merge_edges(&mut graph.edges, recovered);
        Ok(graph)
    }

    fn append_infix_results(
        &self,
        query: &str,
        symbols: &[String],
        limit: usize,
        test_query: bool,
        results: &mut Vec<SearchResult>,
    ) -> Result<()> {
        let mut searched = HashSet::new();
        let mut known: HashSet<_> = results.iter().map(|r| r.node.id.clone()).collect();
        let mut camel: IndexMap<String, (SearchResult, usize)> = IndexMap::new();
        for sym in symbols {
            let t = title(sym);
            let key = t.to_lowercase();
            if utf16_len(&t) < 3 || !searched.insert(key.clone()) {
                continue;
            }
            let mut candidates = Vec::new();
            for kinds in [DEFINITIONS, CALLABLES] {
                for mut r in
                    self.store
                        .find_nodes_by_name_substring(&t, &options(200, kinds), true)?
                {
                    let name = &r.node.name;
                    let Some(idx) = name.to_lowercase().find(&key).filter(|i| *i > 0) else {
                        continue;
                    };
                    // Query symbols are ASCII, but names may carry Unicode before the hump.
                    let before = name.get(..idx).and_then(|s| s.chars().next_back());
                    let at = name.get(idx..).and_then(|s| s.chars().next());
                    if !at.is_some_and(|c| c.is_ascii_uppercase())
                        || !before.is_some_and(|c| c.is_ascii_alphabetic())
                        || known.contains(&r.node.id)
                        || (!test_query && is_test_file(&r.node.file_path))
                    {
                        continue;
                    }
                    r.score = 8.0
                        + (6.0 - (utf16_len(name) as f64 - utf16_len(&t) as f64) / 4.0).max(0.0)
                        + self.path_score(&r.node.file_path, query);
                    candidates.push(r);
                }
            }
            rank(&mut candidates);
            for r in candidates
                .into_iter()
                .take(limit.div_ceil(2).saturating_mul(4))
            {
                if let Some((_, n)) = camel.get_mut(&r.node.id) {
                    *n += 1;
                } else {
                    camel.insert(r.node.id.clone(), (r, 1));
                }
            }
        }
        let mut camel: Vec<_> = camel
            .into_values()
            .map(|(mut r, n)| {
                r.score = r.score * (1 + n) as f64 + (n - 1) as f64 * 30.0;
                r
            })
            .collect();
        rank(&mut camel);
        for r in camel.into_iter().take(limit) {
            known.insert(r.node.id.clone());
            results.push(r);
        }
        if symbols.len() >= 2 {
            let mut compound: IndexMap<String, (Node, HashSet<String>)> = IndexMap::new();
            for sym in symbols {
                let t = title(sym);
                if utf16_len(&t) < 3 {
                    continue;
                }
                for kinds in [DEFINITIONS, CALLABLES] {
                    for r in
                        self.store
                            .find_nodes_by_name_substring(&t, &options(200, kinds), false)?
                    {
                        if known.contains(&r.node.id)
                            || (!test_query && is_test_file(&r.node.file_path))
                        {
                            continue;
                        }
                        compound
                            .entry(r.node.id.clone())
                            .or_insert_with(|| (r.node, HashSet::new()))
                            .1
                            .insert(t.clone());
                    }
                }
            }
            let mut compound: Vec<_> = compound
                .into_values()
                .filter(|(_, terms)| terms.len() >= 2)
                .map(|(node, terms)| {
                    let score = 10.0
                        + (terms.len() - 1) as f64 * 20.0
                        + self.path_score(&node.file_path, query)
                        + (6.0 - utf16_len(&node.name) as f64 / 8.0).max(0.0);
                    SearchResult {
                        node,
                        score,
                        highlights: None,
                    }
                })
                .collect();
            rank(&mut compound);
            results.extend(compound.into_iter().take(limit.div_ceil(2)));
        }
        Ok(())
    }
}

fn has_han(s: &str) -> bool {
    static HAN: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\p{scx:Han}").unwrap());
    HAN.is_match(s)
}

fn resolve_imports(store: &Store, results: Vec<SearchResult>) -> Result<Vec<SearchResult>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for r in results {
        if !matches!(r.node.kind, NodeKind::Import | NodeKind::Export) {
            if seen.insert(r.node.id.clone()) {
                out.push(r);
            }
            continue;
        }
        let kind = if r.node.kind == NodeKind::Import {
            EdgeKind::Imports
        } else {
            EdgeKind::Exports
        };
        for e in store.get_outgoing_edges(&r.node.id, &[kind], None)? {
            if let Some(node) = store.get_node_by_id(&e.target)? {
                if seen.insert(node.id.clone()) {
                    out.push(SearchResult {
                        node,
                        score: r.score,
                        highlights: None,
                    });
                }
            }
        }
    }
    Ok(out)
}

fn copybook_entries(store: &Store, query: &str) -> Result<(Vec<SearchResult>, HashSet<String>)> {
    let named = find_named_copybooks(store, query).map_err(|e| match e {
        codegraph_graph::GraphError::Database(d) => d,
        _ => unreachable!(),
    })?;
    let members = named.iter().map(|c| c.member.to_uppercase()).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |node: Node| {
        if seen.insert(node.id.clone()) {
            out.push(SearchResult {
                node,
                score: 1.0,
                highlights: None,
            });
        }
    };
    for c in &named {
        for f in &c.files {
            let ids: Vec<_> = store
                .get_outgoing_edges(&f.id, &[EdgeKind::Contains], None)?
                .into_iter()
                .map(|e| e.target)
                .collect();
            let mut decl: Vec<_> = store
                .get_nodes_by_ids(&ids)?
                .into_values()
                .filter(|n| !matches!(n.kind, NodeKind::Import | NodeKind::Export))
                .collect();
            decl.sort_by(|a, b| {
                a.start_line
                    .cmp(&b.start_line)
                    .then_with(|| a.id.cmp(&b.id))
            });
            for n in decl.into_iter().take(3) {
                add(n);
            }
        }
    }
    for i in 0..named.iter().map(|c| c.includes.len()).max().unwrap_or(0) {
        for c in &named {
            if let Some(n) = c.includes.get(i) {
                add(n.clone());
            }
        }
    }
    Ok((out, members))
}

fn trim_graph(graph: &mut Subgraph, max: usize, test_query: bool) {
    if graph.nodes.len() > max {
        let mut priority: IndexSet<_> = graph.roots.iter().cloned().collect();
        for e in &graph.edges {
            if priority.contains(&e.source) {
                priority.insert(e.target.clone());
            }
            if priority.contains(&e.target) {
                priority.insert(e.source.clone());
            }
        }
        let mut kept = IndexMap::new();
        for id in priority {
            if kept.len() >= max {
                break;
            }
            if let Some(n) = graph.nodes.get(&id) {
                kept.insert(id, n.clone());
            }
        }
        for (id, n) in &graph.nodes {
            if kept.len() >= max {
                break;
            }
            kept.entry(id.clone()).or_insert_with(|| n.clone());
        }
        graph.nodes = kept;
    }
    let per_file = 5.max((max as f64 * 0.2).ceil() as usize);
    let mut files: IndexMap<String, Vec<String>> = IndexMap::new();
    for (id, n) in &graph.nodes {
        files
            .entry(n.file_path.clone())
            .or_default()
            .push(id.clone());
    }
    for mut ids in files.into_values().filter(|ids| ids.len() > per_file) {
        let priority = |id: &String| {
            let k = graph.nodes[id].kind;
            (if graph.roots.contains(id) { 10 } else { 0 })
                + match k {
                    NodeKind::Class
                    | NodeKind::Interface
                    | NodeKind::Struct
                    | NodeKind::Trait
                    | NodeKind::Protocol
                    | NodeKind::Enum => 3,
                    NodeKind::Method | NodeKind::Function => 1,
                    _ => 0,
                }
        };
        ids.sort_by_key(|id| std::cmp::Reverse(priority(id)));
        for id in ids.into_iter().skip(per_file) {
            graph.nodes.shift_remove(&id);
        }
    }
    if !test_query {
        let max_nonprod = 3.max((max as f64 * 0.15).ceil() as usize);
        let ids: Vec<_> = graph
            .nodes
            .values()
            .filter(|n| is_test_file(&n.file_path))
            .map(|n| n.id.clone())
            .collect();
        for id in ids.into_iter().skip(max_nonprod) {
            graph.nodes.shift_remove(&id);
            graph.roots.retain(|r| r != &id);
        }
    }
    graph
        .edges
        .retain(|e| graph.nodes.contains_key(&e.source) && graph.nodes.contains_key(&e.target));
}
