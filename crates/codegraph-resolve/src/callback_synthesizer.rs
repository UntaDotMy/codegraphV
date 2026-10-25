//! Whole-graph edge synthesis — port of `src/resolution/callback-synthesizer.ts`.
//!
//! Static extraction cannot see computed or indirect calls (a callback registered here and fired
//! there, an event bus, a framework dispatching by name), so these passes bridge them with
//! heuristic edges that carry `metadata.synthesizedBy` and the `registeredAt` wiring site.

mod angular_template;
mod c_fnptr;
mod channels;
mod common;
mod dispatch;
mod go;
mod go_func_field;
mod iface;
mod jsx;
mod jsx_scope;
pub use jsx::is_jsx_child_candidate;
mod native;
mod routers;
mod stores;
mod tier;

use crate::ctx::Ctx;
use crate::frameworks::react::lazy_value_modules;
use codegraph_db::Store;
use codegraph_jsre::re;
use codegraph_types::Edge;
use indexmap::IndexSet;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// `has('a', 'b')`: does the project hold a file in any of these languages?
type Has<'a> = &'a dyn Fn(&[&str]) -> bool;

struct SynthPass {
    name: &'static str,
    gate: fn(Has) -> bool,
    run: fn(&Ctx) -> Vec<Edge>,
}

const JS: [&str; 4] = ["typescript", "javascript", "tsx", "jsx"];
const VUE_AND_JS: [&str; 5] = ["vue", "typescript", "javascript", "tsx", "jsx"];
const IFACE_LANGUAGES: [&str; 12] =
    ["java", "kotlin", "csharp", "swift", "scala", "go", "rust", "arkts", "typescript", "javascript", "tsx", "jsx"];

