//! What an incremental sync asks of the resolver — the second half of `src/resolution/index.ts`
//! (route readers, Python attribute retries, navigation reopening, default-export moves, the
//! synthesis refresh).

use super::{to_db_reference, ReferenceResolver};
use crate::alias_binding::resolve_alias_binding;
use crate::ctx::Ctx;
use crate::import_resolver::python::PythonReopenScope;
use crate::import_resolver::{
    is_esm_import_language, is_js_family_file, is_module_binding_name, js_module_file, module_default_export, python_package_importers,
    python_reopen_scope, python_root_fingerprint, resolve_import_path,
};
use crate::module_importers::MovedDefault;
use crate::name_matcher::python_attr_ref_resolves;
use crate::resurrect_ref::resurrect_ref_from_dropped_edge;
use crate::types::UnresolvedRef;
use codegraph_db::sync_queries::ResolutionEdge;
use codegraph_db::Result as DbResult;
use codegraph_jsre::re;
use codegraph_text::strip_comments_for_regex;
use codegraph_types::{Edge, EdgeKind, Language, Node, NodeKind, UnresolvedReference};
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// How many importing files `changed_default_exports` samples when no edge into a module names
/// its default.
const DEFAULT_READER_SAMPLE_FILES: usize = 8;

/// An edge that reads a module's default through an import binding.
struct DefaultReader {
    edge: Edge,
    edge_id: i64,
    source_file_path: String,
    source_language: Language,
}

/// `fromNodeId\0referenceName`: one answer per (calling symbol, name).
fn key_of(from_node_id: &str, reference_name: &str) -> String {
    format!("{from_node_id}\0{reference_name}")
}

impl ReferenceResolver {
    /// Re-open the references of route nodes that read a module (`lazy_modules`) when one of
    /// `changed_file_paths` moved under them. A route that renders `const Docs = lazy(() =>
    /// import('./pages/Docs'))` bound to the value declared beside it while the module was
    /// missing, and nothing revisited it once the module was added: the reference had resolved,
    /// so it was never parked for the failed-ref retry, and the rebind of the names a sync
    /// defines (CG-33) reaches it only when the module's component is named like the declaration.
    /// An edit that gives the module its default export, or moves it to another component,
    /// changes the answer the same way: a route's edge follows its old target through the
    /// module's re-index. So does a page behind a barrel the module forwards. References written
    /// in a changed file were resolved against the new files already.
    pub fn reopen_route_module_readers(&self, changed_file_paths: &[String]) -> DbResult<usize> {
        if changed_file_paths.is_empty() {
            return Ok(0);
        }
        let frameworks = self.frameworks.borrow().clone();
        if frameworks.is_empty() {
            return Ok(0);
        }
        let changed: HashSet<&str> = changed_file_paths.iter().map(String::as_str).collect();
        let mut edge_ids: Vec<i64> = Vec::new();
        let mut refs: Vec<UnresolvedReference> = Vec::new();
        for edge in self.store.get_route_edges_moved_by(changed_file_paths)? {
            let Some(asked) = resurrect_ref_from_dropped_edge(&edge.edge, &edge.source_file_path, edge.source_language) else {
                continue;
            };
            let reads = frameworks
                .iter()
                .any(|f| f.lazy_modules(&asked, &self.ctx).is_some_and(|files| files.iter().any(|file| changed.contains(file.as_str()))));
            if !reads {
                continue;
            }
            edge_ids.push(edge.edge_id);
            refs.push(to_db_reference(&asked));
        }
        if !refs.is_empty() {
            self.store.replace_resolution_edges_with_unresolved_refs(&edge_ids, &refs)?;
        }
        Ok(refs.len())
    }

