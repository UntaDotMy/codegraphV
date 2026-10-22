//! Dead code and islands — port of `src/graph/dead-code.ts`.
//!
//! The graph can answer "does anything reach this symbol?" exactly, and that is
//! the problem: the exact answer is *no incoming edge*, which is not the same as
//! "nobody uses it". So the honest product is two things at once — a list, and
//! everything the list could not see. Every exclusion rule below is subtractive
//! and counted: a candidate drops the moment there is any reason to believe
//! something outside the graph reaches it.
//!
//! Everything here is query-time and read-only.

use crate::Result;
use codegraph_db::Store;
use codegraph_jsre::re;
use codegraph_types::{EdgeKind, Node, NodeKind};
use std::collections::{HashMap, HashSet};

/// Kinds asked about by default: callables and types, and nothing else.
/// `variable`/`constant`/`field` are out deliberately — a value's uses are
/// `references` edges, the most language-dependent coverage in the resolver.
pub fn dead_code_kinds() -> Vec<NodeKind> {
    vec![
        NodeKind::Function,
        NodeKind::Method,
        NodeKind::Class,
        NodeKind::Component,
        NodeKind::Interface,
        NodeKind::Struct,
        NodeKind::Trait,
        NodeKind::Protocol,
        NodeKind::Enum,
        NodeKind::Union,
        NodeKind::TypeAlias,
    ]
}

/// Kinds a `kinds=` request may ask for.
fn is_allowed_kind(kind: NodeKind) -> bool {
    dead_code_kinds().contains(&kind)
        || matches!(
            kind,
            NodeKind::Variable
                | NodeKind::Constant
                | NodeKind::Property
                | NodeKind::Field
                | NodeKind::EnumMember
                | NodeKind::Namespace
                | NodeKind::Module
        )
}

/// Candidates pulled out of SQL before any exclusion runs.
pub const MAX_DEAD_CODE_CANDIDATES: usize = 20_000;
/// Levels walked up looking for an ancestor that declares the same member.
pub const MAX_OVERRIDE_ANCESTOR_DEPTH: usize = 8;
/// Files read for the corroboration pass, and the biggest one read.
pub const MAX_CORROBORATION_FILES: usize = 600;
pub const MAX_CORROBORATION_BYTES: u64 = 2_000_000;

fn is_container_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Struct
            | NodeKind::Trait
            | NodeKind::Protocol
            | NodeKind::Enum
            | NodeKind::Union
            | NodeKind::TypeAlias
    )
}

fn is_declaration_container_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Interface | NodeKind::Trait | NodeKind::Protocol
    )
}

fn is_overridable_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Method | NodeKind::Function | NodeKind::Property | NodeKind::Field
    )
}

fn is_hook_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Function)
}

fn is_by_name_dispatch_language(language: &str) -> bool {
    matches!(language, "typescript" | "tsx")
}

/// Names a language or a runtime calls without anything in the source naming
/// them — where the *language itself* does the calling.
pub fn is_implicit_entry_name(name: &str) -> bool {
    let dunder = re!(r"^__[a-z0-9_]+__$", "i");
    if dunder.is_match(name) {
        return true;
    }
    matches!(
        name.to_lowercase().as_str(),
        "constructor"
            | "main"
            | "init"
            | "deinit"
            | "finalize"
            | "destructor"
            | "dispose"
            | "drop"
            | "default"
            | "tostring"
            | "equals"
            | "gethashcode"
            | "hashcode"
    )
}

pub fn is_test_scope(qualified_name: &str) -> bool {
    qualified_name
        .split(|c: char| matches!(c, '.' | ':' | '/' | '\\' | '#' | '>'))
        .any(|segment| {
            matches!(
                segment.to_lowercase().as_str(),
                "test" | "tests" | "__tests__" | "spec" | "specs" | "testing"
            )
        })
}

fn is_markup_host_language(language: &str) -> bool {
    matches!(
        language,
        "svelte" | "vue" | "astro" | "liquid" | "html" | "razor" | "twig" | "blade" | "erb" | "handlebars"
    )
}

