//! The resolution context — the `ResolutionContext` interface of
//! `src/resolution/types.ts` together with the caches `ReferenceResolver.createContext`
//! hangs behind it (`src/resolution/index.ts`).
//!
//! One concrete struct, shared as `&Ctx`/`Rc<Ctx>`. Single-threaded: every cache is a
//! `RefCell`. The TypeScript per-resolver LRU caches are bounded maps here that start over
//! when full — the caches are pure memoization of stable DB reads, so eviction order has no
//! effect on results.

use crate::go_module::{find_go_module_for_import, go_module_package_dir, load_go_module, GoModule};
use crate::js_class_expressions::{build_class_expression_index, ClassExpressionIndex};
use crate::import_resolver::{
    extract_import_mappings, extract_re_exports, is_external_import, load_cpp_include_dirs, resolve_import_path,
    ExternalOpts,
};
use crate::name_matcher::same_language_family;
use crate::path_aliases::{load_project_aliases, AliasMap};
use crate::swift_type_visibility::swift_extended_conformances;
use crate::types::{ImportMapping, NodeList, NodeRef, ReExport};
use crate::workspace_packages::{load_workspace_packages, WorkspacePackages};
use codegraph_db::resolver_queries::CppIncludeNode;
use codegraph_db::Store;
use codegraph_jsre::re;
use codegraph_types::log::log_debug;
use codegraph_types::paths::lexical_path_within_root;
use codegraph_types::{nodepath, EdgeKind, Language, Node, NodeKind};
use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::rc::Rc;

/// A Jest or Vitest manual mock: a file under a `__mocks__/` directory (isManualMockPath).
pub fn is_manual_mock_path(file_path: &str) -> bool {
    let p = js_lower(&file_path.replace('\\', "/"));
    p.starts_with("__mocks__/") || p.contains("/__mocks__/")
}

/// Whether a definition in `candidate_path` is a manual mock that a reference from
/// `from_path`, which is no test code, cannot mean (isManualMockHiddenFrom, #2455): the rule
/// the mock-free view applies, for the passes that pick a component or handler by name.
pub fn is_manual_mock_hidden_from(candidate_path: &str, from_path: &str) -> bool {
    is_manual_mock_path(candidate_path) && !codegraph_search::is_test_path(from_path)
}

/// The names the project's manual mocks declare (ReferenceResolver.manualMockNames).
pub struct ManualMockNames {
    pub names: HashSet<String>,
    pub lower_names: HashSet<String>,
}

/// The mock-free view's own answers (the maps of ReferenceResolver.mocklessContextFor).
#[derive(Default)]
struct MocklessCaches {
    by_name: HashMap<String, NodeList>,
    by_lower_name: HashMap<String, NodeList>,
    by_kind: HashMap<NodeKind, NodeList>,
}

/// The nodes of `nodes` outside a manual mock (the same list when none is in one).
fn without_mocks(nodes: NodeList) -> NodeList {
    if nodes.iter().any(|n| is_manual_mock_path(&n.file_path)) {
        Rc::new(nodes.iter().filter(|n| !is_manual_mock_path(&n.file_path)).cloned().collect())
    } else {
        nodes
    }
}

/// Largest source file the resolver reads: the same limit the indexer applies.
use codegraph_types::limits::max_source_file_bytes;

const DEFAULT_CACHE_LIMIT: usize = 5_000;

fn cache_limit() -> usize {
    std::env::var("CODEGRAPHV_RESOLVER_CACHE_SIZE")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_CACHE_LIMIT)
}

/// A size-bounded memo: when full it starts over.
struct Bounded<K, V> {
    map: HashMap<K, V>,
    limit: usize,
}

impl<K: Hash + Eq, V: Clone> Bounded<K, V> {
    fn new(limit: usize) -> Self {
        Bounded { map: HashMap::new(), limit }
    }
    fn get(&self, k: &K) -> Option<V> {
        self.map.get(k).cloned()
    }
    fn contains(&self, k: &K) -> bool {
        self.map.contains_key(k)
    }
    fn set(&mut self, k: K, v: V) {
        if self.map.len() >= self.limit && !self.map.contains_key(&k) {
            self.map.clear();
        }
        self.map.insert(k, v);
    }
    fn clear(&mut self) {
        self.map.clear();
    }
}

/// The edge kinds resolving a reference reads (`getSupertypes`' walk).
pub const RESOLUTION_READ_EDGE_KINDS: [EdgeKind; 2] = [EdgeKind::Implements, EdgeKind::Extends];

/// Node kinds that can declare supertypes (extends/implements).
pub fn is_supertype_bearing_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class | NodeKind::Struct | NodeKind::Interface | NodeKind::Trait | NodeKind::Protocol | NodeKind::Enum
    )
}

/// Node's `builtinModules`.
const NODE_BUILTINS: &[&str] = &[
    "_http_agent", "_http_client", "_http_common", "_http_incoming", "_http_outgoing", "_http_server",
    "_stream_duplex", "_stream_passthrough", "_stream_readable", "_stream_transform", "_stream_wrap",
    "_stream_writable", "_tls_common", "_tls_wrap", "assert", "assert/strict", "async_hooks", "buffer",
    "child_process", "cluster", "console", "constants", "crypto", "dgram", "diagnostics_channel", "dns",
    "dns/promises", "domain", "events", "fs", "fs/promises", "http", "http2", "https", "inspector",
    "inspector/promises", "module", "net", "os", "path", "path/posix", "path/win32", "perf_hooks", "process",
    "punycode", "querystring", "readline", "readline/promises", "repl", "stream", "stream/consumers",
    "stream/promises", "stream/web", "string_decoder", "sys", "timers", "timers/promises", "tls",
    "trace_events", "tty", "url", "util", "util/types", "v8", "vm", "wasi", "worker_threads", "zlib",
    "node:sea", "node:sqlite", "node:test", "node:test/reporters",
];

/// Per directory: the package names its package.json and every enclosing one own and depend on.
#[derive(Debug, Clone, Default)]
pub struct ManifestScope {
    pub own: HashSet<String>,
    pub deps: HashSet<String>,
}

/// Unwrap a store read; a failed read logs and reads as empty, as a thrown query would
/// have ended that reference's resolution in the TypeScript engine's catch.
pub(crate) fn q<T: Default>(what: &str, r: codegraph_db::Result<T>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => {
            log_debug("resolver query failed", &[("query", what.to_string()), ("error", e.to_string())]);
            T::default()
        }
    }
}

fn wrap_nodes(nodes: Vec<Node>) -> NodeList {
    Rc::new(nodes.into_iter().map(Rc::new).collect())
}

/// The project-relative directory holding `p` ('' for the project root).
pub fn go_dir_of(p: &str) -> String {
    let dir = nodepath::posix::dirname(&p.replace('\\', "/"));
    if dir == "." || dir == "/" {
        String::new()
    } else {
        dir
    }
}

pub struct Ctx {
    store: Rc<Store>,
    project_root: String,

