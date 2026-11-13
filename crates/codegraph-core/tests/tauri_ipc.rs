//! Tauri IPC: a frontend's `invoke('cmd')` onto the Rust `#[tauri::command]` that answers it,
//! tauri-specta's typed client, and events across that seam (upstream 2008e2f3; port of
//! tauri-ipc.test.ts's indexed cases). Each fixture is indexed for real.
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::{Edge, EdgeKind, Node};
use std::fs;
use std::path::Path;

fn write(dir: &Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn index(files: &[(&str, &str)]) -> (CodeGraph, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        write(dir.path(), rel, content);
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (cg, dir)
}

fn meta(e: &Edge, key: &str) -> Option<String> {
    e.metadata.as_ref().and_then(|m| m.get(key)).and_then(|v| v.as_str()).map(String::from)
}

struct Link {
    from: Node,
    to: Node,
    edge: Edge,
}

/// Everything the pass drew.
fn links(cg: &CodeGraph, by: &str) -> Vec<Link> {
    let mut out = Vec::new();
    for file in cg.get_files().unwrap() {
        for node in cg.get_nodes_by_file(&file.path).unwrap() {
            for edge in cg.get_outgoing_edges(&node.id).unwrap() {
                if meta(&edge, "synthesizedBy").as_deref() == Some(by) {
                    let to = cg.get_node(&edge.target).unwrap().unwrap();
                    out.push(Link { from: node.clone(), to, edge });
                }
            }
        }
    }
    out
}

fn names(rows: &[Link]) -> Vec<String> {
    let mut v: Vec<String> = rows.iter().map(|r| format!("{} > {}", r.from.name, r.to.name)).collect();
    v.sort();
    v
}

fn commands(cg: &CodeGraph) -> Vec<String> {
    names(&links(cg, "tauri-command"))
}

fn events(cg: &CodeGraph) -> Vec<String> {
    names(&links(cg, "tauri-event"))
}

fn of_kind(rows: Vec<Link>, kind: EdgeKind) -> Vec<Link> {
    rows.into_iter().filter(|r| r.edge.kind == kind).collect()
}

const CORE: &str = "#[tauri::command]
fn greet() {}
#[tauri::command]
fn unregistered() {}
fn not_command() {}
fn run() { tauri::Builder::default().invoke_handler(tauri::generate_handler![greet, not_command]).run(); }
";

fn lines(parts: &[&str]) -> String {
    parts.join("\n")
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[test]
fn links_direct_aliased_and_namespace_imports_to_the_registered_command() {
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", CORE),
        ("src/a.ts", "import { invoke } from \"@tauri-apps/api/core\";\nexport async function click() { return invoke<string>(\"greet\"); }\n"),
        ("src/b.ts", "import { invoke as callRust } from \"@tauri-apps/api/tauri\";\nexport function otherClick() { return callRust(\"greet\"); }\n"),
        ("src/c.ts", "import * as core from \"@tauri-apps/api/core\";\nexport function namespaceClick() { return core.invoke(\"greet\"); }\n"),
    ]);
    let rows = links(&cg, "tauri-command");
    assert_eq!(names(&rows), ["click > greet", "namespaceClick > greet", "otherClick > greet"]);
    for r in &rows {
        assert_eq!(r.edge.kind, EdgeKind::Calls);
        assert_eq!(r.edge.line, Some(2));
        assert_eq!(meta(&r.edge, "registeredAt").as_deref(), Some("src-tauri/src/lib.rs:6"));
        assert_eq!(meta(&r.edge, "command").as_deref(), Some("greet"));
        assert_eq!(meta(&r.edge, "tier").as_deref(), Some("client→server"));
    }
    cg.close();
}

#[test]
fn reads_an_api_imported_where_it_is_used() {
    let rust = format!("{CORE}\nuse tauri::Listener;\nfn wire(app: tauri::AppHandle) {{ app.listen(\"ready\", |_e| {{}}); }}");
    let front = lines(&[
        "export async function lazy() {",
        "  const { invoke } = await import('@tauri-apps/api/core');",
        "  return invoke('greet');",
        "}",
        "export async function tell() {",
        "  const { emit, emitTo: toWindow } = await import('@tauri-apps/api/event');",
        "  await emit('ready');",
        "  await toWindow('main', 'other', 1);",
        "}",
    ]);
    let (cg, _d) = index(&[("src-tauri/src/lib.rs", &rust), ("src/a.ts", &front)]);
    assert_eq!(commands(&cg), ["lazy > greet"]);
    assert_eq!(events(&cg), ["tell > wire"]);
    cg.close();
}

#[test]
fn reads_the_v1_api_root_and_its_tauri_namespace() {
    let (cg, _d) = index(&[
        ("src-tauri/src/main.rs", CORE),
        ("src/a.jsx", "import { invoke } from '@tauri-apps/api';\nexport function Rooted() { invoke('greet'); return null; }\n"),
        ("src/b.js", "import { tauri } from '@tauri-apps/api';\nexport function holder() { return tauri.invoke('greet'); }\n"),
    ]);
    assert_eq!(commands(&cg), ["Rooted > greet", "holder > greet"]);
    cg.close();
}