    /// The failed python attribute-chain references (`self.client.send`, `make().send`) an edit to
    /// `touched` can newly resolve. Such an answer is read only from files reached through imports
    /// — the owner class, its bases, a factory and the class it returns are each looked up through
    /// the imports of the file before — so these are the references of the files that import a
    /// touched file, directly or within six hops, and keep the ones that now resolve.
    ///
    /// Over `ceiling` distinct (symbol, name) pairs, only those whose answer a touched file can
    /// have moved are asked: an attribute a touched file assigns, a call receiver it defines, a
    /// class that subclasses one of its classes, or an attribute assigned from a call to something
    /// it defines. `None` when even that is over the ceiling; those refs wait for a full index.
    ///
    /// Past the ceiling, then, these become resolvable only at the next full index: a factory two
    /// hops from the edit, a subclass two hops away, and an attribute typed by an annotation — a
    /// parameter `c: Client`, or a class-level `c: Client` in a class that only gains the base or
    /// import that resolves `Client` in the touched file.
    pub fn python_attr_retry_candidates(&self, touched: &[String], ceiling: usize) -> DbResult<Option<Vec<UnresolvedReference>>> {
        let touched_set: HashSet<&str> = touched.iter().map(String::as_str).collect();
        let closure: Vec<String> =
            self.store.get_python_importer_closure(touched, 6)?.into_iter().filter(|f| !touched_set.contains(f.as_str())).collect();
        let resolvable = |refs: Vec<UnresolvedReference>| -> Vec<UnresolvedReference> {
            let mut answers: HashMap<String, bool> = HashMap::new();
            refs.into_iter()
                .filter(|r| {
                    if r.file_path.as_deref().is_none_or(str::is_empty) || r.language.is_none() {
                        return false;
                    }
                    let key = key_of(&r.from_node_id, &r.reference_name);
                    if let Some(ok) = answers.get(&key) {
                        return *ok;
                    }
                    let ok = python_attr_ref_resolves(&self.working_ref(r), &self.ctx);
                    answers.insert(key, ok);
                    ok
                })
                .collect()
        };
        let candidates = self.store.get_failed_python_attr_ref_keys(&closure)?;
        let load = |keys: &[codegraph_db::sync_queries::FailedRefKey]| -> DbResult<Vec<UnresolvedReference>> {
            self.store.get_unresolved_references_by_ids(&keys.iter().map(|k| k.id).collect::<Vec<_>>())
        };
        let distinct = |keys: &[codegraph_db::sync_queries::FailedRefKey]| -> usize {
            keys.iter().map(|k| key_of(&k.from_node_id, &k.reference_name)).collect::<HashSet<_>>().len()
        };
        if distinct(&candidates) <= ceiling {
            return Ok(Some(resolvable(load(&candidates)?)));
        }

        let mut assigned: HashSet<String> = HashSet::new();
        let mut defined: HashSet<String> = HashSet::new();
        let mut subclassing: HashSet<String> = HashSet::new();
        let mut calling: HashSet<String> = HashSet::new();
        for file in touched {
            if let Some(source) = self.ctx.read_file(file) {
                let text = strip_comments_for_regex(&source, "python");
                for m in re!(r"\b(?:self|cls)\.(\w+)\s*(?::[^=\n]*)?=(?!=)|^[ \t]+(\w+)\s*(?::[^=\n]*)?=(?!=)", "m").captures_iter(&text) {
                    if let Some(name) = m.text(1).or_else(|| m.text(2)) {
                        assigned.insert(name.to_string());
                    }
                }
            }
            for n in self.ctx.get_nodes_in_file(file).iter() {
                if matches!(n.kind, NodeKind::Class | NodeKind::Function | NodeKind::Method) {
                    defined.insert(n.name.clone());
                }
            }
            for f in self.store.get_python_files_with_edges_into(file, &["extends".to_string()], &["class".to_string()])? {
                subclassing.insert(f);
            }
            let calls = ["calls".to_string(), "instantiates".to_string()];
            let targets = ["function".to_string(), "method".to_string(), "class".to_string()];
            for f in self.store.get_python_files_with_edges_into(file, &calls, &targets)? {
                calling.insert(f);
            }
        }

        // file → attr → callee names
        let mut callees: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
        let mut assigned_callees = |file: &str, attr: &str| -> HashSet<String> {
            let by_attr = callees.entry(file.to_string()).or_insert_with(|| {
                let mut by_attr: HashMap<String, HashSet<String>> = HashMap::new();
                let text = self.ctx.read_file(file).map(|s| strip_comments_for_regex(&s, "python")).unwrap_or_default();
                for m in re!(r"\b(\w+)\s*(?::[^=\n]*)?=(?!=)\s*(?:await\s+)?(?:[\w.]*\.)?(\w+)\s*\(", "g").captures_iter(&text) {
                    if let (Some(a), Some(c)) = (m.text(1), m.text(2)) {
                        by_attr.entry(a.to_string()).or_default().insert(c.to_string());
                    }
                }
                by_attr
            });
            by_attr.get(attr).cloned().unwrap_or_default()
        };

        let mut kept = Vec::new();
        for r in candidates {
            let name = &r.reference_name;
            let keep = if name.contains("().") {
                let head = name.split("().").next().unwrap_or("");
                defined.contains(head.rsplit('.').next().unwrap_or(""))
            } else {
                let segs: Vec<&str> = name.split('.').collect();
                if segs.len() != 3 || !segs.iter().all(|seg| re!(r"^[A-Za-z_]\w*$").is_match(seg)) {
                    false
                } else {
                    let attr = segs[1];
                    if assigned.contains(attr) {
                        true
                    } else if subclassing.contains(&r.file_path) && (segs[0] == "self" || segs[0] == "cls") {
                        true
                    } else {
                        calling.contains(&r.file_path) && assigned_callees(&r.file_path, attr).iter().any(|c| defined.contains(c))
                    }
                }
            };
            if keep {
                kept.push(r);
            }
        }
        Ok(if distinct(&kept) <= ceiling { Some(resolvable(load(&kept)?)) } else { None })
    }