/// Extensions whose contents are declarations for somebody else.
pub fn is_header_file(file_path: &str) -> bool {
    let lower = file_path.to_lowercase();
    [
        ".h", ".hh", ".hpp", ".hxx", ".h++", ".inc", ".d.ts", ".d.mts", ".d.cts", ".pyi", ".pxd",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

/// Directory names that mean "this code is carried, not written here".
pub fn is_vendored_path(file_path: &str) -> bool {
    file_path.replace('\\', "/").split('/').any(|segment| {
        matches!(
            segment.to_lowercase().as_str(),
            "vendor"
                | "vendored"
                | "third_party"
                | "third-party"
                | "thirdparty"
                | "external"
                | "externals"
                | "node_modules"
                | "bower_components"
                | "site-packages"
                | "godeps"
                | "pods"
                | ".venv"
                | "venv"
        )
    })
}

/// One symbol nothing reaches, and what it takes with it.
#[derive(Debug, Clone, PartialEq)]
pub struct DeadCodeEntry {
    pub node: Node,
    /// Members that are themselves unreferenced and live inside `node`.
    pub members: Vec<Node>,
    /// Source lines the entry spans, members included.
    pub lines: usize,
    /// The symbol is exported — only when the caller asked for exports.
    pub exported: bool,
}

/// How many candidates each rule removed, in the order the rules ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeadCodeExclusions {
    pub tests: usize,
    pub generated: usize,
    pub exported: usize,
    pub exports_unknown: usize,
    pub declarations: usize,
    pub decorated: usize,
    pub overriding: usize,
    pub hooks: usize,
    pub implicit: usize,
    pub vendored: usize,
    pub test_scope: usize,
    pub markup: usize,
    pub unreachable_file: usize,
    pub unresolved_name: usize,
    pub ambiguous_name: usize,
    pub mentioned: usize,
    pub unreadable: usize,
    pub nested: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeadCodeReport {
    pub entries: Vec<DeadCodeEntry>,
    /// Entries before `limit` — always the real number.
    pub total: usize,
    pub candidates: usize,
    pub excluded: DeadCodeExclusions,
    pub kinds: Vec<NodeKind>,
    pub include_exported: bool,
    pub bounded: bool,
    pub corroborated: bool,
}

/// How to read a project-relative source file, for the corroboration pass.
pub enum SourceReader<'a> {
    /// Resolve against the project root.
    Default,
    /// Turn the pass off; the report says it did.
    Off,
    /// A caller's own chokepoint.
    Custom(&'a dyn Fn(&str) -> Option<String>),
}

pub struct DeadCodeQuery<'a> {
    pub kinds: Option<&'a [NodeKind]>,
    pub include_exported: bool,
    pub include_tests: bool,
    pub include_generated: bool,
    pub limit: usize,
    pub read_source: SourceReader<'a>,
    pub project_root: &'a str,
}

impl Default for DeadCodeQuery<'_> {
    fn default() -> Self {
        DeadCodeQuery {
            kinds: None,
            include_exported: false,
            include_tests: false,
            include_generated: false,
            limit: 200,
            read_source: SourceReader::Default,
            project_root: ".",
        }
    }
}

fn normalize_kinds(requested: Option<&[NodeKind]>) -> Vec<NodeKind> {
    let Some(requested) = requested.filter(|k| !k.is_empty()) else {
        return dead_code_kinds();
    };
    let mut seen = HashSet::new();
    let kinds: Vec<NodeKind> = requested
        .iter()
        .copied()
        .filter(|k| is_allowed_kind(*k) && seen.insert(*k))
        .collect();
    if kinds.is_empty() {
        dead_code_kinds()
    } else {
        kinds
    }
}