#[test]
fn reads_the_global_api_of_an_app_built_with_no_bundler() {
    let front = lines(&[
        "const { invoke } = window.__TAURI__.core;",
        "function viaDestructure() { return invoke(\"greet\"); }",
        "function viaGlobal() { return window.__TAURI__.core.invoke(\"greet\"); }",
        "function viaV1Global() { return window.__TAURI__.invoke(\"greet\"); }",
    ]);
    let (cg, _d) = index(&[("src-tauri/src/lib.rs", CORE), ("src/main.js", &front)]);
    assert_eq!(commands(&cg), ["viaDestructure > greet", "viaGlobal > greet", "viaV1Global > greet"]);
    cg.close();
}

#[test]
fn links_nothing_a_call_cannot_prove() {
    let front = lines(&[
        "import { invoke } from \"@tauri-apps/api/core\";",
        "function parameter(invoke: (key: string) => unknown) { return invoke(\"greet\"); }",
        "function destructured(other: any) { const { invoke } = other; return invoke(\"greet\"); }",
        "function arrowParam() { return [1].map((invoke: any) => invoke(\"greet\")); }",
        "function dynamic(key: string) { return invoke(key); }",
        "function templated(id: string) { return invoke(`greet_${id}`); }",
        "function concatenated(id: string) { return invoke(\"greet\" + id); }",
        "function absent() { invoke(\"unregistered\"); invoke(\"not_command\"); invoke(\"plugin:store|get\"); }",
        "function literal() { return \"invoke(\\\"greet\\\")\"; }",
        "// invoke(\"greet\")",
        "/* invoke(\"greet\") */",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", CORE),
        ("src/a.ts", &front),
        ("src/wrapped.ts", "import { invoke } from \"./wrapper\";\nexport function wrapperClick() { return invoke(\"greet\"); }\n"),
        ("src/other.ts", "function invoke(key: string) { return key; }\nexport function localClick() { return invoke(\"greet\"); }\n"),
    ]);
    assert_eq!(commands(&cg), Vec::<String>::new());
    cg.close();
}

#[test]
fn keeps_a_namespace_or_destructured_parameter_that_hides_the_import_unlinked() {
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", CORE),
        ("src/a.ts", "import * as core from \"@tauri-apps/api/core\";\nfunction shadow(core: any) { return core.invoke(\"greet\"); }\n"),
        ("src/b.ts", "import { invoke } from \"@tauri-apps/api/core\";\nfunction shadow({ invoke }: { invoke: any }) { return invoke(\"greet\"); }\n"),
    ]);
    assert_eq!(commands(&cg), Vec::<String>::new());
    cg.close();
}

#[test]
fn does_not_take_a_comment_a_string_or_an_unrelated_macro_for_a_registration() {
    let rust = lines(&[
        "#[tauri::command]",
        "fn greet() {}",
        "fn run() {",
        "  let text = \"tauri::generate_handler![greet]\";",
        "  // tauri::generate_handler![greet]",
        "  /* tauri::generate_handler![greet] */",
        "  other_handler![greet];",
        "}",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", &rust),
        ("src/a.ts", "import { invoke } from \"@tauri-apps/api/core\";\nexport function click() { return invoke(\"greet\"); }\n"),
    ]);
    assert_eq!(commands(&cg), Vec::<String>::new());
    cg.close();
}

#[test]
fn reads_a_list_built_apart_from_the_builder_and_tauri_specta_collectors() {
    let lib = lines(&[
        "mod cmd;",
        "fn handlers() -> impl Fn(tauri::ipc::Invoke) -> bool { tauri::generate_handler![cmd::listed] }",
        "pub fn builder() -> tauri_specta::Builder<tauri::Wry> {",
        "  tauri_specta::Builder::new().commands(tauri_specta::collect_commands![",
        "    cmd::typed,",
        "    #[cfg(target_os = \"macos\")]",
        "    cmd::conditional,",
        "    cmd::generic::<tauri::Wry>,",
        "  ])",
        "}",
    ]);
    let system = lines(&[
        "#[tauri::command] pub fn listed() {}",
        "#[tauri::command] #[specta::specta] pub fn typed() {}",
        "#[tauri::command] pub fn conditional() {}",
        "#[tauri::command] pub fn generic<R: tauri::Runtime>() {}",
    ]);
    let front = lines(&[
        "import { invoke } from \"@tauri-apps/api/core\";",
        "export function a() { return invoke(\"listed\"); }",
        "export function b() { return invoke(\"typed\"); }",
        "export function c() { return invoke(\"conditional\"); }",
        "export function d() { return invoke(\"generic\"); }",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", &lib),
        ("src-tauri/src/cmd/mod.rs", "mod system;\npub use system::*;\n"),
        ("src-tauri/src/cmd/system.rs", &system),
        ("src/a.ts", &front),
    ]);
    assert_eq!(commands(&cg), ["a > listed", "b > typed", "c > conditional", "d > generic"]);
    cg.close();
}