    /// Replace synthesis only after every base-resolution pass has finished: drop what the
    /// synthesizers made and run them again over the resolved graph, in one transaction.
    pub fn refresh_synthesis(&self) -> DbResult<usize> {
        self.clear_caches();
        let result = self.store.transaction(|| {
            self.store.delete_synthesized_edges()?;
            self.clear_caches();
            crate::callback_synthesizer::synthesize_callback_edges(&self.store, &self.ctx)
        });
        self.clear_caches();
        result
    }

    /// The resolution context, reading build configs from disk rather than the file cache.
    fn disk_ctx(&self) -> Rc<Ctx> {
        Ctx::new(self.ctx.project_root(), self.store.clone())
    }

    /// Digest of the package roots Python import resolution rests on (see
    /// `python_root_fingerprint`). Build configs are read from disk rather than the file cache, so
    /// a sync after an edit sees the edit. Empty for a project with no Python.
    pub fn get_python_root_fingerprint(&self) -> String {
        python_root_fingerprint(&self.disk_ctx())
    }

    /// What a sync must re-open after these Python files were added or removed (see
    /// `python_reopen_scope`).
    pub fn get_python_reopen_scope(&self, changed_files: &[String]) -> PythonReopenScope {
        python_reopen_scope(&self.disk_ctx(), changed_files)
    }

    /// Importers of Python packages whose `__init__.py` was edited, with the names they bind (see
    /// `python_package_importers`).
    pub fn get_python_package_importers(&self, init_files: &[String]) -> IndexMap<String, IndexMap<String, Option<String>>> {
        python_package_importers(&self.ctx, init_files)
    }

    /// The project file a module specifier written in `from_file` names, as its import resolves
    /// it; `None` when none.
    pub fn resolve_module_file(&self, specifier: &str, from_file: &str, language: Language) -> Option<String> {
        resolve_import_path(specifier, from_file, language, &self.ctx)
    }

    /// What `r` resolves to against the graph as it is now, writing nothing: its target and how it
    /// was found, or `None`. A sync compares it with the edge the reference made before, to
    /// re-open only an edge an edit moved.
    pub fn resolve_now(&self, r: &UnresolvedReference) -> Option<(String, String)> {
        if r.file_path.as_deref().is_none_or(str::is_empty) || r.language.is_none() {
            return None;
        }
        let resolved = self.resolve_one(&self.working_ref(r))?;
        Some((resolved.target_node_id, resolved.resolved_by.as_str().to_string()))
    }