/// The dead code report: unreferenced symbols, minus every reason to doubt it,
/// plus a count of every doubt.
pub fn build_dead_code_report(store: &Store, query: &DeadCodeQuery) -> Result<DeadCodeReport> {
    let kinds = normalize_kinds(query.kinds);
    let include_exported = query.include_exported;
    let include_tests = query.include_tests;
    let include_generated = query.include_generated;
    let limit = query.limit.max(1);
    let reader: Option<Box<dyn Fn(&str) -> Option<String> + '_>> = match &query.read_source {
        SourceReader::Off => None,
        SourceReader::Custom(f) => Some(Box::new(*f)),
        SourceReader::Default => Some(default_source_reader(query.project_root)),
    };

    let mut excluded = DeadCodeExclusions::default();

    let raw = store.get_unreferenced_nodes(&kinds, MAX_DEAD_CODE_CANDIDATES + 1)?;
    let bounded = raw.len() > MAX_DEAD_CODE_CANDIDATES;
    let candidates: Vec<_> = if bounded {
        raw.into_iter().take(MAX_DEAD_CODE_CANDIDATES).collect()
    } else {
        raw
    };
    let candidate_count = candidates.len();
    // The hook rule reads the whole raw candidate set, before any exclusion.
    let candidate_nodes: Vec<Node> = candidates.iter().map(|row| row.node.clone()).collect();

    let languages: Vec<String> = candidates
        .iter()
        .map(|row| row.node.language.as_str().to_owned())
        .collect();
    let languages_with_exports = if include_exported {
        HashSet::new()
    } else {
        store.get_languages_with_exports(&languages)?
    };

    // ---- the cheap, per-row rules -------------------------------------------
    let mut surviving: Vec<(Node, bool)> = Vec::new();
    for row in &candidates {
        let node = &row.node;
        if !include_tests && codegraph_search::is_test_file(&node.file_path) {
            excluded.tests += 1;
            continue;
        }
        if !include_generated && row.generated {
            excluded.generated += 1;
            continue;
        }
        if !include_exported
            && (node.is_exported == Some(true) || is_header_file(&node.file_path))
        {
            excluded.exported += 1;
            continue;
        }
        if !include_exported && !languages_with_exports.contains(node.language.as_str()) {
            excluded.exports_unknown += 1;
            continue;
        }
        if node.is_abstract == Some(true) {
            excluded.declarations += 1;
            continue;
        }
        if is_implicit_entry_name(&node.name) {
            excluded.implicit += 1;
            continue;
        }
        if is_vendored_path(&node.file_path) {
            excluded.vendored += 1;
            continue;
        }
        if !include_tests && is_test_scope(&node.qualified_name) {
            excluded.test_scope += 1;
            continue;
        }
        if is_markup_host_language(node.language.as_str()) {
            excluded.markup += 1;
            continue;
        }
        surviving.push((node.clone(), row.generated));
    }

    // ---- the rules that need the graph --------------------------------------
    let surviving_nodes: Vec<Node> = surviving.iter().map(|(n, _)| n.clone()).collect();
    let decorated = carries_decorator(store, &surviving_nodes)?;
    let containers = containers_of(store, &surviving_nodes)?;
    let overriding = override_candidates(store, &surviving_nodes, &containers)?;
    let file_paths: Vec<String> = surviving.iter().map(|(n, _)| n.file_path.clone()).collect();
    let unreachable = files_nothing_reaches(store, &file_paths)?;
    let before_unreachable = surviving.len();
    surviving.retain(|(node, _)| !unreachable.contains(&node.file_path));
    excluded.unreachable_file += before_unreachable - surviving.len();

    let names: Vec<String> = surviving.iter().map(|(n, _)| n.name.clone()).collect();
    let unresolved = store.get_unresolved_names_among(&names)?;
    let ambiguous = store.get_ambiguous_referenced_names(&names)?;
    let hook_scope = surviving
        .iter()
        .map(|(n, _)| n.clone())
        .filter(|n| !decorated.contains(&n.id) && !overriding.contains(&n.id))
        .collect::<Vec<_>>();
    let hooks = by_name_hook_candidates(store, &hook_scope, &containers, &candidate_nodes)?;

    let mut kept: Vec<(Node, bool)> = Vec::new();
    for row in surviving {
        let node = row.0;
        if decorated.contains(&node.id) {
            excluded.decorated += 1;
            continue;
        }
        if let Some(container) = containers.get(&node.id) {
            if is_declaration_container_kind(container.kind) {
                excluded.declarations += 1;
                continue;
            }
        }
        if overriding.contains(&node.id) {
            excluded.overriding += 1;
            continue;
        }
        if hooks.contains(&node.id) {
            excluded.hooks += 1;
            continue;
        }
        if unresolved.contains(&node.name) {
            excluded.unresolved_name += 1;
            continue;
        }
        if ambiguous.contains(&node.name) {
            excluded.ambiguous_name += 1;
            continue;
        }
        kept.push((node, row.1));
    }

    // ---- the file's own text has the last word -------------------------------
    let mut confirmed: Vec<(Node, bool)> = Vec::new();
    if let Some(reader) = reader {
        let mut sources: HashMap<String, Option<String>> = HashMap::new();
        let mut read = |file: &str| -> Option<String> {
            if let Some(cached) = sources.get(file) {
                return cached.clone();
            }
            let value = if sources.len() >= MAX_CORROBORATION_FILES {
                None
            } else {
                reader(file)
            };
            sources.insert(file.to_owned(), value.clone());
            value
        };
        let mut scopes: HashMap<String, Vec<String>> = HashMap::new();
        for (node, _) in &kept {
            if !scopes.contains_key(&node.file_path) {
                let mut scope = vec![node.file_path.clone()];
                scope.extend(store.get_file_dependents(&node.file_path)?);
                scopes.insert(node.file_path.clone(), scope);
            }
        }
        for (node, generated) in kept {
            let scope = scopes
                .get(&node.file_path)
                .cloned()
                .unwrap_or_else(|| vec![node.file_path.clone()]);
            let mut own: Option<String> = None;
            let mut mentions = 0usize;
            for file in &scope {
                let source = read(file);
                if file == &node.file_path {
                    own = source.clone();
                }
                if let Some(source) = source {
                    mentions += mention_count(&source, &node.name, 2 - mentions.min(2));
                    if mentions >= 2 {
                        break;
                    }
                }
            }
            if own.is_none() {
                excluded.unreadable += 1;
                continue;
            }
            if mentions >= 2 {
                excluded.mentioned += 1;
                continue;
            }
            confirmed.push((node, generated));
        }
    } else {
        confirmed = kept;
    }

    // ---- fold members into a container that is itself dead -------------------
    let kept_ids: HashSet<String> = confirmed.iter().map(|(n, _)| n.id.clone()).collect();
    let mut entries: HashMap<String, DeadCodeEntry> = HashMap::new();
    let mut pending: Vec<(Node, String)> = Vec::new();
    for (node, _) in confirmed {
        if let Some(container) = containers.get(&node.id) {
            if kept_ids.contains(&container.id) {
                pending.push((node, container.id.clone()));
                excluded.nested += 1;
                continue;
            }
        }
        entries.insert(
            node.id.clone(),
            DeadCodeEntry {
                lines: (node.end_line.saturating_sub(node.start_line) as usize + 1).max(1),
                exported: node.is_exported == Some(true),
                node,
                members: Vec::new(),
            },
        );
    }
    for (member, container_id) in pending {
        if let Some(entry) = entries.get_mut(&container_id) {
            entry.members.push(member);
        }
    }
    for entry in entries.values_mut() {
        entry
            .members
            .sort_by(|a, b| a.start_line.cmp(&b.start_line).then_with(|| a.name.cmp(&b.name)));
    }

    let mut ranked: Vec<DeadCodeEntry> = entries.into_values().collect();
    ranked.sort_by(|a, b| {
        b.lines
            .cmp(&a.lines)
            .then_with(|| a.node.file_path.cmp(&b.node.file_path))
            .then_with(|| a.node.start_line.cmp(&b.node.start_line))
    });
    let total = ranked.len();
    ranked.truncate(limit);

    Ok(DeadCodeReport {
        entries: ranked,
        total,
        candidates: candidate_count,
        excluded,
        kinds,
        include_exported,
        bounded,
        corroborated: matches!(
            &query.read_source,
            SourceReader::Default | SourceReader::Custom(_)
        ),
    })
}

