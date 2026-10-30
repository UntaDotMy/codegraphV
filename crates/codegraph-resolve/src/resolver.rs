//! The reference resolver — port of `src/resolution/index.ts`.
//!
//! Drives one reference at a time through the strategy chain (`resolve_one`), turns the hits
//! into edges, and persists a whole index run in keyset batches. The TypeScript engine's
//! worker pool and profiling hooks have no counterpart: resolution here is sequential, which
//! is also the path the pool must agree with byte for byte.

use crate::import_resolver::is_esm_import_language;
use codegraph_search::is_test_path;
use crate::alias_binding::resolve_alias_binding;
use crate::ambient_modules::ambient_module_declarations;
use crate::callback_synthesizer::synthesize_callback_edges;
use crate::cpp_constructor::{is_cpp_constructor_ref, match_cpp_constructor};
use crate::cpp_macro_visibility::is_visible_cpp_macro;
use crate::cpp_supertypes::{is_cpp_supertype_ref, match_cpp_supertype};
use crate::pascal_scope::{
    gate_pascal_call, is_pascal_parenless_ref, is_pascal_system_call, match_pascal_nested_call, match_pascal_parenless_ref,
    match_pascal_system_call,
};
use crate::ctx::{is_supertype_bearing_kind, Ctx, RESOLUTION_READ_EDGE_KINDS};
use crate::dart_local_scope::gate_dart_local;
use crate::frameworks::{detect_frameworks, get_resolving_frameworks, FrameworkRef};
use crate::import_resolver::{
    import_binding_tail, is_bound_to_out_of_repo_import, is_cobol_copybook_ref, is_dart_import_ref, is_js_path_import_ref,
    is_lua_require_ref, is_nix_path_import_ref, is_php_include_path_ref, reaches_ambient_module, resolve_import_path,
    resolve_jvm_import, resolve_php_imported_static_call, resolve_php_qualified_class_ref, resolve_ts_qualified_type_ref,
    resolve_via_import,
};
use crate::js_builtins::JS_BUILT_INS;
use crate::name_matcher::*;
use crate::name_matcher::new_receiver::{is_new_receiver_shape, match_new_receiver_call, new_receiver_class};
use crate::name_matcher::java::is_guessed_record_accessor;
use crate::shopify_themes::match_shopify_theme_file;
use crate::swift_type_visibility::gate_swift_type_target;
use crate::type_parameters::gate_type_parameter;
use crate::types::{
    is_cpp_define_signature, is_importable_kind, is_inheritance_ref, is_supertype_target, CrossFileNodes, NodeRef,
    ResolutionResult, ResolutionStats, ResolvedBy, ResolvedRef, Tri, UnresolvedRef,
};
use crate::vbnet_receivers::{is_vb_member_read, is_vb_path_call, match_vb_member_read, match_vb_path_call};
use codegraph_db::Store;
use codegraph_jsre::{re, Re};
use codegraph_types::log::log_debug;
use codegraph_types::{Edge, EdgeKind, Language, Metadata, Node, NodeKind, ReferenceKind, UnresolvedReference};
use indexmap::IndexMap;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;

mod sync;

macro_rules! name_set {
    ($name:ident, [$($n:expr),* $(,)?]) => {
        static $name: LazyLock<HashSet<&'static str>> = LazyLock::new(|| [$($n),*].into_iter().collect());
    };
}

name_set!(REACT_HOOKS, [
    "useState", "useEffect", "useContext", "useReducer", "useCallback",
    "useMemo", "useRef", "useLayoutEffect", "useImperativeHandle", "useDebugValue",
]);

name_set!(PYTHON_BUILT_INS, [
    "print", "len", "range", "str", "int", "float", "list", "dict", "set", "tuple",
    "open", "input", "type", "isinstance", "hasattr", "getattr", "setattr",
    "super", "self", "cls", "None", "True", "False",
]);

name_set!(PYTHON_BUILT_IN_TYPES, [
    "list", "dict", "set", "tuple", "str", "int", "float", "bool",
    "bytes", "bytearray", "frozenset", "object", "super",
]);

name_set!(PYTHON_BUILT_IN_METHODS, [
    "append", "extend", "insert", "remove", "pop", "clear", "sort", "reverse", "copy",
    "update", "keys", "values", "items", "get",
    "add", "discard", "union", "intersection", "difference",
    "split", "join", "strip", "lstrip", "rstrip", "replace", "lower", "upper",
    "startswith", "endswith", "find", "index", "count", "encode", "decode",
    "format", "isdigit", "isalpha", "isalnum",
    "read", "write", "readline", "readlines", "close", "flush", "seek",
]);

name_set!(GO_BUILT_INS, [
    "make", "new", "len", "cap", "append", "copy", "delete", "close",
    "panic", "recover", "print", "println", "complex", "real", "imag",
    "error", "nil", "true", "false", "iota",
    "int", "int8", "int16", "int32", "int64",
    "uint", "uint8", "uint16", "uint32", "uint64", "uintptr",
    "float32", "float64", "complex64", "complex128",
    "string", "bool", "byte", "rune", "any",
]);

const PASCAL_UNIT_PREFIXES: [&str; 15] = [
    "System.", "Winapi.", "Vcl.", "Fmx.", "Data.", "Datasnap.",
    "Soap.", "Xml.", "Web.", "REST.", "FireDAC.", "IBX.",
    "IdHTTP", "IdTCP", "IdSSL",
];

name_set!(PASCAL_BUILT_INS, [
    "System", "SysUtils", "Classes", "Types", "Variants", "StrUtils",
    "Math", "DateUtils", "IOUtils", "Generics.Collections", "Generics.Defaults",
    "Rtti", "TypInfo", "SyncObjs", "RegularExpressions",
    "SysInit", "Windows", "Messages", "Graphics", "Controls", "Forms",
    "Dialogs", "StdCtrls", "ExtCtrls", "ComCtrls", "Menus", "ActnList",
    "WriteLn", "Write", "ReadLn", "Read", "Inc", "Dec", "Ord", "Chr",
    "Length", "SetLength", "High", "Low", "Assigned", "FreeAndNil",
    "Format", "IntToStr", "StrToInt", "FloatToStr", "StrToFloat",
    "Trim", "UpperCase", "LowerCase", "Pos", "Copy", "Delete", "Insert",
    "Now", "Date", "Time", "DateToStr", "StrToDate",
    "Raise", "Exit", "Break", "Continue", "Abort",
    "True", "False", "nil", "Self", "Result",
    "Create", "Destroy", "Free",
    "TObject", "TComponent", "TPersistent", "TInterfacedObject",
    "TList", "TStringList", "TStrings", "TStream", "TMemoryStream", "TFileStream",
    "Exception", "EAbort", "EConvertError", "EAccessViolation",
    "IInterface", "IUnknown",
]);

/// PASCAL_BUILT_INS lowercased: Pascal names ignore case.
static PASCAL_BUILT_INS_LOWER: LazyLock<HashSet<String>> = LazyLock::new(|| PASCAL_BUILT_INS.iter().map(|n| n.to_lowercase()).collect());

name_set!(C_BUILT_INS, [
    "printf", "fprintf", "sprintf", "snprintf", "scanf", "fscanf", "sscanf",
    "malloc", "calloc", "realloc", "free",
    "memcpy", "memmove", "memset", "memcmp", "memchr",
    "strlen", "strcpy", "strncpy", "strcat", "strncat", "strcmp", "strncmp",
    "strstr", "strchr", "strrchr", "strtok", "strdup",
    "fopen", "fclose", "fread", "fwrite", "fgets", "fputs", "fputc", "fgetc",
    "feof", "ferror", "fflush", "fseek", "ftell", "rewind",
    "exit", "abort", "atexit", "atoi", "atol", "atof", "strtol", "strtoul", "strtod",
    "qsort", "bsearch",
    "abs", "labs", "rand", "srand",
    "sin", "cos", "tan", "sqrt", "pow", "log", "log10", "exp", "ceil", "floor", "fabs",
    "time", "clock", "difftime", "mktime", "localtime", "gmtime", "strftime", "asctime",
    "assert", "errno",
    "perror", "remove", "rename", "tmpfile", "tmpnam",
    "getenv", "system",
    "signal", "raise",
    "setjmp", "longjmp",
    "va_start", "va_end", "va_arg", "va_copy",
    "NULL", "EOF", "BUFSIZ", "FILENAME_MAX", "RAND_MAX", "EXIT_SUCCESS", "EXIT_FAILURE",
    "size_t", "ptrdiff_t", "wchar_t", "intptr_t", "uintptr_t",
    "int8_t", "int16_t", "int32_t", "int64_t",
    "uint8_t", "uint16_t", "uint32_t", "uint64_t",
    "FILE",
    "stat", "lstat", "fstat", "open", "close", "read", "write", "pipe",
    "fork", "exec", "waitpid", "getpid", "getppid", "kill", "sleep", "usleep",
    "pthread_create", "pthread_join", "pthread_mutex_lock", "pthread_mutex_unlock",
    "dlopen", "dlsym", "dlclose",
]);

name_set!(CPP_BUILT_INS, [
    "cout", "cin", "cerr", "clog", "endl", "flush", "ws",
    "std",
    "nullptr", "true", "false", "this", "sizeof", "alignof", "typeid",
    "static_cast", "dynamic_cast", "reinterpret_cast", "const_cast",
    "make_unique", "make_shared", "make_pair",
    "move", "forward", "swap",
]);

/// Languages whose chained calls (`a().b()`) wait for conformance edges.
fn is_chain_language(language: Language) -> bool {
    matches!(
        language,
        Language::Java
            | Language::Kotlin
            | Language::CSharp
            | Language::Swift
            | Language::Rust
            | Language::Go
            | Language::Scala
            | Language::Dart
            | Language::ObjC
            | Language::Pascal
    )
}

fn is_js_family(language: Language) -> bool {
    matches!(language, Language::TypeScript | Language::JavaScript | Language::Tsx | Language::Jsx)
}

fn is_pascal_cast_target_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Class | NodeKind::Struct | NodeKind::Interface | NodeKind::Enum | NodeKind::TypeAlias)
}

/// `{ ...a, framework }` — the framework name lands after the strategy's own keys.
fn with_framework(metadata: Option<Metadata>, name: &str) -> Metadata {
    let mut m = metadata.unwrap_or_default();
    m.insert("framework".to_string(), Value::String(name.to_string()));
    m
}

/// A JSON number the way JavaScript prints it: an integral value has no fraction.
fn json_number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

/// Where a batch's results were persisted, for the loop's progress guard.
#[derive(Default)]
struct PersistOutcome {
    removed: usize,
}

/// Below this many pending references a run stays on one thread: a worker's own connection and
/// cold caches cost more than it saves.
/// A CFML call on a construction, `a.b.Batch().init` (#2652); CFML names may hold `$` (`$init`).
const CFML_CONSTRUCTED_CALL: &str = r"^([A-Za-z_$][\w$.-]*)\(\)\.([A-Za-z_$][\w$]*)$";

/// Whether a CFML `imports` name is the path of a template an include names (rather than a
/// component `import` path): no `:`, and a separator or a CFML template extension.
fn is_cfml_template_path(name: &str) -> bool {
    !name.contains(':')
        && (name.contains('/') || name.contains('\\') || re!(r"\.(?:cfml?|cfc|cfs)$", "i").is_match(codegraph_types::jscompat::js_trim(name)))
}

const PARALLEL_MIN_REFS: usize = 100_000;
/// Address space for a worker's stack, as for the parse workers: the matchers recurse.
const RESOLVE_STACK_BYTES: usize = 256 * 1024 * 1024;
/// References per work item, at least. A batch is cut into chunks that idle workers take in
/// turn, so one slow file no longer holds the batch while the other workers wait. A chunk ends
/// at a file boundary: a file's references stay on one worker, whose caches (imports, the
/// file's nodes) then fill once rather than on every worker.
const RESOLVE_CHUNK: usize = 128;
/// ...and at most this many: a file with more is split, so it does not run alone on one worker.
/// (Measured on a 1.9k-file repository: 128..256 beat smaller chunks, which spread a file over
/// every worker's caches, and larger ones, which leave one worker finishing a batch alone.)
const RESOLVE_CHUNK_MAX: usize = 256;

/// `CODEGRAPHV_RESOLVE_WORKERS` sets the count (1 = single-threaded); unset, a big run takes one
/// worker per physical core. A second hardware thread on a core adds no resolve throughput (the
/// workers contend for the same caches and memory bandwidth), only CPU time and per-worker memory:
/// on a 6-core/12-thread machine 12 workers took 25% more CPU-seconds and 40% more memory than 6
/// for the same wall time.
fn resolve_worker_count(total: usize) -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    worker_count_for(std::env::var("CODEGRAPHV_RESOLVE_WORKERS").ok().as_deref(), total, cpus.min(num_cpus::get_physical()))
}

