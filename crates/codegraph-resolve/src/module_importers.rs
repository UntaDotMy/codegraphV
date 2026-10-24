//! The references an import statement makes through a module that appears or goes away — port
//! of `src/resolution/module-importers.ts`.
//!
//! `from shop import Order` reaches `shop/models.py` through `shop/__init__.py`, which
//! re-exports `Order`; `import { Button } from './components'` reaches `components/Button.ts`
//! through `components/index.ts`. When the re-exporting file goes away, their edges point into
//! files that stay, so the cascade keeps them; when it comes back, the edges a name lookup made
//! in its absence stay. Nothing else revisits either: the importers did not change. A fresh index
//! binds them through the module when it is there, and another way (by name) or not at all when
//! it is not.
//!
//! A module that re-exports another one (`export * from './components'` in an `index.ts`,
//! `from .shop import *` or a package's `__init__.py` importing names from it) is the way in for
//! its own importers, so theirs are followed too, as deep as the resolver chases a name through
//! such modules.

use crate::alias_binding::{default_export_binding, extract_local_export_aliases};
use crate::ambient_modules::{is_declaration_file, scan_ambient_modules};
use crate::import_resolver::{extract_import_mappings, extract_re_exports, python_module_path, REEXPORT_MAX_DEPTH};
use crate::resurrect_ref::resurrect_ref_from_dropped_edge;
use crate::types::{ImportMapping, ReExport};
use codegraph_db::reference_tail::module_reference_keys;
use codegraph_db::sync_queries::ResolutionEdge;
use codegraph_db::Store;
use codegraph_jsre::re;
use codegraph_types::{EdgeKind, Language, NodeKind, UnresolvedReference};
use indexmap::{IndexMap, IndexSet};
use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

type DbResult<T> = codegraph_db::Result<T>;

/// Languages whose import bindings name what their references are written as: Python's
/// `from x import Y` and the ES-module family's `import { Y } from`. An `#include` or a Go
/// package import binds no reference name of its own.
fn is_binding_language(language: Language) -> bool {
    matches!(
        language,
        Language::Python
            | Language::TypeScript
            | Language::Tsx
            | Language::JavaScript
            | Language::Jsx
            | Language::ArkTs
            | Language::Vue
            | Language::Svelte
            | Language::Astro
    )
}

/// The languages a barrel a lazy route reads through is written in: the ES-module family.
fn is_barrel_language(language: Language) -> bool {
    matches!(language, Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx)
}

/// How many re-exporting modules deep the importers of a file are followed: as deep as the
/// resolver chases a name through them, three for Python's star imports and re-exports
/// (`pythonModuleSymbol`), `REEXPORT_MAX_DEPTH` for ES-module `export … from`.
fn re_export_depth(language: Language) -> usize {
    if language == Language::Python {
        3
    } else {
        REEXPORT_MAX_DEPTH
    }
}

/// A file this sync adds or removes, or a module re-exporting from one, with the names it passes
/// on from them (`None`: whatever it has).
struct Module {
    path: String,
    language: Language,
    passes: Option<IndexSet<String>>,
}

type ModuleRef = Rc<Module>;

/// A statement naming a module: whole, or as `only_name` of the package around it.
struct Statement {
    source: String,
    module: ModuleRef,
    only_name: Option<String>,
}

struct Naming {
    language: Language,
    statements: Vec<Statement>,
}

/// For each module specifier of a statement naming one of the modules, which of its bindings go
/// through it: all of them (`None`), or those importing one of these names — what a re-exporting
/// module passes on, or the module the package around a file imports as a name (`from django.db
/// import models`, whose `models` is the file's module). A specifier absent from the map is not
/// imported at all.
struct ImporterEntry {
    language: Language,
    sources: IndexMap<String, Option<IndexSet<String>>>,
}

type Importers = IndexMap<String, ImporterEntry>;

/// Per importer, the modules it names whole and the local names its statements bind.
struct Binding {
    modules: Vec<String>,
    names: Vec<String>,
}

type Bindings = IndexMap<String, Binding>;

/// Resolves a module specifier written in `from_file` as its import does: the project file it
/// names, or `None`.
pub type ModuleResolver<'a> = &'a dyn Fn(&str, &str, Language) -> Option<String>;

/// The folder of the project's own package (a workspace member, a path dependency) a file is
/// in, or `None`.
pub type PackageDirOf<'a> = &'a dyn Fn(&str) -> Option<String>;

/// A file the sync adds or removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub language: Language,
}

/// A module whose default export moved, with the declarations its default was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedDefault {
    pub path: String,
    pub language: Language,
    pub was: Vec<String>,
}

/// What one file's import statements say.
pub struct ReaderEntry {
    content: String,
    pub mappings: Vec<ImportMapping>,
    pub re_exports: Vec<ReExport>,
    pub star_imports: Vec<String>,
    exported: OnceCell<IndexMap<String, Vec<String>>>,
}

impl ReaderEntry {
    /// The bindings an ES module's own exports hand over as they are, each with the names it is
    /// exported as: each local an export clause names (`export { Page }`, `export { Card as
    /// AppCard }`), and its default export when that is a binding (`export default Page;`,
    /// exported as `default`).
    pub fn exported_as(&self) -> &IndexMap<String, Vec<String>> {
        self.exported.get_or_init(|| {
            // Most importers export nothing they import; a commented-out clause read as one only
            // costs a key lookup.
            let mut exported: IndexMap<String, Vec<String>> = IndexMap::new();
            if re!(r"\bexport\s*\{|\bexport\s+default\s|\bexports\b").is_match(&self.content) {
                for alias in extract_local_export_aliases(&self.content) {
                    exported.entry(alias.local_name).or_default().push(alias.exported_name);
                }
                if let Some(binding) = default_export_binding(&self.content) {
                    exported.entry(binding).or_default().push("default".to_string());
                }
            }
            exported
        })
    }
}