    /// True when a detected router binds navigation calls to its routes (see
    /// [`ReferenceResolver::reopen_navigations_for`]).
    pub fn has_navigation_routers(&self) -> bool {
        self.frameworks.borrow().iter().any(|f| f.navigation().is_some())
    }

    /// Put back in the pending set the navigation calls `routes` — route nodes a sync added,
    /// removed or renamed (`changed_routes`) — may now resolve differently, for the sync's
    /// resolution sweep: the calls parked as failed, and the calls whose `navigates` edges a
    /// router's resolver made. Calls in `changed_file_paths` are left alone; the sync resolved
    /// them against the new routes already. Returns the number of references put back.
    ///
    /// A navigation call names its route by path (`navigate('/login')`) and its reference by the
    /// router's method (`navigate`, `history.push`), so a route table that changed in another file
    /// is invisible to the rest of sync. A call parked as failed while its route was missing is
    /// keyed on `push`, which no synced file defines, and a resolved call keeps the route it bound
    /// to while a better one was missing (a catch-all, a parameter route, one arm of a
    /// conditional) or a route renamed under it. A full index resolves every call against the
    /// final table. Each router says which calls a route of its own can answer
    /// (`FrameworkResolver::navigation`): the method tails, and the apps whose files match against
    /// that route's table. A name more of those calls share than `per_name_ceiling` is skipped, as
    /// the symbol retry skips a common tail.
    pub fn reopen_navigations_for(&self, routes: &[Node], changed_file_paths: &[String], per_name_ceiling: usize) -> DbResult<usize> {
        if routes.is_empty() {
            return Ok(0);
        }
        let frameworks = self.frameworks.borrow().clone();
        let mut plans: Vec<(crate::frameworks::FrameworkRef, Vec<String>)> = Vec::new();
        let mut tails: IndexSet<String> = IndexSet::new();
        for router in &frameworks {
            let Some(navigation) = router.navigation() else { continue };
            let mut scopes: IndexSet<String> = IndexSet::new();
            for route in routes {
                if let Some(found) = (navigation.scope)(route, &self.ctx) {
                    scopes.extend(found);
                }
            }
            if scopes.is_empty() {
                continue;
            }
            plans.push((router.clone(), scopes.into_iter().collect()));
            tails.extend(navigation.tails.iter().map(|t| t.to_string()));
        }
        if plans.is_empty() {
            return Ok(0);
        }
        let claimed = |name: &str, file_path: &str| -> bool {
            plans.iter().any(|(router, scopes)| scopes.iter().any(|scope| file_path.starts_with(scope.as_str())) && router.claims_reference(name))
        };

        let fresh: HashSet<&str> = changed_file_paths.iter().map(String::as_str).collect();
        let mut failed: IndexMap<String, Vec<i64>> = IndexMap::new();
        let tail_list: Vec<String> = tails.into_iter().collect();
        for (row_id, reference_name, file_path) in self.store.get_failed_calls_by_tail(&tail_list)? {
            if fresh.contains(file_path.as_str()) || !claimed(&reference_name, &file_path) {
                continue;
            }
            failed.entry(reference_name).or_default().push(row_id);
        }
        let mut resolved: IndexMap<String, Vec<ResolutionEdge>> = IndexMap::new();
        for edge in self.store.get_resolved_navigations()? {
            let Some(name) = edge.edge.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            if fresh.contains(edge.source_file_path.as_str()) || !claimed(&name, &edge.source_file_path) {
                continue;
            }
            resolved.entry(name).or_default().push(edge);
        }

        let mut row_ids: Vec<i64> = Vec::new();
        let mut edge_ids: Vec<i64> = Vec::new();
        let mut refs: Vec<UnresolvedReference> = Vec::new();
        // A conditional's arms are edges of one call: it goes back as one reference.
        let mut sites: HashSet<String> = HashSet::new();
        let names: IndexSet<&String> = failed.keys().chain(resolved.keys()).collect();
        for name in names {
            let rows = failed.get(name).map(Vec::as_slice).unwrap_or(&[]);
            let edges = resolved.get(name).map(Vec::as_slice).unwrap_or(&[]);
            if rows.len() + edges.len() > per_name_ceiling {
                continue;
            }
            row_ids.extend_from_slice(rows);
            for edge in edges {
                let Some(asked) = resurrect_ref_from_dropped_edge(&edge.edge, &edge.source_file_path, edge.source_language) else {
                    continue;
                };
                edge_ids.push(edge.edge_id);
                let site = format!("{}\0{}\0{}\0{}", asked.from_node_id, asked.reference_name, asked.line, asked.column);
                if !sites.insert(site) {
                    continue;
                }
                refs.push(to_db_reference(&asked));
            }
        }
        if !edge_ids.is_empty() {
            self.store.replace_resolution_edges_with_unresolved_refs(&edge_ids, &refs)?;
        }
        Ok(self.store.reopen_failed_references(&row_ids)? + refs.len())
    }