/// The independent passes, in MERGE ORDER — the first-seen dedup in `synthesize_callback_edges`
/// follows this array, so reordering entries changes which duplicate edge wins. The two Go
/// pre-passes (cross-file method `contains`, implicit `implements`) are NOT here: they persist
/// before these run because the interface pass reads their edges from the DB.
fn synth_passes() -> Vec<SynthPass> {
    fn always(_: Has) -> bool {
        true
    }
    let p = |name, gate, run| SynthPass { name, gate, run };
    vec![
        p("fieldEdges", always, channels::field_channel_edges),
        p("closureCollEdges", always, channels::closure_collection_edges),
        // Cross-tier channels — a client's `fetch('/api/x')` onto its own route, a queue job onto
        // its consumer, a bus / socket event onto its handler. Before the in-process emitter
        // pass: the same (source, target) pair keeps the more specific edge.
        p("tierEdges", |has| has(&JS), tier::cross_tier_edges),
        // A Spring / Laravel test's request onto the route it reaches.
        p("testRequestEdges", |has| has(&["java", "kotlin", "php"]), tier::test_request_edges),
        p("emitterEdges", always, channels::event_emitter_edges),
        p("renderEdges", always, channels::react_render_edges),
        p("jsxEdges", |has| has(&JS), jsx::react_jsx_child_edges),
        p("vueEdges", |has| has(&["vue"]), jsx::vue_template_edges),
        p("svelteKitEdges", |has| has(&["svelte"]), native::svelte_kit_load_edges),
        p("pascalEdges", always, native::pascal_form_edges),
        p("flutterEdges", |has| has(&["dart"]), channels::flutter_build_edges),
        p("arkuiStateEdges", |has| has(&["arkts"]), channels::arkui_state_build_edges),
        p("arkuiEmitter", |has| has(&["arkts"]), channels::arkui_emitter_edges),
        p("arkuiRoutes", |has| has(&["arkts"]), channels::arkui_router_edges),
        p("cppEdges", |has| has(&["cpp"]), channels::cpp_override_edges),
        p("ifaceEdges", |has| has(&IFACE_LANGUAGES), iface::interface_override_edges),
        p("kotlinExpectActual", |has| has(&["kotlin"]), iface::kotlin_expect_actual_edges),
        p("goGrpcEdges", |has| has(&["go"]), iface::go_grpc_stub_impl_edges),
        p("rnEventEdgesList", |has| has(&JS), native::rn_event_edges),
        p("fabricNativeEdges", always, native::fabric_native_impl_edges),
        // Expo module nodes are emitted only from .swift/.kt files, and a pair needs BOTH
        // platforms.
        p("expoXPlatEdges", |has| has(&["swift"]) && has(&["kotlin"]), native::expo_cross_platform_edges),
        // An RN cross-platform edge requires a JS-language caller on the native method.
        p("rnXPlatEdges", |has| has(&JS), native::rn_cross_platform_edges),
        p("mybatisEdges", |has| has(&["java", "kotlin"]) && has(&["xml"]), native::mybatis_java_xml_edges),
        p("ginEdges", |has| has(&["go"]), native::gin_middleware_chain_edges),
        p("thunkEdges", |has| has(&JS), stores::redux_thunk_edges),
        p("registryEdges", always, stores::object_registry_edges),
        p("rtkEdges", |has| has(&JS), stores::rtk_query_edges),
        p("piniaEdges", |has| has(&VUE_AND_JS), stores::pinia_store_edges),
        p("vuexEdges", |has| has(&VUE_AND_JS), stores::vuex_dispatch_edges),
        p("celeryEdges", |has| has(&["python"]), stores::celery_dispatch_edges),
        p("springEdges", |has| has(&["java"]), dispatch::spring_event_edges),
        p("mediatrEdges", |has| has(&["csharp"]), dispatch::mediatr_dispatch_edges),
        p("sidekiqEdges", |has| has(&["ruby"]), dispatch::sidekiq_dispatch_edges),
        p("erlangBehaviourEdges", |has| has(&["erlang"]), dispatch::erlang_behaviour_dispatch_edges),
        p("laravelEdges", |has| has(&["php"]), dispatch::laravel_event_edges),
        p("cFnPtrEdges", |has| has(&["c", "cpp"]), c_fnptr::c_fn_pointer_dispatch_edges),
        p("goframeEdges", |has| has(&["go"]), routers::goframe_route_edges),
        // `d.refreshf(ctx)` onto the functions put in that func field. After the Go framework
        // passes: a pair one of them bridges keeps its more specific edge.
        p("goFuncFieldEdges", |has| has(&["go"]), go_func_field::go_func_field_edges),
        // `router.push(await helper())` — the helper's return literals are the screens.
        p("expoRouterReturnEdges", |has| has(&JS), routers::expo_router_return_edges),
        // `<Link href="/x">` / an internal `<a href>` — markup, not a call; the component navigates.
        p("nextLinkEdges", |has| has(&JS), routers::next_link_edges),
        p("reactRouterLinkEdges", |has| has(&JS), routers::react_router_link_edges),
        p("tanstackLinkEdges", |has| has(&JS), routers::tanstack_link_edges),
        p("vueRouterLinkEdges", |has| has(&VUE_AND_JS), routers::vue_router_link_edges),
        // An Angular template: the child components it renders and its `routerLink`s.
        p("angularTemplateEdges", |has| has(&["typescript"]), angular_template::angular_template_edges),
        p("svelteKitPageEdges", |has| has(&["svelte"]), routers::svelte_kit_page_component_edges),
        p("svelteKitLinkEdges", |has| has(&["svelte"]), routers::svelte_kit_link_edges),
        p("nixOptionEdges", |has| has(&["nix"]), dispatch::nix_option_path_edges),
    ]
}

/// Insert synthesized edges in chunks (the Linux kernel's merged set is ~275k rows).
fn insert_chunked(store: &Store, edges: &[Edge]) -> codegraph_db::Result<()> {
    for chunk in edges.chunks(2000) {
        store.insert_edges(chunk)?;
    }
    Ok(())
}