fn worker_count_for(env: Option<&str>, total: usize, cores: usize) -> usize {
    if let Some(n) = env.and_then(|v| v.trim().parse::<usize>().ok()) {
        return n.max(1);
    }
    if total < PARALLEL_MIN_REFS {
        return 1;
    }
    cores.max(1)
}

/// One batch for the pool: workers take chunk indexes from `next` until none are left.
struct BatchJob {
    refs: Vec<UnresolvedReference>,
    /// Where each chunk starts in `refs`; the last chunk runs to the end.
    starts: Vec<usize>,
    next: std::sync::atomic::AtomicUsize,
    /// Which batch this is: a worker's first chunk of a new batch refreshes its supertype memo,
    /// since the batch before it persisted edges.
    number: usize,
}

impl BatchJob {
    fn new(refs: Vec<UnresolvedReference>, number: usize) -> BatchJob {
        let starts = chunk_starts(&refs, RESOLVE_CHUNK, RESOLVE_CHUNK_MAX);
        BatchJob { refs, starts, next: std::sync::atomic::AtomicUsize::new(0), number }
    }
    fn chunks(&self) -> usize {
        self.starts.len()
    }
    fn chunk(&self, i: usize) -> &[UnresolvedReference] {
        let end = self.starts.get(i + 1).copied().unwrap_or(self.refs.len());
        &self.refs[self.starts[i]..end]
    }
}

/// Chunk starts: a chunk takes at least `min` references, then runs on to the end of its last
/// file (references arrive grouped by file), but never past `max`.
fn chunk_starts(refs: &[UnresolvedReference], min: usize, max: usize) -> Vec<usize> {
    let (min, max) = (min.max(1), max.max(min.max(1)));
    let mut starts = Vec::new();
    let mut i = 0;
    while i < refs.len() {
        starts.push(i);
        let mut end = (i + min).min(refs.len());
        let limit = (i + max).min(refs.len());
        while end < limit && refs[end].file_path == refs[end - 1].file_path {
            end += 1;
        }
        i = end;
    }
    starts
}

/// One chunk's result, with the references its resolver parked for the deferred passes.
struct ChunkOutcome {
    result: ResolutionResult,
    chain: Vec<UnresolvedRef>,
    this_member: Vec<UnresolvedRef>,
}

impl ChunkOutcome {
    /// Resolved on the calling resolver, which parks into its own queues directly.
    fn local(resolver: &ReferenceResolver, chunk: &[UnresolvedReference]) -> ChunkOutcome {
        ChunkOutcome { result: resolver.resolve_batch(chunk), chain: Vec::new(), this_member: Vec::new() }
    }
}

/// What a worker sends back for one chunk: its index and outcome, or None when resolving it
/// panicked (the caller then resolves that chunk itself).
type ChunkReply = (usize, Option<ChunkOutcome>, std::time::Duration);

/// A resolve worker thread with a read-only connection and a resolver of its own.
struct Worker {
    jobs: std::sync::mpsc::Sender<std::sync::Arc<BatchJob>>,
}

impl Worker {
    /// Detached: once its jobs channel closes the thread frees its caches and connection on its
    /// own, while indexing goes on.
    fn spawn(
        n: usize,
        db_path: &std::path::Path,
        project_root: &str,
        replies: std::sync::mpsc::Sender<ChunkReply>,
    ) -> Option<Worker> {
        let (jobs, job_rx) = std::sync::mpsc::channel::<std::sync::Arc<BatchJob>>();
        let db_path = db_path.to_path_buf();
        let root = project_root.to_string();
        let body = move || {
            let store = match Store::open(&db_path, codegraph_db::OpenOptions { read_only: true }) {
                Ok(store) => store,
                Err(e) => {
                    log_debug("resolve worker could not open the index", &[("error", e.to_string())]);
                    return;
                }
            };
            let store = Rc::new(store);
            let resolver = ReferenceResolver::create(&root, store.clone());
            let mut last_batch = None;
            for job in job_rx {
                loop {
                    let i = job.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= job.chunks() {
                        break;
                    }
                    let fresh_batch = last_batch != Some(job.number);
                    last_batch = Some(job.number);
                    let started = std::time::Instant::now();
                    // One read transaction per chunk. Outside one, every statement takes and
                    // drops SQLite's WAL read lock, a kernel call that all workers contend on.
                    // Nothing writes while a batch resolves, so the chunk sees the same rows.
                    let _ = store.begin_batch();
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let result = resolver.resolve_chunk(job.chunk(i), fresh_batch);
                        let chain = std::mem::take(&mut *resolver.deferred_chain_refs.borrow_mut());
                        let this_member = std::mem::take(&mut *resolver.deferred_this_member_refs.borrow_mut());
                        ChunkOutcome { result, chain, this_member }
                    }))
                    .ok();
                    store.rollback_batch(); // read-only: ends the transaction, nothing to undo
                    resolver.deferred_chain_refs.borrow_mut().clear();
                    resolver.deferred_this_member_refs.borrow_mut().clear();
                    resolver.deferred_row_ids.borrow_mut().clear();
                    if replies.send((i, outcome, started.elapsed())).is_err() {
                        return;
                    }
                }
            }
        };
        std::thread::Builder::new()
            .name(format!("codegraph-resolve-{n}"))
            .stack_size(RESOLVE_STACK_BYTES)
            .spawn(body)
            .ok()
            .map(|_| Worker { jobs })
    }
}

pub struct ReferenceResolver {
    ctx: Rc<Ctx>,
    store: Rc<Store>,
    frameworks: RefCell<Vec<FrameworkRef>>,
    frameworks_by_language: RefCell<HashMap<Language, Vec<FrameworkRef>>>,
    /// Chained calls (`a().b()`) that need conformance edges to place; retried after the main pass.
    deferred_chain_refs: RefCell<Vec<UnresolvedRef>>,
    /// `this.x` function references whose method lives on a supertype.
    deferred_this_member_refs: RefCell<Vec<UnresolvedRef>>,
    /// Row ids of references parked in a deferred queue (not to be marked failed).
    deferred_row_ids: RefCell<HashSet<i64>>,
    razor_usings_cache: RefCell<HashMap<String, Rc<Vec<String>>>>,
}

impl ReferenceResolver {
    pub fn new(project_root: &str, store: Rc<Store>) -> ReferenceResolver {
        ReferenceResolver {
            ctx: Ctx::new(project_root, store.clone()),
            store,
            frameworks: RefCell::new(Vec::new()),
            frameworks_by_language: RefCell::new(HashMap::new()),
            deferred_chain_refs: RefCell::new(Vec::new()),
            deferred_this_member_refs: RefCell::new(Vec::new()),
            deferred_row_ids: RefCell::new(HashSet::new()),
            razor_usings_cache: RefCell::new(HashMap::new()),
        }
    }

    /// `createResolver`: a resolver with its frameworks detected.
    pub fn create(project_root: &str, store: Rc<Store>) -> ReferenceResolver {
        let resolver = ReferenceResolver::new(project_root, store);
        resolver.initialize();
        resolver
    }

    /// Detect the frameworks the project uses (call once after the index is populated).
    pub fn initialize(&self) {
        // A full index resolves by the package.json files on disk now, as the fingerprint it
        // stamps says, even in a process that read them earlier.
        self.ctx.set_workspace_packages(None);
        // Detection reads through the caches (file contents, existence probes, declared
        // dependencies): clear them first, or a process that read the manifests earlier detects by
        // what they said then (upstream 31ec9082, #2519).
        self.clear_caches();
        *self.frameworks.borrow_mut() = detect_frameworks(&self.ctx);
        self.frameworks_by_language.borrow_mut().clear();
    }

    pub fn context(&self) -> &Rc<Ctx> {
        &self.ctx
    }

    pub fn detected_frameworks(&self) -> Vec<String> {
        self.frameworks.borrow().iter().map(|f| f.name().to_string()).collect()
    }

    fn frameworks_for(&self, language: Language) -> Vec<FrameworkRef> {
        if let Some(f) = self.frameworks_by_language.borrow().get(&language) {
            return f.clone();
        }
        let frameworks = get_resolving_frameworks(&self.frameworks.borrow(), language);
        self.frameworks_by_language.borrow_mut().insert(language, frameworks.clone());
        frameworks
    }

    pub fn warm_caches(&self) {
        self.ctx.warm_caches();
    }

    pub fn clear_caches(&self) {
        self.ctx.clear_caches();
    }

    fn node_by_id(&self, id: &str) -> Option<NodeRef> {
        self.ctx.node_by_id(id)
    }

    // -- Post-extraction passes ------------------------------------------------

    /// Run each framework's cross-file `post_extract` pass and update the nodes it returns;
    /// then reconcile the nodes only several files together can name.
    pub fn run_post_extract(&self) -> usize {
        let mut updated = 0;
        self.clear_caches();
        let frameworks = self.frameworks.borrow().clone();
        for fw in &frameworks {
            if let Some(nodes) = fw.post_extract(&self.ctx) {
                for node in nodes {
                    if let Err(e) = self.store.update_node(&node) {
                        log_debug(&format!("Framework '{}' postExtract failed", fw.name()), &[("error", e.to_string())]);
                        continue;
                    }
                    updated += 1;
                }
            }
        }
        for fw in &frameworks {
            if let Some(result) = fw.cross_file_nodes(&self.ctx) {
                match self.reconcile_cross_file_nodes(result) {
                    Ok(n) => updated += n,
                    Err(e) => log_debug(&format!("Framework '{}' crossFileNodes failed", fw.name()), &[("error", e.to_string())]),
                }
            }
        }
        if updated > 0 {
            self.clear_caches();
        }
        updated
    }

    /// Make the stored nodes of `result.kind` that this pass owns equal the nodes it now names.
    fn reconcile_cross_file_nodes(&self, result: CrossFileNodes) -> codegraph_db::Result<usize> {
        let mut wanted: IndexMap<String, Node> = IndexMap::new();
        for node in &result.nodes {
            if (result.owns)(node) {
                wanted.insert(node.id.clone(), node.clone());
            }
        }
        let mut existing: IndexMap<String, Node> = IndexMap::new();
        for node in self.store.get_nodes_by_kind(result.kind)? {
            if (result.owns)(&node) {
                existing.insert(node.id.clone(), node);
            }
        }
        let mut changed = 0;
        for (id, old) in &existing {
            if wanted.contains_key(id) {
                continue;
            }
            let refs: Vec<UnresolvedReference> = self
                .store
                .get_cross_file_incoming_edges_with_target(&old.file_path)?
                .into_iter()
                .filter(|e| &e.edge.target == id)
                .filter_map(|e| crate::resurrect_ref::resurrect_ref_from_dropped_edge(&e.edge, &e.source_file_path, e.source_language))
                .map(|r| to_db_reference(&r))
                .collect();
            if !refs.is_empty() {
                self.store.insert_unresolved_refs_batch(&refs)?;
            }
            self.store.delete_node(id)?;
            changed += 1;
        }
        let mut added: Vec<Node> = Vec::new();
        for (id, node) in &wanted {
            match existing.get(id) {
                None => added.push(node.clone()),
                Some(old) => {
                    if old.name != node.name || old.qualified_name != node.qualified_name {
                        self.store.update_node(node)?;
                        changed += 1;
                    }
                }
            }
        }
        if !added.is_empty() {
            self.store.insert_nodes(&added)?;
            let ids: HashSet<&str> = added.iter().map(|n| n.id.as_str()).collect();
            let refs: Vec<UnresolvedReference> = result
                .references
                .iter()
                .filter(|r| ids.contains(r.from_node_id.as_str()))
                .map(to_db_reference)
                .collect();
            self.store.insert_unresolved_refs_batch(&refs)?;
            changed += added.len();
        }
        Ok(changed)
    }

    // -- One reference ---------------------------------------------------------

    /// Lift a stored reference into the resolver's working form, filling the file and language
    /// the row does not carry from the node it was written in.
    fn working_ref(&self, raw: &UnresolvedReference) -> UnresolvedRef {
        let file_path = match raw.file_path.as_deref() {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => self.ctx.file_path_from_node_id(&raw.from_node_id),
        };
        let language = match raw.language {
            Some(l) => l,
            None => self.ctx.language_from_node_id(&raw.from_node_id),
        };
        UnresolvedRef {
            from_node_id: raw.from_node_id.clone(),
            reference_name: raw.reference_name.clone(),
            reference_kind: raw.reference_kind,
            line: raw.line,
            column: raw.column,
            file_path,
            language,
            candidates: None,
            row_id: raw.row_id,
            name_tail: None,
        }
    }

    /// A reference that failed to resolve, tagged with the tail it is parked under.
    fn parkable(&self, mut r: UnresolvedRef) -> UnresolvedRef {
        if let Some(tail) = import_binding_tail(&r, &self.ctx) {
            r.name_tail = Some(tail);
        }
        r
    }

