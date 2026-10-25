//! Shared exact/qualified symbol lookup for CLI and MCP. A miss never selects
//! an unrelated fuzzy result. Ranking belongs to the database/search layer;
//! the native Store implements this contract directly.
use crate::{go_package::GoPackageOf, Result};
use codegraph_db::Store;
use codegraph_jsre::re;
use codegraph_types::{IndexMap, Language, Node, NodeKind};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub trait SymbolLookupHost {
    fn get_nodes_by_name(&self, name: &str) -> Result<Vec<Node>>;
    fn get_nodes_by_name_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Node>>;
    fn search_nodes(&self, query: &str, limit: usize) -> Result<Vec<Node>>;
    fn generated_paths(&self, paths: &[String]) -> Result<HashSet<String>>;
}

impl SymbolLookupHost for Store {
    fn get_nodes_by_name(&self, name: &str) -> Result<Vec<Node>> {
        Ok(Store::get_nodes_by_name(self, name)?)
    }
    fn get_nodes_by_name_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Node>> {
        Ok(Store::get_nodes_by_name_prefix(self, prefix, limit)?)
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
    fn generated_paths(&self, paths: &[String]) -> Result<HashSet<String>> {
        let mut generated = self.get_generated_paths_among(paths)?;
        generated.extend(
            paths
                .iter()
                .filter(|p| codegraph_search::is_generated_file(p))
                .cloned(),
        );
        Ok(generated)
    }
}

pub type SearchFn<'a> = &'a dyn Fn(&str, usize) -> Result<Vec<Node>>;

/// Adapter for callers with a custom ranked-search context, such as a
/// project-specific deprioritized-path predicate.
pub struct StoreLookup<'a> {
    pub store: &'a Store,
    pub search: SearchFn<'a>,
}

impl SymbolLookupHost for StoreLookup<'_> {
    fn get_nodes_by_name(&self, name: &str) -> Result<Vec<Node>> {
        Ok(self.store.get_nodes_by_name(name)?)
    }
    fn get_nodes_by_name_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Node>> {
        Ok(self.store.get_nodes_by_name_prefix(prefix, limit)?)
    }
    fn search_nodes(&self, query: &str, limit: usize) -> Result<Vec<Node>> {
        (self.search)(query, limit)
    }
    fn generated_paths(&self, paths: &[String]) -> Result<HashSet<String>> {
        let mut generated = self.store.get_generated_paths_among(paths)?;
        generated.extend(
            paths
                .iter()
                .filter(|p| codegraph_search::is_generated_file(p))
                .cloned(),
        );
        Ok(generated)
    }
}

pub fn is_qualified_symbol(symbol: &str) -> bool {
    symbol.contains(['.', '/']) || symbol.contains("::")
}

pub fn last_qualifier_part(symbol: &str) -> String {
    let no_arity = re!(r"/\d{1,3}$").replace_first(symbol, |_| String::new());
    let no_arity = if no_arity.is_empty() {
        symbol
    } else {
        &no_arity
    };
    re!(r"::|[./]")
        .split(no_arity)
        .into_iter()
        .rfind(|s| !s.is_empty())
        .unwrap_or(symbol)
        .to_owned()
}

fn canonical_scope(s: &str) -> String {
    s.replace("::", ".").replace('/', ".")
}
fn without_extension(s: &str) -> String {
    re!(r"\.[^.]+$").replace_first(s, |_| String::new())
}

pub fn matches_symbol(node: &Node, symbol: &str, go_package_of: Option<GoPackageOf<'_>>) -> bool {
    let arity = re!(r"^(.+)/(\d{1,3})$").captures(symbol);
    let mut symbol = symbol;
    if let Some(c) = &arity {
        if let Some(node_arity) = re!(r"/(\d{1,3})$").captures(&node.qualified_name) {
            if node_arity.text(1) != c.text(2) {
                return false;
            }
            symbol = c.text(1).unwrap();
        }
    }
    if node.name == symbol {
        return true;
    }
    if node.kind == NodeKind::File && without_extension(&node.name) == symbol {
        return true;
    }
    if !is_qualified_symbol(symbol) {
        return false;
    }
    let parts: Vec<_> = re!(r"::|[./]")
        .split(symbol)
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() < 2 || node.name != *parts.last().unwrap() {
        return false;
    }
    if node.qualified_name.contains(&parts.join("::")) {
        return true;
    }
    let canonical_query = canonical_scope(symbol);
    let canonical_node = canonical_scope(&node.qualified_name);
    if canonical_node == canonical_query || canonical_node.ends_with(&format!(".{canonical_query}"))
    {
        return true;
    }
    let hints: Vec<_> = parts[..parts.len() - 1]
        .iter()
        .copied()
        .filter(|p| !matches!(*p, "crate" | "super" | "self"))
        .collect();
    if hints.is_empty() {
        return false;
    }
    let segments: Vec<_> = node
        .file_path
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let in_path = |hint: &&str| {
        segments
            .iter()
            .any(|seg| seg == hint || without_extension(seg) == *hint)
    };
    if hints.iter().all(in_path) {
        return true;
    }
    let canonical_scope = canonical_scope(&node.qualified_name);
    let mut scope: Vec<_> = canonical_scope.split('.').collect();
    scope.pop();
    let mut splits = Vec::new();
    for k in 1..hints.len() {
        let inner = &hints[k..];
        if scope.len() < inner.len() || &scope[scope.len() - inner.len()..] != inner {
            continue;
        }
        if hints[..k].iter().all(in_path) {
            return true;
        }
        splits.push(k);
    }
    if node.language != Language::Go || (!scope.is_empty() && splits.is_empty()) {
        return false;
    }
    let Some(declared) = go_package_of.and_then(|package_of| package_of(&node.file_path)) else {
        return false;
    };
    if declared.is_empty() {
        return false;
    }
    let names_package = |hint: &&str| *hint == declared || in_path(hint);
    if scope.is_empty() && hints.iter().all(names_package) {
        return true;
    }
    splits
        .into_iter()
        .any(|k| hints[..k].iter().all(names_package))
}