/// Reads a project-relative file off disk, refusing anything that escapes the
/// project root. `filePath` comes out of the index, and the index is a file on
/// disk — this module should not be the thing that trusts it.
fn default_source_reader(project_root: &str) -> Box<dyn Fn(&str) -> Option<String> + '_> {
    let root = codegraph_types::paths::resolve_path(project_root);
    let root_string = root.to_string_lossy().to_string();
    Box::new(move |file_path: &str| {
        let absolute = codegraph_types::paths::resolve_path(root.join(file_path));
        let absolute_string = absolute.to_string_lossy().to_string();
        if !codegraph_types::paths::is_within_dir(&absolute_string, &root_string) {
            return None;
        }
        let metadata = std::fs::metadata(&absolute).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_CORROBORATION_BYTES {
            return None;
        }
        std::fs::read_to_string(&absolute).ok()
    })
}

/// How many times `name` is written in `source` as a whole identifier.
///
/// Deliberately dumb: no parsing, no comment or string stripping. A mention in a
/// comment or a string is exactly the kind of thing that turns out to be a
/// reflective call or a registration key, and this count only ever says LESS.
pub fn mention_count(source: &str, name: &str, stop_at: usize) -> usize {
    if name.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut from = 0usize;
    while let Some(offset) = source[from..].find(name) {
        let at = from + offset;
        from = at + name.len();
        let before = source[..at].chars().next_back();
        let after = source[from..].chars().next();
        if !before.is_some_and(is_identifier_char) && !after.is_some_and(is_identifier_char) {
            count += 1;
            if count >= stop_at {
                return count;
            }
        }
    }
    count
}