/// Run every synthesis pass over the resolved graph, inserting the edges they find. Returns how
/// many were added.
pub fn synthesize_callback_edges(store: &Rc<Store>, ctx: &Rc<Ctx>) -> codegraph_db::Result<usize> {
    // Language gating: one indexed DISTINCT over the files table lets a pass whose own filters
    // reference a specific language be skipped outright when the project has no such files — its
    // result is provably empty (the Kotlin pass was the OOM culprit on the pure-C Linux kernel).
    let langs = store.get_distinct_file_languages()?;
    let has = |ls: &[&str]| ls.iter().any(|l| langs.contains(*l));
    let ctx: &Ctx = ctx;

    // Cross-file Go method→type `contains` edges are synthesized AND persisted FIRST: a method
    // declared in a different file from its receiver type is otherwise orphaned from the struct,
    // and the implements pass derives a struct's method set from its `contains` edges. (#583)
    let go_method_contains = if has(&["go"]) { go::go_cross_file_method_contains_edges(ctx) } else { Vec::new() };
    insert_chunked(store, &go_method_contains)?;

    // Go implicit `implements` edges next: the interface-dispatch bridge reads `implements` edges
    // from the DB, and Go has none statically.
    let go_impl = if has(&["go"]) { go::go_implements_edges(ctx) } else { Vec::new() };
    insert_chunked(store, &go_impl)?;

    // The independent passes. Their results merge in REGISTRY ORDER below and none of their edges
    // persist until that merge, so every pass sees the same committed post-resolution DB state.
    // A Jest or Vitest manual mock answers only for test code (#2455): whatever pass paired
    // them, production code never links into a `__mocks__/` file.
    let mock_nodes: HashSet<String> = ctx
        .get_all_files()
        .iter()
        .filter(|f| crate::ctx::is_manual_mock_path(f))
        .flat_map(|f| ctx.get_nodes_in_file(f).iter().map(|n| n.id.clone()).collect::<Vec<_>>())
        .collect();
    let from_test_code = |id: &str| ctx.node_by_id(id).is_none_or(|n| codegraph_search::is_test_path(&n.file_path));
    let mut merged: Vec<Edge> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let passes: Vec<SynthPass> = synth_passes().into_iter().filter(|pass| (pass.gate)(&has)).collect();
    for edges in run_passes(store, ctx, &passes) {
        for e in edges {
            if !seen.insert(format!("{}>{}", e.source, e.target)) {
                continue;
            }
            if !mock_nodes.is_empty() && mock_nodes.contains(&e.target) && !from_test_code(&e.source) {
                continue;
            }
            merged.push(e);
        }
    }
    insert_chunked(store, &merged)?;
    store.replace_synthesis_inputs(&collect_inputs(ctx))?;
    Ok(merged.len() + go_impl.len() + go_method_contains.len())
}

/// Address space for a pass thread's stack: the passes walk nested code recursively.
const PASS_STACK_BYTES: usize = 256 * 1024 * 1024;