const SPECTA_LIB: &str = "#[tauri::command] #[specta::specta] fn get_mcp_port() -> u16 { 0 }
fn run() { tauri_specta::Builder::new().commands(tauri_specta::collect_commands![get_mcp_port]); }";

#[test]
fn walks_through_a_generated_tauri_specta_client_to_the_command() {
    let bindings = lines(&[
        "// This file was generated by [tauri-specta](https://github.com/oscartbeaumont/tauri-specta). Do not edit this file manually.",
        "export const commands = {",
        "  async getMcpPort(): Promise<number> {",
        "    return await TAURI_INVOKE(\"get_mcp_port\");",
        "  },",
        "  getMcpPortArrow: () => __TAURI_INVOKE<{ a: string; b: Array<number> }>(\"get_mcp_port\"),",
        "};",
        "import { invoke as TAURI_INVOKE, invoke as __TAURI_INVOKE } from \"@tauri-apps/api/core\";",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", SPECTA_LIB),
        ("src/bindings.ts", &bindings),
        ("src/settings.ts", "import { commands } from \"./bindings\";\nexport async function loadPort() { return commands.getMcpPort(); }\n"),
    ]);
    assert_eq!(commands(&cg), ["getMcpPort > get_mcp_port", "getMcpPortArrow > get_mcp_port"]);
    let port = cg.get_nodes_by_name("getMcpPort").unwrap().into_iter().next().unwrap();
    let callers: Vec<String> = cg.get_callers(&port.id, 1).unwrap().into_iter().map(|c| c.node.name).collect();
    assert!(callers.contains(&"loadPort".to_string()), "{callers:?}");
    cg.close();
}

#[test]
fn reads_commands_foo_bar_by_name_when_the_bindings_are_not_in_the_index() {
    let front = lines(&[
        "import { commands } from \"./bindings\";",
        "import { commands as other } from \"some-package\";",
        "export async function loadPort() { return commands.getMcpPort(); }",
        "export function notACommand() { return commands.somethingElse(); }",
        "export function notBindings() { return other.getMcpPort(); }",
    ]);
    let (cg, _d) = index(&[("src-tauri/src/lib.rs", SPECTA_LIB), ("src/settings.ts", &front)]);
    assert_eq!(commands(&cg), ["loadPort > get_mcp_port"]);
    cg.close();
}

#[test]
fn answers_from_what_the_app_declares_when_a_macro_builds_its_handler_list() {
    let ipc = lines(&[
        "macro_rules! build_invoke_handler {",
        "  ($($($typed:ident)::+,)*) => { tauri::generate_handler![ $($($typed)::+,)* ] };",
        "}",
        "#[tauri::command] pub fn from_manifest() {}",
    ]);
    let front = lines(&[
        "import { invoke } from \"@tauri-apps/api/core\";",
        "export function click() { return invoke(\"from_manifest\"); }",
        "export function other() { return invoke(\"from_another_file\"); }",
    ]);
    let (cg, _d) = index(&[
        ("apps/desktop/src-tauri/src/ipc.rs", &ipc),
        ("apps/desktop/src-tauri/src/commands/other.rs", "#[tauri::command]\npub fn from_another_file() {}\n"),
        ("apps/desktop/src/a.ts", &front),
    ]);
    let rows = links(&cg, "tauri-command");
    assert_eq!(names(&rows), ["click > from_manifest", "other > from_another_file"]);
    assert!(rows.iter().all(|r| meta(&r.edge, "registeredAt").as_deref() == Some("apps/desktop/src-tauri/src/ipc.rs:2")));
    cg.close();
}

#[test]
fn accepts_a_command_an_attribute_macro_makes_and_no_plain_namesake_of_an_outside_entry() {
    let lib = lines(&[
        "use tauri::command;",
        "use tauri_plugin_x::commands::version;",
        "#[command]",
        "async fn short_form() {}",
        "#[app_api(try_from = Thing)]",
        "#[tracing::instrument]",
        "pub fn macro_made() {}",
        "fn version() {}",
        "fn run() { tauri::Builder::default().invoke_handler(tauri::generate_handler![short_form, api::tauri_macro_made::macro_made, tauri_plugin_x::commands::version]); }",
    ]);
    let front = lines(&[
        "import { invoke } from \"@tauri-apps/api/core\";",
        "export function a() { return invoke(\"short_form\"); }",
        "export function b() { return invoke(\"macro_made\"); }",
        "export function c() { return invoke(\"version\"); }",
    ]);
    let (cg, _d) = index(&[("src-tauri/src/lib.rs", &lib), ("src/a.ts", &front)]);
    assert_eq!(commands(&cg), ["a > short_form", "b > macro_made"]);
    cg.close();
}