    node_cache: RefCell<Bounded<String, NodeList>>,
    file_cache: RefCell<Bounded<String, Option<Rc<str>>>>,
    import_mapping_cache: RefCell<Bounded<String, Rc<Vec<ImportMapping>>>>,
    re_export_cache: RefCell<Bounded<String, Rc<Vec<ReExport>>>>,
    name_cache: RefCell<Bounded<String, NodeList>>,
    lower_name_cache: RefCell<Bounded<String, NodeList>>,
    qualified_name_cache: RefCell<Bounded<String, NodeList>>,
    file_lines_cache: RefCell<Bounded<String, Option<Rc<Vec<String>>>>>,
    node_by_id_cache: RefCell<Bounded<String, NodeRef>>,
    method_match_cache: RefCell<Bounded<String, NodeList>>,
    method_owner_index_cache: RefCell<HashMap<String, Rc<HashMap<String, Vec<NodeRef>>>>>,
    supertype_gen: Cell<u64>,
    supertype_memo: RefCell<HashMap<String, (u64, Rc<Vec<String>>)>>,
    supertype_node_memo: RefCell<HashMap<String, (u64, NodeList)>>,
    /// get_superclass_nodes' memo (`extends` edges alone), under the same generation tag.
    superclass_node_memo: RefCell<HashMap<String, (u64, NodeList)>>,
    nodes_by_kind_cache: RefCell<HashMap<NodeKind, NodeList>>,
    file_exists_memo: RefCell<HashMap<String, bool>>,
    known_names: RefCell<Option<Rc<HashSet<String>>>>,
    /// The receiver chains of the Rust calls being resolved by their method's name
    /// (`rustChainedCallByName`), each under the address of the by-name reference: as the TS
    /// WeakMap, a copy of the reference doesn't carry it.
    rust_chains: RefCell<Vec<(usize, Rc<Vec<String>>)>>,
    known_lower_names: RefCell<Option<Rc<HashSet<String>>>>,
    known_files: RefCell<Option<Rc<HashSet<String>>>>,
    caches_warmed: Cell<bool>,
    project_aliases: RefCell<Option<Option<Rc<AliasMap>>>>,
    dir_aliases: RefCell<HashMap<String, Option<Rc<AliasMap>>>>,
    go_module_by_dir: RefCell<HashMap<String, Option<Rc<GoModule>>>>,
    go_modules: RefCell<Option<Rc<Vec<Rc<GoModule>>>>>,
    go_package_dirs: RefCell<HashMap<String, Option<String>>>,
    workspace_packages: RefCell<Option<Option<Rc<WorkspacePackages>>>>,
    manifest_scopes: RefCell<HashMap<String, Rc<ManifestScope>>>,
    /// Module-level memos (the TypeScript per-context `WeakMap`s), by memo type.
    memos: RefCell<HashMap<TypeId, Rc<dyn Any>>>,
    /// A file list standing in for the index's, for framework detection before anything is stored.
    file_list: RefCell<Option<Rc<Vec<String>>>>,
    /// Resolving production code in a project with manual mocks: lookups answer as if no
    /// `__mocks__/` file were indexed, and memos are the view's own (#2455).
    mockless: Cell<bool>,
    /// The manual mocks' names: None until read, then Some(None) when the project has none.
    manual_mocks: RefCell<Option<Option<Rc<ManualMockNames>>>>,
    mockless_caches: RefCell<MocklessCaches>,
    /// Resolving a TS/JS reference outside the named class expressions: lookups answer as if
    /// their classes and members were not indexed (#2499).
    class_free: Cell<bool>,
    /// The class expressions' classes: None until read, then Some(None) when there are none.
    class_expressions: RefCell<Option<Option<Rc<ClassExpressionIndex>>>>,
    /// The names only the class expressions' classes and members carry.
    class_only_names: RefCell<Option<Rc<HashSet<String>>>>,
    /// The class-expression-free view's nodes by kind, by whether the mock-free view is on too.
    class_free_by_kind: RefCell<HashMap<(bool, NodeKind), NodeList>>,
    /// The memos of the views other than the whole graph, by view (`view_bits`): a TypeScript
    /// WeakMap keyed by the view's context object.
    view_memos: RefCell<HashMap<(u8, TypeId), Rc<dyn Any>>>,
}

impl Ctx {
    pub fn new(project_root: impl Into<String>, store: Rc<Store>) -> Rc<Ctx> {
        let limit = cache_limit();
        let content_limit = std::cmp::max(64, limit / 5);
        Rc::new(Ctx {
            store,
            project_root: project_root.into(),
            node_cache: RefCell::new(Bounded::new(limit)),
            file_cache: RefCell::new(Bounded::new(content_limit)),
            import_mapping_cache: RefCell::new(Bounded::new(limit)),
            re_export_cache: RefCell::new(Bounded::new(limit)),
            name_cache: RefCell::new(Bounded::new(limit)),
            lower_name_cache: RefCell::new(Bounded::new(limit)),
            qualified_name_cache: RefCell::new(Bounded::new(limit)),
            file_lines_cache: RefCell::new(Bounded::new(content_limit)),
            node_by_id_cache: RefCell::new(Bounded::new(std::cmp::max(limit * 4, 20_000))),
            method_match_cache: RefCell::new(Bounded::new(limit)),
            method_owner_index_cache: RefCell::new(HashMap::new()),
            supertype_gen: Cell::new(0),
            supertype_memo: RefCell::new(HashMap::new()),
            supertype_node_memo: RefCell::new(HashMap::new()),
            superclass_node_memo: RefCell::new(HashMap::new()),
            nodes_by_kind_cache: RefCell::new(HashMap::new()),
            file_exists_memo: RefCell::new(HashMap::new()),
            known_names: RefCell::new(None),
            rust_chains: RefCell::new(Vec::new()),
            known_lower_names: RefCell::new(None),
            known_files: RefCell::new(None),
            caches_warmed: Cell::new(false),
            project_aliases: RefCell::new(None),
            dir_aliases: RefCell::new(HashMap::new()),
            go_module_by_dir: RefCell::new(HashMap::new()),
            go_modules: RefCell::new(None),
            go_package_dirs: RefCell::new(HashMap::new()),
            workspace_packages: RefCell::new(None),
            manifest_scopes: RefCell::new(HashMap::new()),
            memos: RefCell::new(HashMap::new()),
            file_list: RefCell::new(None),
            mockless: Cell::new(false),
            manual_mocks: RefCell::new(None),
            mockless_caches: RefCell::new(MocklessCaches::default()),
            class_free: Cell::new(false),
            class_expressions: RefCell::new(None),
            class_only_names: RefCell::new(None),
            class_free_by_kind: RefCell::new(HashMap::new()),
            view_memos: RefCell::new(HashMap::new()),
        })
    }

    /// A context whose file list is `files` rather than the index's — the orchestrator's
    /// detection context, built from a scan before any file is stored. Node queries still go to
    /// `store` (an empty scratch database there), as detectors only read files.
    pub fn with_file_list(project_root: impl Into<String>, store: Rc<Store>, files: Vec<String>) -> Rc<Ctx> {
        let ctx = Ctx::new(project_root, store);
        *ctx.file_list.borrow_mut() = Some(Rc::new(files));
        ctx
    }

    pub fn store(&self) -> &Rc<Store> {
        &self.store
    }

    /// Get project root.
    pub fn project_root(&self) -> &str {
        &self.project_root
    }

    // -- Memo and cache lifetime ---------------------------------------------