/// Every pass's edges, in `passes` order. The passes read only the committed database, so they
/// run on every core at once, each thread with its own read-only connection and context; a pass
/// whose thread fails runs here on `ctx`. Inside a transaction (a sync's refresh) other
/// connections cannot see its writes, so every pass runs here.
fn run_passes(store: &Store, ctx: &Ctx, passes: &[SynthPass]) -> Vec<Vec<Edge>> {
    use std::sync::{atomic::{AtomicUsize, Ordering}, Mutex};
    let timed = std::env::var("CODEGRAPHV_SYNTH_TIMINGS").is_ok_and(|v| !v.is_empty());
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let threads = if store.in_transaction() { 1 } else { cpus.min(passes.len()) };
    let db_path = store.path().to_path_buf();
    let slots: Vec<Mutex<Option<(Vec<Edge>, std::time::Duration)>>> = passes.iter().map(|_| Mutex::new(None)).collect();
    if threads > 1 && db_path.is_file() {
        let next = AtomicUsize::new(0);
        let root = ctx.project_root().to_string();
        std::thread::scope(|scope| {
            for n in 0..threads {
                let (next, slots, db_path, root) = (&next, &slots, &db_path, &root);
                let body = move || {
                    let Ok(store) = Store::open(db_path, codegraph_db::OpenOptions { read_only: true }) else { return };
                    let store = Rc::new(store);
                    let ctx = Ctx::new(root.clone(), store.clone());
                    // One read transaction for all of this thread's passes (nothing writes while
                    // they run): outside one, every statement takes SQLite's WAL read lock anew.
                    let _ = store.begin_batch();
                    ctx.warm_caches();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(pass) = passes.get(i) else { break };
                        let start = std::time::Instant::now();
                        if let Ok(edges) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (pass.run)(&ctx))) {
                            *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some((edges, start.elapsed()));
                        }
                    }
                    store.rollback_batch(); // read-only: ends the transaction, nothing to undo
                };
                let _ = std::thread::Builder::new()
                    .name(format!("codegraph-synth-{n}"))
                    .stack_size(PASS_STACK_BYTES)
                    .spawn_scoped(scope, body);
            }
        });
    }
    let mut timings = Vec::new();
    let out = slots
        .into_iter()
        .zip(passes)
        .map(|(slot, pass)| {
            let (edges, took) = slot.into_inner().unwrap_or_else(|e| e.into_inner()).unwrap_or_else(|| {
                let start = std::time::Instant::now();
                ((pass.run)(ctx), start.elapsed())
            });
            timings.push((took, pass.name));
            edges
        })
        .collect();
    if timed {
        timings.sort_by(|a, b| b.cmp(a));
        let top: Vec<String> = timings.iter().take(8).map(|(d, name)| format!("{name} {}ms", d.as_millis())).collect();
        eprintln!("[phase-timing]     synthesis passes on {threads} threads, slowest: {}", top.join(", "));
    }
    out
}

/// Files whose text could feed a pass (inputs that produce NO edges included, e.g. an over-cap
/// channel: deleting one may make the full pass viable). A module a file's own lazy value loads
/// feeds the JSX that names the value, even with no markup of its own: a page given its default
/// export, or a barrel rewired, redraws it. So does every file a tag imported from the project is
/// read through.
fn collect_inputs(ctx: &Ctx) -> Vec<String> {
    let mut found: IndexSet<String> = IndexSet::new();
    let mut tag_modules: HashMap<String, IndexSet<String>> = HashMap::new();
    for file in ctx.get_all_files() {
        let Some(content) = ctx.read_file(&file) else { continue };
        if !has_synthesis_pattern(&file, &content) {
            continue;
        }
        found.insert(file.clone());
        if re!(r"\bimport\s*\(").is_match(&content) {
            for read in lazy_value_modules(&file, ctx) {
                found.insert(read);
            }
        }
        if re!(r"\.(?:[cm]?[jt]sx?|vue|svelte|astro)$").is_match(&file) && (content.contains("</") || content.contains("/>")) {
            for read in jsx::imported_tag_modules(ctx, &file, &content, &mut tag_modules) {
                found.insert(read);
            }
        }
    }
    found.into_iter().collect()
}