/// What a file's import statements say, each file read once per sync.
pub struct ImportReader {
    root_dir: String,
    memo: RefCell<HashMap<String, Rc<ReaderEntry>>>,
}

impl ImportReader {
    pub fn new(root_dir: &str) -> ImportReader {
        ImportReader { root_dir: root_dir.to_string(), memo: RefCell::new(HashMap::new()) }
    }

    /// Its import mappings, its ES-module `export … from` statements (which have no import
    /// mapping), and the modules a Python file star-imports (likewise).
    pub fn entry(&self, file: &str, language: Language) -> Rc<ReaderEntry> {
        if let Some(entry) = self.memo.borrow().get(file) {
            return entry.clone();
        }
        // Gone or unreadable: it imports nothing.
        let content = std::fs::read(std::path::Path::new(&self.root_dir).join(file))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let star_imports = if language == Language::Python {
            re!(r"^\s*from\s+([\w.]+)\s+import\s+\*", "m")
                .captures_iter(&content)
                .filter_map(|c| c.text(1).map(String::from))
                .collect()
        } else {
            Vec::new()
        };
        let entry = Rc::new(ReaderEntry {
            mappings: if content.is_empty() { Vec::new() } else { extract_import_mappings(file, &content, language) },
            re_exports: if content.is_empty() { Vec::new() } else { extract_re_exports(&content, language) },
            star_imports,
            content,
            exported: OnceCell::new(),
        });
        self.memo.borrow_mut().insert(file.to_string(), entry.clone());
        entry
    }

