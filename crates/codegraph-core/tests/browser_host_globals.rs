//! Upstream f1da755a (2907): calls on the browser's own objects link no project method, while a
//! name the file binds, imports or takes as a parameter still links. Ported from
//! `__tests__/browser-host-globals.test.ts`.
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::collections::BTreeSet;
use std::fs;

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, source) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn calls_from(cg: &CodeGraph, file: &str) -> BTreeSet<String> {
    cg.get_nodes_by_file(file)
        .unwrap()
        .into_iter()
        .flat_map(|n| {
            cg.get_outgoing_edges(&n.id)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == EdgeKind::Calls)
                .filter_map(move |e| {
                    let target = cg.get_node(&e.target).unwrap()?;
                    Some(format!("{} -> {}", n.qualified_name, target.qualified_name))
                })
        })
        .collect()
}

const SERVICES: &str = "export class WindowService {\n  open(url: string): void {}\n  close(): void {}\n  postMessage(m: unknown): void {}\n}\nexport class StorageService {\n  getItem(key: string): string | null { return null; }\n}\nexport class PageService {\n  reload(): void {}\n  back(): void {}\n}\n";

#[test]
fn calls_on_browser_host_objects_link_no_project_method() {
    let (_dir, cg) = index(&[
        ("src/services.ts", SERVICES),
        (
            "src/browser.ts",
            "export function openHelp(): void { window.open('/help'); }\nexport function readToken(): string | null { return localStorage.getItem('token'); }\nexport function refresh(): void { location.reload(); }\nexport function goBack(): void { history.back(); }\nexport function readTokenChained(): string | null { return window.localStorage.getItem('token'); }\nexport function goBackChained(): void { window.history.back(); }\nexport function closeSelf(): void { self.close(); }\nexport class Popup {\n  dismiss(): void { self.close(); }\n  close(): void {}\n}\n",
        ),
        ("src/worker.js", "self.postMessage('ready');\nexport function reply(x) { self.postMessage(x); }\n"),
    ]);
    assert_eq!(calls_from(&cg, "src/browser.ts"), BTreeSet::new());
    assert_eq!(calls_from(&cg, "src/worker.js"), BTreeSet::new());
    cg.close();
}

#[test]
fn a_name_bound_imported_or_a_parameter_still_links() {
    let (_dir, cg) = index(&[
        ("src/router.ts", "export class Router { forward(): void {} }\nexport function createRouter(): Router { return new Router(); }\n"),
        ("src/nav.ts", "export const history = { push(to: string): void {} };\n"),
        ("src/loc.ts", "export class Loc { reload(): void {} }\n"),
        (
            "src/uses.ts",
            "import { createRouter } from './router';\nimport { history } from './nav';\nimport { Loc } from './loc';\nexport function local(): void {\n  const navigator = createRouter();\n  navigator.forward();\n}\nexport function imported(): void { history.push('/x'); }\nexport function param(location: Loc): void { location.reload(); }\n",
        ),
    ]);
    let calls = calls_from(&cg, "src/uses.ts");
    for want in ["local -> Router::forward", "local -> createRouter", "imported -> history::push", "param -> Loc::reload"] {
        assert!(calls.contains(want), "missing {want} in {calls:?}");
    }
    cg.close();
}

#[test]
fn a_destructured_document_param_and_a_typeof_guard_still_link_correctly() {
    let (_dir, cg) = index(&[
        ("src/models/Model.ts", "export class Model {\n  save(): void {}\n}\n"),
        ("src/models/Document.ts", "import Model from './Model';\nexport default class Document extends Model {\n  save(): void {}\n  archive(): void {}\n}\n"),
        ("src/services.ts", SERVICES),
        (
            "src/DocumentPublish.tsx",
            "import type Document from './models/Document';\n\ntype Props = {\n  document: Document;\n};\n\nexport function DocumentPublish({ document }: Props) {\n  const publish = async () => {\n    await document.save();\n  };\n  return publish;\n}\n",
        ),
        (
            "src/help.ts",
            "export function openHelp(): void {\n  if (typeof window !== 'undefined') window.open('/help');\n}\nexport function goBack(nav: { canGoBack: boolean }): void {\n  if (nav.canGoBack && window.history.length > 1) history.back();\n}\n",
        ),
    ]);
    let publish = calls_from(&cg, "src/DocumentPublish.tsx");
    assert_eq!(publish, ["DocumentPublish::publish -> Document::save"].into_iter().map(String::from).collect());
    assert_eq!(calls_from(&cg, "src/help.ts"), BTreeSet::new());
    cg.close();
}

#[test]
fn this_a_bound_self_and_a_project_object_on_window_still_link() {
    let (_dir, cg) = index(&[
        (
            "src/dialog.js",
            "export class Dialog {\n  close() {}\n  open() { this.close(); }\n  later() { var self = this; setTimeout(function () { self.close(); }); }\n}\nexport const store = {\n  close() {},\n  open() { this.close(); },\n};\n",
        ),
        ("src/app.js", "window.App = {\n  init() {},\n};\nexport function boot() { window.App.init(); }\n"),
    ]);
    let dialog = calls_from(&cg, "src/dialog.js");
    for want in ["Dialog::later -> Dialog::close", "Dialog::open -> Dialog::close", "store::open -> store::close"] {
        assert!(dialog.contains(want), "missing {want} in {dialog:?}");
    }
    assert_eq!(calls_from(&cg, "src/app.js"), ["boot -> window.App::init"].into_iter().map(String::from).collect());
    cg.close();
}