    /// The module-level memo of type `T` (created on first use, dropped by `clear_caches`).
    /// `T` carries its own interior mutability; give each memo its own newtype.
    pub fn memo<T: Default + 'static>(&self) -> Rc<T> {
        let id = TypeId::of::<T>();
        // Each view keeps memos of its own, as a TypeScript WeakMap keyed by the view's context
        // object does.
        let view = self.view_bits();
        if view != 0 {
            if let Some(m) = self.view_memos.borrow().get(&(view, id)) {
                return Rc::clone(m).downcast::<T>().ok().expect("memo type id collision");
            }
            let m: Rc<T> = Rc::new(T::default());
            self.view_memos.borrow_mut().insert((view, id), m.clone() as Rc<dyn Any>);
            return m;
        }
        if let Some(m) = self.memos.borrow().get(&id) {
            return Rc::clone(m).downcast::<T>().ok().expect("memo type id collision");
        }
        let m: Rc<T> = Rc::new(T::default());
        self.memos.borrow_mut().insert(id, m.clone() as Rc<dyn Any>);
        m
    }

    /// Which view lookups answer for: bit 0 the mock-free one, bit 1 the class-expression-free one.
    fn view_bits(&self) -> u8 {
        u8::from(self.mockless.get()) | (u8::from(self.class_free.get()) << 1)
    }

    /// The names the project's manual mocks declare, None when it has none; read once per
    /// cache window (ReferenceResolver.manualMockNames).
    pub fn manual_mock_names(&self) -> Option<Rc<ManualMockNames>> {
        if let Some(cached) = self.manual_mocks.borrow().as_ref() {
            return cached.clone();
        }
        let files: Vec<String> = match self.known_files.borrow().as_ref() {
            Some(known) => known.iter().filter(|f| is_manual_mock_path(f)).cloned().collect(),
            None => q("getAllFilePaths", self.store.get_all_file_paths()).into_iter().filter(|f| is_manual_mock_path(f)).collect(),
        };
        let mut names = HashSet::new();
        let mut lower_names = HashSet::new();
        if !files.is_empty() {
            for n in q("getNodesByFiles", self.store.get_nodes_by_files(&files)) {
                lower_names.insert(js_lower(&n.name));
                names.insert(n.name);
            }
        }
        let mocks = (!names.is_empty()).then(|| Rc::new(ManualMockNames { names, lower_names }));
        *self.manual_mocks.borrow_mut() = Some(mocks.clone());
        mocks
    }

    /// Run `f` on the mock-free view when `on` (production code in a project with manual
    /// mocks, ReferenceResolver.resolveOne), else on the whole graph.
    pub fn with_mockless<R>(&self, on: bool, f: impl FnOnce() -> R) -> R {
        self.with_view(on, self.class_free.get(), f)
    }

    /// Run `f` with the mock-free view when `mockless` and the class-expression-free view when
    /// `class_free` (ReferenceResolver.resolveOne), restoring the outer view after.
    pub fn with_view<R>(&self, mockless: bool, class_free: bool, f: impl FnOnce() -> R) -> R {
        let outer = (self.mockless.replace(mockless), self.class_free.replace(class_free));
        let result = f();
        self.mockless.set(outer.0);
        self.class_free.set(outer.1);
        result
    }

    /// Whether lookups hide the class expressions' classes and members.
    pub fn class_free_view(&self) -> bool {
        self.class_free.get()
    }

    /// The classes named class expressions declare, read once per cache window; None when
    /// there are none (ReferenceResolver.classExpressions).
    pub fn class_expressions(&self) -> Option<Rc<ClassExpressionIndex>> {
        if let Some(cached) = self.class_expressions.borrow().as_ref() {
            return cached.clone();
        }
        let classes = self.all_nodes_by_kind(NodeKind::Class);
        let index = build_class_expression_index(
            &classes,
            |file| {
                self.get_file_lines(file)
                    .or_else(|| self.read_file(file).map(|s| Rc::new(re!(r"\r?\n").split(&s).into_iter().map(String::from).collect())))
                    .unwrap_or_default()
            },
            |files| q("getNodesByFiles", self.store.get_nodes_by_files(files)),
        )
        .map(Rc::new);
        *self.class_expressions.borrow_mut() = Some(index.clone());
        index
    }

    /// The names only the class expressions' classes and members carry, which name nothing in
    /// the view that hides them (classExpressionOnlyNames).
    pub fn class_expression_only_names(&self) -> Rc<HashSet<String>> {
        if let Some(cached) = self.class_only_names.borrow().as_ref() {
            return Rc::clone(cached);
        }
        let mut only = HashSet::new();
        if let Some(index) = self.class_expressions() {
            let names: HashSet<String> = index.hidden_ids.iter().filter_map(|id| self.node_by_id(id)).map(|n| n.name.clone()).collect();
            for name in names {
                if !self.all_nodes_by_name(&name).iter().any(|n| !index.hidden_ids.contains(&n.id)) {
                    only.insert(name);
                }
            }
        }
        let only = Rc::new(only);
        *self.class_only_names.borrow_mut() = Some(Rc::clone(&only));
        only
    }

    /// The class expressions while the class-expression-free view is on.
    fn active_class_expressions(&self) -> Option<Rc<ClassExpressionIndex>> {
        if self.class_free.get() {
            self.class_expressions()
        } else {
            None
        }
    }

    /// `nodes` without the class expressions' classes and members while the view hiding them is
    /// on (the same list when none is among them).
    fn keep_visible(&self, nodes: NodeList) -> NodeList {
        match self.active_class_expressions() {
            Some(index) if nodes.iter().any(|n| index.hidden_ids.contains(&n.id)) => {
                Rc::new(nodes.iter().filter(|n| !index.hidden_ids.contains(&n.id)).cloned().collect())
            }
            _ => nodes,
        }
    }

    /// The manual mocks' names while the mock-free view is on.
    fn active_mocks(&self) -> Option<Rc<ManualMockNames>> {
        if self.mockless.get() {
            self.manual_mock_names()
        } else {
            None
        }
    }

    /// Drop every cache and memo.
    pub fn clear_caches(&self) {
        self.node_cache.borrow_mut().clear();
        self.file_cache.borrow_mut().clear();
        self.import_mapping_cache.borrow_mut().clear();
        self.re_export_cache.borrow_mut().clear();
        self.name_cache.borrow_mut().clear();
        self.lower_name_cache.borrow_mut().clear();
        self.qualified_name_cache.borrow_mut().clear();
        self.file_lines_cache.borrow_mut().clear();
        self.node_by_id_cache.borrow_mut().clear();
        self.method_match_cache.borrow_mut().clear();
        self.method_owner_index_cache.borrow_mut().clear();
        self.supertype_memo.borrow_mut().clear();
        self.supertype_node_memo.borrow_mut().clear();
        self.superclass_node_memo.borrow_mut().clear();
        self.supertype_gen.set(self.supertype_gen.get() + 1);
        self.nodes_by_kind_cache.borrow_mut().clear();
        self.file_exists_memo.borrow_mut().clear();
        self.manifest_scopes.borrow_mut().clear();
        *self.known_names.borrow_mut() = None;
        *self.known_lower_names.borrow_mut() = None;
        *self.known_files.borrow_mut() = None;
        *self.go_modules.borrow_mut() = None;
        self.go_package_dirs.borrow_mut().clear();
        self.caches_warmed.set(false);
        // The import resolver's and name matcher's per-context memos assume the same stable
        // window as the caches above — drop them together.
        self.memos.borrow_mut().clear();
        *self.manual_mocks.borrow_mut() = None;
        *self.mockless_caches.borrow_mut() = MocklessCaches::default();
        *self.class_expressions.borrow_mut() = None;
        *self.class_only_names.borrow_mut() = None;
        self.class_free_by_kind.borrow_mut().clear();
        self.view_memos.borrow_mut().clear();
    }

    /// Pre-build the lightweight caches for resolution: the known file paths and symbol names.
    pub fn warm_caches(&self) {
        if self.caches_warmed.get() {
            return;
        }
        *self.known_files.borrow_mut() = Some(Rc::new(q("getAllFilePaths", self.store.get_all_file_paths()).into_iter().collect()));
        *self.known_names.borrow_mut() = Some(Rc::new(q("getAllNodeNames", self.store.get_all_node_names()).into_iter().collect()));
        self.caches_warmed.set(true);
    }

    /// Invalidate the getSupertypes memo — call when resolved edges may have advanced.
    pub fn advance_supertype_generation(&self) {
        self.supertype_gen.set(self.supertype_gen.get() + 1);
        if self.supertype_memo.borrow().len() > 50_000 {
            self.supertype_memo.borrow_mut().clear();
        }
        if self.superclass_node_memo.borrow().len() > 50_000 {
            self.superclass_node_memo.borrow_mut().clear();
        }
        if self.supertype_node_memo.borrow().len() > 50_000 {
            self.supertype_node_memo.borrow_mut().clear();
        }
    }

    /// Run `f` with `chain` the receiver chain of the Rust call `r` (see `rust_chain_of`).
    pub fn with_rust_chain<T>(&self, r: &crate::types::UnresolvedRef, chain: Rc<Vec<String>>, f: impl FnOnce() -> T) -> T {
        self.rust_chains.borrow_mut().push((r as *const crate::types::UnresolvedRef as usize, chain));
        let out = f();
        self.rust_chains.borrow_mut().pop();
        out
    }

    /// The receiver chain, root first, of the Rust call `r` resolution reads by its method's name.
    pub fn rust_chain_of(&self, r: &crate::types::UnresolvedRef) -> Option<Rc<Vec<String>>> {
        let at = r as *const crate::types::UnresolvedRef as usize;
        self.rust_chains.borrow().iter().rev().find(|(a, _)| *a == at).map(|(_, c)| c.clone())
    }

    pub fn known_names(&self) -> Option<Rc<HashSet<String>>> {
        self.known_names.borrow().clone()
    }

    pub fn known_files(&self) -> Option<Rc<HashSet<String>>> {
        self.known_files.borrow().clone()
    }

    /// Lowercased `known_names`, built the first time a case-insensitive language asks.
    pub fn known_lower_names(&self) -> Option<Rc<HashSet<String>>> {
        let names = self.known_names()?;
        let mut slot = self.known_lower_names.borrow_mut();
        if slot.as_ref().is_none_or(|s| s.is_empty()) {
            *slot = Some(Rc::new(names.iter().map(|n| js_lower(n)).collect()));
        }
        slot.clone()
    }

    // -- Nodes ---------------------------------------------------------------

    /// `queries.getNodeById` through the resolver's own id → node cache.
    pub fn node_by_id(&self, id: &str) -> Option<NodeRef> {
        if let Some(n) = self.node_by_id_cache.borrow().get(&id.to_string()) {
            return Some(n);
        }
        let node = q("getNodeById", self.store.get_node_by_id(id)).map(Rc::new);
        if let Some(n) = &node {
            self.node_by_id_cache.borrow_mut().set(id.to_string(), Rc::clone(n));
        }
        node
    }

    /// Get all nodes in a file.
    pub fn get_nodes_in_file(&self, file_path: &str) -> NodeList {
        let nodes = self.all_nodes_in_file(file_path);
        match self.active_class_expressions() {
            Some(index) if index.files.contains(file_path) => self.keep_visible(nodes),
            _ => nodes,
        }
    }

    fn all_nodes_in_file(&self, file_path: &str) -> NodeList {
        if let Some(c) = self.node_cache.borrow().get(&file_path.to_string()) {
            return c;
        }
        let nodes = wrap_nodes(q("getNodesByFile", self.store.get_nodes_by_file(file_path)));
        self.node_cache.borrow_mut().set(file_path.to_string(), Rc::clone(&nodes));
        nodes
    }

    /// Whether any node in the file is exported.
    pub fn file_has_exported_node(&self, file_path: &str) -> bool {
        match self.node_cache.borrow().get(&file_path.to_string()) {
            Some(c) => c.iter().any(|n| n.is_exported == Some(true)),
            None => q("fileHasExportedNode", self.store.file_has_exported_node(file_path)),
        }
    }

    /// `get_nodes_in_file(f)` narrowed to the exported ones.
    pub fn get_exported_nodes_in_file(&self, file_path: &str) -> Vec<NodeRef> {
        match self.node_cache.borrow().get(&file_path.to_string()) {
            Some(c) => c.iter().filter(|n| n.is_exported == Some(true)).cloned().collect(),
            None => q("getExportedNodesByFile", self.store.get_exported_nodes_by_file(file_path)).into_iter().map(Rc::new).collect(),
        }
    }

    /// `get_nodes_in_file(f)` narrowed to the nodes named `name`.
    pub fn get_nodes_in_file_named(&self, file_path: &str, name: &str) -> Vec<NodeRef> {
        match self.node_cache.borrow().get(&file_path.to_string()) {
            Some(c) => c.iter().filter(|n| n.name == name).cloned().collect(),
            None => q("getNodesByFileAndName", self.store.get_nodes_by_file_and_name(file_path, name)).into_iter().map(Rc::new).collect(),
        }
    }

    /// Get all nodes by name.
    pub fn get_nodes_by_name(&self, name: &str) -> NodeList {
        self.keep_visible(self.mockless_nodes_by_name(name))
    }

    fn mockless_nodes_by_name(&self, name: &str) -> NodeList {
        if let Some(mocks) = self.active_mocks() {
            if mocks.names.contains(name) {
                if let Some(c) = self.mockless_caches.borrow().by_name.get(name) {
                    return Rc::clone(c);
                }
                let r = without_mocks(self.all_nodes_by_name(name));
                self.mockless_caches.borrow_mut().by_name.insert(name.to_string(), Rc::clone(&r));
                return r;
            }
        }
        self.all_nodes_by_name(name)
    }

    /// `get_nodes_by_name` on the whole graph.
    fn all_nodes_by_name(&self, name: &str) -> NodeList {
        if let Some(c) = self.name_cache.borrow().get(&name.to_string()) {
            return c;
        }
        let r = wrap_nodes(q("getNodesByName", self.store.get_nodes_by_name(name)));
        self.name_cache.borrow_mut().set(name.to_string(), Rc::clone(&r));
        r
    }

    /// Get all nodes by qualified name.
    pub fn get_nodes_by_qualified_name(&self, qualified_name: &str) -> NodeList {
        let cached = self.qualified_name_cache.borrow().get(&qualified_name.to_string());
        let r = cached.unwrap_or_else(|| {
            let r = wrap_nodes(q("getNodesByQualifiedNameExact", self.store.get_nodes_by_qualified_name_exact(qualified_name)));
            self.qualified_name_cache.borrow_mut().set(qualified_name.to_string(), Rc::clone(&r));
            r
        });
        self.keep_visible(if self.active_mocks().is_some() { without_mocks(r) } else { r })
    }

    /// Get nodes by lowercase name (O(1) lookup for fuzzy matching).
    pub fn get_nodes_by_lower_name(&self, lower_name: &str) -> NodeList {
        self.keep_visible(self.mockless_nodes_by_lower_name(lower_name))
    }

    /// The names of every node of the given kinds in one language (`getNodeNamesOfKinds`).
    pub fn get_node_names_of_kinds(&self, language: Language, kinds: &[NodeKind]) -> Vec<String> {
        q("getNodeNamesOfKinds", self.store.get_node_names_of_kinds(language, kinds))
    }

    fn mockless_nodes_by_lower_name(&self, lower_name: &str) -> NodeList {
        if let Some(mocks) = self.active_mocks() {
            if mocks.lower_names.contains(lower_name) {
                if let Some(c) = self.mockless_caches.borrow().by_lower_name.get(lower_name) {
                    return Rc::clone(c);
                }
                let r = without_mocks(self.all_nodes_by_lower_name(lower_name));
                self.mockless_caches.borrow_mut().by_lower_name.insert(lower_name.to_string(), Rc::clone(&r));
                return r;
            }
        }
        self.all_nodes_by_lower_name(lower_name)
    }

    fn all_nodes_by_lower_name(&self, lower_name: &str) -> NodeList {
        if let Some(c) = self.lower_name_cache.borrow().get(&lower_name.to_string()) {
            return c;
        }
        let r = wrap_nodes(q("getNodesByLowerName", self.store.get_nodes_by_lower_name(lower_name)));
        self.lower_name_cache.borrow_mut().set(lower_name.to_string(), Rc::clone(&r));
        r
    }

    /// Get all nodes of a kind (cached; callers must treat the list as read-only).
    pub fn get_nodes_by_kind(&self, kind: NodeKind) -> NodeList {
        if self.active_class_expressions().is_none() {
            return self.mockless_nodes_by_kind(kind);
        }
        let key = (self.mockless.get(), kind);
        if let Some(c) = self.class_free_by_kind.borrow().get(&key) {
            return Rc::clone(c);
        }
        let r = self.keep_visible(self.mockless_nodes_by_kind(kind));
        self.class_free_by_kind.borrow_mut().insert(key, Rc::clone(&r));
        r
    }

    fn mockless_nodes_by_kind(&self, kind: NodeKind) -> NodeList {
        if self.active_mocks().is_some() {
            if let Some(c) = self.mockless_caches.borrow().by_kind.get(&kind) {
                return Rc::clone(c);
            }
            let r = without_mocks(self.all_nodes_by_kind(kind));
            self.mockless_caches.borrow_mut().by_kind.insert(kind, Rc::clone(&r));
            return r;
        }
        self.all_nodes_by_kind(kind)
    }

    fn all_nodes_by_kind(&self, kind: NodeKind) -> NodeList {
        if let Some(c) = self.nodes_by_kind_cache.borrow().get(&kind) {
            return Rc::clone(c);
        }
        let r = wrap_nodes(q("getNodesByKind", self.store.get_nodes_by_kind(kind)));
        self.nodes_by_kind_cache.borrow_mut().insert(kind, Rc::clone(&r));
        r
    }

    /// Stream nodes of a kind one at a time, without populating the per-kind cache.
    pub fn for_each_node_of_kind(&self, kind: NodeKind, mut visit: impl FnMut(Node)) {
        let mockless = self.active_mocks().is_some();
        let hidden = self.active_class_expressions();
        let r = self.store.for_each_node_of_kind(kind, |n| {
            if !(mockless && is_manual_mock_path(&n.file_path)) && !hidden.as_ref().is_some_and(|h| h.hidden_ids.contains(&n.id)) {
                visit(n);
            }
            true
        });
        q("iterateNodesByKind", r.map(|_| ()));
    }

    /// The method-definition nodes matching `typeName::methodName` in the language family —
    /// exactly `resolveMethodOnType`'s kind/language/qualifiedName-suffix filter, cached per
    /// (language, type, method).
    pub fn get_method_matches(&self, type_name: &str, method_name: &str, language: Language) -> NodeList {
        let matches = self.all_method_matches(type_name, method_name, language);
        self.keep_visible(match self.active_mocks() {
            Some(mocks) if mocks.names.contains(method_name) => without_mocks(matches),
            _ => matches,
        })
    }

    fn all_method_matches(&self, type_name: &str, method_name: &str, language: Language) -> NodeList {
        let key = format!("{} {}::{}", language.as_str(), type_name, method_name);
        if let Some(c) = self.method_match_cache.borrow().get(&key) {
            return c;
        }
        let candidates = self.all_nodes_by_name(method_name);
        let want = format!("{type_name}::{method_name}");
        let matches: Vec<NodeRef>;
        if type_name.contains("::") || method_name.contains(':') {
            // Legacy linear filter for the shapes the owner index below can't key exactly.
            let suffix = format!("::{want}");
            matches = candidates
                .iter()
                .filter(|m| {
                    m.kind == NodeKind::Method
                        && same_language_family(m.language, language)
                        && (m.qualified_name == want || m.qualified_name.ends_with(&suffix))
                })
                .cloned()
                .collect();
        } else {
            // Owner index: bucket each (language, methodName)'s candidates ONCE by the
            // qualifiedName's last two `::` segments, then every query is a map lookup.
            let idx_key = format!("{} {}", language.as_str(), method_name);
            let cached = self.method_owner_index_cache.borrow().get(&idx_key).cloned();
            let index = match cached {
                Some(i) => i,
                None => {
                    let mut idx: HashMap<String, Vec<NodeRef>> = HashMap::new();
                    for m in candidates.iter() {
                        if m.kind != NodeKind::Method || !same_language_family(m.language, language) {
                            continue;
                        }
                        let qn = &m.qualified_name;
                        let Some(i2) = qn.rfind("::") else { continue };
                        let bucket_key = match qn[..i2].rfind("::") {
                            Some(i1) => &qn[i1 + 2..],
                            None => qn.as_str(),
                        };
                        idx.entry(bucket_key.to_string()).or_default().push(Rc::clone(m));
                    }
                    let idx = Rc::new(idx);
                    self.method_owner_index_cache.borrow_mut().insert(idx_key, Rc::clone(&idx));
                    idx
                }
            };
            matches = index.get(&want).cloned().unwrap_or_default();
        }
        let matches = Rc::new(matches);
        self.method_match_cache.borrow_mut().set(key, Rc::clone(&matches));
        matches
    }

    // -- Files ---------------------------------------------------------------

    /// Every indexed file path, in path order.
    pub fn get_all_files(&self) -> Vec<String> {
        if let Some(files) = self.file_list.borrow().as_ref() {
            return files.as_ref().clone();
        }
        q("getAllFilePaths", self.store.get_all_file_paths())
    }

    /// Is `file_path` (project-relative) one of the files the index holds?
    pub fn is_indexed_file(&self, file_path: &str) -> bool {
        let normalized = file_path.replace('\\', "/");
        match self.known_files.borrow().as_ref() {
            Some(k) => k.contains(&normalized),
            None => q("getFileByPath", self.store.get_file_by_path(&normalized)).is_some(),
        }
    }

    /// Check if a file exists (indexed, or on disk within the project root).
    pub fn file_exists(&self, file_path: &str) -> bool {
        if let Some(known) = self.known_files.borrow().as_ref() {
            let normalized = file_path.replace('\\', "/");
            if known.contains(file_path) || known.contains(&normalized) {
                return true;
            }
        }
        // Fall back to the filesystem for files not yet indexed. The probe is contained
        // lexically (#1631) and memoized (the tree does not change within a pass).
        if let Some(p) = self.file_exists_memo.borrow().get(file_path) {
            return *p;
        }
        let exists = match lexical_path_within_root(&self.project_root, file_path) {
            Some(full) => std::path::Path::new(&full).exists(),
            None => false,
        };
        let mut memo = self.file_exists_memo.borrow_mut();
        if memo.len() >= 200_000 {
            memo.clear();
        }
        memo.insert(file_path.to_string(), exists);
        exists
    }

    /// Read file content through the content cache (`None` = read failed, also cached).
    pub fn read_file(&self, file_path: &str) -> Option<Rc<str>> {
        if let Some(c) = self.file_cache.borrow().get(&file_path.to_string()) {
            return c;
        }
        let full = nodepath::join(&[&self.project_root, file_path]);
        let content = (|| {
            // Reject anything extraction would not accept before decoding can multiply a
            // large binary blob into gigabytes of heap.
            let meta = std::fs::metadata(&full).ok()?;
            if !meta.is_file() || meta.len() > max_source_file_bytes() {
                return None;
            }
            let bytes = std::fs::read(&full).ok()?;
            Some(Rc::<str>::from(String::from_utf8_lossy(&bytes).as_ref()))
        })();
        self.file_cache.borrow_mut().set(file_path.to_string(), content.clone());
        content
    }

    /// `read_file(f)?.includes(needle)` for an ASCII needle, searched in the raw bytes.
    pub fn file_contains(&self, file_path: &str, needle: &str) -> bool {
        if let Some(c) = self.file_cache.borrow().get(&file_path.to_string()) {
            return c.is_some_and(|s| s.contains(needle));
        }
        let full = nodepath::join(&[&self.project_root, file_path]);
        let Ok(meta) = std::fs::metadata(&full) else { return false };
        if !meta.is_file() || meta.len() > max_source_file_bytes() {
            return false;
        }
        match std::fs::read(&full) {
            Ok(bytes) => {
                let n = needle.as_bytes();
                n.is_empty() || bytes.windows(n.len()).any(|w| w == n)
            }
            Err(_) => false,
        }
    }

    /// `read_file(f)` split into lines on `/\r?\n/`, cached per file.
    pub fn get_file_lines(&self, file_path: &str) -> Option<Rc<Vec<String>>> {
        if self.file_lines_cache.borrow().contains(&file_path.to_string()) {
            return self.file_lines_cache.borrow().get(&file_path.to_string()).flatten();
        }
        let lines = self.read_file(file_path).map(|s| Rc::new(split_lines(&s)));
        self.file_lines_cache.borrow_mut().set(file_path.to_string(), lines.clone());
        lines
    }

    /// Immediate subdirectories of `relative_path` (relative to the project root).
    pub fn list_directories(&self, relative_path: &str) -> Vec<String> {
        let target = if relative_path == "." || relative_path.is_empty() {
            self.project_root.clone()
        } else {
            nodepath::join(&[&self.project_root, relative_path])
        };
        match std::fs::read_dir(&target) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir() || (t.is_symlink() && false)).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(e) => {
                log_debug("Failed to list directory for resolution", &[("relativePath", relative_path.to_string()), ("error", e.to_string())]);
                Vec::new()
            }
        }
    }

    // -- Supertypes ----------------------------------------------------------

    /// Direct supertypes of the type named `type_name` (same language), by simple name: the
    /// union of the `implements`/`extends` targets of every same-named type node.
    pub fn get_supertypes(&self, type_name: &str, language: Language) -> Rc<Vec<String>> {
        let memo_key = format!("{} {}", language.as_str(), type_name);
        if let Some((gen, supers)) = self.supertype_memo.borrow().get(&memo_key) {
            if *gen == self.supertype_gen.get() {
                return Rc::clone(supers);
            }
        }
        let type_nodes: Vec<NodeRef> = self
            .get_nodes_by_name(type_name)
            .iter()
            .filter(|n| {
                n.language == language
                    && (is_supertype_bearing_kind(n.kind) || (n.language == Language::Scala && n.kind == NodeKind::Module))
            })
            .cloned()
            .collect();
        let mut supers: Vec<String> = Vec::new();
        if !type_nodes.is_empty() {
            let mut seen: HashSet<String> = HashSet::new();
            let mut add = |name: &str, supers: &mut Vec<String>| {
                if seen.insert(name.to_string()) {
                    supers.push(name.to_string());
                }
            };
            for tn in &type_nodes {
                for edge in q("getOutgoingEdges", self.store.get_outgoing_edges(&tn.id, &RESOLUTION_READ_EDGE_KINDS, None)) {
                    if let Some(target) = self.node_by_id(&edge.target) {
                        if !target.name.is_empty() && target.name != type_name {
                            add(&target.name, &mut supers);
                        }
                    }
                }
                // A Swift conformance to a type the project only extends (SwiftUI's `View`)
                // resolves to nothing, yet the members its extensions add are still the conformer's.
                if language == Language::Swift {
                    for name in swift_extended_conformances(tn, self) {
                        if name != type_name {
                            add(&name, &mut supers);
                        }
                    }
                }
            }
        }
        let supers = Rc::new(supers);
        self.supertype_memo.borrow_mut().insert(memo_key, (self.supertype_gen.get(), Rc::clone(&supers)));
        supers
    }

    /// The nodes one type declaration extends or implements, through its own resolved edges.
    pub fn get_supertype_nodes(&self, id: &str) -> NodeList {
        if let Some((gen, nodes)) = self.supertype_node_memo.borrow().get(id) {
            if *gen == self.supertype_gen.get() {
                return Rc::clone(nodes);
            }
        }
        let mut nodes: Vec<NodeRef> = Vec::new();
        for edge in q("getOutgoingEdges", self.store.get_outgoing_edges(id, &RESOLUTION_READ_EDGE_KINDS, None)) {
            if let Some(target) = self.node_by_id(&edge.target) {
                nodes.push(target);
            }
        }
        let nodes = Rc::new(nodes);
        self.supertype_node_memo.borrow_mut().insert(id.to_string(), (self.supertype_gen.get(), Rc::clone(&nodes)));
        nodes
    }

    /// The classes one class declaration extends, through its own resolved `extends` edges
    /// alone: the parent a `super.m()` call reaches, never an interface (#2497).
    pub fn get_superclass_nodes(&self, id: &str) -> NodeList {
        if let Some((gen, nodes)) = self.superclass_node_memo.borrow().get(id) {
            if *gen == self.supertype_gen.get() {
                return Rc::clone(nodes);
            }
        }
        let mut nodes: Vec<NodeRef> = Vec::new();
        for edge in q("getOutgoingEdges", self.store.get_outgoing_edges(id, &[EdgeKind::Extends], None)) {
            if let Some(target) = self.node_by_id(&edge.target) {
                nodes.push(target);
            }
        }
        let nodes = Rc::new(nodes);
        self.superclass_node_memo.borrow_mut().insert(id.to_string(), (self.supertype_gen.get(), Rc::clone(&nodes)));
        nodes
    }

    // -- Imports -------------------------------------------------------------

    /// Get cached import mappings for a file.
    pub fn get_import_mappings(&self, file_path: &str, language: Language) -> Rc<Vec<ImportMapping>> {
        if let Some(c) = self.import_mapping_cache.borrow().get(&file_path.to_string()) {
            return c;
        }
        let mappings = match self.read_file(file_path) {
            Some(content) if !content.is_empty() => Rc::new(extract_import_mappings(file_path, &content, language)),
            _ => Rc::new(Vec::new()),
        };
        self.import_mapping_cache.borrow_mut().set(file_path.to_string(), Rc::clone(&mappings));
        mappings
    }

    /// Re-exports declared by a file (`export { x } from './other'`, `export * from './other'`).
    pub fn get_re_exports(&self, file_path: &str, language: Language) -> Rc<Vec<ReExport>> {
        if let Some(c) = self.re_export_cache.borrow().get(&file_path.to_string()) {
            return c;
        }
        let re_exports = match self.read_file(file_path) {
            Some(content) if !content.is_empty() => {
                // What matters is the BARREL file's own language — not the consuming reference's
                // (#629). Python re-exports through its imports (a package `__init__.py`).
                let js_family = re!(r"\.(?:d\.ts|[cm]?tsx?|[cm]?jsx?|ets)$", "i").is_match(file_path);
                let barrel = if js_family {
                    Language::TypeScript
                } else if re!(r"\.py$", "i").is_match(file_path) {
                    Language::Python
                } else {
                    language
                };
                Rc::new(extract_re_exports(&content, barrel))
            }
            _ => Rc::new(Vec::new()),
        };
        self.re_export_cache.borrow_mut().set(file_path.to_string(), Rc::clone(&re_exports));
        re_exports
    }

    /// Whether an import specifier, written in `from_file`, names a module outside the
    /// repository: a package that is not a relative path, a tsconfig alias, a workspace member
    /// or the repository's own package name, and that resolves to no project file.
    pub fn is_out_of_repo_import(&self, source: &str, from_file: &str, language: Language) -> bool {
        if !is_external_import(source, language, self, ExternalOpts { alias_prefixes: false, from_file: Some(from_file) }) {
            return false;
        }
        let resolved = resolve_import_path(source, from_file, language, self);
        if let Some(r) = &resolved {
            if self.is_indexed_file(r) {
                return false;
            }
        }
        self.is_declared_outside_package(source, from_file)
    }

    /// The project file a module specifier, written in `from_file`, names.
    pub fn resolve_module_file(&self, source: &str, from_file: &str, language: Language) -> Option<String> {
        resolve_import_path(source, from_file, language, self)
    }

    // -- Project configuration -----------------------------------------------

    /// Project import-path aliases (tsconfig/jsconfig `paths`, and `baseUrl`).
    pub fn get_project_aliases(&self) -> Option<Rc<AliasMap>> {
        if let Some(v) = self.project_aliases.borrow().as_ref() {
            return v.clone();
        }
        let loaded = load_project_aliases(&self.project_root).map(Rc::new);
        *self.project_aliases.borrow_mut() = Some(loaded.clone());
        loaded
    }

    /// The aliases of the tsconfig / jsconfig nearest `from_file` below the project root that
    /// declares `paths` or `baseUrl`.
    pub fn get_nearest_aliases(&self, from_file: &str) -> Option<Rc<AliasMap>> {
        let mut dir = nodepath::posix::dirname(&from_file.replace('\\', "/"));
        let mut walked: Vec<String> = Vec::new();
        let mut found: Option<Rc<AliasMap>> = None;
        while !dir.is_empty() && dir != "." && dir != "/" {
            if let Some(hit) = self.dir_aliases.borrow().get(&dir) {
                found = hit.clone();
                break;
            }
            walked.push(dir.clone());
            let abs = nodepath::join(&[&self.project_root, &dir]);
            if ["tsconfig.json", "jsconfig.json"].iter().any(|name| std::path::Path::new(&nodepath::join(&[&abs, name])).exists()) {
                if let Some(aliases) = load_project_aliases(&abs) {
                    found = Some(Rc::new(aliases));
                    break;
                }
            }
            dir = nodepath::posix::dirname(&dir);
        }
        let mut cache = self.dir_aliases.borrow_mut();
        for d in walked {
            cache.insert(d, found.clone());
        }
        found
    }

    /// Monorepo workspace member packages, keyed by declared package name.
    pub fn get_workspace_packages(&self) -> Option<Rc<WorkspacePackages>> {
        if let Some(v) = self.workspace_packages.borrow().as_ref() {
            return v.clone();
        }
        let loaded = load_workspace_packages(&self.project_root).map(Rc::new);
        *self.workspace_packages.borrow_mut() = Some(loaded.clone());
        loaded
    }

    /// The workspace map, when one was read: `Some(None)` for a project with none.
    pub fn workspace_packages_if_loaded(&self) -> Option<Option<Rc<WorkspacePackages>>> {
        self.workspace_packages.borrow().clone()
    }

    /// Replace the workspace map this context resolves by: `None` reads the manifests again on
    /// the next lookup, `Some(map)` installs a map read elsewhere (a sync's refresh).
    pub fn set_workspace_packages(&self, packages: Option<Option<Rc<WorkspacePackages>>>) {
        *self.workspace_packages.borrow_mut() = packages;
    }

    /// C/C++ include search directories (relative to the project root).
    pub fn get_cpp_include_dirs(&self) -> Vec<String> {
        load_cpp_include_dirs(&self.project_root)
    }

    /// The import node of every C / C++ `#include`.
    pub fn get_cpp_include_nodes(&self) -> Vec<CppIncludeNode> {
        q("getCppIncludeNodes", self.store.get_cpp_include_nodes())
    }

    // -- Go modules ----------------------------------------------------------

    /// The module of the nearest `go.mod` at or above project-relative `dir` ('' = the root),
    /// never above the project root (#2322). Each directory is read at most once.
    fn nearest_go_module(&self, dir: &str) -> Option<Rc<GoModule>> {
        let mut dir = dir.to_string();
        let mut walked: Vec<String> = Vec::new();
        let found: Option<Rc<GoModule>>;
        loop {
            if let Some(hit) = self.go_module_by_dir.borrow().get(&dir) {
                found = hit.clone();
                break;
            }
            walked.push(dir.clone());
            let loaded = load_go_module(&nodepath::join(&[&self.project_root, &dir]));
            if loaded.is_some() || dir.is_empty() {
                found = loaded;
                break;
            }
            dir = go_dir_of(&dir);
        }
        let mut cache = self.go_module_by_dir.borrow_mut();
        for d in walked {
            cache.insert(d, found.clone());
        }
        found
    }

    /// The project's Go modules: the root `go.mod`'s, and the nearest `go.mod` of every
    /// directory holding an indexed `.go` file (#2322).
    fn get_go_modules(&self) -> Rc<Vec<Rc<GoModule>>> {
        if let Some(m) = self.go_modules.borrow().as_ref() {
            return Rc::clone(m);
        }
        let mut dirs: Vec<String> = vec![String::new()];
        let mut seen: HashSet<String> = HashSet::from([String::new()]);
        let files: Vec<String> = match self.known_files() {
            Some(k) => {
                let mut v: Vec<String> = k.iter().cloned().collect();
                v.sort();
                v
            }
            None => self.get_all_files(),
        };
        for file in &files {
            if file.ends_with(".go") {
                let d = go_dir_of(file);
                if seen.insert(d.clone()) {
                    dirs.push(d);
                }
            }
        }
        let mut modules: Vec<Rc<GoModule>> = Vec::new();
        for dir in &dirs {
            let Some(m) = self.nearest_go_module(dir) else { continue };
            let rel = nodepath::relative(&self.project_root, &m.root_dir);
            let mod_dir = rel.split(nodepath::SEP).collect::<Vec<_>>().join("/");
            if !re!(r"(?:^|/)(?:testdata|[_.][^/]*)(?:/|$)").is_match(&mod_dir) && !modules.iter().any(|x| Rc::ptr_eq(x, &m)) {
                modules.push(m);
            }
        }
        let modules = Rc::new(modules);
        *self.go_modules.borrow_mut() = Some(Rc::clone(&modules));
        modules
    }

    /// The project-relative directory (`/`-separated, `.` for the root) of the Go package an
    /// import path names, when it is a package of one of the project's own modules.
    pub fn get_go_package_dir(&self, import_path: &str, from_file: Option<&str>) -> Option<String> {
        let own = from_file.and_then(|f| self.nearest_go_module(&go_dir_of(f)));
        let key = format!("{}\0{}", import_path, own.as_ref().map(|o| o.root_dir.as_str()).unwrap_or(""));
        if let Some(hit) = self.go_package_dirs.borrow().get(&key) {
            return hit.clone();
        }
        let modules = self.get_go_modules();
        let m = find_go_module_for_import(import_path, &modules, own.as_ref());
        let dir = m.map(|m| go_module_package_dir(import_path, &m, &self.project_root));
        self.go_package_dirs.borrow_mut().insert(key, dir.clone());
        dir
    }

    // -- Package manifests ---------------------------------------------------

    /// Is `source` a package from outside the repository? Only when the importing file's
    /// package.json (or an enclosing one) declares it, or it is a Node built-in or a runtime's
    /// virtual module.
    pub fn is_declared_outside_package(&self, source: &str, from_file: &str) -> bool {
        // Deno's standard library is `@std/…` from JSR.
        if re!(r"^(?:node|bun|jsr|npm|https?):").is_match(source) || source.starts_with("@std/") || NODE_BUILTINS.contains(&source) {
            return true;
        }
        let name: String = if source.starts_with('@') {
            source.split('/').take(2).collect::<Vec<_>>().join("/")
        } else {
            source.split('/').next().unwrap_or("").to_string()
        };
        let normalized = from_file.replace('\\', "/");
        let dir = match normalized.rfind('/') {
            Some(i) => &normalized[..i],
            None => "",
        };
        let scope = self.manifest_scope(dir);
        if scope.own.contains(&name) {
            return false;
        }
        if scope.deps.contains(&name) {
            return true;
        }
        // SvelteKit's `$app/…` and Astro's `astro:…` belong to the framework — unless this
        // repository is that framework.
        let provider = if source.starts_with("astro:") {
            Some("astro")
        } else if re!(r"^\$(?:app|env|service-worker)(?:/|$)").is_match(source) {
            Some("@sveltejs/kit")
        } else {
            None
        };
        match provider {
            Some(p) => !scope.own.contains(p) && !self.get_workspace_packages().is_some_and(|w| w.by_name.contains_key(p)),
            None => false,
        }
    }

    fn manifest_scope(&self, dir: &str) -> Rc<ManifestScope> {
        if let Some(m) = self.manifest_scopes.borrow().get(dir) {
            return Rc::clone(m);
        }
        let parent: Option<Rc<ManifestScope>> = if dir.is_empty() {
            None
        } else {
            Some(self.manifest_scope(match dir.rfind('/') {
                Some(i) => &dir[..i],
                None => "",
            }))
        };
        let mut scope: ManifestScope = parent.as_ref().map(|p| (**p).clone()).unwrap_or_default();
        let pkg_path = if dir.is_empty() { "package.json".to_string() } else { format!("{dir}/package.json") };
        if let Some(raw) = self.read_file(&pkg_path) {
            if let Ok(serde_json::Value::Object(json)) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(serde_json::Value::String(n)) = json.get("name") {
                    if !n.is_empty() {
                        scope.own.insert(n.clone());
                    }
                }
                for field in ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"] {
                    let Some(serde_json::Value::Object(deps)) = json.get(field) else { continue };
                    for (dep, version) in deps {
                        // `workspace:*`, `file:../shared`, `link:…`: a package in this repository.
                        match version {
                            serde_json::Value::String(v) if re!(r"^(?:workspace|file|link|portal):").is_match(v) => {
                                scope.own.insert(dep.clone());
                            }
                            _ => {
                                scope.deps.insert(dep.clone());
                            }
                        }
                    }
                }
            }
        }
        // A Deno import map names packages the same way — an entry that maps to a registry or
        // a URL, not one that maps to a path in the repository.
        for file in ["deno.json", "deno.jsonc"] {
            let p = if dir.is_empty() { file.to_string() } else { format!("{dir}/{file}") };
            let Some(raw) = self.read_file(&p) else { continue };
            if raw.is_empty() {
                continue;
            }
            let parsed = jsonc_parser::parse_to_serde_value(&raw, &jsonc_parser::ParseOptions::default()).ok().flatten();
            let Some(serde_json::Value::Object(json)) = parsed else { continue };
            let Some(serde_json::Value::Object(imports)) = json.get("imports") else { continue };
            for (key, target) in imports {
                let serde_json::Value::String(t) = target else { continue };
                if !re!(r"^(?:jsr|npm|https?):").is_match(t) {
                    continue;
                }
                scope.deps.insert(key.strip_suffix('/').unwrap_or(key).to_string());
            }
        }
        let scope = Rc::new(scope);
        self.manifest_scopes.borrow_mut().insert(dir.to_string(), Rc::clone(&scope));
        scope
    }

    /// The file path of the node `node_id`, or '' when it has no node.
    pub fn file_path_from_node_id(&self, node_id: &str) -> String {
        self.node_by_id(node_id).map(|n| n.file_path.clone()).unwrap_or_default()
    }

    /// The language of the node `node_id`, or `unknown`.
    pub fn language_from_node_id(&self, node_id: &str) -> Language {
        self.node_by_id(node_id).map(|n| n.language).unwrap_or(Language::Unknown)
    }
}

/// `s.split(/\r?\n/)`.
pub fn split_lines(s: &str) -> Vec<String> {
    s.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect()
}

/// `String.prototype.toLowerCase` for the strings the resolver lowers.
pub fn js_lower(s: &str) -> String {
    s.to_lowercase()
}