    fn defer_reference(&self, r: &UnresolvedRef, queue: &RefCell<Vec<UnresolvedRef>>) {
        queue.borrow_mut().push(r.clone());
        if let Some(id) = r.row_id {
            self.deferred_row_ids.borrow_mut().insert(id);
        }
    }

    fn non_deferred_failures(&self, unresolved: &[UnresolvedRef]) -> Vec<UnresolvedRef> {
        let deferred = self.deferred_row_ids.borrow();
        unresolved.iter().filter(|r| r.row_id.is_none_or(|id| !deferred.contains(&id))).cloned().collect()
    }

    fn has_any_possible_match_ignoring_case(&self, name: &str) -> bool {
        if self.ctx.known_names().is_none() {
            return true;
        }
        let Some(lower_names) = self.ctx.known_lower_names() else { return true };
        let lower = crate::ctx::js_lower(name);
        if lower_names.contains(&lower) {
            return true;
        }
        re!(r"::|->|\.").split(&lower).iter().any(|part| !part.is_empty() && lower_names.contains(*part))
    }

    /// Cheap pre-filter: could any indexed symbol be what this name refers to?
    fn has_any_possible_match(&self, name: &str) -> bool {
        let Some(all) = self.ctx.known_names() else { return true };
        // In the view outside the class expressions, names only their classes and members carry
        // name nothing.
        let only = self.ctx.class_free_view().then(|| self.ctx.class_expression_only_names());
        let known = Known { all: &all, only: only.as_deref() };
        if known.contains(name) {
            return true;
        }
        let capitalize = |s: &str| -> String {
            let mut chars = s.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        };
        if let Some(dot) = name.find('.').filter(|&i| i > 0) {
            let receiver = &name[..dot];
            let member = &name[dot + 1..];
            if known.contains(receiver) || known.contains(member) {
                return true;
            }
            if known.contains(capitalize(receiver).as_str()) {
                return true;
            }
            if let Some(last) = name.rfind('.').filter(|&i| i > dot) {
                let tail = &name[last + 1..];
                if !tail.is_empty() && known.contains(tail) {
                    return true;
                }
            }
        }
        if let Some(colon) = name.find("::").filter(|&i| i > 0) {
            let receiver = &name[..colon];
            let member = &name[colon + 2..];
            if known.contains(receiver) || known.contains(member) {
                return true;
            }
            if let Some(last) = name.rfind("::").filter(|&i| i > colon) {
                let tail = &name[last + 2..];
                if !tail.is_empty() && known.contains(tail) {
                    return true;
                }
            }
        }
        for sep in [':', '$'] {
            if sep == ':' && name.contains("::") {
                continue;
            }
            if let Some(i) = name.find(sep).filter(|&i| i > 0) {
                let receiver = &name[..i];
                let member = &name[i + 1..];
                if known.contains(member) || known.contains(receiver) {
                    return true;
                }
                if known.contains(capitalize(receiver).as_str()) {
                    return true;
                }
            }
        }
        if let Some(slash) = name.rfind('/').filter(|&i| i > 0) {
            if known.contains(&name[slash + 1..]) {
                return true;
            }
        }
        false
    }

    /// Does the reference's name begin at one of its file's imports?
    fn matches_any_import(&self, r: &UnresolvedRef) -> bool {
        let imports = self.ctx.get_import_mappings(&r.file_path, r.language);
        imports
            .iter()
            .any(|imp| imp.local_name == r.reference_name || r.reference_name.starts_with(&format!("{}.", imp.local_name)))
    }

    /// Resolve one reference through the strategy chain and the gates. Production code is
    /// resolved as if the project's Jest/Vitest manual mocks were not indexed: a mock
    /// re-declares its module's names by design and stands in for it only while a test runs
    /// (#2455). Test code, a mock included, keeps the whole graph.
    pub fn resolve_one(&self, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let mockless = !is_test_path(&r.file_path) && self.ctx.manual_mock_names().is_some();
        // Code outside the named class expressions resolves as if their classes were not
        // indexed: a class expression's name is bound inside it alone (#2499).
        let class_free = crate::js_class_expressions::is_class_expression_ref_language(r.language)
            && self.ctx.class_expressions().is_some_and(|index| crate::js_class_expressions::wants_class_expression_free_view(r, &index));
        self.ctx.with_view(mockless, class_free, || self.resolve_one_in(r))
    }

    fn resolve_one_in(&self, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let ctx = &*self.ctx;
        // A Rust call kept with its receiver chain (`self.services.store.put`, `self.store().put`,
        // #2836) resolves as the call by its method's name did before the extractor kept the
        // chain; the receiver's type is read through the whole chain where the call is written
        // (match_rust_written_receiver_call). The edge keeps the call as written.
        if let Some((by_name, chain)) = rust_chained_call_by_name(r) {
            let resolved = ctx.with_rust_chain(&by_name, chain, || self.resolve_one_in(&by_name));
            return resolved.map(|mut res| {
                res.original = r.clone();
                res
            });
        }
        if is_visible_cpp_macro(r, ctx) {
            return None;
        }
        let candidate = gate_dart_local(
            gate_type_parameter(
                gate_swift_type_target(self.gate_target_kind(self.resolve_one_inner(r), r), r, ctx),
                r,
                ctx,
            ),
            r,
            ctx,
        );
        let scoped = self.gate_rust_scope(candidate, r);
        let language_gated = if scoped.as_ref().is_some_and(|s| s.resolved_by == ResolvedBy::Framework) {
            self.gate_framework_language(scoped, r)
        } else {
            self.gate_language(scoped, r)
        };
        let resolved = prefer_go_build_twin(self.gate_super_self_call(language_gated, r), r, ctx);

        let resolved = resolved?;
        // A Dart declaration naming itself is the declaration, not a reference to it.
        if r.language == Language::Dart && r.is_kind(EdgeKind::References) && resolved.target_node_id == r.from_node_id {
            return None;
        }
        // `from pkg import name` binds a module only; a class or function of that name is not it.
        if is_python_from_import_module(r, ctx) {
            if let Some(module) = self.node_by_id(&resolved.target_node_id) {
                if module.kind != NodeKind::File && module.kind != NodeKind::Module {
                    return None;
                }
            }
        }
        if (resolved.by_name || matches!(resolved.resolved_by, ResolvedBy::ExactMatch | ResolvedBy::Fuzzy))
            && is_guessed_record_accessor(&resolved, r, ctx)
        {
            return None;
        }
        if resolved.resolved_by == ResolvedBy::Import && is_import_shadowed_by_js_binding(&resolved, r, ctx) {
            return None;
        }
        // A symbol an ambient module declares is reachable only from files that reach the module.
        if resolved.resolved_by != ResolvedBy::Import {
            let declarations = ambient_module_declarations(ctx);
            if let Some(specifier) = declarations.get(&resolved.target_node_id) {
                let declaring_file = self.node_by_id(&resolved.target_node_id).map(|n| n.file_path.clone());
                if let Some(declaring_file) = declaring_file {
                    if declaring_file != r.file_path && !reaches_ambient_module(r, specifier, &declaring_file, ctx) {
                        return None;
                    }
                }
            }
        }
        if !r.is_kind(EdgeKind::Calls) {
            return Some(resolved);
        }
        // A Pascal call to data runs only a routine the data holds, in scope, and one to a
        // routine nested in another comes only from inside that one (gate_pascal_call).
        let resolved = if r.language == Language::Pascal { gate_pascal_call(resolved, r, ctx)? } else { resolved };
        let Some(target) = self.node_by_id(&resolved.target_node_id) else { return Some(resolved) };
        let member_name = r.reference_name.rfind('.').map(|dot| &r.reference_name[dot + 1..]);
        let Some(forwarded) = resolve_alias_binding(&target, member_name, ctx) else { return Some(resolved) };
        if forwarded.id == resolved.target_node_id {
            return Some(resolved);
        }
        let confidence = resolved.confidence.min(0.85);
        let mut forwarded_ref = resolved;
        forwarded_ref.target_node_id = forwarded.id.clone();
        forwarded_ref.confidence = confidence;
        self.gate_language(Some(forwarded_ref), r)
    }