fn is_identifier_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}

/// Files in the candidate set that nothing else in the index reaches.
fn files_nothing_reaches(store: &Store, file_paths: &[String]) -> Result<HashSet<String>> {
    let mut unique: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for path in file_paths {
        if seen.insert(path.clone()) {
            unique.push(path.clone());
        }
    }
    if unique.is_empty() {
        return Ok(HashSet::new());
    }
    let counts = store.get_file_dependent_counts(&unique)?;
    Ok(unique
        .into_iter()
        .filter(|path| counts.get(path).copied().unwrap_or(0) == 0)
        .collect())
}

/// The type each candidate is declared in, for candidates that are members.
fn containers_of(store: &Store, nodes: &[Node]) -> Result<HashMap<String, Node>> {
    let member_ids: Vec<String> = nodes
        .iter()
        .filter(|n| is_overridable_kind(n.kind))
        .map(|n| n.id.clone())
        .collect();
    let mut out = HashMap::new();
    if member_ids.is_empty() {
        return Ok(out);
    }
    let edges = store.get_incoming_edges_to(&member_ids, &[EdgeKind::Contains])?;
    let mut by_member: HashMap<String, String> = HashMap::new();
    for edge in edges {
        by_member.entry(edge.target).or_insert(edge.source);
    }
    let container_ids: Vec<&String> = by_member.values().collect();
    let container_nodes = store.get_nodes_by_ids(&container_ids)?;
    for (member_id, container_id) in by_member {
        if let Some(container) = container_nodes.get(&container_id) {
            if is_container_kind(container.kind) {
                out.insert(member_id, container.clone());
            }
        }
    }
    Ok(out)
}

/// Every ancestor of `ids` along `kinds` edges, mapped back to the ids it is an
/// ancestor of. One query per level over every id at once.
fn ancestors_of(store: &Store, ids: &[String], kinds: &[EdgeKind]) -> Result<HashMap<String, HashSet<String>>> {
    let mut reach: HashMap<String, HashSet<String>> = HashMap::new();
    let mut seen: HashMap<String, HashSet<String>> = ids
        .iter()
        .map(|id| (id.clone(), HashSet::from([id.clone()])))
        .collect();
    let mut frontier: Vec<(String, HashSet<String>)> = ids
        .iter()
        .map(|id| (id.clone(), HashSet::from([id.clone()])))
        .collect();

    for _ in 0..MAX_OVERRIDE_ANCESTOR_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let roots_of: HashMap<&str, &HashSet<String>> =
            frontier.iter().map(|(id, roots)| (id.as_str(), roots)).collect();
        let frontier_ids: Vec<String> = frontier.iter().map(|(id, _)| id.clone()).collect();
        let edges = store.get_outgoing_edges_from(&frontier_ids, kinds)?;
        let mut next: HashMap<String, HashSet<String>> = HashMap::new();
        for edge in edges {
            if edge.target == edge.source {
                continue;
            }
            let Some(roots) = roots_of.get(edge.source.as_str()) else {
                continue;
            };
            let merged = next.entry(edge.target.clone()).or_default();
            let visited = seen.entry(edge.target.clone()).or_default();
            let known = reach.entry(edge.target.clone()).or_default();
            // An ancestor may already be a root. Only skip pairs we have
            // propagated, so later descendants still reach all its ancestors.
            for root in *roots {
                if visited.contains(root) {
                    continue;
                }
                visited.insert(root.clone());
                merged.insert(root.clone());
                known.insert(root.clone());
            }
        }
        next.retain(|_, roots| !roots.is_empty());
        frontier = next.into_iter().collect();
    }
    reach.retain(|_, known| !known.is_empty());
    Ok(reach)
}