#[test]
fn keeps_two_apps_of_one_repository_apart_and_a_key_two_handlers_claim_unlinked() {
    let click = |name: &str| format!("import {{ invoke }} from \"@tauri-apps/api/core\";\nexport function {name}() {{ return invoke(\"greet\"); }}\n");
    let c_lib = lines(&["mod one; mod two;", "fn run() { tauri::Builder::default().invoke_handler(tauri::generate_handler![one::greet, two::greet]); }"]);
    let (root, app, unreg, tied) = (click("rootClick"), click("appClick"), click("unregisteredAppClick"), click("tiedClick"));
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", CORE),
        ("src/root.ts", &root),
        ("apps/a/src-tauri/src/lib.rs", CORE),
        ("apps/a/src/click.ts", &app),
        ("apps/b/src-tauri/src/lib.rs", "#[tauri::command]\nfn greet() {}\n"),
        ("apps/b/src/click.ts", &unreg),
        ("apps/c/src-tauri/src/lib.rs", &c_lib),
        ("apps/c/src-tauri/src/one.rs", "#[tauri::command]\npub fn greet() {}\n"),
        ("apps/c/src-tauri/src/two.rs", "#[tauri::command]\npub fn greet() {}\n"),
        ("apps/c/src/click.ts", &tied),
    ]);
    let rows = links(&cg, "tauri-command");
    assert_eq!(names(&rows), ["appClick > greet", "rootClick > greet"]);
    for r in &rows {
        let want = if r.from.name == "appClick" { "apps/a/src-tauri/src/lib.rs" } else { "src-tauri/src/lib.rs" };
        assert_eq!(r.to.file_path, want);
    }
    cg.close();
}

#[test]
fn finds_a_crate_and_frontend_that_sit_apart() {
    let (cg, _d) = index(&[
        ("crates/app-tauri/Cargo.toml", "[package]\nname = \"app-tauri\"\nversion = \"0.1.0\"\n"),
        ("crates/app-tauri/src/main.rs", "fn main() { tauri::Builder::default().invoke_handler(tauri::generate_handler![api::save]); }\n"),
        ("crates/api/Cargo.toml", "[package]\nname = \"api\"\nversion = \"0.1.0\"\n"),
        ("crates/api/src/lib.rs", "#[tauri::command]\npub fn save() {}\n"),
        ("apps/desktop/src/a.ts", "import { invoke } from \"@tauri-apps/api/core\";\nexport function click() { return invoke(\"save\"); }\n"),
    ]);
    assert_eq!(commands(&cg), ["click > save"]);
    cg.close();
}

#[test]
fn routes_a_plugin_command_to_the_plugin_that_registers_it() {
    let plugin = lines(&[
        "use tauri::{generate_handler, plugin::{Builder, TauriPlugin}, Runtime};",
        "#[tauri::command] pub fn list() {}",
        "pub fn init<R: Runtime>() -> TauriPlugin<R> { Builder::new(\"fonts\").invoke_handler(generate_handler![list]).build() }",
    ]);
    let front = lines(&[
        "import { invoke } from \"@tauri-apps/api/core\";",
        "export function pluginList() { return invoke(\"plugin:fonts|list\"); }",
        "export function appList() { return invoke(\"list\"); }",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/main.rs", "fn main() { tauri::Builder::default().invoke_handler(tauri::generate_handler![list]); }\n#[tauri::command] fn list() {}\n"),
        ("plugins/fonts/Cargo.toml", "[package]\nname = \"fonts\"\nversion = \"0.1.0\"\n"),
        ("plugins/fonts/src/lib.rs", &plugin),
        ("src/a.ts", &front),
    ]);
    let rows = links(&cg, "tauri-command");
    assert_eq!(names(&rows), ["appList > list", "pluginList > list"]);
    assert_eq!(rows.iter().find(|r| r.from.name == "pluginList").unwrap().to.file_path, "plugins/fonts/src/lib.rs");
    assert_eq!(rows.iter().find(|r| r.from.name == "appList").unwrap().to.file_path, "src-tauri/src/main.rs");
    cg.close();
}

#[test]
fn reads_a_plugin_named_through_a_constant_and_tauri_spectas_builder() {
    let plugin = lines(&[
        "const PLUGIN_NAME: &str = \"my-plugin\";",
        "#[tauri::command] #[specta::specta] fn add_numbers() {}",
        "fn builder<R: tauri::Runtime>() -> tauri_specta::Builder<R> {",
        "  tauri_specta::Builder::new().plugin_name(PLUGIN_NAME).commands(tauri_specta::collect_commands![add_numbers::<tauri::Wry>])",
        "}",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/main.rs", "fn main() { tauri::Builder::default().plugin(my_plugin::init()); }\n"),
        ("plugins/mine/Cargo.toml", "[package]\nname = \"my_plugin\"\nversion = \"0.1.0\"\n"),
        ("plugins/mine/src/lib.rs", &plugin),
        ("plugins/mine/guest-js/index.ts", "import { invoke } from \"@tauri-apps/api/core\";\nexport function add() { return invoke(\"plugin:my-plugin|add_numbers\"); }"),
    ]);
    assert_eq!(commands(&cg), ["add > add_numbers"]);
    cg.close();
}