/// Conservative input gates for the passes; keep these in sync when adding a pass.
pub fn has_synthesis_pattern(file_path: &str, content: &str) -> bool {
    // These passes consume declarations/layouts as well as dispatch sites. A header or markup
    // edit can change a channel whose endpoints live elsewhere.
    if re!(r"\.(?:vue|svelte|dfm|fmx|nix|xml)$").is_match(file_path) {
        return true;
    }
    if re!(r"\.(?:c|h|cc|cpp|cxx|hpp|hh|hxx|cppm|ipp|inl|tcc|def|inc|tbl)$", "i").is_match(file_path)
        && re!(r"\b(?:struct|union|typedef|virtual|override)\b|#\s*(?:include|define|if)|=|->|\[").is_match(content)
    {
        return true;
    }
    if re!(r"\b(?:class|interface|protocol|trait|impl|extends|implements|expect|actual)\b").is_match(content) {
        return true;
    }
    // Every Go file: besides the declarations the interface passes read, a call through a func
    // field, or a function put in one, can sit in any of them.
    if file_path.ends_with(".go") {
        return true;
    }
    if tier::has_cross_tier_pattern(content) || tier::has_test_request_pattern(file_path, content) {
        return true;
    }
    if re!(r"\b(?:render|build|setState|defineStore|createStore|createApi|Store|href|sendEvent|sendEventWithName)\b|</|/>")
        .is_match(content)
    {
        return true;
    }
    // A wrapper renders the component it wraps, in a file with no markup of its own too.
    if re!(r"\b(?:styled|memo|forwardRef)\s*\(\s*[A-Z]").is_match(content) {
        return true;
    }
    if re!(r"\.(?:forEach|append|add|push|insert|fire|dispatchEvent|addListener|Use|GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD|Any|Handle)\s*\(")
        .is_match(content)
    {
        return true;
    }
    if re!(r"[\w$]\s*\[\s*[A-Za-z_$]").is_match(content) || re!(r"\b(?:dispatch|commit)\s*\(").is_match(content) {
        return true;
    }
    let patterns = [
        re!(r"create(?:Async)?Thunk"),
        re!(r"@\s*(?:[A-Za-z_][\w.]*\.)?(?:shared_task|task)\b"),
        re!(r"\b([A-Za-z_]\w*)\s*\.\s*(?:delay|apply_async)\s*\("),
        re!(r"@(?:EventListener|TransactionalEventListener)\b"),
        re!(r"\bApplicationListener\s*<"),
        re!(r"\.publishEvent\s*\(\s*new\s+([A-Z][A-Za-z0-9_]*)"),
        re!(r"(?:IRequestHandler|INotificationHandler)\s*<\s*([A-Za-z_]\w*)"),
        re!(r"([A-Za-z_][\w.]*)\s*\.\s*(?:Send|Publish)\s*\(\s*(new\s+[A-Z]\w*|[A-Za-z_]\w*)"),
        re!(r"\binclude\s+Sidekiq::(?:Job|Worker)\b"),
        re!(r"([A-Z][A-Za-z0-9_]*(?:::[A-Z][A-Za-z0-9_]*)*)\s*\.\s*perform_(?:async|in|at)\b"),
        re!(r"(^|\n)\s*-callback\s+('[^'\n]+'|[a-z][A-Za-z0-9_@]*)\s*\("),
        re!(r"(^|[^?\w@'])([A-Z][A-Za-z0-9_@]*):([a-z][A-Za-z0-9_@]*)\("),
        re!(r"\bevent\s*\(\s*new\s+\\?([A-Za-z_][\w\\]*)"),
        re!(r"\bemitter\s*\.\s*(emit|on|once)\s*\(\s*([A-Za-z_$][\w$.]*|\{[^)]{0,120}?\beventId\s*:\s*[^,}]+[^)]*?\})"),
        re!(r##"\brouter\s*\.\s*(?:pushUrl|replaceUrl)\s*\(\s*\{[^)]{0,200}?\burl\s*:\s*['"]([\w\-./]+)['"]"##),
    ];
    if patterns.iter().any(|p| p.is_match(content)) {
        return true;
    }
    // Field-backed observers use method-name gates rather than fixed call names.
    let registrar = re!(r"^(on[A-Z]\w*|subscribe|addListener|addEventListener|register|watch|listen|addCallback)$");
    let dispatcher = re!(r"(emit|trigger|notify|dispatch|fire|publish|flush)", "i");
    let bytes = content.as_bytes();
    let is_start = |b: u8| b.is_ascii_alphabetic() || b == b'_' || b == b'$';
    let is_part = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut i = 0;
    while i < bytes.len() {
        if !is_start(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_part(bytes[i]) {
            i += 1;
        }
        let name = &content[start..i];
        if registrar.is_match(name) || dispatcher.is_match(name) {
            return true;
        }
    }
    false
}