/// Which candidates override something — the ids to drop.
fn override_candidates(
    store: &Store,
    nodes: &[Node],
    containers: &HashMap<String, Node>,
) -> Result<HashSet<String>> {
    let mut dropped = HashSet::new();
    let mut container_ids: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for container in containers.values() {
        if seen.insert(container.id.clone()) {
            container_ids.push(container.id.clone());
        }
    }
    if container_ids.is_empty() {
        return Ok(dropped);
    }

    let reach = ancestors_of(store, &container_ids, &[EdgeKind::Extends, EdgeKind::Implements])?;

    let mut opaque: HashSet<String> = HashSet::new();
    let mut supertype_scope = container_ids.clone();
    supertype_scope.extend(reach.keys().cloned());
    let external_based = store.get_unresolved_supertype_sources_among(&supertype_scope)?;
    for id in &container_ids {
        if external_based.contains(id) {
            opaque.insert(id.clone());
        }
    }
    for (ancestor, roots) in &reach {
        if external_based.contains(ancestor) {
            for root in roots {
                opaque.insert(root.clone());
            }
        }
    }

    let ancestor_ids: Vec<String> = reach.keys().cloned().collect();
    let member_edges = store.get_outgoing_edges_from(&ancestor_ids, &[EdgeKind::Contains])?;
    let mut member_ids_by_ancestor: HashMap<String, Vec<String>> = HashMap::new();
    for edge in &member_edges {
        member_ids_by_ancestor
            .entry(edge.source.clone())
            .or_default()
            .push(edge.target.clone());
    }
    let member_ids: Vec<&String> = member_edges.iter().map(|e| &e.target).collect();
    let member_nodes = store.get_nodes_by_ids(&member_ids)?;

    let mut names_by_container: HashMap<String, HashSet<String>> = HashMap::new();
    for (ancestor, roots) in &reach {
        let names: Vec<&str> = member_ids_by_ancestor
            .get(ancestor)
            .into_iter()
            .flatten()
            .filter_map(|id| member_nodes.get(id))
            .filter(|member| is_overridable_kind(member.kind))
            .map(|member| member.name.as_str())
            .collect();
        for root in roots {
            if names.is_empty() {
                opaque.insert(root.clone());
                continue;
            }
            let bucket = names_by_container.entry(root.clone()).or_default();
            for name in &names {
                bucket.insert((*name).to_owned());
            }
        }
    }

    for node in nodes {
        let Some(container) = containers.get(&node.id) else {
            continue;
        };
        if opaque.contains(&container.id) {
            dropped.insert(node.id.clone());
            continue;
        }
        if names_by_container
            .get(&container.id)
            .is_some_and(|names| names.contains(&node.name))
        {
            dropped.insert(node.id.clone());
        }
    }
    Ok(dropped)
}

/// Which of `nodes` carry a decorator: their own outgoing `decorates` edge, an
/// unresolved `decorates` row, or the `decorators` list on the node itself.
fn carries_decorator(store: &Store, nodes: &[Node]) -> Result<HashSet<String>> {
    let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
    let mut found: HashSet<String> = store
        .get_outgoing_edges_from(&ids, &[EdgeKind::Decorates])?
        .into_iter()
        .map(|edge| edge.source)
        .collect();
    found.extend(
        store
            .get_unresolved_reference_names_from(&ids, &[EdgeKind::Decorates])?
            .into_keys(),
    );
    for node in nodes {
        if node.decorators.as_ref().is_some_and(|d| !d.is_empty()) {
            found.insert(node.id.clone());
        }
    }
    Ok(found)
}