#[test]
fn reads_svelte_and_vue_components_in_their_scripts() {
    let page = lines(&[
        "<script lang=\"ts\">",
        "  import { invoke } from \"@tauri-apps/api/core\";",
        "  async function save() { await invoke(\"greet\"); }",
        "</script>",
        "<button on:click={save}>Save</button>",
    ]);
    let top = lines(&["<script>", "  import { invoke } from \"@tauri-apps/api/core\";", "  invoke(\"greet\");", "</script>"]);
    let panel = lines(&[
        "<template><button @click=\"load\">Load</button></template>",
        "<script setup lang=\"ts\">",
        "import { invoke } from \"@tauri-apps/api/core\";",
        "async function load() { return invoke(\"greet\"); }",
        "</script>",
    ]);
    let (cg, _d) = index(&[("src-tauri/src/main.rs", CORE), ("src/Page.svelte", &page), ("src/Top.svelte", &top), ("src/Panel.vue", &panel)]);
    let rows = links(&cg, "tauri-command");
    assert_eq!(names(&rows), ["Top > greet", "load > greet", "save > greet"]);
    assert_eq!(rows.iter().find(|r| r.from.name == "save").unwrap().edge.line, Some(3));
    assert_eq!(rows.iter().find(|r| r.from.name == "load").unwrap().edge.line, Some(4));
    cg.close();
}

#[test]
fn attributes_a_call_to_the_function_it_is_in_when_two_share_a_line() {
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", CORE),
        ("src/a.ts", "import { invoke } from \"@tauri-apps/api/core\";\nfunction first() { return invoke(\"greet\"); } function second() { return 0; }\n"),
    ]);
    assert_eq!(commands(&cg), ["first > greet"]);
    cg.close();
}

#[test]
fn links_a_command_written_for_each_platform_to_every_one_of_them() {
    let main = lines(&["mod ocr;", "mod stubs;", "fn main() { tauri::Builder::default().invoke_handler(tauri::generate_handler![ocr::read_text, stubs::only_stub]); }"]);
    let ocr = lines(&[
        "#[tauri::command(async)]",
        "#[cfg(target_os = \"windows\")]",
        "pub fn read_text() {}",
        "#[tauri::command(async)]",
        "#[cfg(target_os = \"macos\")]",
        "pub fn read_text() {}",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/main.rs", &main),
        ("src-tauri/src/ocr.rs", &ocr),
        ("src-tauri/src/stubs.rs", "#[tauri::command]\npub fn only_stub() {}\n"),
        ("src/a.ts", "import { invoke } from \"@tauri-apps/api/core\";\nexport function read() { return invoke(\"read_text\"); }"),
    ]);
    let rows = links(&cg, "tauri-command");
    assert_eq!(names(&rows), ["read > read_text", "read > read_text"]);
    let mut starts: Vec<u32> = rows.iter().map(|r| r.to.start_line).collect();
    starts.sort();
    assert_eq!(starts, [3, 6]);
    cg.close();
}

#[test]
fn follows_the_projects_own_functions_around_invoke_and_a_field_set_to_one() {
    let ipc = lines(&[
        "import { invoke as tauriInvoke } from \"@tauri-apps/api/core\";",
        "export async function safeInvoke<T>(command: string, args?: unknown): Promise<T> {",
        "  return await tauriInvoke<T>(command, args as never);",
        "}",
        "export const logged = (command: string) => { console.log(command); return safeInvoke(command); };",
        "export function notAWrapper(command: string, other: string) { return tauriInvoke(other); }",
        "export class Backend { invoke = safeInvoke; }",
    ]);
    let c = lines(&[
        "export class Service {",
        "  constructor(private backend: { invoke(c: string): unknown }) {}",
        "  load() { return this.backend.invoke(\"greet\"); }",
        "}",
    ]);
    let settings = lines(&[
        "export class Settings {",
        "  constructor(private backend: { invoke(c: string): unknown }) {}",
        "  async update() { await this.invokeAndRefresh(\"greet\"); }",
        "  private async invokeAndRefresh(command: string) { await this.backend.invoke(command); }",
        "}",
    ]);
    let api = lines(&[
        "import { invoke } from \"@tauri-apps/api/core\";",
        "export function plain(cmd: string) { return invoke(cmd); }",
        "export class Api { call(cmd: string) { return invoke(cmd); } }",
        "export function noParameter() { const cmd = \"greet\"; return invoke(cmd); }",
    ]);
    let e = lines(&[
        "import { plain, Api } from \"./api\";",
        "export function viaPlain() { return plain(\"greet\"); }",
        "export function viaMethod(api: Api) { return api.call(\"greet\"); }",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", CORE),
        ("src/ipc.ts", &ipc),
        ("src/a.ts", "import { safeInvoke } from \"./ipc\";\nexport function click() { return safeInvoke(\"greet\"); }\n"),
        ("src/b.ts", "import { logged as log } from \"./ipc\";\nexport function twice() { return log(\"greet\"); }\n"),
        ("src/c.ts", &c),
        ("src/d.ts", "import { notAWrapper } from \"./ipc\";\nexport function notLinked() { return notAWrapper(\"greet\", \"x\"); }\n"),
        ("src/settings.ts", &settings),
        ("src/api.ts", &api),
        ("src/e.ts", &e),
    ]);
    assert_eq!(
        commands(&cg),
        ["click > greet", "load > greet", "noParameter > greet", "twice > greet", "update > greet", "viaMethod > greet", "viaPlain > greet"]
    );
    cg.close();
}