    fn resolve_one_inner(&self, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let ctx = &*self.ctx;
        if is_cpp_constructor_ref(r) {
            return match_cpp_constructor(r, ctx);
        }
        if is_cpp_supertype_ref(r) {
            return match_cpp_supertype(r, ctx);
        }
        // A Pascal name written without parentheses (#2514) links only to a routine its scope
        // reaches, or through a receiver to a method of the receiver's declared type.
        if is_pascal_parenless_ref(r) {
            return match_pascal_parenless_ref(r, ctx);
        }
        // So does a Pascal call to a name the System unit gives meaning to: only a declaration in
        // the call's scope shadows it.
        if is_pascal_system_call(r, ctx) {
            return match_pascal_system_call(r, ctx);
        }
        // A bare Pascal call to a routine nested where it is made is that one.
        if let Some(nested) = match_pascal_nested_call(r, ctx) {
            return Some(nested);
        }
        if is_dart_member_read(r) {
            return match_dart_member_read(r, ctx);
        }
        if is_dart_chain_link(r, ctx) {
            return match_dart_chain_link(r, ctx);
        }
        if is_dart_annotation(r) {
            return match_dart_annotation(r, ctx);
        }
        // Each is tried in turn, only if the one before had no opinion.
        let go_matchers: [fn(&UnresolvedRef, &Ctx) -> Tri<ResolvedRef>; 3] = [match_go_asserted_call, match_go_indexed_call, match_go_chained_call];
        for matcher in go_matchers {
            let decided = matcher(r, ctx);
            if !decided.is_undefined() {
                return decided.value();
            }
        }
        let theme_file = match_shopify_theme_file(r, ctx);
        if !theme_file.is_undefined() {
            return theme_file.value();
        }

        // A TS/JS `super.run(…)` runs the method of the class's parent or a nearer ancestor, or
        // one outside the project: never a method found by its name alone.
        let super_call = crate::js_super_calls::match_js_super_call(r, ctx);
        if !super_call.is_undefined() {
            return super_call.value();
        }

        // A Kotlin call through a receiver chain (`engine.pump.drain()`) resolves on the chain's
        // declared type, or gets no edge when that type is a library one. An untyped chain
        // resolves as the bare method name, the ref the extractor emitted before it kept the
        // chain.
        if r.language == Language::Kotlin && r.is_kind(EdgeKind::Calls) {
            use crate::name_matcher::kotlin_receiver::{match_kotlin_receiver_chain, KotlinChain};
            match match_kotlin_receiver_chain(r, ctx) {
                KotlinChain::NotAChain => {}
                KotlinChain::NoEdge => return None,
                KotlinChain::Method(method) => {
                    let mut bare = r.clone();
                    bare.reference_name = method;
                    return self.resolve_one_inner(&bare).map(|mut resolved| {
                        resolved.original = r.clone();
                        resolved
                    });
                }
                KotlinChain::Resolved(resolved) => return self.gate_language(Some(resolved), r),
            }
        }

        // Built-ins and external modules never resolve to project symbols — except a python call
        // of a builtin-named method through a local or a parameter its def types as a project
        // class (`reg = Registry(); reg.get(k)`, #2583): that reading, by evidence only, is the one
        // way back.
        if self.is_built_in_or_external(r) {
            return if is_python_builtin_named_method_call(r) {
                self.gate_language(crate::name_matcher::python_attr::match_python_typed_local_call(r, ctx), r)
            } else {
                None
            };
        }

        if matches!(r.language, Language::Cfml | Language::CfScript)
            && (r.is_kind(EdgeKind::Extends) || r.is_kind(EdgeKind::Implements))
            && (r.reference_name.contains('.') || r.reference_name.contains('/'))
        {
            return self.resolve_cfml_component_path(r);
        }
        // The component a CFML `createObject("component", "a.b.Batch")` makes, named by its
        // component path, and a call chained onto a construction (`a.b.Batch().init`): a method
        // of that component or of nothing — no name match may guess past it (#2652).
        if matches!(r.language, Language::Cfml | Language::CfScript) {
            if r.is_kind(EdgeKind::Instantiates) && r.reference_name.contains('.') {
                return self.resolve_cfml_component_path(r);
            }
            if r.is_kind(EdgeKind::Calls) && re!(CFML_CONSTRUCTED_CALL).is_match(&r.reference_name) {
                return self.resolve_cfml_constructed_call(r);
            }
        }
        // A CFML include names a template by its path from the including file's directory, or
        // from the web root (#2651); a path no file here matches goes by file name, to a file
        // only, and never to the including file itself.
        if matches!(r.language, Language::Cfml | Language::CfScript) && r.is_kind(EdgeKind::Imports) && is_cfml_template_path(&r.reference_name) {
            let included = self.resolve_cfml_include_path(r).or_else(|| match_by_file_path(r, &self.ctx));
            if let Some(inc) = &included {
                if self.node_by_id(&inc.target_node_id).is_some_and(|n| n.file_path == r.file_path) {
                    return None;
                }
            }
            return self.gate_language(included, r);
        }

        let php_qualified = resolve_php_qualified_class_ref(r, ctx);
        if !php_qualified.is_undefined() {
            return self.gate_language(php_qualified.value(), r);
        }

        // A Ruby constant written with its namespace (`Billing::ChargeCard`, `::Foo`) and a call
        // on one are what Ruby's constant lookup finds from where they are written, or nothing
        // the project declares (ruby_constant_paths). The pre-filter below drops a `::`-rooted
        // name, and the strategies past it would take a namesake by its last name.
        let ruby_path = crate::ruby_constant_paths::match_ruby_constant_path_ref(r, ctx);
        if !ruby_path.is_undefined() {
            return self.gate_language(ruby_path.value(), r);
        }

        // A bare Rust name a `use` renames (`use crate::util::take as consume; consume()`) is the
        // item its path names, which no declaration carries by that name — the pre-filter below
        // would drop it — and a call to a name a local binds where it is written (a parameter, a
        // `let`, a closure's) is to that local, whatever the file imports (see
        // `match_rust_use_bound_name`).
        let rust_bound = match_rust_use_bound_name(r, ctx);
        if !rust_bound.is_undefined() {
            return self.gate_language(rust_bound.value(), r);
        }

        let mut existence_name: String = if r.language == Language::ArkTs && r.reference_name.starts_with('.') {
            r.reference_name[1..].to_string()
        } else {
            r.reference_name.clone()
        };
        if r.language == Language::Erlang {
            existence_name = re!(r"/\d{1,3}$").replace_first(&existence_name, |_| String::new());
        }
        if matches!(r.language, Language::Cpp | Language::C) && existence_name.starts_with("::") {
            existence_name = existence_name[2..].to_string();
        }
        let pre_filter_pass = is_nix_path_import_ref(r)
            || is_js_path_import_ref(r)
            || self.has_any_possible_match(&existence_name)
            || (CASE_INSENSITIVE_LANGUAGES.contains(r.language.as_str()) && self.has_any_possible_match_ignoring_case(&existence_name))
            || self.matches_any_import(r)
            || self.frameworks_for(r.language).iter().any(|f| f.claims_reference(&r.reference_name));
        if !pre_filter_pass {
            return self.gate_language(match_js_store_binding_call(r, ctx), r);
        }

        if is_vb_member_read(r) {
            return self.gate_language(match_vb_member_read(r, ctx), r);
        }
        if is_vb_path_call(r) {
            return self.gate_language(match_vb_path_call(r, ctx, &|name| is_std_method_name("vbnet", name)), r);
        }

        let ts_qualified = resolve_ts_qualified_type_ref(r, ctx);
        if !ts_qualified.is_undefined() {
            return self.gate_language(ts_qualified.value(), r);
        }

        // Function-as-value references (#756).
        if r.reference_kind == ReferenceKind::FunctionRef {
            if r.reference_name.starts_with("this.") {
                let hit = self.resolve_this_member_fn_ref(r);
                return self.gate_language(hit, r);
            }
            if matches!(r.language, Language::Python | Language::Go) && r.reference_name.contains('.') {
                return self.gate_language(match_function_ref(r, ctx), r);
            }
            let via_import = self.gate_language(resolve_via_import(r, ctx), r);
            if let Some(via_import) = via_import {
                if let Some(target) = self.node_by_id(&via_import.target_node_id) {
                    if matches!(target.kind, NodeKind::Function | NodeKind::Method)
                        || (r.language == Language::Python && target.kind == NodeKind::Class)
                    {
                        return Some(via_import);
                    }
                }
            }
            return self.gate_language(match_function_ref(r, ctx), r);
        }

        if let Some(jvm_import) = resolve_jvm_import(r, ctx) {
            return Some(jvm_import);
        }

        if r.language == Language::Razor {
            if let Some(razor) = self.resolve_razor_using(r) {
                return Some(razor);
            }
        }

        let php_static_import = resolve_php_imported_static_call(r, ctx);
        if !php_static_import.is_undefined() {
            return self.gate_language(php_static_import.value(), r);
        }

        let mut candidates: Vec<ResolvedRef> = Vec::new();

        let mut framework_early: Option<ResolvedRef> = None;
        for framework in self.frameworks_for(r.language) {
            let resolved = self.gate_framework_language(framework.resolve(r, ctx), r);
            let result = resolved.map(|mut hit| {
                hit.metadata = Some(with_framework(hit.metadata.take(), framework.name()));
                hit
            });
            if let Some(result) = result {
                if result.confidence >= 0.9 {
                    framework_early = Some(result);
                    break;
                }
                candidates.push(result);
            }
        }
        if framework_early.is_some() {
            return framework_early;
        }
        // A reference a framework answers alone ends with the frameworks' answer
        // (FrameworkResolver::owns_reference): Nuxt's `navigateTo(x)` with a destination no route
        // serves is no namesake's call.
        if self.frameworks_for(r.language).iter().any(|f| f.owns_reference(r, ctx)) {
            let mut best: Option<ResolvedRef> = None;
            for c in candidates {
                if best.as_ref().is_none_or(|b| c.confidence > b.confidence) {
                    best = Some(c);
                }
            }
            return best;
        }

        if is_unresolved_js_member_call(r) {
            let root = &r.reference_name[..r.reference_name.find('.').unwrap_or(r.reference_name.len())];
            let namespace = ctx.get_import_mappings(&r.file_path, r.language).iter().any(|m| m.is_namespace && m.local_name == root);
            if !namespace {
                return self.gate_language(match_object_path_call(r, ctx), r);
            }
            let via_namespace = self.gate_language(resolve_via_import(r, ctx), r);
            let target = via_namespace.as_ref().and_then(|v| self.node_by_id(&v.target_node_id));
            return match target {
                Some(t)
                    if matches!(
                        t.kind,
                        NodeKind::Function | NodeKind::Method | NodeKind::Class | NodeKind::Constant | NodeKind::Variable
                    ) =>
                {
                    via_namespace
                }
                _ => None,
            };
        }

        if r.is_kind(EdgeKind::Calls)
            && re!(r"^(.+)\(\)\.(\w+)$").is_match(&r.reference_name)
            && (is_js_family(r.language) || r.language == Language::Python)
        {
            let chain_result = self.gate_language(match_reference(r, ctx), r);
            // `new Sub().inherited()`: the method may live on a supertype of a project class,
            // resolvable once extends edges exist — deferred to the conformance pass like the
            // other chained calls.
            if chain_result.is_none() && is_new_receiver_shape(r) {
                let cls = new_receiver_class(r, ctx);
                if cls.is_some_and(|c| {
                    ctx.get_nodes_by_name(&c).iter().any(|n| n.kind == NodeKind::Class && same_language_family(n.language, r.language))
                }) {
                    self.defer_reference(r, &self.deferred_chain_refs);
                }
            }
            return chain_result;
        }

        let self_call = r.language == Language::Python
            && r.is_kind(EdgeKind::Calls)
            && ctx.get_import_mappings(&r.file_path, r.language).iter().any(|m| m.local_name == r.reference_name)
            && is_python_self_call(r, ctx);
        let import_result = if self_call { None } else { self.gate_language(resolve_via_import(r, ctx), r) };
        if let Some(import_result) = import_result {
            if import_result.confidence >= 0.9 {
                return Some(import_result);
            }
            candidates.push(import_result);
        }

        // Path-shaped references name files, never symbols.
        if is_php_include_path_ref(r)
            || is_cobol_copybook_ref(r)
            || is_nix_path_import_ref(r)
            || is_dart_import_ref(r)
            || is_lua_require_ref(r)
            || r.language == Language::Terraform
        {
            return best_candidate(candidates);
        }

        let mut name_result = self.gate_language(match_reference(r, ctx), r);
        if let Some(found) = &name_result {
            let target = self.node_by_id(&found.target_node_id);
            if target.as_ref().is_some_and(|t| !is_visible_across_files(t, r, ctx)) {
                name_result = None;
            } else if r.language == Language::Nix {
                if target.as_ref().is_none_or(|t| t.file_path != r.file_path) {
                    name_result = None;
                }
            } else if target.as_ref().is_some_and(|t| t.language == Language::Nix) {
                name_result = None;
            }
        }
        if let Some(name_result) = name_result {
            candidates.push(name_result);
        }

        if candidates.is_empty() {
            // A chained call waits for the conformance edges the main pass is still writing.
            if r.is_kind(EdgeKind::Calls) && is_chain_language(r.language) && re!(r"^(.+)\(\)\.(\w+)$").is_match(&r.reference_name) {
                self.defer_reference(r, &self.deferred_chain_refs);
            } else if r.is_kind(EdgeKind::Calls) && r.language == Language::Php && re!(r"^this->\w+\.\w+$").is_match(&r.reference_name) {
                self.defer_reference(r, &self.deferred_chain_refs);
            }
            return None;
        }
        best_candidate(candidates)
    }

    /// Turn resolved references into edges.
    pub fn create_edges(&self, resolved: &[ResolvedRef]) -> Vec<Edge> {
        let mut edges = Vec::new();
        for r in resolved {
            let mut kind: EdgeKind = r.edge_kind.unwrap_or(match r.original.reference_kind {
                ReferenceKind::FunctionRef => EdgeKind::References,
                ReferenceKind::Edge(k) => k,
            });

            // A class `extends` an interface/protocol is conformance, not inheritance.
            if kind == EdgeKind::Extends {
                if let Some(target) = self.node_by_id(&r.target_node_id) {
                    if matches!(target.kind, NodeKind::Interface | NodeKind::Protocol) {
                        if let Some(source) = self.node_by_id(&r.original.from_node_id) {
                            if !matches!(source.kind, NodeKind::Interface | NodeKind::Protocol) {
                                kind = EdgeKind::Implements;
                            }
                        }
                    }
                }
            }
            // A Go conversion `T(x)` names a type, not a call.
            if kind == EdgeKind::Calls && r.original.language == Language::Go {
                if let Some(target) = self.node_by_id(&r.target_node_id) {
                    if is_go_type_kind(target.kind) {
                        kind = EdgeKind::References;
                    }
                }
            }
            // A Pascal cast `TFoo(x)` references the type.
            if kind == EdgeKind::Calls
                && r.original.language == Language::Pascal
                && !r.original.reference_name.contains('.')
                && self.node_by_id(&r.target_node_id).is_some_and(|t| is_pascal_cast_target_kind(t.kind))
            {
                kind = EdgeKind::References;
            }
            // Calling a class/struct/union constructs it.
            if kind == EdgeKind::Calls {
                if let Some(target) = self.node_by_id(&r.target_node_id) {
                    if matches!(target.kind, NodeKind::Class | NodeKind::Struct | NodeKind::Union) {
                        kind = EdgeKind::Instantiates;
                    }
                }
            }

            let mut targets: Vec<(&str, Option<&Metadata>)> = vec![(r.target_node_id.as_str(), r.metadata.as_ref())];
            if let Some(also) = &r.also_targets {
                for t in also {
                    targets.push((t.target_node_id.as_str(), t.metadata.as_ref()));
                }
            }
            for (target, metadata) in targets {
                let mut meta: Metadata = metadata.cloned().unwrap_or_default();
                meta.insert("confidence".to_string(), json_number(r.confidence));
                meta.insert("resolvedBy".to_string(), Value::String(r.resolved_by.as_str().to_string()));
                meta.insert("refName".to_string(), Value::String(r.original.reference_name.clone()));
                if r.original.reference_kind != ReferenceKind::Edge(kind) {
                    meta.insert("refKind".to_string(), Value::String(r.original.reference_kind.as_str().to_string()));
                }
                if r.original.reference_kind == ReferenceKind::FunctionRef {
                    meta.insert("fnRef".to_string(), Value::Bool(true));
                }
                let mut edge = Edge::new(r.original.from_node_id.clone(), target.to_string(), kind);
                edge.line = Some(r.original.line);
                edge.column = Some(r.original.column);
                edge.metadata = Some(meta);
                edges.push(edge);
            }
        }
        edges
    }

    // -- Result sets ---------------------------------------------------------------