pub fn qualified_symbol_matches(
    host: &impl SymbolLookupHost,
    symbol: &str,
    go: Option<GoPackageOf<'_>>,
) -> Result<Vec<Node>> {
    let tail = last_qualifier_part(symbol);
    let mut hits = host.search_nodes(symbol, 50)?;
    if hits.is_empty() && !tail.is_empty() && tail != symbol {
        hits = host.search_nodes(&tail, 50)?;
    }
    let ranked: Vec<_> = hits
        .into_iter()
        .filter(|n| matches_symbol(n, symbol, go))
        .collect();
    if !ranked.is_empty() || tail.is_empty() {
        return Ok(ranked);
    }
    Ok(host
        .get_nodes_by_name(&tail)?
        .into_iter()
        .filter(|n| matches_symbol(n, symbol, go))
        .collect())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolLookupResult {
    pub nodes: Vec<Node>,
    pub ambiguous: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedDefinitions {
    pub groups: Vec<Vec<Node>>,
    pub filtered_out: bool,
}

pub fn group_definitions(nodes: &[Node], file_filter: Option<&str>) -> GroupedDefinitions {
    let mut pool: Vec<_> = nodes.iter().collect();
    let mut filtered_out = false;
    if let Some(filter) = file_filter.filter(|f| !f.is_empty()) {
        let wanted = filter.strip_prefix("./").unwrap_or(filter);
        let narrowed: Vec<_> = pool
            .iter()
            .copied()
            .filter(|n| {
                n.file_path == wanted
                    || n.file_path.ends_with(wanted)
                    || n.file_path.ends_with(&format!("/{wanted}"))
            })
            .collect();
        if narrowed.is_empty() {
            filtered_out = true;
        } else {
            pool = narrowed;
        }
    }
    let mut groups = IndexMap::<String, Vec<Node>>::new();
    for n in pool {
        groups
            .entry(format!("{}|{}", n.file_path, n.qualified_name))
            .or_default()
            .push(n.clone());
    }
    GroupedDefinitions {
        groups: groups.into_values().collect(),
        filtered_out,
    }
}

pub fn lookup_symbol_nodes(
    host: &impl SymbolLookupHost,
    symbol: &str,
    go: Option<GoPackageOf<'_>>,
) -> Result<SymbolLookupResult> {
    let qualified = is_qualified_symbol(symbol);
    let tail = if qualified {
        last_qualifier_part(symbol)
    } else {
        symbol.to_owned()
    };
    let mut nodes = if tail.is_empty() {
        Vec::new()
    } else {
        host.get_nodes_by_name(&tail)?
    };
    if qualified {
        nodes.retain(|n| matches_symbol(n, symbol, go));
    }
    if nodes.is_empty() {
        nodes = host
            .search_nodes(symbol, 50)?
            .into_iter()
            .filter(|n| matches_symbol(n, symbol, go))
            .collect();
    }
    if nodes.is_empty() {
        return Ok(SymbolLookupResult {
            nodes,
            ambiguous: false,
        });
    }
    rank_generated_last(host, &mut nodes)?;
    let ambiguous = group_definitions(&nodes, None).groups.len() > 1;
    Ok(SymbolLookupResult { nodes, ambiguous })
}

pub(crate) fn rank_generated_last(host: &impl SymbolLookupHost, nodes: &mut [Node]) -> Result<()> {
    let paths: Vec<_> = nodes.iter().map(|n| n.file_path.clone()).collect();
    let generated = host.generated_paths(&paths)?;
    nodes.sort_by_key(|n| generated.contains(&n.file_path));
    Ok(())
}

pub fn describe_symbol_node(node: &Node) -> String {
    format!(
        "{} {} ({}) — {}:{}",
        node.kind,
        if node.qualified_name.is_empty() {
            &node.name
        } else {
            &node.qualified_name
        },
        node.language,
        node.file_path,
        node.start_line
    )
}