#[test]
fn leaves_test_code_out_where_the_backend_is_mocked() {
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", CORE),
        ("src/a.test.ts", "import { invoke } from \"@tauri-apps/api/core\";\nexport function testClick() { return invoke(\"greet\"); }\n"),
    ]);
    assert_eq!(commands(&cg), Vec::<String>::new());
    cg.close();
}

#[test]
fn follows_a_registration_and_a_call_through_sync_as_a_fresh_index_would() {
    let click = "import { invoke } from \"@tauri-apps/api/core\";\nexport function click() { return invoke(\"greet\"); }\n";
    let (mut cg, dir) = index(&[("src-tauri/src/lib.rs", CORE), ("src/a.ts", click)]);
    assert_eq!(commands(&cg), ["click > greet"]);
    let sync = |cg: &mut CodeGraph| {
        cg.sync(&IndexOptions::default(), None).unwrap();
    };
    write(dir.path(), "src-tauri/src/lib.rs", &CORE.replace("generate_handler![greet, not_command]", "generate_handler![not_command]"));
    sync(&mut cg);
    assert_eq!(commands(&cg), Vec::<String>::new());
    write(dir.path(), "src-tauri/src/lib.rs", CORE);
    sync(&mut cg);
    assert_eq!(commands(&cg), ["click > greet"]);
    write(dir.path(), "src/a.ts", "export function click() { return 0; }\n");
    sync(&mut cg);
    assert_eq!(commands(&cg), Vec::<String>::new());
    write(dir.path(), "src/late.ts", "import { invoke } from \"@tauri-apps/api/core\";\nexport function late() { return invoke(\"greet\"); }\n");
    sync(&mut cg);
    assert_eq!(commands(&cg), ["late > greet"]);
    cg.close();
}

#[test]
fn marks_what_a_sync_must_read_again() {
    use codegraph_resolve::callback_synthesizer::has_synthesis_pattern;
    assert!(has_synthesis_pattern("src/a.ts", "import { invoke as callRust } from \"@tauri-apps/api/core\"; callRust(\"greet\");"));
    assert!(has_synthesis_pattern("src/a.js", "const x = window.__TAURI__;"));
    assert!(has_synthesis_pattern("src-tauri/src/lib.rs", "#[tauri::command] fn greet() {}"));
    assert!(has_synthesis_pattern("src-tauri/src/lib.rs", "fn run() { b.invoke_handler(generate_handler![greet]); }"));
}

#[test]
fn draws_nothing_in_a_rust_and_typescript_project_that_is_not_a_tauri_app() {
    let (cg, _d) = index(&[
        ("native/src/lib.rs", "pub fn greet() {}\n#[derive(Clone)]\npub struct Saved;\n"),
        ("src/a.ts", "import { invoke } from \"./rpc\";\nexport function click() { return invoke(\"greet\"); }\n"),
        ("src/rpc.ts", "export function invoke(name: string) { return name; }\n"),
    ]);
    assert_eq!(commands(&cg), Vec::<String>::new());
    assert_eq!(events(&cg), Vec::<String>::new());
    cg.close();
}

// ── Events ────────────────────────────────────────────────────────────────────

#[test]
fn links_a_raw_emit_to_a_named_or_inline_listener_across_languages() {
    let main = lines(&[
        "use tauri::Emitter;",
        "const CHANGED: &str = \"theme-changed\";",
        "fn notify(window: tauri::Window) { window.emit(\"new_text\", \"x\").unwrap(); }",
        "fn broadcast(app: tauri::AppHandle) { app.emit_all(\"new_text\", ()).unwrap(); }",
        "fn targeted(app: tauri::AppHandle) { app.emit_to(\"main\", \"new_text\", ()).unwrap(); }",
        "fn constant(app: tauri::AppHandle) { app.emit(CHANGED, ()).unwrap(); }",
    ]);
    let front = lines(&[
        "import { listen, once } from '@tauri-apps/api/event';",
        "function onText(e: unknown) { return e; }",
        "export function named() { listen('new_text', onText); }",
        "export function inline() { void once('theme-changed', (e) => console.log(e)); }",
        "export function unknown(name: string) { listen(name, onText); }",
    ]);
    let (cg, _d) = index(&[("src-tauri/src/main.rs", &main), ("src/a.ts", &front)]);
    let rows = links(&cg, "tauri-event");
    assert_eq!(names(&rows), ["broadcast > onText", "constant > inline", "notify > onText", "targeted > onText"]);
    for r in &rows {
        assert_eq!(r.edge.kind, EdgeKind::Calls);
        assert_eq!(meta(&r.edge, "tier").as_deref(), Some("server→client"));
        assert_eq!(meta(&r.edge, "channel").as_deref(), Some("event"));
    }
    let notify = rows.iter().find(|r| r.from.name == "notify").unwrap();
    assert_eq!(meta(&notify.edge, "event").as_deref(), Some("new_text"));
    assert_eq!(meta(&notify.edge, "registeredAt").as_deref(), Some("src/a.ts:3"));
    assert_eq!(notify.edge.line, Some(3));
    cg.close();
}