    /// Resolve a list of stored references (no persistence).
    pub fn resolve_all(&self, unresolved_refs: &[UnresolvedReference]) -> ResolutionResult {
        self.warm_caches();
        self.ctx.advance_supertype_generation();
        let mut resolved: Vec<ResolvedRef> = Vec::new();
        let mut unresolved: Vec<UnresolvedRef> = Vec::new();
        let mut by_method: IndexMap<String, usize> = IndexMap::new();
        for raw in unresolved_refs {
            let r = self.working_ref(raw);
            match self.resolve_one(&r) {
                Some(hit) => {
                    *by_method.entry(hit.resolved_by.as_str().to_string()).or_insert(0) += 1;
                    resolved.push(hit);
                }
                None => unresolved.push(self.parkable(r)),
            }
        }
        ResolutionResult {
            stats: ResolutionStats {
                total: unresolved_refs.len(),
                resolved: resolved.len(),
                unresolved: unresolved.len(),
                by_method,
            },
            resolved,
            unresolved,
        }
    }

    fn is_prerequisite(r: &UnresolvedReference) -> bool {
        matches!(r.reference_kind, ReferenceKind::Edge(EdgeKind::Imports | EdgeKind::Extends | EdgeKind::Implements))
    }

    fn merge_results(first: ResolutionResult, rest: ResolutionResult) -> ResolutionResult {
        let mut by_method = first.stats.by_method.clone();
        for (method, count) in &rest.stats.by_method {
            *by_method.entry(method.clone()).or_insert(0) += count;
        }
        let stats = ResolutionStats {
            total: first.stats.total + rest.stats.total,
            resolved: first.stats.resolved + rest.stats.resolved,
            unresolved: first.stats.unresolved + rest.stats.unresolved,
            by_method,
        };
        let mut resolved = first.resolved;
        resolved.extend(rest.resolved);
        let mut unresolved = first.unresolved;
        unresolved.extend(rest.unresolved);
        ResolutionResult { resolved, unresolved, stats }
    }

    /// Resolve `refs` and persist the outcome. Wiring references (imports, inheritance) go first
    /// so the rest can read the edges they write.
    pub fn resolve_and_persist(&self, unresolved_refs: &[UnresolvedReference]) -> codegraph_db::Result<ResolutionResult> {
        let prerequisites: Vec<UnresolvedReference> = unresolved_refs.iter().filter(|r| Self::is_prerequisite(r)).cloned().collect();
        if !prerequisites.is_empty() && prerequisites.len() < unresolved_refs.len() {
            let first = self.resolve_and_persist(&prerequisites)?;
            let rest: Vec<UnresolvedReference> = unresolved_refs.iter().filter(|r| !Self::is_prerequisite(r)).cloned().collect();
            let rest = self.resolve_and_persist(&rest)?;
            return Ok(Self::merge_results(first, rest));
        }
        let result = self.resolve_all(unresolved_refs);
        self.persist_resolution_result(&result)?;
        Ok(result)
    }

    /// Persist a result: edges in chunks, resolved rows deleted, failures parked.
    /// One transaction for the whole batch: a commit per statement chunk rewrote the same pages.
    fn persist_resolution_result(&self, result: &ResolutionResult) -> codegraph_db::Result<(usize, PersistOutcome)> {
        self.store.transaction(|| self.persist_resolution_result_now(result))
    }

    fn persist_resolution_result_now(&self, result: &ResolutionResult) -> codegraph_db::Result<(usize, PersistOutcome)> {
        const PERSIST_CHUNK: usize = 1000;
        let edges = self.create_edges(&result.resolved);
        for chunk in edges.chunks(PERSIST_CHUNK) {
            self.store.insert_edges(chunk)?;
        }
        let mut outcome = PersistOutcome::default();

        let mut row_ids: Vec<i64> = Vec::new();
        let mut legacy_keys: Vec<(String, String, String)> = Vec::new();
        for r in &result.resolved {
            match r.original.row_id {
                Some(id) => row_ids.push(id),
                None => legacy_keys.push((
                    r.original.from_node_id.clone(),
                    r.original.reference_name.clone(),
                    r.original.reference_kind.as_str().to_string(),
                )),
            }
        }
        for chunk in row_ids.chunks(PERSIST_CHUNK) {
            outcome.removed += self.store.delete_references_by_row_ids(chunk)?;
        }
        for chunk in legacy_keys.chunks(PERSIST_CHUNK) {
            outcome.removed += self.store.delete_specific_resolved_references(chunk)?;
        }

        let failures = self.non_deferred_failures(&result.unresolved);
        let mut by_row_id: Vec<(i64, String, String, Option<String>)> = Vec::new();
        let mut legacy_failed: Vec<(String, String, String, Option<String>)> = Vec::new();
        for r in &failures {
            match r.row_id {
                Some(id) => by_row_id.push((id, r.reference_name.clone(), r.reference_kind.as_str().to_string(), r.name_tail.clone())),
                None => legacy_failed.push((
                    r.from_node_id.clone(),
                    r.reference_name.clone(),
                    r.reference_kind.as_str().to_string(),
                    r.name_tail.clone(),
                )),
            }
        }
        for chunk in by_row_id.chunks(PERSIST_CHUNK) {
            outcome.removed += self.store.mark_references_failed_by_row_ids(chunk)?;
        }
        for chunk in legacy_failed.chunks(PERSIST_CHUNK) {
            outcome.removed += self.store.mark_references_failed(chunk)?;
        }
        Ok((edges.len(), outcome))
    }

    /// Persist the outcome of a deferred pass: `deferred` were attempted, `resolved` hit.
    fn persist_deferred_references(&self, deferred: &[UnresolvedRef], resolved: Vec<ResolvedRef>) -> codegraph_db::Result<usize> {
        for r in deferred {
            if let Some(id) = r.row_id {
                self.deferred_row_ids.borrow_mut().remove(&id);
            }
        }
        let matched: HashSet<(Option<i64>, &str, &str, u32, u32)> = resolved
            .iter()
            .map(|r| (r.original.row_id, r.original.from_node_id.as_str(), r.original.reference_name.as_str(), r.original.line, r.original.column))
            .collect();
        let unresolved: Vec<UnresolvedRef> = deferred
            .iter()
            .filter(|r| !matched.contains(&(r.row_id, r.from_node_id.as_str(), r.reference_name.as_str(), r.line, r.column)))
            .cloned()
            .collect();
        let result = ResolutionResult {
            stats: ResolutionStats { total: deferred.len(), resolved: resolved.len(), unresolved: unresolved.len(), by_method: IndexMap::new() },
            resolved,
            unresolved,
        };
        let (count, _) = self.persist_resolution_result(&result)?;
        if count > 0 {
            self.clear_caches();
        }
        Ok(count)
    }

    /// Chained calls whose receiver type needs conformance edges: now that the main pass has
    /// written them, try again.
    pub fn resolve_chained_calls_via_conformance(&self) -> codegraph_db::Result<usize> {
        let deferred = std::mem::take(&mut *self.deferred_chain_refs.borrow_mut());
        if deferred.is_empty() {
            return Ok(0);
        }
        self.clear_caches();
        let ctx = &*self.ctx;
        let mut resolved: Vec<ResolvedRef> = Vec::new();
        for r in &deferred {
            let chain_match = if r.language == Language::Php && re!(r"^this->\w+\.\w+$").is_match(&r.reference_name) {
                match_method_call(r, ctx)
            } else if is_new_receiver_shape(r) {
                match_new_receiver_call(r, ctx)
            } else if r.language == Language::Rust {
                match_scoped_call_chain(r, ctx)
            } else {
                match_dotted_call_chain(r, ctx)
            };
            if let Some(m) = self.gate_language(chain_match, r) {
                resolved.push(m);
            }
        }
        self.persist_deferred_references(&deferred, resolved)
    }