    fn read_content(&self, file: &str) -> Option<String> {
        std::fs::read(std::path::Path::new(&self.root_dir).join(file)).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

/// Re-open, as pending references for the sync's sweep, the references the files importing any of
/// `removed` make through those imports. The sweep binds them against the post-sync graph, or
/// parks them where a sync that brings a file back finds them. Called before the files' rows are
/// deleted, for all of them at once, so an importer of several is visited once; an edge into a
/// removed file is left to the resurrection its removal does. Returns the number of references
/// re-opened.
pub fn reopen_references_through_removed_files(store: &Store, reader: &ImportReader, removed: &[ChangedFile]) -> DbResult<usize> {
    let gone: HashSet<&str> = removed.iter().map(|f| f.path.as_str()).collect();
    let bindings = bindings_of(&importers_of(store, removed, reader, &[], None)?, reader);
    reopen_edges(store, &bindings, &|e| !gone.contains(e.target_file_path.as_deref().unwrap_or("")))
}

/// The other half: after a sync's retries, re-open the references the files importing any of
/// `added` make through those imports — the edges a name lookup made while a module was missing
/// (only those up to `resolved_up_to_edge_id`: later ones were just resolved with the modules
/// there) and the references that failed, the import of the module itself among them. (The
/// failed-import retry looks those up by the module's name, and skips a name as common as
/// `models` or `test`.) With `resolve_module`, also an import that resolved to a file beside an
/// added one that now outranks it (`./button` to `button.vue` while `button.ts` was missing), or,
/// with `package_dir_of`, to another file of the package an added file is in, and what that
/// statement binds. Returns the number re-opened.
pub fn reopen_references_through_added_files(
    store: &Store,
    reader: &ImportReader,
    added: &[ChangedFile],
    resolved_up_to_edge_id: i64,
    resolve_module: Option<ModuleResolver>,
    package_dir_of: Option<PackageDirOf>,
) -> DbResult<usize> {
    let outranked = match resolve_module {
        Some(resolve) => outranked_imports(store, added, resolve, package_dir_of)?,
        None => Vec::new(),
    };
    let bindings = bindings_of(&importers_of(store, added, reader, &outranked, None)?, reader);
    let mut failed: Vec<i64> = Vec::new();
    for (file, binding) in &bindings {
        failed.extend(store.get_failed_references_through(file, &binding.modules, &binding.names)?);
    }
    Ok(reopen_module_edges(store, &outranked)?
        + reopen_edges(store, &bindings, &|e| e.edge_id <= resolved_up_to_edge_id)?
        + store.reopen_failed_references(&failed)?)
}

/// The way in for the files importing a package that a declaration file this sync adds or edits
/// declares itself: `declare module 'mammoth' { … }` in `typings/mammoth/index.d.ts` (see
/// ambient-modules). They import the package by its name, which no import edge into the file
/// records, so the pass over an added file's importers never finds them, and an aliased, default
/// or namespace binding waits under a name the file never declares. What each importer reaches
/// through those bindings goes back to pending for the sweep: the references that failed, and the
/// edges bound before this sync resolved anything (up to `resolved_up_to_edge_id`), which a name
/// match may have made while nothing declared the package. A removed declaration needs nothing of
/// its own: the edges into its declarations are resurrected with them. Returns the number
/// re-opened.
pub fn reopen_references_through_declared_packages(
    store: &Store,
    reader: &ImportReader,
    changed: &[String],
    resolved_up_to_edge_id: i64,
) -> DbResult<usize> {
    let mut specifiers: IndexSet<String> = IndexSet::new();
    for file in changed {
        if !is_declaration_file(file) {
            continue;
        }
        let Some(content) = reader.read_content(file) else { continue };
        if !content.contains("declare") {
            continue;
        }
        for block in scan_ambient_modules(&content).blocks {
            if !block.pattern {
                specifiers.insert(block.specifier);
            }
        }
    }
    if specifiers.is_empty() {
        return Ok(0);
    }
    let changed_files: HashSet<&str> = changed.iter().map(String::as_str).collect();
    let mut importers: Importers = IndexMap::new();
    for specifier in &specifiers {
        // An import statement's node is named by the specifier it imports.
        for node in store.get_nodes_by_name(specifier)? {
            if node.kind != NodeKind::Import || !is_binding_language(node.language) || changed_files.contains(node.file_path.as_str()) {
                continue;
            }
            let entry = importers
                .entry(node.file_path.clone())
                .or_insert_with(|| ImporterEntry { language: node.language, sources: IndexMap::new() });
            entry.sources.insert(specifier.clone(), None);
        }
    }
    let bindings = bindings_of(&importers, reader);
    let mut failed: Vec<i64> = Vec::new();
    for (file, binding) in &bindings {
        failed.extend(store.get_failed_references_through(file, &binding.modules, &binding.names)?);
    }
    Ok(reopen_edges(store, &bindings, &|e| e.edge_id <= resolved_up_to_edge_id)? + store.reopen_failed_references(&failed)?)
}

/// Re-open, for the sync's sweep, what reads the default of any of `files` when it moved: an edit
/// made `export default A` into `export default B`, or took the default away, while `A` stayed
/// declared (`was`, see `ReferenceResolver::changed_default_exports`). `new Y()` after `import Y
/// from './x'` kept `A`, the node that replaced its old target. The edges bound before this sync
/// resolved anything (up to `resolved_up_to_edge_id`) go back:
/// - those through every binding that reads the default: a default import, `{ default as Y }`,
///   `require('./x').default`, the names a module passes it on as (`export { default as Y } from
///   './x'`), and a route rendering one as its layout (`layout:Y`). A named import reads what it
///   did, and a default import's own edge links the module's file, which stays;
/// - those into the members `A` declares, wherever they are written: a value typed by the binding
///   reaches them by their own name (`document.delete()` with `document: Document`, a subclass's
///   `this.add`). `A` itself is reached through a binding, the default's or a named import's,
///   which reads it still.
/// Returns the number re-opened.
pub fn reopen_references_through_changed_defaults(
    store: &Store,
    reader: &ImportReader,
    files: &[MovedDefault],
    resolved_up_to_edge_id: i64,
) -> DbResult<usize> {
    let stays = |e: &ResolutionEdge| e.edge_id <= resolved_up_to_edge_id && !e.edge.target.starts_with("file:");
    let changed: Vec<ChangedFile> = files.iter().map(|f| ChangedFile { path: f.path.clone(), language: f.language }).collect();
    let default_only: IndexSet<String> = IndexSet::from(["default".to_string()]);
    let mut bindings = bindings_of(&importers_of(store, &changed, reader, &[], Some(&default_only))?, reader);
    // React, Vue and Angular routers write a layout a route renders as `layout:<name>`.
    for entry in bindings.values_mut() {
        let layouts: Vec<String> = entry.names.iter().map(|name| format!("layout:{name}")).collect();
        entry.names.extend(layouts);
    }
    let reopened = reopen_edges(store, &bindings, &stays)?;
    let was: Vec<String> = files.iter().flat_map(|f| f.was.iter().cloned()).collect::<IndexSet<_>>().into_iter().collect();
    let members: Vec<String> = if was.is_empty() {
        Vec::new()
    } else {
        store.get_outgoing_edges_from(&was, &[EdgeKind::Contains])?.into_iter().map(|e| e.target).collect()
    };
    if members.is_empty() {
        return Ok(reopened);
    }
    let into: Vec<ResolutionEdge> = store.get_resolution_edges_into(&members)?.into_iter().filter(|e| stays(e)).collect();
    Ok(reopened + reopen_edge_list(store, &into)?)
}

/// Re-open, for the sync's sweep, the imports of `files` that a name match took instead of the
/// import, when the binding now resolves another way. `import { EmptyState } from
/// './layouts/shared'`, linked by name to a namesake while the barrel did not pass `EmptyState`
/// on, kept the namesake after an edit gave the barrel `export { EmptyState } from
/// './EmptyState'`: that edit declares nothing, so neither the rebind nor the walk below a barrel
/// sees it, and a fresh index links the binding through the module. A name match into the module
/// itself (`import { Card } from './card'` where `card.ts` exports `Card` only as its default)
/// can come undone the same way. Each importer's edges bound before this sync resolved anything
/// (up to `resolved_up_to_edge_id`) go back: those through the name, and those into the members
/// of what the name match took, which a value typed by the binding reached (`w.run()` with `w:
/// Widget`). Only the importers' bindings that resolved some other way than their import are
/// read, an indexed range per importer, then only those importers' import statements, and
/// `resolve_now` says what each binding of `files` resolves to against the graph as it is: one
/// that resolves as it did, such as a name match a fresh index makes too, stays. Returns the
/// number re-opened.
pub fn reopen_name_matched_imports(
    store: &Store,
    reader: &ImportReader,
    files: &[ChangedFile],
    resolved_up_to_edge_id: i64,
    resolve_module: ModuleResolver,
    resolve_now: &dyn Fn(&UnresolvedReference) -> Option<(String, String)>,
) -> DbResult<usize> {
    let mut bindings: Bindings = IndexMap::new();
    // What each importer's name matches took, whose members a typed value reached.
    let mut took: IndexSet<String> = IndexSet::new();
    for file in files {
        if !is_barrel_language(file.language) {
            continue;
        }
        let mut importers: IndexSet<String> = IndexSet::new();
        for i in store.get_imports_into(&format!("file:{}", file.path), &file.path)? {
            if i.language != Language::Python && is_binding_language(i.language) {
                importers.insert(i.file_path);
            }
        }
        if importers.is_empty() {
            continue;
        }
        let importer_list: Vec<String> = importers.into_iter().collect();
        for edge in store.get_import_bindings_resolved_elsewhere(&importer_list)? {
            let Some(name) = edge.edge.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            if edge.edge_id > resolved_up_to_edge_id {
                continue;
            }
            let source_entry = reader.entry(&edge.source_file_path, edge.source_language);
            let Some(binding) = source_entry.mappings.iter().find(|m| m.local_name == name) else { continue };
            if binding.is_namespace {
                continue;
            }
            if resolve_module(&binding.source, &edge.source_file_path, edge.source_language).as_deref() != Some(file.path.as_str()) {
                continue;
            }
            let Some(reference) = resurrect(&edge) else { continue };
            if let Some((target_node_id, resolved_by)) = resolve_now(&reference) {
                let recorded = edge.edge.metadata.as_ref().and_then(|m| m.get("resolvedBy")).and_then(|v| v.as_str());
                if target_node_id == edge.edge.target && Some(resolved_by.as_str()) == recorded {
                    continue;
                }
            }
            let entry = bindings.entry(edge.source_file_path.clone()).or_insert_with(|| Binding { modules: Vec::new(), names: Vec::new() });
            if !entry.names.contains(&name) {
                entry.names.push(name);
            }
            took.insert(edge.edge.target.clone());
        }
    }
    let stays = |e: &ResolutionEdge| e.edge_id <= resolved_up_to_edge_id;
    let reopened = reopen_edges(store, &bindings, &stays)?;
    if took.is_empty() {
        return Ok(reopened);
    }
    let took_list: Vec<String> = took.into_iter().collect();
    let members: Vec<String> = store.get_outgoing_edges_from(&took_list, &[EdgeKind::Contains])?.into_iter().map(|e| e.target).collect();
    if members.is_empty() {
        return Ok(reopened);
    }
    let into: Vec<ResolutionEdge> =
        store.get_resolution_edges_into(&members)?.into_iter().filter(|e| bindings.contains_key(&e.source_file_path) && stays(e)).collect();
    Ok(reopened + reopen_edge_list(store, &into)?)
}

/// Re-open, for the sync's sweep, the references that reach a module through a barrel when a file
/// below the barrel appears or gains a definition. React Router's `import:@/pages#HomePage` waits
/// under the key of the module it names, `module:pages`. When `pages/index.ts` passes on
/// `pages/Home/index.tsx`, that file is what the reference waits for, but its own keys name its
/// folder and stem (`module:Home`), never the barrel's.
///
/// The barrels are the modules that pass one of `files` on, through `export … from` or an import
/// they export again (`import { HomePage } from './Home'; export { HomePage }`), followed up as
/// deep as the resolver chases a name (`passed_on`). What waits under their keys goes back, and so
/// does what their importers reach through a name they pass the file on as: matx's `export {
/// default as MatxLoading } from './MatxLoading'` hands `MatxLoading.jsx` to `import { MatxLoading
/// } from 'app/components'`, a name the file never declares, so neither the retry by the file's
/// names nor the rebind of the edges into them reaches it. Such an importer's failed references
/// through those names go back. So do its edges through the names the files never declare, bound
/// before this sync resolved anything (up to `resolved_up_to_edge_id`): while the module lacked
/// the binding, a name match could take the name elsewhere (vben's `alert`, which the barrel
/// passes on from `vbenAlert`, to the `alert.vue` component). A name a file declares is the
/// rebind's, which caps a common one, and a chain of `export *` hands over only those. Call after
/// the failed-ref retry: an added file's importers reach it through the import edges it made.
/// Returns the number re-opened.
pub fn reopen_references_through_barrels(
    store: &Store,
    reader: &ImportReader,
    files: &[ChangedFile],
    resolved_up_to_edge_id: i64,
) -> DbResult<usize> {
    let mut level: Vec<ModuleRef> = files
        .iter()
        .filter(|f| is_barrel_language(f.language))
        .map(|f| Rc::new(Module { path: f.path.clone(), language: f.language, passes: None }))
        .collect();
    if level.is_empty() {
        return Ok(0);
    }
    let waiting: HashSet<String> = store.get_failed_module_keys()?.into_iter().collect();
    let mut keys: IndexSet<String> = IndexSet::new();
    let level_paths: Vec<String> = level.iter().map(|m| m.path.clone()).collect();
    let declared: HashSet<String> = store.get_nodes_by_files(&level_paths)?.into_iter().map(|n| n.name).collect();
    // The importers below a barrel, with the names it passes the files on as, and with those of
    // them the files never declare.
    let mut importers: Importers = IndexMap::new();
    let mut renamed: Importers = IndexMap::new();
    fn record(into: &mut Importers, importer: &str, language: Language, source: &str, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let entry = into.entry(importer.to_string()).or_insert_with(|| ImporterEntry { language, sources: IndexMap::new() });
        let mut merged: IndexSet<String> = entry.sources.get(source).cloned().flatten().unwrap_or_default();
        merged.extend(names.iter().cloned());
        entry.sources.insert(source.to_string(), Some(merged));
    }
    let mut seen: HashSet<String> = level_paths.into_iter().collect();
    let mut depth = 1;
    while !level.is_empty() {
        // Per importer of this level, its statements naming each module of it.
        let mut naming: IndexMap<String, Naming> = IndexMap::new();
        for module in &level {
            for i in store.get_imports_into(&format!("file:{}", module.path), &module.path)? {
                let Some(ref_name) = i.ref_name else { continue };
                if seen.contains(&i.file_path) {
                    continue;
                }
                if let Some(passes) = &module.passes {
                    if i.language != Language::Python && is_binding_language(i.language) {
                        let all: Vec<String> = passes.iter().cloned().collect();
                        record(&mut importers, &i.file_path, i.language, &ref_name, &all);
                        let undeclared: Vec<String> = all.iter().filter(|name| !declared.contains(*name)).cloned().collect();
                        record(&mut renamed, &i.file_path, i.language, &ref_name, &undeclared);
                    }
                }
                if depth > REEXPORT_MAX_DEPTH || !is_barrel_language(i.language) {
                    continue;
                }
                naming
                    .entry(i.file_path.clone())
                    .or_insert_with(|| Naming { language: i.language, statements: Vec::new() })
                    .statements
                    .push(Statement { source: ref_name, module: module.clone(), only_name: None });
            }
        }
        let mut next: Vec<ModuleRef> = Vec::new();
        for (importer, Naming { language, statements }) in &naming {
            let Some(passes) = passed_on(importer, *language, statements, reader) else { continue };
            seen.insert(importer.clone());
            for key in module_reference_keys(importer) {
                if waiting.contains(&key) {
                    keys.insert(key);
                }
            }
            next.push(Rc::new(Module { path: importer.clone(), language: *language, passes }));
        }
        level = next;
        depth += 1;
    }
    let mut failed: IndexSet<i64> = IndexSet::new();
    for (file, binding) in &bindings_of(&importers, reader) {
        failed.extend(store.get_failed_references_through(file, &[], &binding.names)?);
    }
    if !keys.is_empty() {
        let key_list: Vec<String> = keys.into_iter().collect();
        for r in store.get_retryable_failed_references(&key_list, 500, 5000)? {
            if let Some(row_id) = r.row_id {
                failed.insert(row_id);
            }
        }
    }
    let failed_list: Vec<i64> = failed.into_iter().collect();
    Ok(reopen_edges(store, &bindings_of(&renamed, reader), &|e| e.edge_id <= resolved_up_to_edge_id)?
        + store.reopen_failed_references(&failed_list)?)
}

/// An import's module edge into a file that an added file now outranks.
struct OutrankedImport {
    edge: ResolutionEdge,
    specifier: String,
    added: String,
}

/// The module edges an import resolved into a file beside one of `added` — another extension of
/// its name, or its folder's own entry — when the import's specifier now names the added file:
/// `./button` resolved to `button.vue` while `button.ts` was missing, and `button.ts` comes first.
/// With `package_dir_of`, also the imports of a package by its name into any file of the package
/// an added file is in: the entry its manifest declares outranks the file an import fell back to
/// while it was missing, such as the folder's own index (#2502).
fn outranked_imports(
    store: &Store,
    added: &[ChangedFile],
    resolve_module: ModuleResolver,
    package_dir_of: Option<PackageDirOf>,
) -> DbResult<Vec<OutrankedImport>> {
    let mut out = Vec::new();
    let mut package_imports: HashMap<String, Vec<ResolutionEdge>> = HashMap::new();
    for file in added {
        if !is_binding_language(file.language) {
            continue;
        }
        let (dir, name) = imported_name(&file.path);
        let mut edges = store.get_module_imports_named(&dir, &name)?;
        if let Some(package_dir) = package_dir_of.and_then(|f| f(&file.path)) {
            if !package_imports.contains_key(&package_dir) {
                let imports: Vec<ResolutionEdge> = store
                    .get_module_imports_under(&package_dir)?
                    .into_iter()
                    .filter(|edge| {
                        edge.edge.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str()).is_some_and(|s| !s.starts_with('.') && !s.starts_with('/'))
                    })
                    .collect();
                package_imports.insert(package_dir.clone(), imports);
            }
            let seen: HashSet<i64> = edges.iter().map(|e| e.edge_id).collect();
            edges.extend(package_imports[&package_dir].iter().filter(|e| !seen.contains(&e.edge_id)).cloned());
        }
        for edge in edges {
            let Some(specifier) = edge.edge.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            if edge.target_file_path.as_deref() == Some(file.path.as_str()) {
                continue;
            }
            if resolve_module(&specifier, &edge.source_file_path, edge.source_language).as_deref() == Some(file.path.as_str()) {
                out.push(OutrankedImport { edge, specifier, added: file.path.clone() });
            }
        }
    }
    Ok(out)
}

/// The folder and name an import finds a file by: `a/b.ts` → `a`, `b`; a folder's own entry
/// `a/b/index.ts` → `a`, `b`.
fn imported_name(file_path: &str) -> (String, String) {
    let slash = file_path.rfind('/');
    let base = slash.map_or(file_path, |s| &file_path[s + 1..]);
    let dir = slash.map_or("", |s| &file_path[..s]);
    if base.starts_with("index.") || base == "__init__.py" {
        let up = dir.rfind('/');
        return (up.map_or("", |u| &dir[..u]).to_string(), dir[up.map_or(0, |u| u + 1)..].to_string());
    }
    // `base.indexOf('.', 1)`: the first dot after the first character.
    let first_len = base.chars().next().map_or(0, char::len_utf8);
    let dot = base[first_len..].find('.').map(|d| d + first_len);
    (dir.to_string(), dot.map_or(base, |d| &base[..d]).to_string())
}

/// Swap the outranked module edges for their references, for the sweep. Returns how many.
fn reopen_module_edges(store: &Store, outranked: &[OutrankedImport]) -> DbResult<usize> {
    let mut edge_ids: Vec<i64> = Vec::new();
    let mut refs: Vec<UnresolvedReference> = Vec::new();
    for OutrankedImport { edge, .. } in outranked {
        let Some(reference) = resurrect(edge) else { continue };
        if edge_ids.contains(&edge.edge_id) {
            continue;
        }
        edge_ids.push(edge.edge_id);
        refs.push(reference);
    }
    if !refs.is_empty() {
        store.replace_resolution_edges_with_unresolved_refs(&edge_ids, &refs)?;
    }
    Ok(refs.len())
}

/// The files with an import statement naming one of `files`, or one of the modules re-exporting
/// from them, other than `files` themselves (going away, or just resolved with the others in
/// place); `outranked` imports name the added file they now resolve to. `names` limits what of
/// `files` counts: all of it (`None`), or what they export under these names.
fn importers_of(
    store: &Store,
    files: &[ChangedFile],
    reader: &ImportReader,
    outranked: &[OutrankedImport],
    names: Option<&IndexSet<String>>,
) -> DbResult<Importers> {
    let mut importers: Importers = IndexMap::new();
    let changed: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let mut seen: HashSet<String> = changed.iter().map(|s| s.to_string()).collect();
    let mut level: Vec<ModuleRef> = files
        .iter()
        .filter(|f| is_binding_language(f.language))
        .map(|f| Rc::new(Module { path: f.path.clone(), language: f.language, passes: names.cloned() }))
        .collect();
    let mut depth = 1;
    while !level.is_empty() {
        // Per importer found here, its statements naming each module of the level.
        let mut naming: IndexMap<String, Naming> = IndexMap::new();
        let mut add = |importer: &str, language: Language, source: &str, module: &ModuleRef, only_name: Option<&str>| {
            if !is_binding_language(language) || changed.contains(importer) {
                return;
            }
            let only: Option<IndexSet<String>> = match only_name {
                Some(n) => Some(IndexSet::from([n.to_string()])),
                None => module.passes.clone(),
            };
            let entry = importers.entry(importer.to_string()).or_insert_with(|| ImporterEntry { language, sources: IndexMap::new() });
            let merged = match (entry.sources.get(source), only) {
                (Some(None), _) | (_, None) => None,
                (existing, Some(only)) => {
                    let mut set: IndexSet<String> = existing.cloned().flatten().unwrap_or_default();
                    set.extend(only);
                    Some(set)
                }
            };
            entry.sources.insert(source.to_string(), merged);
            naming
                .entry(importer.to_string())
                .or_insert_with(|| Naming { language, statements: Vec::new() })
                .statements
                .push(Statement { source: source.to_string(), module: module.clone(), only_name: only_name.map(String::from) });
        };
        for module in &level {
            // A module-path import that resolved to the module: an ES module specifier, a dotted
            // Python import.
            for i in store.get_imports_into(&format!("file:{}", module.path), &module.path)? {
                if let Some(ref_name) = &i.ref_name {
                    add(&i.file_path, i.language, ref_name, module, None);
                }
            }
            if depth > 1 {
                continue;
            }
            for o in outranked {
                if o.added == module.path {
                    add(&o.edge.source_file_path, o.edge.source_language, &o.specifier, module, None);
                }
            }
        }
        let python: Vec<ModuleRef> = level.iter().filter(|m| m.language == Language::Python).cloned().collect();
        if !python.is_empty() {
            add_python_importers(store, &python, reader, &mut add)?;
        }
        drop(add);
        // The next level: the importers that pass on what they import from these.
        let mut next: Vec<ModuleRef> = Vec::new();
        for (importer, Naming { language, statements }) in &naming {
            if seen.contains(importer) || depth > re_export_depth(*language) {
                continue;
            }
            let Some(passes) = passed_on(importer, *language, statements, reader) else { continue };
            seen.insert(importer.clone());
            next.push(Rc::new(Module { path: importer.clone(), language: *language, passes }));
        }
        level = next;
        depth += 1;
    }
    Ok(importers)
}

/// What `importer` passes on of what its `statements` import, under the names its own importers
/// use — `Some(None)` for whatever it has — or `None` when the resolver reads no name through it:
/// an ES module what it `export … from`s and the imports it exports again (`import Card from
/// './Card'; export { Card as AppCard }`); a Python module what it star-imports, and a package's
/// `__init__.py` the names it imports too.
fn passed_on(importer: &str, language: Language, statements: &[Statement], reader: &ImportReader) -> Option<Option<IndexSet<String>>> {
    /// `undefined` / `null` (everything) / a set of names.
    enum Passes {
        Undefined,
        All,
        Names(IndexSet<String>),
    }
    let mut passes = Passes::Undefined;
    let mut pass = |names: Option<&IndexSet<String>>| {
        passes = match (std::mem::replace(&mut passes, Passes::Undefined), names) {
            (Passes::All, _) | (_, None) => Passes::All,
            (Passes::Undefined, Some(names)) => Passes::Names(names.clone()),
            (Passes::Names(mut set), Some(names)) => {
                set.extend(names.iter().cloned());
                Passes::Names(set)
            }
        };
    };
    let passing = |module: &Module, name: &str| module.passes.as_ref().map_or(true, |p| p.contains(name));
    let entry = reader.entry(importer, language);
    if language != Language::Python {
        for rex in &entry.re_exports {
            for Statement { source, module, .. } in statements {
                if source != rex.source() {
                    continue;
                }
                match rex {
                    ReExport::Wildcard { .. } => pass(module.passes.as_ref()),
                    ReExport::Namespace { exported_name, .. } => pass(Some(&IndexSet::from([exported_name.clone()]))),
                    ReExport::Named { exported_name, original_name, .. } => {
                        if passing(module, original_name) {
                            pass(Some(&IndexSet::from([exported_name.clone()])));
                        }
                    }
                }
            }
        }
        let exported_as = entry.exported_as();
        if !exported_as.is_empty() {
            for m in &entry.mappings {
                let Some(names) = (if m.is_namespace { None } else { exported_as.get(&m.local_name) }) else { continue };
                for Statement { source, module, .. } in statements {
                    if *source == m.source && passing(module, if m.is_default { "default" } else { &m.exported_name }) {
                        pass(Some(&names.iter().cloned().collect()));
                    }
                }
            }
        }
        return match passes {
            Passes::Undefined => None,
            Passes::All => Some(None),
            Passes::Names(set) => Some(Some(set)),
        };
    }
    let stars: HashSet<&String> = entry.star_imports.iter().collect();
    let is_package = importer == "__init__.py" || importer.ends_with("/__init__.py");
    for Statement { source, module, only_name } in statements {
        if only_name.is_none() && stars.contains(source) {
            pass(module.passes.as_ref());
        }
        if !is_package {
            continue;
        }
        let names: IndexSet<String> = entry
            .mappings
            .iter()
            .filter(|m| {
                m.source == *source
                    && match only_name {
                        Some(only) => m.exported_name == *only,
                        None => passing(module, &m.exported_name),
                    }
            })
            .map(|m| m.local_name.clone())
            .collect();
        if !names.is_empty() {
            pass(Some(&names));
        }
    }
    match passes {
        Passes::Undefined => None,
        Passes::All => Some(None),
        Passes::Names(set) => Some(Some(set)),
    }
}

/// Python's other ways to name the modules: an import node whose module path denotes one,
/// relative to the importer's own package or absolutely (`from . import Order`, `from .. import
/// Order`, `from shop import Order`), which never resolve to a file, or the package around one
/// with its module among the names imported (`from django.db import models`). An import inside a
/// function has no import node, so a file that reaches what the modules pass on — the targets of
/// their own import edges — counts too when its imports name one.
fn add_python_importers(
    store: &Store,
    modules: &[ModuleRef],
    reader: &ImportReader,
    add: &mut dyn FnMut(&str, Language, &str, &ModuleRef, Option<&str>),
) -> DbResult<()> {
    // What a module path a statement names stands for: a module whole, or a package with the
    // module of `only_name` among the names imported.
    type NamingOf = (ModuleRef, Option<String>);
    let mut by_path: HashMap<String, Vec<NamingOf>> = HashMap::new();
    let mut by_suffix: HashMap<String, Vec<NamingOf>> = HashMap::new();
    let mut import_names: IndexSet<String> = IndexSet::new();
    let mut index = |module_path: &str, naming: NamingOf| {
        by_path.entry(module_path.to_string()).or_default().push(naming.clone());
        let segments: Vec<&str> = module_path.split('/').collect();
        for i in 0..segments.len() {
            by_suffix.entry(segments[i..].join("/")).or_default().push(naming.clone());
        }
        import_names.extend(python_import_names_for(module_path));
    };
    for module in modules {
        let Some(module_path) = python_module_of_file(&module.path) else { continue };
        index(&module_path, (module.clone(), None));
        if let Some(slash) = module_path.rfind('/').filter(|s| *s > 0) {
            index(&module_path[..slash], (module.clone(), Some(module_path[slash + 1..].to_string())));
        }
    }
    if import_names.is_empty() {
        return Ok(());
    }
    // A relative import names a module by where the importer sits, an absolute one by the tail of
    // its path (the project's import root is not known).
    let named = |source: &str, importer: &str| -> Vec<NamingOf> {
        if source.starts_with('.') {
            let key = python_module_path(source, importer).map(|p| p.trim_end_matches('/').to_string()).unwrap_or_default();
            by_path.get(&key).cloned().unwrap_or_default()
        } else {
            by_suffix.get(&source.replace('.', "/")).cloned().unwrap_or_default()
        }
    };

    let names: Vec<String> = import_names.into_iter().collect();
    for node in store.get_import_nodes_by_names(&names)? {
        if node.language != Language::Python {
            continue;
        }
        for (module, only_name) in named(&node.name, &node.file_path) {
            add(&node.file_path, Language::Python, &node.name, &module, only_name.as_deref());
        }
    }
    let paths: HashSet<&str> = modules.iter().map(|m| m.path.as_str()).collect();
    let mut passed_on_targets: IndexSet<String> = IndexSet::new();
    for file_path in &paths {
        passed_on_targets.extend(store.get_import_targets_from_file(file_path)?);
    }
    if passed_on_targets.is_empty() {
        return Ok(());
    }
    let target_list: Vec<String> = passed_on_targets.into_iter().collect();
    for candidate in store.get_files_with_edges_into(&target_list)? {
        if !candidate.ends_with(".py") || paths.contains(candidate.as_str()) {
            continue;
        }
        let entry = reader.entry(&candidate, Language::Python);
        for m in &entry.mappings {
            for (module, only_name) in named(&m.source, &candidate) {
                if only_name.is_none() || only_name.as_deref() == Some(m.exported_name.as_str()) {
                    add(&candidate, Language::Python, &m.source, &module, only_name.as_deref());
                }
            }
        }
    }
    Ok(())
}

/// Per importer, what its statements naming the modules import: the modules named whole (not the
/// package around one), and the local names bound. An ES namespace import (`import * as z`) reads
/// whatever its module passes on, and an ES `export { A } from './x'` names `A`, which it
/// resolves through `./x` as an import binds it.
fn bindings_of(importers: &Importers, reader: &ImportReader) -> Bindings {
    let mut bindings: Bindings = IndexMap::new();
    for (file, ImporterEntry { language, sources }) in importers {
        let entry = reader.entry(file, *language);
        let mut names: IndexSet<String> = entry
            .mappings
            .iter()
            .filter(|m| match sources.get(&m.source) {
                Some(None) => true,
                None => false,
                Some(Some(only)) => only.contains(&m.exported_name) || (m.is_namespace && *language != Language::Python && !only.is_empty()),
            })
            .map(|m| m.local_name.clone())
            .collect();
        for rex in &entry.re_exports {
            let ReExport::Named { original_name, source, .. } = rex else { continue };
            match sources.get(source) {
                Some(None) => {
                    names.insert(original_name.clone());
                }
                Some(Some(only)) if only.contains(original_name) => {
                    names.insert(original_name.clone());
                }
                _ => {}
            }
        }
        let modules: Vec<String> = sources.iter().filter(|(_, only)| only.is_none()).map(|(source, _)| source.clone()).collect();
        if !modules.is_empty() || !names.is_empty() {
            bindings.insert(file.clone(), Binding { modules, names: names.into_iter().collect() });
        }
    }
    bindings
}

/// The reference behind a resolution edge, in its database form.
fn resurrect(edge: &ResolutionEdge) -> Option<UnresolvedReference> {
    resurrect_ref_from_dropped_edge(&edge.edge, &edge.source_file_path, edge.source_language).map(|r| crate::resolver::to_db_reference(&r))
}

/// Swap for their references the edges of each importer's references through its bound names —
/// the binding itself or a member read through it (`Order.objects`, `ns.fn`) — that pass `keep`.
/// Returns how many.
fn reopen_edges(store: &Store, bindings: &Bindings, keep: &dyn Fn(&ResolutionEdge) -> bool) -> DbResult<usize> {
    let mut edge_ids: Vec<i64> = Vec::new();
    let mut refs: Vec<UnresolvedReference> = Vec::new();
    for (file, Binding { names, .. }) in bindings {
        if names.is_empty() {
            continue;
        }
        for e in store.get_resolution_edges_through(file, names)? {
            if !keep(&e) {
                continue;
            }
            let Some(reference) = resurrect(&e) else { continue };
            edge_ids.push(e.edge_id);
            refs.push(reference);
        }
    }
    if !refs.is_empty() {
        store.replace_resolution_edges_with_unresolved_refs(&edge_ids, &refs)?;
    }
    Ok(refs.len())
}

/// Swap `edges` for their references, one per reference: a conditional's arms are edges of one
/// call. Returns how many references went back.
pub fn reopen_edge_list(store: &Store, edges: &[ResolutionEdge]) -> DbResult<usize> {
    let mut edge_ids: Vec<i64> = Vec::new();
    let mut refs: Vec<UnresolvedReference> = Vec::new();
    let mut sites: HashSet<String> = HashSet::new();
    for edge in edges {
        let Some(reference) = resurrect(edge) else { continue };
        edge_ids.push(edge.edge_id);
        let site = format!("{}\0{}\0{}\0{}", reference.from_node_id, reference.reference_name, reference.line, reference.column);
        if !sites.insert(site) {
            continue;
        }
        refs.push(reference);
    }
    if !edge_ids.is_empty() {
        store.replace_resolution_edges_with_unresolved_refs(&edge_ids, &refs)?;
    }
    Ok(refs.len())
}

/// A Python file's module as a path: `shop/models.py` → 'shop/models', `shop/__init__.py` →
/// 'shop'. `None` for a top-level `__init__.py`.
fn python_module_of_file(file_path: &str) -> Option<String> {
    if !file_path.ends_with(".py") || file_path == "__init__.py" {
        return None;
    }
    if let Some(stripped) = file_path.strip_suffix("/__init__.py") {
        return Some(stripped.to_string());
    }
    Some(file_path[..file_path.len() - 3].to_string())
}

/// Deepest relative import a lookup spells out: `from ........ import x`.
const MAX_RELATIVE_DOTS: usize = 8;

/// The import names that can denote the module at `module_path`: its trailing dotted paths
/// (`models`, `shop.models`), each also behind one dot or more (`.models`, `..shop.models`), and
/// the dots alone (`.`, `..`), which name a package by where the importer sits.
fn python_import_names_for(module_path: &str) -> Vec<String> {
    let segments: Vec<&str> = module_path.split('/').collect();
    let absolute: Vec<String> = (0..segments.len()).map(|i| segments[i..].join(".")).collect();
    let mut names: IndexSet<String> = absolute.iter().cloned().collect();
    for dots in 1..=MAX_RELATIVE_DOTS {
        let prefix = ".".repeat(dots);
        names.insert(prefix.clone());
        for name in &absolute {
            names.insert(format!("{prefix}{name}"));
        }
    }
    names.into_iter().collect()
}