#[test]
fn reads_a_listener_on_a_window_handle_and_leaves_built_in_events_alone() {
    let front = lines(&[
        "import { getCurrentWindow } from '@tauri-apps/api/window';",
        "export function viaHandle() { getCurrentWindow().listen('ready', () => {}); }",
        "export function builtIn() { getCurrentWindow().listen('tauri://resize', () => {}); }",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/main.rs", "use tauri::Emitter;\nfn notify(window: tauri::Window) { window.emit(\"ready\", ()).unwrap(); }\n"),
        ("src/a.ts", &front),
    ]);
    assert_eq!(events(&cg), ["notify > viaHandle"]);
    cg.close();
}

#[test]
fn links_a_frontend_emit_to_a_rust_listener_and_to_another_window() {
    let main = lines(&[
        "use tauri::Listener;",
        "fn wire(app: tauri::AppHandle) { app.listen(\"write-settled\", |_e| {}); }",
        "fn other(app: tauri::AppHandle) { app.listen_any(\"write-settled\", |_e| {}); }",
    ]);
    let front = lines(&[
        "import { emit, listen } from '@tauri-apps/api/event';",
        "export async function settle() { await emit('write-settled', { ok: true }); }",
        "export function otherWindow() { listen('write-settled', () => {}); }",
    ]);
    let (cg, _d) = index(&[("src-tauri/src/main.rs", &main), ("src/a.ts", &front)]);
    let rows = links(&cg, "tauri-event");
    assert_eq!(names(&rows), ["settle > other", "settle > otherWindow", "settle > wire"]);
    assert_eq!(meta(&rows.iter().find(|r| r.to.name == "wire").unwrap().edge, "tier").as_deref(), Some("client→server"));
    assert_eq!(meta(&rows.iter().find(|r| r.to.name == "otherWindow").unwrap().edge, "tier"), None);
    cg.close();
}

const TYPED: &str = "use serde::Serialize;
use tauri::Manager;
use tauri_specta::Event;
#[derive(Clone, Serialize, specta::Type, Event)]
pub struct FileSaved { pub path: String }
#[derive(Clone, Serialize, specta::Type, Event)]
#[tauri_specta(event_name = \"low-disk\")]
pub struct LowDiskPayload { pub path: String }
#[derive(Clone, Serialize, specta::Type, tauri_specta::Event)]
pub struct AIModelReady;
#[derive(Clone, Serialize)]
pub struct NotAnEvent;
fn literal(app: &tauri::AppHandle) { let _ = FileSaved { path: String::new() }.emit(app); }
fn wrapped(app: &tauri::AppHandle) { if let Err(e) = (LowDiskPayload { path: String::new() }).emit(app) { drop(e); } }
fn unit(app: &tauri::AppHandle) { let _ = AIModelReady.emit(app); }
fn local(app: &tauri::AppHandle) {
  let event = FileSaved { path: String::new() };
  let _ = event.emit(app);
}
fn from_param(app: &tauri::AppHandle, payload: &LowDiskPayload) { let _ = payload.emit(app); }
fn chained(app: &tauri::AppHandle) {
  let _ = FileSaved {
    path: String::new(),
  }
  .emit(app);
}
fn targeted(app: &tauri::AppHandle) { let _ = FileSaved { path: String::new() }.emit_to(app, \"main\"); }
fn not_one(app: &tauri::AppHandle) { let _ = NotAnEvent.emit(app); }
fn run() { tauri_specta::Builder::<tauri::Wry>::new().events(tauri_specta::collect_events![FileSaved, LowDiskPayload, AIModelReady]); }";

#[test]
fn links_tauri_specta_events_through_their_structs() {
    let front = lines(&[
        "import { events } from '#lib/ipc/bindings.js';",
        "export function onSaved() { return events.fileSaved.listen((e) => e); }",
        "export function onDisk() { return events.lowDiskPayload(appWindow).once((e) => e); }",
        "export function onModel() { return events.aiModelReady.listen((e) => e); }",
        "export function notAnEvent() { return events.notAnEvent.listen((e) => e); }",
    ]);
    let (cg, _d) = index(&[("src-tauri/src/lib.rs", TYPED), ("src/a.ts", &front)]);
    let rows = links(&cg, "tauri-event");
    let typed = rows.iter().find(|r| r.to.name == "LowDiskPayload").map(|r| (meta(&r.edge, "event"), meta(&r.edge, "registeredAt")));
    assert_eq!(names(&of_kind(links(&cg, "tauri-event"), EdgeKind::References)), ["onDisk > LowDiskPayload", "onModel > AIModelReady", "onSaved > FileSaved"]);
    assert_eq!(
        names(&of_kind(rows, EdgeKind::Calls)),
        ["chained > onSaved", "from_param > onDisk", "literal > onSaved", "local > onSaved", "targeted > onSaved", "unit > onModel", "wrapped > onDisk"]
    );
    assert_eq!(typed, Some((Some("low-disk".into()), Some("src-tauri/src/lib.rs:8".into()))));
    cg.close();
}

#[test]
fn meets_a_raw_listener_on_the_wire_name_of_a_typed_event() {
    let front = lines(&[
        "import { listen } from '@tauri-apps/api/event';",
        "export function rawSaved() { return listen('file-saved', () => {}); }",
        "export function rawDisk() { return listen('low-disk', () => {}); }",
    ]);
    let (cg, _d) = index(&[("src-tauri/src/lib.rs", TYPED), ("src/a.ts", &front)]);
    assert_eq!(names(&of_kind(links(&cg, "tauri-event"), EdgeKind::References)), ["rawDisk > LowDiskPayload", "rawSaved > FileSaved"]);
    assert_eq!(
        names(&of_kind(links(&cg, "tauri-event"), EdgeKind::Calls)),
        ["chained > rawSaved", "from_param > rawDisk", "literal > rawSaved", "local > rawSaved", "targeted > rawSaved", "wrapped > rawDisk"]
    );
    cg.close();
}

#[test]
fn links_a_typed_emit_in_the_frontend_to_a_typed_listener_in_rust() {
    let lib = lines(&[
        "use tauri_specta::Event;",
        "#[derive(Clone, serde::Serialize, specta::Type, Event)]",
        "pub struct DemoEvent(String);",
        "fn wire(app: &tauri::AppHandle) { DemoEvent::listen(app, |event| { drop(event); }); }",
    ]);
    let (cg, _d) = index(&[
        ("src-tauri/src/lib.rs", &lib),
        ("src/a.ts", "import { events } from './bindings';\nexport function tell() { return events.demoEvent.emit('x'); }\n"),
    ]);
    let rows = links(&cg, "tauri-event");
    assert_eq!(names(&rows), ["tell > wire"]);
    assert_eq!(meta(&rows[0].edge, "tier").as_deref(), Some("client→server"));
    assert_eq!(meta(&rows[0].edge, "event").as_deref(), Some("demo-event"));
    cg.close();
}

#[test]
fn keeps_the_events_of_two_apps_apart() {
    let app = |name: &str| {
        lines(&[
            "use tauri_specta::Event;",
            "#[derive(Clone, serde::Serialize, specta::Type, Event)]",
            "pub struct Ready;",
            &format!("fn {name}_raw(app: tauri::AppHandle) {{ app.emit(\"tick\", ()).unwrap(); }}"),
            &format!("fn {name}_typed(app: &tauri::AppHandle) {{ let _ = Ready.emit(app); }}"),
        ])
    };
    let front = |name: &str| {
        lines(&[
            "import { listen } from '@tauri-apps/api/event';",
            "import { events } from './bindings';",
            &format!("export function {name}Tick() {{ return listen('tick', () => {{}}); }}"),
            &format!("export function {name}Ready() {{ return events.ready.listen(() => {{}}); }}"),
        ])
    };
    let (a, fa, b, fb) = (app("a"), front("a"), app("b"), front("b"));
    let (cg, _d) = index(&[("apps/a/src-tauri/src/main.rs", &a), ("apps/a/src/main.ts", &fa), ("apps/b/src-tauri/src/main.rs", &b), ("apps/b/src/main.ts", &fb)]);
    assert_eq!(names(&of_kind(links(&cg, "tauri-event"), EdgeKind::Calls)), ["a_raw > aTick", "a_typed > aReady", "b_raw > bTick", "b_typed > bReady"]);
    let mut refs: Vec<String> =
        of_kind(links(&cg, "tauri-event"), EdgeKind::References).iter().map(|r| format!("{} {}", r.from.name, r.to.file_path)).collect();
    refs.sort();
    assert_eq!(refs, ["aReady apps/a/src-tauri/src/main.rs", "bReady apps/b/src-tauri/src/main.rs"]);
    cg.close();
}

#[test]
fn skips_an_event_too_many_functions_emit_to_read_as_a_flow() {
    let mut main = vec!["use tauri::Emitter;".to_string()];
    main.extend((0..20).map(|i| format!("fn e{i}(a: tauri::AppHandle) {{ a.emit(\"busy\", ()).unwrap(); }}")));
    let (cg, _d) = index(&[
        ("src-tauri/src/main.rs", &main.join("\n")),
        ("src/a.ts", "import { listen } from '@tauri-apps/api/event';\nexport function on() { listen('busy', () => {}); }\n"),
    ]);
    assert_eq!(events(&cg), Vec::<String>::new());
    cg.close();
}