    /// The edited modules among `modified_file_paths` whose default export moved: an edit made
    /// `export default A` into `export default B`, or took the default away, while `A` stayed
    /// declared. A sync moves the edges other files made into a changed file onto the node that
    /// replaces each target, so `new Y()` after `import Y from './x'` kept `A` where a full index
    /// links `B`. An `export default` statement is no node, so neither the definition delta nor a
    /// failed-reference retry sees it change.
    ///
    /// The default a module had is read off the edges into it, with no file read: a default or
    /// namespace import of it is an `imports` edge into its file node named for the binding, and a
    /// whole-name reference through that binding (`Y`, not `Y.run`) that resolved through the
    /// import names exactly its default (a namespace binding's `NS` names the file). Only edges
    /// bound before this sync resolved anything count (up to `resolved_up_to_edge_id`), and only
    /// from files it left alone. A module that forwards its default from another file (`export {
    /// default } from './impl'`) has those edges there, so a few of its importers' are read
    /// instead. An edit that leaves the default alone, nearly every one, finds them all still on
    /// it. Each module comes with the declarations its default was (`was`), as those edges say.
    pub fn changed_default_exports(
        &self,
        modified_file_paths: &[String],
        changed_file_paths: &[String],
        resolved_up_to_edge_id: i64,
    ) -> DbResult<Vec<MovedDefault>> {
        let fresh: HashSet<&str> = changed_file_paths.iter().map(String::as_str).collect();
        let mut moved = Vec::new();
        for file_path in modified_file_paths {
            if !is_js_family_file(file_path) {
                continue;
            }
            let mut bindings: IndexMap<String, IndexSet<String>> = IndexMap::new();
            for i in self.store.get_imports_into(&format!("file:{file_path}"), file_path)? {
                let Some(ref_name) = i.ref_name else { continue };
                if fresh.contains(i.file_path.as_str()) || !is_module_binding_name(&ref_name) {
                    continue;
                }
                bindings.entry(i.file_path).or_default().insert(ref_name);
            }
            if bindings.is_empty() {
                continue;
            }
            let Some(language) = self.store.get_file_by_path(file_path)?.map(|f| f.language) else { continue };
            let reads_default = |e: &DefaultReader| -> bool {
                let Some(name) = e.edge.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str()) else {
                    return false;
                };
                e.edge.metadata.as_ref().and_then(|m| m.get("resolvedBy")).and_then(|v| v.as_str()) == Some("import")
                    && e.edge_id <= resolved_up_to_edge_id
                    && !e.edge.target.starts_with("file:")
                    && bindings.get(&e.source_file_path).is_some_and(|names| names.contains(name))
            };
            let mut readers: Vec<DefaultReader> = self
                .store
                .get_cross_file_incoming_edges_with_target(file_path)?
                .into_iter()
                .map(|e| DefaultReader {
                    edge: e.edge,
                    edge_id: e.edge_id,
                    source_file_path: e.source_file_path,
                    source_language: e.source_language,
                })
                .filter(|e| reads_default(e))
                .collect();
            if readers.is_empty() {
                let Some(now) = module_default_export(file_path, language, &self.ctx) else { continue };
                if now.file_path == *file_path {
                    continue;
                }
                for (file, names) in bindings.iter().take(DEFAULT_READER_SAMPLE_FILES) {
                    let names: Vec<String> = names.iter().cloned().collect();
                    for e in self.store.get_resolution_edges_through(file, &names)? {
                        let reader = DefaultReader {
                            edge: e.edge,
                            edge_id: e.edge_id,
                            source_file_path: e.source_file_path,
                            source_language: e.source_language,
                        };
                        if reads_default(&reader) {
                            readers.push(reader);
                        }
                    }
                }
            }
            let mut was: IndexSet<String> = IndexSet::new();
            for e in &readers {
                let now = module_default_export(file_path, e.source_language, &self.ctx);
                // A call that lands on an alias binding goes on to the callable it names (resolve_one).
                if let Some(now) = &now {
                    let lands = e.edge.target == now.id
                        || (e.edge.kind == EdgeKind::Calls
                            && resolve_alias_binding(now, None, &self.ctx).is_some_and(|resolved| resolved.id == e.edge.target));
                    if lands {
                        continue;
                    }
                }
                was.insert(e.edge.target.clone());
            }
            if !was.is_empty() {
                moved.push(MovedDefault { path: file_path.clone(), language, was: was.into_iter().collect() });
            }
        }
        Ok(moved)
    }

    /// Whether a failed import could name one of `files`, the files a sync just added: the test
    /// the import retry puts a crowded key's imports to (`Store::get_retryable_failed_imports`).
    /// `utils` is the path stem of a project's own `./utils` and of every `jotai/utils` and
    /// `@scope/pkg/utils` import alike, and `assert` the name of every binding of `import * as
    /// assert from 'assert'`, so past the retry's ceiling a key holds the few imports an added
    /// `utils.ts` or `assert.ts` is for among hundreds that no file of the project is.
    ///
    /// A JavaScript or TypeScript import reaches a file by the module it is written as (a path, a
    /// tsconfig alias or a workspace package, an `index` file and the emitted `.js` of a `.ts`
    /// included), by a framework's alias (SvelteKit's `$lib/…`) or by the file's own name
    /// (`…/utils.js`). One that none of those lead to an added file — a package, an import
    /// binding, a workspace package's subpath only its `exports` map resolves — names none of
    /// them. Of an import written in another language this knows nothing, so a crowded key skips
    /// it, as it always has.
    pub fn could_name_any_of<'a>(&'a self, files: &[String]) -> Box<dyn Fn(&UnresolvedReference) -> bool + 'a> {
        let added: HashSet<String> = files.iter().cloned().collect();
        let names: HashSet<String> = files.iter().map(|file| file.rsplit('/').next().unwrap_or(file).to_string()).collect();
        Box::new(move |reference: &UnresolvedReference| {
            let r: UnresolvedRef = self.working_ref(reference);
            if !is_esm_import_language(r.language) {
                return false;
            }
            let name = r.reference_name.as_str();
            if names.contains(name.rsplit('/').next().unwrap_or(name)) {
                return true;
            }
            // Where the specifier leads first: a package's leads nowhere, and only then is it
            // worth asking whether the resolver reads the reference as a module, which for a bare
            // name means reading the file's imports.
            if let Some(target) = resolve_import_path(name, &r.file_path, r.language, &self.ctx) {
                if added.contains(&target) && js_module_file(&r, &self.ctx).as_deref() == Some(target.as_str()) {
                    return true;
                }
            }
            // A framework's alias is a path (`$lib/x`, `~/x`); a bare name, which every binding
            // is, is no alias, and asking reads the file.
            name.contains('/')
                && self.frameworks_for(r.language).iter().any(|framework| {
                    framework
                        .resolve(&r, &self.ctx)
                        .is_some_and(|result| self.node_by_id(&result.target_node_id).is_some_and(|n| added.contains(&n.file_path)))
                })
        })
    }
}
