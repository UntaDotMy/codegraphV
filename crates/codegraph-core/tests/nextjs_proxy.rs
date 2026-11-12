//! Next.js proxy, end to end: each route its `config.matcher` accepts runs it first, so a route
//! links to what runs in it (upstream 446d08c2; port of nextjs-proxy.test.ts's indexed cases —
//! the routeRoots and Screens cases test viewer code the Rust tree doesn't have).
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_graph::named_symbol_flow::{resolve_named_symbol_flow, NamedSymbolFlowOptions};
use codegraph_types::{Edge, EdgeKind, Node, NodeKind, Provenance};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn write(dir: &Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn project(files: &[(&str, &str)]) -> (CodeGraph, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        write(dir.path(), rel, content);
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (cg, dir)
}

fn page(name: &str) -> String {
    format!("export default function {name}() {{\n  return null\n}}\n")
}

fn manifest(next: &str) -> String {
    format!(r#"{{"name":"web","dependencies":{{"next":"{next}","react":"19.0.0"}}}}"#)
}

fn meta_str(e: &Edge, key: &str) -> Option<String> {
    e.metadata.as_ref().and_then(|m| m.get(key)).and_then(|v| v.as_str()).map(String::from)
}

fn proxy_edges(cg: &CodeGraph) -> Vec<Edge> {
    cg.get_nodes_by_kind(NodeKind::Route)
        .unwrap()
        .into_iter()
        .flat_map(|r| cg.get_outgoing_edges(&r.id).unwrap())
        .filter(|e| meta_str(e, "synthesizedBy").as_deref() == Some("next-proxy"))
        .collect()
}

/// Route name → the matcher entry that linked it (`None` for "every request").
fn routes_running(cg: &CodeGraph, target: &Node) -> BTreeMap<String, Option<String>> {
    proxy_edges(cg).into_iter().filter(|e| e.target == target.id).map(|e| (cg.get_node(&e.source).unwrap().unwrap().name, meta_str(&e, "matcher"))).collect()
}

fn func(cg: &CodeGraph, name: &str, file: &str) -> Node {
    cg.get_nodes_by_name(name)
        .unwrap()
        .into_iter()
        .find(|n| n.file_path == file && n.kind == NodeKind::Function)
        .unwrap_or_else(|| panic!("no function {name} in {file}"))
}

fn source_names(cg: &CodeGraph) -> Vec<String> {
    let mut out: Vec<String> = proxy_edges(cg).iter().map(|e| cg.get_node(&e.source).unwrap().unwrap().name).collect();
    out.sort();
    out
}

fn flow(cg: &CodeGraph, query: &str) -> Vec<String> {
    let store = cg.store().as_ref();
    let flow = resolve_named_symbol_flow(store, store, query, &NamedSymbolFlowOptions::default(), None).unwrap();
    flow.chains.first().map(|c| c.steps.iter().map(|s| s.node.name.clone()).collect()).unwrap_or_default()
}

fn expect_running(got: BTreeMap<String, Option<String>>, want: &[(&str, Option<&str>)]) {
    let want: BTreeMap<String, Option<String>> = want.iter().map(|(k, v)| (k.to_string(), v.map(String::from))).collect();
    assert_eq!(got, want);
}

const PROXY_TS: &str = "import { NextResponse } from 'next/server'
import { readSession } from './lib/session'

export function proxy(request) {
  if (!readSession(request.cookies.get('session'))) return NextResponse.redirect(new URL('/login', request.url))
  return NextResponse.next()
}

export const config = {
  matcher: [
    '/dashboard/:rest*',
    { source: '/api/:rest+', missing: [{ type: 'header', key: 'x-internal' }] },
    '/docs',
  ],
}
";

#[test]
fn links_the_routes_the_matcher_accepts_under_src_and_only_from_the_proxy_next_loads() {
    let (cg, _dir) = project(&[
        ("package.json", &manifest("^16.2.1")),
        ("src/app/page.tsx", &page("Home")),
        ("src/app/login/page.tsx", &page("Login")),
        ("src/app/dashboard/page.tsx", &page("Dashboard")),
        ("src/app/dashboard/settings/page.tsx", &page("Settings")),
        ("src/app/blog/[slug]/page.tsx", &page("Post")),
        ("src/app/docs/[[...all]]/page.tsx", &page("Docs")),
        (
            "src/app/api/projects/[id]/route.ts",
            "export async function GET() {\n  return Response.json({})\n}\nexport async function POST() {\n  return Response.json({})\n}\n",
        ),
        ("src/lib/session.ts", "export function readSession(cookie) {\n  return cookie ?? null\n}\n"),
        ("src/proxy.ts", PROXY_TS),
        // Not where Next loads a proxy for an app under src/.
        ("proxy.ts", "export function proxy(request) {\n  return request\n}\n"),
        ("src/lib/proxy.ts", "export function proxy(target) {\n  return fetch(target)\n}\n"),
    ]);
    let proxy = func(&cg, "proxy", "src/proxy.ts");
    expect_running(
        routes_running(&cg, &proxy),
        &[
            ("/dashboard", Some("/dashboard/:rest*")),
            ("/dashboard/settings", Some("/dashboard/:rest*")),
            ("/docs/:all*", Some("/docs")),
            ("GET /api/projects/:id", Some("/api/:rest+")),
            ("POST /api/projects/:id", Some("/api/:rest+")),
        ],
    );
    assert!(proxy_edges(&cg).iter().all(|e| e.target == proxy.id));
    let edge = proxy_edges(&cg).into_iter().find(|e| cg.get_node(&e.source).unwrap().unwrap().name == "/dashboard").unwrap();
    assert_eq!(edge.kind, EdgeKind::Calls);
    assert_eq!(edge.provenance, Some(Provenance::Heuristic));
    let meta = edge.metadata.as_ref().unwrap();
    assert_eq!(meta.len(), 3, "{meta:?}");
    assert_eq!(meta_str(&edge, "matcher").as_deref(), Some("/dashboard/:rest*"));
    assert_eq!(meta_str(&edge, "registeredAt").as_deref(), Some("src/proxy.ts:11"));
    cg.close();
}

#[test]
fn the_routes_are_the_proxys_callers_its_impact_and_a_flow_reaches_what_it_calls() {
    let (cg, _dir) = project(&[
        ("package.json", &manifest("15.5.0")),
        ("app/dashboard/page.tsx", &page("Dashboard")),
        ("app/about/page.tsx", &page("About")),
        ("app/page.tsx", "import Link from 'next/link'\nexport default function Home() {\n  return <Link href=\"/dashboard\">Dashboard</Link>\n}\n"),
        ("lib/session.ts", "export function readSession(cookie) {\n  return cookie ?? null\n}\n"),
        (
            "middleware.ts",
            "import { readSession } from './lib/session'\nexport function middleware(request) {\n  return readSession(request)\n}\nexport const config = { matcher: '/dashboard/:rest*' }\n",
        ),
    ]);
    let middleware = func(&cg, "middleware", "middleware.ts");
    let callers: Vec<String> = cg.get_callers(&middleware.id, 1).unwrap().into_iter().map(|c| c.node.name).collect();
    assert_eq!(callers, ["/dashboard"]);
    let impact = cg.get_impact_radius(&func(&cg, "readSession", "lib/session.ts").id, 2).unwrap();
    assert!(impact.nodes.values().any(|n| n.name == "/dashboard"));
    assert_eq!(flow(&cg, "Home readSession"), ["Home", "/dashboard", "middleware", "readSession"]);
    cg.close();
}

#[test]
fn takes_the_default_export_and_a_proxy_ts_beside_middleware_is_an_ordinary_module() {
    let (cg, _dir) = project(&[
        ("package.json", &manifest("15.5.0")),
        ("app/page.tsx", &page("Home")),
        ("app/account/page.tsx", &page("Account")),
        ("app/account/[id]/page.tsx", &page("AccountDetail")),
        ("pages/api/health.ts", "export default function handler(req, res) {\n  res.status(200).end()\n}\n"),
        ("middleware.ts", "function guard(request) {\n  return undefined\n}\nexport default guard\nexport const config = { matcher: '/account/:id?' }\n"),
        ("proxy.ts", "export function proxy(request) {\n  return request\n}\n"),
    ]);
    expect_running(routes_running(&cg, &func(&cg, "guard", "middleware.ts")), &[("/account", Some("/account/:id?")), ("/account/:id", Some("/account/:id?"))]);
    assert_eq!(proxy_edges(&cg).len(), 2);
    assert_eq!(cg.get_impact_radius(&func(&cg, "proxy", "proxy.ts").id, 3).unwrap().nodes.len(), 1);
    cg.close();
}

#[test]
fn links_a_wrapped_default_export_through_the_file() {
    let (cg, _dir) = project(&[
        ("package.json", &manifest("15.5.0")),
        ("app/dashboard/page.tsx", &page("Dashboard")),
        ("app/page.tsx", "import Link from 'next/link'\nexport default function Home() {\n  return <Link href=\"/dashboard\">Dashboard</Link>\n}\n"),
        ("lib/session.ts", "export function isProtected(request) {\n  return !!request\n}\n"),
        (
            "middleware.ts",
            "import { clerkMiddleware } from '@clerk/nextjs/server'\nimport { isProtected } from './lib/session'\nexport default clerkMiddleware(async (auth, req) => {\n  if (isProtected(req)) await auth.protect()\n})\nexport const config = { matcher: ['/((?!_next|[^?]*\\\\.(?:html?|css)).*)', '/(api|trpc)(.*)'] }\n",
        ),
    ]);
    assert_eq!(proxy_edges(&cg).len(), 2);
    let edge = proxy_edges(&cg).into_iter().find(|e| cg.get_node(&e.source).unwrap().unwrap().name == "/dashboard").unwrap();
    let target = cg.get_node(&edge.target).unwrap().unwrap();
    assert_eq!(target.kind, NodeKind::File);
    assert_eq!(target.file_path, "middleware.ts");
    assert_eq!(meta_str(&edge, "registeredAt").as_deref(), Some("middleware.ts:6"));
    assert!(cg.get_callees(&target.id, 1).unwrap().iter().any(|c| c.node.name == "isProtected"));
    assert_eq!(flow(&cg, "Home isProtected"), ["Home", "/dashboard", "middleware.ts", "isProtected"]);
    cg.close();
}

#[test]
fn a_proxy_with_no_matcher_runs_on_every_route() {
    let (cg, _dir) = project(&[
        ("package.json", &manifest("16.0.0")),
        ("app/page.tsx", &page("Home")),
        ("app/users/[id]/page.tsx", &page("User")),
        ("app/api/users/route.ts", "export async function GET() {\n  return Response.json([])\n}\n"),
        ("proxy.ts", "export const proxy = (request) => {\n  return undefined\n}\n"),
    ]);
    let proxy = cg.get_nodes_by_name("proxy").unwrap().into_iter().find(|n| n.file_path == "proxy.ts").expect("proxy node");
    expect_running(routes_running(&cg, &proxy), &[("/", None), ("/users/:id", None), ("GET /api/users", None)]);
    let first = &proxy_edges(&cg)[0];
    assert_eq!(first.metadata.as_ref().unwrap().len(), 2);
    assert_eq!(meta_str(first, "registeredAt").as_deref(), Some("proxy.ts:1"));
    cg.close();
}

#[test]
fn links_nothing_for_a_matcher_it_cant_read() {
    let (unknown, _a) = project(&[
        ("package.json", &manifest("16.0.0")),
        ("app/page.tsx", &page("Home")),
        ("proxy.ts", "export function proxy(request) {\n  return undefined\n}\nexport const config = { matcher: ['/(\\\\d+)'] }\n"),
    ]);
    assert!(proxy_edges(&unknown).is_empty());
    unknown.close();
    let (variable, _b) = project(&[
        ("package.json", &manifest("16.0.0")),
        ("app/page.tsx", &page("Home")),
        ("proxy.ts", "const PROTECTED = ['/']\nexport function proxy(request) {\n  return undefined\n}\nexport const config = { matcher: PROTECTED }\n"),
    ]);
    assert!(proxy_edges(&variable).is_empty());
    variable.close();
}

#[test]
fn no_proxy_ts_outside_a_next_app_or_in_one_pinned_before_16() {
    let (tool, _a) = project(&[
        ("package.json", r#"{"name":"tool","dependencies":{"commander":"14"}}"#),
        ("src/mcp/proxy.ts", "export function proxy(request) {\n  return request\n}\n"),
        ("src/proxy.ts", "export function proxy(request) {\n  return request\n}\n"),
        ("src/app/page.tsx", &page("Home")),
    ]);
    assert!(proxy_edges(&tool).is_empty());
    tool.close();
    let (pinned, _b) = project(&[
        ("package.json", &manifest("~15.3.0")),
        ("src/app/page.tsx", &page("Home")),
        ("src/proxy.ts", "export function proxy(target) {\n  return fetch(target)\n}\n"),
    ]);
    let routes: Vec<String> = pinned.get_nodes_by_kind(NodeKind::Route).unwrap().into_iter().map(|r| r.name).collect();
    assert_eq!(routes, ["/"]);
    assert!(proxy_edges(&pinned).is_empty());
    pinned.close();
}

#[test]
fn keeps_each_app_of_a_monorepo_to_its_own_proxy() {
    let (cg, _dir) = project(&[
        ("package.json", r#"{"name":"repo","private":true,"workspaces":["apps/*"]}"#),
        ("apps/shop/package.json", &manifest("16.1.0")),
        ("apps/shop/app/account/page.tsx", &page("ShopAccount")),
        ("apps/shop/proxy.ts", "export function proxy(request) {\n  return undefined\n}\nexport const config = { matcher: '/account' }\n"),
        ("apps/blog/package.json", &manifest("16.1.0")),
        ("apps/blog/app/account/page.tsx", &page("BlogAccount")),
    ]);
    let edges = proxy_edges(&cg);
    let sources: Vec<String> = edges.iter().map(|e| cg.get_node(&e.source).unwrap().unwrap().file_path).collect();
    assert_eq!(sources, ["apps/shop/app/account/page.tsx"]);
    assert_eq!(cg.get_node(&edges[0].target).unwrap().unwrap().file_path, "apps/shop/proxy.ts");
    cg.close();
}

#[test]
fn follows_a_sync_an_edited_matcher_a_new_route_and_the_proxy_going_away() {
    let (mut cg, dir) = project(&[
        ("package.json", &manifest("16.0.0")),
        ("app/page.tsx", &page("Home")),
        ("app/admin/page.tsx", &page("Admin")),
        ("proxy.ts", "export function proxy(request) {\n  return undefined\n}\nexport const config = { matcher: '/nowhere' }\n"),
    ]);
    assert!(source_names(&cg).is_empty());
    write(dir.path(), "proxy.ts", "export function proxy(request) {\n  return undefined\n}\nexport const config = { matcher: '/admin/:rest*' }\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(source_names(&cg), ["/admin"]);
    write(dir.path(), "app/admin/api/route.ts", "export async function DELETE() {\n  return new Response(null)\n}\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(source_names(&cg), ["/admin", "DELETE /admin/api"]);
    fs::remove_file(dir.path().join("proxy.ts")).unwrap();
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert!(source_names(&cg).is_empty());
    cg.close();
}