    /// `this.x` function references whose method is inherited: walk the supertypes.
    pub fn resolve_deferred_this_member_refs(&self) -> codegraph_db::Result<usize> {
        let deferred = std::mem::take(&mut *self.deferred_this_member_refs.borrow_mut());
        if deferred.is_empty() {
            return Ok(0);
        }
        self.clear_caches();
        let ctx = &*self.ctx;
        let mut resolved: Vec<ResolvedRef> = Vec::new();
        let mut supertype_edges: HashMap<String, Rc<Vec<Edge>>> = HashMap::new();
        let mut callable_members: HashMap<String, Rc<IndexMap<String, Vec<NodeRef>>>> = HashMap::new();

        for r in &deferred {
            let member = &r.reference_name["this.".len()..];
            let Some(written) = self.node_by_id(&r.from_node_id) else { continue };
            if member.is_empty() {
                continue;
            }
            let from_node = this_scope_caller(&written, ctx);
            let class_name: String = if is_supertype_bearing_kind(from_node.kind) || from_node.kind == NodeKind::Module {
                from_node.name.clone()
            } else {
                let Some(sep) = from_node.qualified_name.rfind("::").filter(|&i| i > 0) else { continue };
                let class_prefix = &from_node.qualified_name[..sep];
                match class_prefix.rfind("::") {
                    Some(i) if class_prefix.contains("::") => class_prefix[i + 2..].to_string(),
                    _ => class_prefix.to_string(),
                }
            };

            let mut frontier: Vec<NodeRef> = ctx
                .get_nodes_by_name(&class_name)
                .iter()
                .filter(|n| is_supertype_bearing_kind(n.kind) && n.file_path == r.file_path)
                .cloned()
                .collect();
            if frontier.is_empty() {
                frontier = ctx
                    .get_nodes_by_name(&class_name)
                    .iter()
                    .filter(|n| is_supertype_bearing_kind(n.kind) && same_language_family(n.language, r.language))
                    .cloned()
                    .collect();
            }
            let mut seen: HashSet<String> = frontier.iter().map(|n| n.id.clone()).collect();
            let mut target: Option<NodeRef> = None;
            let mut depth = 0;
            while depth < 5 && !frontier.is_empty() && target.is_none() {
                let mut next: Vec<NodeRef> = Vec::new();
                'frontier: for type_node in &frontier {
                    let edges = match supertype_edges.get(&type_node.id) {
                        Some(e) => e.clone(),
                        None => {
                            let e = Rc::new(
                                self.store
                                    .get_outgoing_edges(&type_node.id, &RESOLUTION_READ_EDGE_KINDS, None)
                                    .unwrap_or_default(),
                            );
                            supertype_edges.insert(type_node.id.clone(), e.clone());
                            e
                        }
                    };
                    for edge in edges.iter() {
                        let Some(super_node) = self.node_by_id(&edge.target) else { continue };
                        if seen.contains(&super_node.id) {
                            continue;
                        }
                        seen.insert(super_node.id.clone());
                        if !is_supertype_bearing_kind(super_node.kind) {
                            continue;
                        }
                        let members = match callable_members.get(&super_node.id) {
                            Some(m) => m.clone(),
                            None => {
                                let mut by_name: IndexMap<String, Vec<NodeRef>> = IndexMap::new();
                                for c in self.store.get_outgoing_edges(&super_node.id, &[EdgeKind::Contains], None).unwrap_or_default() {
                                    let Some(m) = self.node_by_id(&c.target) else { continue };
                                    if !matches!(m.kind, NodeKind::Function | NodeKind::Method) {
                                        continue;
                                    }
                                    by_name.entry(m.name.clone()).or_default().push(m);
                                }
                                let m = Rc::new(by_name);
                                callable_members.insert(super_node.id.clone(), m.clone());
                                m
                            }
                        };
                        target = members
                            .get(member)
                            .and_then(|list| list.iter().find(|m| same_language_family(m.language, r.language)).cloned());
                        if target.is_some() {
                            break 'frontier;
                        }
                        next.push(super_node);
                    }
                }
                frontier = next;
                depth += 1;
            }

            if let Some(target) = target {
                resolved.push(ResolvedRef::new(r, target.id.clone(), 0.85, ResolvedBy::FunctionRef));
            }
        }
        self.persist_deferred_references(&deferred, resolved)
    }

    // -- Index run -------------------------------------------------------------------

    /// Resolve every pending reference in keyset batches, persisting each batch before the next
    /// is read, then synthesize the callback edges. Returns the aggregate stats.
    ///
    /// A big run splits each batch across worker threads (see [`resolve_worker_count`]). Every
    /// worker owns a read-only connection and its own resolver; a reference reads only what the
    /// batches before its own persisted, so the merged result (chunks in order) is the one a
    /// single thread produces.
    pub fn resolve_and_persist_batched(
        &self,
        on_progress: Option<&dyn Fn(usize, usize)>,
        batch_size: usize,
        synthesize: bool,
    ) -> codegraph_db::Result<ResolutionStats> {
        self.warm_caches();
        let total = self.store.get_unresolved_references_count()? as usize;
        let workers = resolve_worker_count(total);
        let db_path = self.store.path().to_path_buf();
        // Other connections cannot see an open transaction's writes.
        let parallel = workers > 1 && db_path.is_file() && !self.store.in_transaction();
        let timed = std::env::var("CODEGRAPHV_SYNTH_TIMINGS").is_ok_and(|v| !v.is_empty());
        let scope_started = std::time::Instant::now();
        let ran_ms: u128;
        let mut aggregate = {
            let (reply_tx, replies) = std::sync::mpsc::channel::<ChunkReply>();
            let pool: Vec<Worker> = if parallel {
                (0..workers).filter_map(|n| Worker::spawn(n, &db_path, self.ctx.project_root(), reply_tx.clone())).collect()
            } else {
                Vec::new()
            };
            drop(reply_tx);
            let run_started = std::time::Instant::now();
            let out = self.run_batches(&pool, &replies, on_progress, batch_size, total);
            ran_ms = run_started.elapsed().as_millis();
            out
        }?;
        if timed {
            eprintln!("[phase-timing]     pool setup + teardown {}ms", scope_started.elapsed().as_millis().saturating_sub(ran_ms));
        }
        if synthesize {
            let count = synthesize_callback_edges(&self.store, &self.ctx).unwrap_or(0);
            aggregate.by_method.insert("callback-synthesis".to_string(), count);
        }
        Ok(aggregate)
    }

    fn run_batches(
        &self,
        pool: &[Worker],
        replies: &std::sync::mpsc::Receiver<ChunkReply>,
        on_progress: Option<&dyn Fn(usize, usize)>,
        batch_size: usize,
        total: usize,
    ) -> codegraph_db::Result<ResolutionStats> {
        let mut processed = 0usize;
        let mut aggregate = ResolutionStats::default();

        // Wiring references (imports/extends/implements) first, so name matching can read the
        // edges they write; then everything else.
        let mut prerequisites = true;
        let mut after_row_id: i64 = 0;
        let read_next_batch = |prerequisites: &mut bool, after_row_id: &mut i64| -> codegraph_db::Result<Vec<UnresolvedReference>> {
            let mut next = self.store.get_unresolved_references_batch_after(*after_row_id, batch_size, Some(*prerequisites))?;
            if next.is_empty() && *prerequisites {
                *prerequisites = false;
                *after_row_id = 0;
                next = self.store.get_unresolved_references_batch_after(*after_row_id, batch_size, Some(*prerequisites))?;
            }
            if let Some(last) = next.last() {
                *after_row_id = last.row_id.unwrap_or(*after_row_id);
            }
            Ok(next)
        };
        let mut prev_remaining = i64::MAX;
        let timed = std::env::var("CODEGRAPHV_SYNTH_TIMINGS").is_ok_and(|v| !v.is_empty());
        let (mut read_ms, mut resolve_ms, mut persist_ms) = (0u128, 0u128, 0u128);
        let mut batch = read_next_batch(&mut prerequisites, &mut after_row_id)?;
        let mut batch_number = 0usize;
        while !batch.is_empty() {
            batch_number += 1;
            let t = std::time::Instant::now();
            let next_batch = read_next_batch(&mut prerequisites, &mut after_row_id)?;
            read_ms += t.elapsed().as_millis();
            let t = std::time::Instant::now();
            let result = if pool.is_empty() {
                // One transaction around the batch's reads, as the workers do (see Worker), unless
                // the caller already holds one: commit_batch must not end someone else's.
                let opened = !self.store.in_transaction() && self.store.begin_batch().is_ok();
                let result = self.resolve_batch(&batch);
                if opened {
                    self.store.commit_batch()?;
                }
                result
            } else {
                self.resolve_batch_on(pool, replies, std::mem::take(&mut batch), batch_number)
            };
            resolve_ms += t.elapsed().as_millis();
            let t = std::time::Instant::now();
            let (_, outcome) = self.persist_resolution_result(&result)?;
            persist_ms += t.elapsed().as_millis();
            let deferred_count = result.unresolved.len() - self.non_deferred_failures(&result.unresolved).len();

            aggregate.total += result.stats.total;
            aggregate.resolved += result.stats.resolved;
            aggregate.unresolved += result.stats.unresolved;
            for (method, count) in &result.stats.by_method {
                *aggregate.by_method.entry(method.clone()).or_insert(0) += count;
            }
            processed += result.stats.total;
            if let Some(cb) = on_progress {
                cb(processed, total);
            }

            // A batch that removed nothing and deferred nothing is stuck; stop if the pending
            // count did not move since the last such batch.
            if outcome.removed + deferred_count == 0 {
                let remaining = self.store.get_unresolved_references_count()?;
                if remaining >= prev_remaining {
                    break;
                }
                prev_remaining = remaining;
            }
            batch = next_batch;
        }
        if timed {
            eprintln!("[phase-timing]     read {read_ms}ms, resolve {resolve_ms}ms, persist {persist_ms}ms ({} workers)", pool.len().max(1));
        }
        Ok(aggregate)
    }

    /// One batch across the pool: contiguous chunks, merged back in order. A chunk whose worker
    /// is gone (its connection failed to open) is resolved here.
    fn resolve_batch_on(
        &self,
        pool: &[Worker],
        replies: &std::sync::mpsc::Receiver<ChunkReply>,
        batch: Vec<UnresolvedReference>,
        number: usize,
    ) -> ResolutionResult {
        let job = std::sync::Arc::new(BatchJob::new(batch, number));
        let chunks = job.chunks();
        let live = pool.iter().filter(|w| w.jobs.send(job.clone()).is_ok()).count();
        let mut parts: Vec<Option<ChunkOutcome>> = (0..chunks).map(|_| None).collect();
        let started = std::time::Instant::now();
        let (mut slowest, mut busy) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        if live > 0 {
            // Every chunk is answered once: by the worker that took it, or as failed.
            for _ in 0..chunks {
                match replies.recv() {
                    Ok((i, outcome, took)) => {
                        parts[i] = outcome;
                        slowest = slowest.max(took);
                        busy += took;
                    }
                    Err(_) => break, // every worker is gone; the rest is resolved below
                }
            }
        }
        if std::env::var("CODEGRAPHV_SYNTH_TIMINGS").is_ok_and(|v| !v.is_empty()) {
            let wall = started.elapsed();
            if wall.as_millis() >= 500 {
                eprintln!(
                    "[phase-timing]       batch {number}: {} refs, wall {}ms, slowest chunk {}ms, busy {}ms",
                    job.refs.len(),
                    wall.as_millis(),
                    slowest.as_millis(),
                    busy.as_millis()
                );
            }
        }
        let mut merged = ResolutionResult { resolved: Vec::new(), unresolved: Vec::new(), stats: ResolutionStats::default() };
        for (i, part) in parts.into_iter().enumerate() {
            let part = part.unwrap_or_else(|| ChunkOutcome::local(self, job.chunk(i)));
            for r in &part.chain {
                self.defer_reference(r, &self.deferred_chain_refs);
            }
            for r in &part.this_member {
                self.defer_reference(r, &self.deferred_this_member_refs);
            }
            let stats = &mut merged.stats;
            stats.total += part.result.stats.total;
            stats.resolved += part.result.stats.resolved;
            stats.unresolved += part.result.stats.unresolved;
            for (method, count) in part.result.stats.by_method {
                *stats.by_method.entry(method).or_insert(0) += count;
            }
            merged.resolved.extend(part.result.resolved);
            merged.unresolved.extend(part.result.unresolved);
        }
        merged
    }

    fn resolve_batch(&self, batch: &[UnresolvedReference]) -> ResolutionResult {
        self.resolve_chunk(batch, true)
    }

    /// `fresh_batch`: the first chunk this resolver sees of a batch, after which earlier batches'
    /// edges are in the database.
    fn resolve_chunk(&self, batch: &[UnresolvedReference], fresh_batch: bool) -> ResolutionResult {
        self.warm_caches();
        if fresh_batch {
            self.ctx.advance_supertype_generation();
        }
        let mut resolved: Vec<ResolvedRef> = Vec::new();
        let mut unresolved: Vec<UnresolvedRef> = Vec::new();
        let mut by_method: IndexMap<String, usize> = IndexMap::new();
        for raw in batch {
            let r = self.working_ref(raw);
            match self.resolve_one(&r) {
                Some(hit) => {
                    *by_method.entry(hit.resolved_by.as_str().to_string()).or_insert(0) += 1;
                    resolved.push(hit);
                }
                None => unresolved.push(self.parkable(r)),
            }
        }
        ResolutionResult {
            stats: ResolutionStats { total: batch.len(), resolved: resolved.len(), unresolved: unresolved.len(), by_method },
            resolved,
            unresolved,
        }
    }

    // -- Built-ins and externals ---------------------------------------------------

    fn is_python_project_module(&self, r: &UnresolvedRef, receiver: &str) -> bool {
        for imp in self.ctx.get_import_mappings(&r.file_path, r.language).iter() {
            if imp.local_name != receiver {
                continue;
            }
            let specifier = if imp.is_namespace {
                imp.source.clone()
            } else if imp.source.ends_with('.') {
                format!("{}{}", imp.source, imp.exported_name)
            } else {
                format!("{}.{}", imp.source, imp.exported_name)
            };
            if resolve_import_path(&specifier, &r.file_path, r.language, &self.ctx).is_some() {
                return true;
            }
        }
        false
    }

    /// Names that are language built-ins or belong to an external package, which no project
    /// symbol is the target of.
    fn is_built_in_or_external(&self, r: &UnresolvedRef) -> bool {
        let ctx = &*self.ctx;
        let name = r.reference_name.as_str();
        let is_js_ts = matches!(
            r.language,
            Language::TypeScript
                | Language::JavaScript
                | Language::Tsx
                | Language::Jsx
                | Language::ArkTs
                | Language::Vue
                | Language::Svelte
                | Language::Astro
        );

        if is_js_ts
            && JS_BUILT_INS.contains(name)
            && !ctx.get_import_mappings(&r.file_path, r.language).iter().any(|m| m.local_name == name)
        {
            return true;
        }
        if r.language == Language::ArkTs && (name == "$r" || name == "$rawfile") {
            return true;
        }
        if is_js_ts && (name.starts_with("console.") || name.starts_with("Math.") || name.starts_with("JSON.")) {
            return true;
        }
        if is_js_ts && REACT_HOOKS.contains(name) {
            return true;
        }
        if r.language == Language::Python && PYTHON_BUILT_INS.contains(name) {
            return true;
        }
        if r.language == Language::Python {
            if let Some(dot) = name.find('.').filter(|&i| i > 0) {
                let receiver = &name[..dot];
                let method = &name[dot + 1..];
                if PYTHON_BUILT_IN_TYPES.contains(receiver) {
                    return true;
                }
                if PYTHON_BUILT_IN_METHODS.contains(method) {
                    // `items.append(...)` on a module-level collection literal.
                    let is_collection = ctx.get_nodes_by_name(receiver).iter().any(|node| {
                        node.language == Language::Python
                            && node.file_path == r.file_path
                            && matches!(node.kind, NodeKind::Variable | NodeKind::Constant)
                            && node.qualified_name == receiver
                            && re!(r"^=\s*(?:[\[{]|(?:dict|list|set|tuple|frozenset)\s*\(|\(\s*\)|\([^()]*,)")
                                .is_match(node.signature.as_deref().unwrap_or(""))
                    });
                    if is_collection {
                        return true;
                    }
                    let capitalized = {
                        let mut chars = receiver.chars();
                        match chars.next() {
                            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                            None => String::new(),
                        }
                    };
                    let is_known_class = ctx.get_nodes_by_name(&capitalized).iter().any(|node| {
                        node.language == Language::Python
                            && matches!(node.kind, NodeKind::Class | NodeKind::Struct | NodeKind::Interface)
                    });
                    let is_project_module = !is_known_class && self.is_python_project_module(r, receiver);
                    if !is_known_class && !is_project_module {
                        return true;
                    }
                }
            }
            if PYTHON_BUILT_IN_METHODS.contains(name) && !ctx.known_names().is_some_and(|k| k.contains(name)) {
                return true;
            }
        }
        if r.language == Language::Go {
            // What the file imports decides before the name does: a project package may be
            // called `errors`, `log` or `types` too.
            if let Some(dot) = name.find('.').filter(|&i| i > 0 && !is_go_project_package(r, &name[..i], ctx)) {
                let pkg = &name[..dot];
                if GO_STDLIB_PACKAGES.contains(pkg) && !is_go_asserted_local(pkg, r, ctx) && !is_go_project_local_call(r, ctx) {
                    return true;
                }
            }
            if GO_BUILT_INS.contains(name) {
                return true;
            }
        }
        if r.language == Language::Pascal {
            // Whatever their case: Pascal names ignore it (`length(S)`, `system.TypeInfo(T)`).
            let lower = crate::ctx::js_lower(name);
            if PASCAL_UNIT_PREFIXES.iter().any(|p| name.starts_with(p)) || lower.starts_with("system.") {
                return true;
            }
            if PASCAL_BUILT_INS_LOWER.contains(lower.as_str()) {
                return true;
            }
        }
        if matches!(r.language, Language::C | Language::Cpp) {
            let scoped = name.strip_prefix("::").unwrap_or(name);
            if scoped.starts_with("std::") {
                return true;
            }
            if C_BUILT_INS.contains(scoped) || CPP_BUILT_INS.contains(scoped) {
                return !self.has_any_possible_match(scoped);
            }
        }
        false
    }

    // -- Razor / CFML / this-member ------------------------------------------------

    fn razor_usings(&self, file_path: &str) -> Rc<Vec<String>> {
        if let Some(cached) = self.razor_usings_cache.borrow().get(file_path) {
            return cached.clone();
        }
        let mut usings: IndexMap<String, ()> = IndexMap::new();
        let mut add_from = |src: Option<Rc<str>>| {
            let Some(src) = src else { return };
            if src.is_empty() {
                return;
            }
            let pattern = re!(r"^\s*@using\s+(?:static\s+)?([A-Za-z_][\w.]*)", "gm");
            for caps in pattern.captures_iter(&src) {
                if let Some(m) = caps.text(1) {
                    usings.insert(m.to_string(), ());
                }
            }
        };
        add_from(self.ctx.read_file(file_path));
        let mut dir: String = match file_path.rfind('/') {
            Some(i) if file_path.contains('/') => file_path[..i].to_string(),
            _ => String::new(),
        };
        loop {
            let imports = if dir.is_empty() { "_Imports.razor".to_string() } else { format!("{dir}/_Imports.razor") };
            add_from(self.ctx.read_file(&imports));
            if dir.is_empty() {
                break;
            }
            dir = match dir.rfind('/') {
                Some(i) => dir[..i].to_string(),
                None => String::new(),
            };
        }
        let arr: Rc<Vec<String>> = Rc::new(usings.into_keys().collect());
        self.razor_usings_cache.borrow_mut().insert(file_path.to_string(), arr.clone());
        arr
    }

    /// A Razor type name written bare: resolved against the file's `@using` namespaces.
    fn resolve_razor_using(&self, r: &UnresolvedRef) -> Option<ResolvedRef> {
        if r.reference_name.contains('.') || r.reference_name.contains("::") {
            return None;
        }
        let usings = self.razor_usings(&r.file_path);
        if usings.is_empty() {
            return None;
        }
        let mut found: IndexMap<String, NodeRef> = IndexMap::new();
        for ns in usings.iter() {
            for cand in self.ctx.get_nodes_by_qualified_name(&format!("{ns}::{}", r.reference_name)).iter() {
                found.insert(cand.id.clone(), cand.clone());
            }
        }
        if found.len() != 1 {
            return None;
        }
        let target = found.values().next()?;
        Some(ResolvedRef::new(r, target.id.clone(), 0.9, ResolvedBy::Import))
    }

    /// A CFML `extends="path.to.Component"` or `"../path/Component"` to the component's class.
    fn resolve_cfml_component_path(&self, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let ctx = &*self.ctx;
        let cfml_candidates = |name: &str| -> Vec<NodeRef> {
            ctx.get_nodes_by_name(name)
                .iter()
                .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Interface) && matches!(n.language, Language::Cfml | Language::CfScript))
                .cloned()
                .collect()
        };
        let norm = |p: &str| p.replace('\\', "/").to_lowercase();

        if r.reference_name.contains('/') {
            let rel = re!(r"\.cfc$", "i").replace_first(&r.reference_name, |_| String::new());
            let from_dir: Vec<String> = {
                let p = r.file_path.replace('\\', "/");
                let parts: Vec<&str> = p.split('/').collect();
                parts[..parts.len() - 1].iter().map(|s| s.to_string()).collect()
            };
            let mut parts = from_dir;
            for seg in rel.split('/') {
                if seg.is_empty() || seg == "." {
                    continue;
                }
                if seg == ".." {
                    if parts.is_empty() {
                        return None;
                    }
                    parts.pop();
                } else {
                    parts.push(seg.to_string());
                }
            }
            let want_path = norm(&format!("{}.cfc", parts.join("/")));
            let class_name = parts.last()?;
            if class_name.is_empty() {
                return None;
            }
            let target = cfml_candidates(class_name).into_iter().find(|c| norm(&c.file_path) == want_path)?;
            return Some(ResolvedRef::new(r, target.id.clone(), 0.95, ResolvedBy::FilePath));
        }

        let segments: Vec<&str> = r
            .reference_name
            .split('.')
            .map(codegraph_types::jscompat::js_trim)
            .filter(|s| !s.is_empty())
            .collect();
        if segments.len() < 2 {
            return None;
        }
        let class_name = segments[segments.len() - 1];
        let dir_segments = &segments[..segments.len() - 1];

        // Production code never names a component in a test suite; a test can name one, by
        // the path to it.
        let from_test = is_test_path(&r.file_path);
        let mut best_score = 0f64;
        let mut tied: Vec<NodeRef> = Vec::new();
        for cand in cfml_candidates(class_name) {
            if !from_test && is_test_suite_path(&cand.file_path) {
                continue;
            }
            let path = cand.file_path.replace('\\', "/");
            let all: Vec<&str> = path.split('/').collect();
            let dirs = &all[..all.len() - 1];
            let mut matched = 0usize;
            while matched < dir_segments.len()
                && matched < dirs.len()
                && crate::ctx::js_lower(dir_segments[dir_segments.len() - 1 - matched]) == crate::ctx::js_lower(dirs[dirs.len() - 1 - matched])
            {
                matched += 1;
            }
            let mut score = matched as f64;
            if matched == 0
                && dirs.is_empty()
                && dir_segments.len() == 1
                && crate::ctx::js_lower(&re!(r"\.cfc$", "i").replace_first(&cand.file_path, |_| String::new())) == crate::ctx::js_lower(class_name)
            {
                score = 0.5; // a root component, named by the mapping onto the root
            }
            if score > best_score {
                best_score = score;
                tied = vec![cand.clone()];
            } else if score == best_score && score > 0.0 {
                tied.push(cand.clone());
            }
        }
        // A test's tie between a component and a suite mirroring its path: the path names the
        // component, as it does from production code.
        if tied.len() > 1 {
            tied.retain(|c| !is_test_suite_path(&c.file_path));
        }
        if best_score == 0.0 || tied.len() != 1 {
            return None;
        }
        let confidence = if best_score >= 1.0 { 0.9 } else { 0.8 };
        Some(ResolvedRef::new(r, tied[0].id.clone(), confidence, ResolvedBy::QualifiedName))
    }

    /// A CFML call chained onto a construction (#2652), encoded `a.b.Batch().init`: the method
    /// of the component the path (or a bare name) reaches, its own or inherited; else nothing.
    fn resolve_cfml_constructed_call(&self, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let caps = re!(CFML_CONSTRUCTED_CALL).captures(&r.reference_name)?;
        let component = caps.text(1)?;
        let method_name = caps.text(2)?;
        let cls = if component.contains('.') {
            let mut sub = r.clone();
            sub.reference_name = component.to_string();
            sub.reference_kind = ReferenceKind::Edge(EdgeKind::Instantiates);
            let target = self.resolve_cfml_component_path(&sub)?;
            self.node_by_id(&target.target_node_id)
        } else {
            self.cfml_component_by_name(component, &r.file_path)
        }?;
        let mut seen = HashSet::new();
        let method = self.cfml_method_of(&cls, &crate::ctx::js_lower(method_name), &mut seen)?;
        Some(ResolvedRef::new(r, method.id.clone(), 0.9, ResolvedBy::InstanceMethod))
    }

    /// The CFML component a bare name reaches from `from_file`: the one in its directory, else
    /// the only one so named.
    fn cfml_component_by_name(&self, name: &str, from_file: &str) -> Option<NodeRef> {
        let components: Vec<NodeRef> = self
            .ctx
            .get_nodes_by_lower_name(&crate::ctx::js_lower(name))
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Interface) && matches!(n.language, Language::Cfml | Language::CfScript))
            .cloned()
            .collect();
        let dir_of = |p: &str| -> String { p[..p.rfind('/').map_or(0, |i| i + 1)].to_string() };
        let from_dir = dir_of(from_file);
        let beside: Vec<&NodeRef> = components.iter().filter(|n| dir_of(&n.file_path) == from_dir).collect();
        if beside.len() == 1 {
            return Some(beside[0].clone());
        }
        if components.len() == 1 {
            components.into_iter().next()
        } else {
            None
        }
    }

    /// A CFML component's method by its lowercased name (CFML names ignore case): its own, else
    /// one a supertype has.
    fn cfml_method_of(&self, cls: &NodeRef, lower_name: &str, seen: &mut HashSet<String>) -> Option<NodeRef> {
        if seen.contains(&cls.id) || seen.len() >= 8 {
            return None;
        }
        seen.insert(cls.id.clone());
        let own_prefix = format!("{}::", cls.name);
        let own = self
            .ctx
            .get_nodes_in_file(&cls.file_path)
            .iter()
            .find(|n| {
                n.kind == NodeKind::Method
                    && crate::ctx::js_lower(&n.name) == lower_name
                    && crate::ctx::js_lower(&n.qualified_name) == crate::ctx::js_lower(&format!("{}{}", own_prefix, n.name))
                    && n.start_line >= cls.start_line
                    && n.end_line <= cls.end_line
            })
            .cloned();
        if own.is_some() {
            return own;
        }
        for supertype in self.ctx.get_supertype_nodes(&cls.id).iter() {
            if let Some(inherited) = self.cfml_method_of(supertype, lower_name, seen) {
                return Some(inherited);
            }
        }
        None
    }

    /// The file a CFML include names (#2651): relative to the including file's directory, or to
    /// the web root (taken as the project root, less surely) when it starts with `/`. A single
    /// file that matches ignoring case counts too.
    fn resolve_cfml_include_path(&self, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let written = codegraph_types::jscompat::js_trim(&r.reference_name).replace('\\', "/");
        let from_root = written.starts_with('/');
        let mut parts: Vec<String> = if from_root {
            Vec::new()
        } else {
            let p = r.file_path.replace('\\', "/");
            let all: Vec<&str> = p.split('/').collect();
            all[..all.len() - 1].iter().map(|s| s.to_string()).collect()
        };
        for seg in written.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            if seg == ".." {
                parts.pop()?; // above the project root
            } else {
                parts.push(seg.to_string());
            }
        }
        let file_name = parts.last().filter(|f| !f.is_empty())?;
        let wanted = parts.join("/");
        let files: Vec<NodeRef> = self
            .ctx
            .get_nodes_by_lower_name(&crate::ctx::js_lower(file_name))
            .iter()
            .filter(|n| n.kind == NodeKind::File)
            .cloned()
            .collect();
        if let Some(exact) = files.iter().find(|n| n.file_path == wanted) {
            return Some(ResolvedRef::new(r, exact.id.clone(), if from_root { 0.85 } else { 0.95 }, ResolvedBy::FilePath));
        }
        let wanted_lower = crate::ctx::js_lower(&wanted);
        let folded: Vec<&NodeRef> = files.iter().filter(|n| crate::ctx::js_lower(&n.file_path) == wanted_lower).collect();
        if folded.len() == 1 {
            Some(ResolvedRef::new(r, folded[0].id.clone(), if from_root { 0.8 } else { 0.9 }, ResolvedBy::FilePath))
        } else {
            None
        }
    }

    /// `this.method` passed as a value: the method on the enclosing class.
    fn resolve_this_member_fn_ref(&self, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let ctx = &*self.ctx;
        let member = &r.reference_name["this.".len()..];
        if member.is_empty() {
            return None;
        }
        let written = self.node_by_id(&r.from_node_id)?;
        let from_node = this_scope_caller(&written, ctx);
        let class_prefix: String = if is_supertype_bearing_kind(from_node.kind) || from_node.kind == NodeKind::Module {
            from_node.qualified_name.clone()
        } else {
            let sep = from_node.qualified_name.rfind("::").filter(|&i| i > 0)?;
            from_node.qualified_name[..sep].to_string()
        };
        let candidates: Vec<NodeRef> = ctx
            .get_nodes_by_qualified_name(&format!("{class_prefix}::{member}"))
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method) && n.file_path == r.file_path && n.id != r.from_node_id)
            .cloned()
            .collect();
        if candidates.is_empty() {
            // The method may live on a supertype; wait for the conformance edges.
            self.defer_reference(r, &self.deferred_this_member_refs);
            return None;
        }
        let mut target = candidates[0].clone();
        for c in &candidates[1..] {
            if !(target.start_line <= c.start_line) {
                target = c.clone();
            }
        }
        Some(ResolvedRef::new(r, target.id.clone(), 0.95, ResolvedBy::FunctionRef))
    }

    // -- Gates ---------------------------------------------------------------------

    /// The value a file declares beside a type of the same name — `export function Ok()` beside
    /// `export interface Ok`, a class merged with an interface — which a call or `new` written
    /// with the name reaches. The first by position; `None` when the file declares none.
    fn same_named_value_of_type(&self, ty: &Node) -> Option<NodeRef> {
        const TYPE_COMPANION_VALUE_KINDS: [NodeKind; 5] = [NodeKind::Function, NodeKind::Class, NodeKind::Component, NodeKind::Constant, NodeKind::Variable];
        self.ctx
            .get_nodes_in_file(&ty.file_path)
            .iter()
            .filter(|n| n.name == ty.name && TYPE_COMPANION_VALUE_KINDS.contains(&n.kind))
            .min_by_key(|n| (n.start_line, n.start_column))
            .cloned()
    }

    fn gate_target_kind(&self, result: Option<ResolvedRef>, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let mut result = result?;
        let ctx = &*self.ctx;
        if r.language == Language::Go
            && (r.is_kind(EdgeKind::Calls) || r.reference_kind == ReferenceKind::FunctionRef)
            && is_go_bare_name(r, ctx)
            && is_go_locally_bound(r, ctx)
        {
            return None;
        }
        if r.is_kind(EdgeKind::Calls) {
            let target = self.node_by_id(&result.target_node_id);
            if let Some(t) = &target {
                if t.kind == NodeKind::Constant && is_cpp_define_signature(t.signature.as_deref().unwrap_or("")) {
                    return None;
                }
                if r.language == Language::Go && is_go_type_kind(t.kind) && !r.reference_name.contains('.') && !is_go_bare_name(r, ctx) {
                    return None;
                }
            }
        }
        // A TypeScript interface or type alias is no value: nothing calls one or makes an
        // instance of it. A strategy that found one by name meant the value its file declares
        // beside it (`interface Ok` and `function Ok`); with none there, the name is a value from
        // elsewhere the project only types (`new Response()`), and links nothing.
        if (r.is_kind(EdgeKind::Calls) || r.is_kind(EdgeKind::Instantiates)) && is_esm_import_language(r.language) {
            if let Some(t) = self.node_by_id(&result.target_node_id) {
                if t.kind == NodeKind::Interface || t.kind == NodeKind::TypeAlias {
                    let value = self.same_named_value_of_type(&t)?;
                    result.target_node_id = value.id.clone();
                }
            }
        }
        if r.is_kind(EdgeKind::Imports) {
            let target = self.node_by_id(&result.target_node_id);
            if let Some(t) = &target {
                if t.kind == NodeKind::Import && t.file_path == r.file_path {
                    return None;
                }
                if !is_importable_kind(t.kind) {
                    return None;
                }
            }
            return Some(result);
        }
        if r.language == Language::Go
            && r.is_kind(EdgeKind::References)
            && self.node_by_id(&r.from_node_id).is_some_and(|n| is_go_type_kind(n.kind))
            && is_go_unknown_qualified(r, ctx)
        {
            return None;
        }
        if r.language == Language::Go
            && (r.is_kind(EdgeKind::Instantiates)
                || (r.is_kind(EdgeKind::References) && self.node_by_id(&r.from_node_id).map(|n| n.kind) != Some(NodeKind::Route)))
        {
            return go_type_position_target(result, r, ctx);
        }
        if !is_inheritance_ref(r) {
            return Some(result);
        }
        let target = self.node_by_id(&result.target_node_id);
        if let Some(t) = &target {
            if !is_supertype_target(t) {
                let ty = self.same_named_type_of_value(t)?;
                result.target_node_id = ty.id.clone();
            }
        }
        if is_bound_to_out_of_repo_import(r, ctx) {
            return None;
        }
        if r.language == Language::Go && is_go_unknown_qualified(r, ctx) {
            return None;
        }
        if r.language == Language::Go {
            return go_dot_imported_supertype(result, r, ctx);
        }
        Some(result)
    }

    /// A call back to the caller itself through `super.x()` / `base.x()` / `parent::x()` is the
    /// parent's method, not a recursion.
    fn gate_super_self_call(&self, result: Option<ResolvedRef>, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let result = result?;
        if !r.is_kind(EdgeKind::Calls) || result.target_node_id != r.from_node_id {
            return Some(result);
        }
        // A TS/JS call is read where it starts, so a `this.run()` that shares its line with a
        // `super.run()` keeps its edge (#2497).
        if crate::js_super_calls::is_js_super_call_language(r.language) {
            return if crate::js_super_calls::is_js_super_call_site(r, &self.ctx) { None } else { Some(result) };
        }
        let cut = r.reference_name.rfind('.').map_or(0, |i| i + 1).max(r.reference_name.rfind(':').map_or(0, |i| i + 1));
        let name = &r.reference_name[cut..];
        if !re!(r"^[A-Za-z_$][\w$]*$").is_match(name) {
            return Some(result);
        }
        let ctx = &*self.ctx;
        let line_text: Option<String> = match ctx.get_file_lines(&r.file_path) {
            Some(lines) => lines.get((r.line as usize).wrapping_sub(1)).cloned(),
            None => ctx
                .read_file(&r.file_path)
                .and_then(|c| re!(r"\r?\n").split(&c).get((r.line as usize).wrapping_sub(1)).map(|s| s.to_string())),
        };
        let Some(line_text) = line_text.filter(|l| !l.is_empty()) else { return Some(result) };
        let escaped = name.replace('$', "\\$");
        let via_super = Re::cached(
            &format!(
                r"(?:\b(?:super|base)\s*(?:\(\s*(?:[\w.]+\s*,\s*\w+)?\s*\))?\s*\??\.\s*|\[\s*super\s+|\bparent\s*::\s*){escaped}\b"
            ),
            "",
        );
        if via_super.is_match(&line_text) {
            None
        } else {
            Some(result)
        }
    }

    /// Rust: a bare name binds only if the module scope sees it, and a call must be one a
    /// foreign `impl` can make.
    fn gate_rust_scope(&self, result: Option<ResolvedRef>, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let result = result?;
        if r.language != Language::Rust {
            return Some(result);
        }
        let bare = re!(r"^[A-Za-z_]\w*$").is_match(&r.reference_name);
        if !bare && !r.is_kind(EdgeKind::Calls) {
            return Some(result);
        }
        let Some(target) = self.node_by_id(&result.target_node_id) else { return Some(result) };
        let ctx = &*self.ctx;
        if bare && !is_rust_name_in_scope(&target, r, ctx) {
            return None;
        }
        if !r.is_kind(EdgeKind::Calls) {
            return Some(result);
        }
        // A call through a path whose type is from outside the project reaches only the project's
        // impls for that type, or of that trait.
        if !bare && !is_rust_path_call_reachable(&target, r, ctx) {
            return None;
        }
        is_rust_foreign_impl_callable(&target, r, ctx).then_some(result)
    }

    /// A TS `const Foo = …` value that shares its name with the `type Foo` / `interface Foo` an
    /// inheritance clause actually means.
    fn same_named_type_of_value(&self, value: &Node) -> Option<NodeRef> {
        if !matches!(value.kind, NodeKind::Constant | NodeKind::Variable) {
            return None;
        }
        if !matches!(value.language, Language::TypeScript | Language::Tsx) {
            return None;
        }
        let types: Vec<NodeRef> = self
            .ctx
            .get_nodes_in_file(&value.file_path)
            .iter()
            .filter(|n| n.name == value.name && is_supertype_target(n))
            .cloned()
            .collect();
        if types.len() == 1 {
            Some(types[0].clone())
        } else {
            None
        }
    }

    fn gate_language(&self, result: Option<ResolvedRef>, r: &UnresolvedRef) -> Option<ResolvedRef> {
        result.and_then(|res| gate_language_match(Some(res), r, &self.ctx))
    }

    /// Frameworks may bridge code languages freely for calls, but not for other reference kinds.
    fn gate_framework_language(&self, result: Option<ResolvedRef>, r: &UnresolvedRef) -> Option<ResolvedRef> {
        let result = result?;
        if r.is_kind(EdgeKind::Calls) {
            return Some(result);
        }
        let target_language = self.ctx.language_from_node_id(&result.target_node_id);
        if r.is_kind(EdgeKind::Imports) && target_language.as_str() == "markdown" && r.language.as_str() != "markdown" {
            return None;
        }
        if crosses_code_boundary(target_language, r.language) {
            return None;
        }
        Some(result)
    }
}