/// Which members are hooks their framework calls by name.
fn by_name_hook_candidates(
    store: &Store,
    nodes: &[Node],
    containers: &HashMap<String, Node>,
    unreferenced: &[Node],
) -> Result<HashSet<String>> {
    let mut dropped = HashSet::new();
    let is_hook_shaped = |node: &Node| {
        is_hook_kind(node.kind) && is_by_name_dispatch_language(node.language.as_str())
    };
    let methods: Vec<&Node> = nodes
        .iter()
        .filter(|node| {
            is_hook_shaped(node)
                && containers
                    .get(&node.id)
                    .is_some_and(|c| c.kind == NodeKind::Class)
        })
        .collect();
    if methods.is_empty() {
        return Ok(dropped);
    }

    let mut classes: HashMap<String, Node> = HashMap::new();
    for node in &methods {
        if let Some(container) = containers.get(&node.id) {
            classes.insert(container.id.clone(), container.clone());
        }
    }
    let class_list: Vec<Node> = classes.values().cloned().collect();
    let registered = carries_decorator(store, &class_list)?;
    let eligible: Vec<&Node> = methods
        .into_iter()
        .filter(|node| {
            containers
                .get(&node.id)
                .is_some_and(|c| registered.contains(&c.id))
        })
        .collect();
    if eligible.is_empty() {
        return Ok(dropped);
    }

    let names: HashSet<&str> = eligible.iter().map(|n| n.name.as_str()).collect();
    let declarations: Vec<&Node> = unreferenced
        .iter()
        .filter(|node| names.contains(node.name.as_str()) && is_hook_shaped(node))
        .collect();
    let need_container: Vec<Node> = declarations
        .iter()
        .filter(|node| !containers.contains_key(&node.id))
        .map(|node| (*node).clone())
        .collect();
    let declared_in = containers_of(store, &need_container)?;
    let mut declaring: HashMap<String, HashSet<String>> = HashMap::new();
    for node in &declarations {
        let container = containers.get(&node.id).or_else(|| declared_in.get(&node.id));
        let Some(container) = container.filter(|c| c.kind == NodeKind::Class) else {
            continue;
        };
        declaring
            .entry(node.name.clone())
            .or_default()
            .insert(container.id.clone());
    }

    let declarers: Vec<String> = declaring
        .values()
        .flatten()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let interfaces_of = store.get_unresolved_reference_names_from(&declarers, &[EdgeKind::Implements])?;
    let interfaces: Vec<String> = interfaces_of
        .values()
        .flatten()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if interfaces.is_empty() {
        return Ok(dropped);
    }

    let writers = store.get_unresolved_reference_sources_named(&interfaces, &[EdgeKind::Implements])?;
    let writer_ids: Vec<String> = writers
        .values()
        .flatten()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let writer_nodes = store.get_nodes_by_ids(&writer_ids)?;
    let typescript: Vec<Node> = writer_nodes
        .into_values()
        .filter(|node| is_by_name_dispatch_language(node.language.as_str()))
        .collect();
    let typescript_ids: HashSet<&str> = typescript.iter().map(|n| n.id.as_str()).collect();
    let registered_writers = carries_decorator(store, &typescript)?;
    let mut implementers: HashMap<String, Vec<String>> = HashMap::new();
    for (name, ids) in &writers {
        let own: Vec<String> = ids
            .iter()
            .filter(|id| typescript_ids.contains(id.as_str()))
            .cloned()
            .collect();
        let registered_count = own
            .iter()
            .filter(|id| registered_writers.contains(*id))
            .count();
        if registered_count * 2 > own.len() {
            implementers.insert(name.clone(), own);
        }
    }
    if implementers.is_empty() {
        return Ok(dropped);
    }

    let implementer_ids: Vec<String> = implementers
        .values()
        .flatten()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let ancestors = ancestors_of(store, &implementer_ids, &[EdgeKind::Extends])?;
    let mut inherits: HashMap<String, Vec<String>> = HashMap::new();
    for (ancestor, roots) in &ancestors {
        for root in roots {
            inherits.entry(root.clone()).or_default().push(ancestor.clone());
        }
    }
    let mut outside_scope = implementer_ids.clone();
    outside_scope.extend(ancestors.keys().cloned());
    let outside = store.get_unresolved_reference_names_from(&outside_scope, &[EdgeKind::Extends])?;
    let mut opaque: HashSet<String> = implementer_ids
        .iter()
        .filter(|id| outside.contains_key(*id))
        .cloned()
        .collect();
    for (ancestor, roots) in &ancestors {
        if outside.contains_key(ancestor) {
            for root in roots {
                opaque.insert(root.clone());
            }
        }
    }

    let mut hook_names: HashSet<String> = HashSet::new();
    for name in names {
        let Some(declared) = declaring.get(name) else {
            continue;
        };
        let has = |id: &str| -> bool {
            declared.contains(id)
                || opaque.contains(id)
                || inherits
                    .get(id)
                    .is_some_and(|ancestors| ancestors.iter().any(|a| declared.contains(a)))
        };
        let contracts: HashSet<String> = declared
            .iter()
            .flat_map(|id| interfaces_of.get(id).cloned().unwrap_or_default())
            .collect();
        for contract in contracts {
            let Some(own) = implementers.get(&contract) else {
                continue;
            };
            let declared_count = own.iter().filter(|id| declared.contains(*id)).count();
            if declared_count >= 2
                && declared_count * 2 > own.len()
                && own.iter().all(|id| has(id))
            {
                hook_names.insert(name.to_owned());
                break;
            }
        }
    }

    for node in eligible {
        if hook_names.contains(&node.name) {
            dropped.insert(node.id.clone());
        }
    }
    Ok(dropped)
}