/// The highest-confidence candidate; the earliest wins a tie.
fn best_candidate(candidates: Vec<ResolvedRef>) -> Option<ResolvedRef> {
    let mut best: Option<ResolvedRef> = None;
    for curr in candidates {
        match &best {
            Some(b) if !(curr.confidence > b.confidence) => {}
            _ => best = Some(curr),
        }
    }
    best
}

/// The database form of a working reference.
pub fn to_db_reference(r: &UnresolvedRef) -> UnresolvedReference {
    UnresolvedReference {
        from_node_id: r.from_node_id.clone(),
        reference_name: r.reference_name.clone(),
        reference_kind: r.reference_kind,
        line: r.line,
        column: r.column,
        file_path: Some(r.file_path.clone()),
        language: Some(r.language),
        candidates: r.candidates.clone(),
        row_id: None,
    }
}

/// Routes that exist in `after` but not `before`, or vice versa (by file and name).
pub fn changed_routes(before: &[Node], after: &[Node]) -> Vec<Node> {
    let key = |route: &Node| format!("{}\0{}", route.file_path, route.name);
    let before_keys: HashSet<String> = before.iter().map(key).collect();
    let after_keys: HashSet<String> = after.iter().map(key).collect();
    let mut out: Vec<Node> = before.iter().filter(|r| !after_keys.contains(&key(r))).cloned().collect();
    out.extend(after.iter().filter(|r| !before_keys.contains(&key(r))).cloned());
    out
}

/// A python call `x.get(…)` of a method named like a builtin collection, string or file method.
fn is_python_builtin_named_method_call(r: &UnresolvedRef) -> bool {
    if r.language != Language::Python || !r.is_kind(EdgeKind::Calls) {
        return false;
    }
    match r.reference_name.find('.') {
        Some(dot) if dot > 0 => PYTHON_BUILT_IN_METHODS.contains(&r.reference_name[dot + 1..]) && !PYTHON_BUILT_IN_TYPES.contains(&r.reference_name[..dot]),
        _ => false,
    }
}

/// The known-name set with the names a view hides taken out (hasAnyPossibleMatch's `known`).
struct Known<'a> {
    all: &'a std::collections::HashSet<String>,
    only: Option<&'a std::collections::HashSet<String>>,
}

impl Known<'_> {
    fn contains(&self, name: &str) -> bool {
        self.all.contains(name) && !self.only.is_some_and(|o| o.contains(name))
    }
}

/// Whether the Go file imports `qualifier` — by its last path segment or an alias — from a package
/// of one of the project's own modules.
fn is_go_project_package(r: &UnresolvedRef, qualifier: &str, ctx: &Ctx) -> bool {
    ctx.get_import_mappings(&r.file_path, r.language)
        .iter()
        .any(|imp| imp.local_name == qualifier && ctx.get_go_package_dir(&imp.source, Some(&r.file_path)).is_some())
}

#[cfg(test)]
mod worker_count_tests {
    use super::{chunk_starts, worker_count_for, PARALLEL_MIN_REFS};

    #[test]
    fn chunks_end_at_file_boundaries() {
        let refs: Vec<codegraph_types::UnresolvedReference> = ["a", "a", "a", "b", "b", "c", "d", "d", "d", "d"]
            .iter()
            .map(|f| codegraph_types::UnresolvedReference {
                from_node_id: String::new(),
                reference_name: String::new(),
                reference_kind: codegraph_types::ReferenceKind::FunctionRef,
                line: 0,
                column: 0,
                file_path: Some(f.to_string()),
                language: None,
                candidates: None,
                row_id: None,
            })
            .collect();
        assert_eq!(chunk_starts(&refs, 2, 100), vec![0, 3, 5]);
        assert_eq!(chunk_starts(&refs, 1, 100), vec![0, 3, 5, 6]);
        assert_eq!(chunk_starts(&refs, 100, 100), vec![0]);
        assert_eq!(chunk_starts(&refs, 1, 2), vec![0, 2, 3, 5, 6, 8], "a big file is split at max");
        assert!(chunk_starts(&[], 4, 8).is_empty());
    }

    #[test]
    fn a_big_run_uses_a_worker_per_core_and_the_variable_overrides() {
        assert_eq!(worker_count_for(None, PARALLEL_MIN_REFS, 6), 6);
        assert_eq!(worker_count_for(None, PARALLEL_MIN_REFS, 64), 64);
        assert_eq!(worker_count_for(None, PARALLEL_MIN_REFS - 1, 12), 1, "a small run stays on one thread");
        assert_eq!(worker_count_for(Some("24"), 10, 12), 24);
        assert_eq!(worker_count_for(Some("0"), PARALLEL_MIN_REFS, 12), 1);
    }
}
