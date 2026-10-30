//! Go method calls through a receiver bound from a call or a conversion (upstream 55948436, cases
//! first written for PR #1954): `r := newRing(); r.Reset()` is the type `newRing` returns, a
//! `c, _ := net.Dial(…)` is an outside type's and gets no edge rather than a project method that
//! shares the name. The typer also reads a trailing assertion, a func-typed field's result and a
//! dot-imported function.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn index_project(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("go.mod"), "module example.com/app\n\ngo 1.22\n").unwrap();
    for (file, source) in files {
        let path = dir.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

/// `file::Qualified::name` of every method the function or method `caller` calls, sorted.
fn method_callees(cg: &CodeGraph, caller: &str, kind: NodeKind) -> Vec<String> {
    let node = cg
        .get_nodes_by_kind(kind)
        .unwrap()
        .into_iter()
        .find(|n| n.name == caller)
        .unwrap_or_else(|| panic!("no {kind:?} {caller}"));
    let mut out: Vec<String> = cg
        .get_outgoing_edges(&node.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .filter(|t| t.kind == NodeKind::Method)
        .map(|t| format!("{}::{}", t.file_path, t.qualified_name))
        .collect();
    out.sort();
    out
}

fn callees(cg: &CodeGraph, caller: &str) -> Vec<String> {
    method_callees(cg, caller, NodeKind::Function)
}

const MAIN_GO: &str = r#"package main

import (
	"bytes"
	"net"
	"net/http"
	"os"

	"example.com/app/other"
	"example.com/app/store"
)

type ringLog struct{ buf []byte }

func (r *ringLog) Write(b []byte)  {}
func (r *ringLog) Reset()          {}
func (r *ringLog) Reader() *reader { return &reader{} }

type reader struct{}

func (r *reader) Reset() {}

type Engine struct{}

func (e *Engine) Run() *Engine { return e }
func (e *Engine) Stop()        {}

func newRing() *ringLog              { return &ringLog{} }
func NewEngine(n int) (*Engine, error) { return &Engine{}, nil }
func open() (*os.File, error)        { return nil, nil }
func wrap(e *Engine) *ringLog        { return nil }

type limiter struct{}

func (l *limiter) RoundTrip(r *http.Request) (*http.Response, error) { return nil, nil }

func transport() http.RoundTripper { return &limiter{} }

func lowerParam(ring *ringLog, b []byte) { ring.Write(b) }
func lowerValueParam(ring ringLog)       { ring.Reset() }
func groupedParam(a, ring *ringLog)      { ring.Reset() }
func lowerCtor()                         { r := newRing(); r.Reset() }
func exportedCtor()                      { e, _ := NewEngine(1); e.Stop() }
func qualifiedParam(s *store.Store)      { s.Put("a") }
func qualifiedCtor()                     { s := store.NewStore(); s.Put("b") }

func multiLineCtor() {
	e, err := NewEngine(
		1,
	)
	_ = err
	e.Stop()
}

func ifCtor() {
	if r := newRing(); r != nil {
		r.Write(nil)
	}
}

// `r` is what Reader returns: a reader, not a ringLog.
func chainedCtor() { r := newRing().Reader(); r.Reset() }

// On the binding's own line `e` is still the parameter; inside the
// `if`, `e` is what wrap returns.
func selfArg(e *Engine) {
	if e := wrap(e.Run()); e != nil {
		e.Write(nil)
	}
}

func stdlibParam(buf *bytes.Buffer)     { buf.Truncate(0) }
func stdlibCtor()                       { c, _ := net.Dial("tcp", ""); c.RemoteAddr() }
func projectFuncReturningStdlib()       { f, _ := open(); f.Truncate(0) }
func interfaceResult()                  { rt := transport(); rt.RoundTrip(nil) }
func netParam(conn net.Conn)            { conn.LocalAddr() }

// The `r` of the block, or of the function literal, is gone by the call.
func blockShadow() {
	r := newRing()
	if true {
		r := other.New()
		_ = r
	}
	r.Write(nil)
}

func closureShadow() {
	r := newRing()
	go func() { r := other.New(); _ = r }()
	r.Write(nil)
}

type Holder struct{}

func (h *Holder) New() *Item { return &Item{} }

type Item struct{}

func (i *Item) Save() {}

// `store` is the parameter, not the imported package.
func shadowPkg(store *Holder) { x := store.New(); x.Save() }

func main() {}
"#;

const STORE_GO: &str = "package store\n\ntype Store struct{}\n\nfunc NewStore() *Store { return &Store{} }\n\nfunc (s *Store) Put(k string) {}\n\ntype Thing struct{}\n\nfunc New() *Thing { return &Thing{} }\n\nfunc (t *Thing) Save() {}\n";
const OTHER_GO: &str = "package other\n\ntype Other struct{}\n\nfunc New() *Other { return &Other{} }\n\nfunc (o *Other) Write(b []byte) {}\n";
// Same-named methods elsewhere, so a guess by name has somewhere wrong to go.
const DECOY_GO: &str = "package decoy\n\ntype Decoy struct{}\n\nfunc (d *Decoy) Write(b []byte) {}\nfunc (d *Decoy) Reset()         {}\nfunc (d *Decoy) Run()           {}\nfunc (d *Decoy) Stop()          {}\nfunc (d *Decoy) Put(k string)   {}\nfunc (d *Decoy) Truncate(n int) {}\nfunc (d *Decoy) RemoteAddr()    {}\nfunc (d *Decoy) LocalAddr()     {}\n\ntype Store struct{}\n\nfunc (s *Store) Put(k string) {}\n";

fn receiver_typing() -> (tempfile::TempDir, CodeGraph) {
    index_project(&[("main.go", MAIN_GO), ("store/store.go", STORE_GO), ("other/other.go", OTHER_GO), ("decoy/decoy.go", DECOY_GO)])
}

#[test]
fn a_receiver_named_like_a_standard_library_package_resolves_on_its_type() {
    let (_dir, cg) = receiver_typing();
    for (caller, expected) in [("lowerParam", "main.go::ringLog::Write"), ("lowerValueParam", "main.go::ringLog::Reset"), ("groupedParam", "main.go::ringLog::Reset")] {
        assert_eq!(callees(&cg, caller), [expected], "{caller}");
    }
}

#[test]
fn a_receiver_bound_to_a_call_resolves_on_what_the_callee_returns() {
    let (_dir, cg) = receiver_typing();
    for (caller, expected) in [
        ("lowerCtor", "main.go::ringLog::Reset"),
        ("exportedCtor", "main.go::Engine::Stop"),
        ("multiLineCtor", "main.go::Engine::Stop"),
        ("ifCtor", "main.go::ringLog::Write"),
        ("qualifiedCtor", "store/store.go::Store::Put"),
    ] {
        assert_eq!(callees(&cg, caller), [expected], "{caller}");
    }
}

#[test]
fn a_package_qualified_parameter_resolves_in_that_package() {
    let (_dir, cg) = receiver_typing();
    assert_eq!(callees(&cg, "qualifiedParam"), ["store/store.go::Store::Put"]);
}

#[test]
fn a_binding_is_typed_by_the_whole_chain_and_from_the_end_of_its_own_line() {
    let (_dir, cg) = receiver_typing();
    assert_eq!(callees(&cg, "chainedCtor"), ["main.go::reader::Reset", "main.go::ringLog::Reader"]);
    assert_eq!(callees(&cg, "selfArg"), ["main.go::Engine::Run", "main.go::ringLog::Write"]);
}

#[test]
fn a_receiver_typed_outside_the_project_gets_no_edge() {
    let (_dir, cg) = receiver_typing();
    for caller in ["stdlibParam", "stdlibCtor", "projectFuncReturningStdlib", "netParam"] {
        assert_eq!(callees(&cg, caller), Vec::<String>::new(), "{caller}");
    }
}

// Real code's by-name guesses here mostly took a namesake: prometheus's `handler :=
// NewWriteHandler(…)` went to `readHandler::ServeHTTP`.
#[test]
fn an_outside_interface_a_project_function_returns_links_nothing() {
    let (_dir, cg) = receiver_typing();
    assert_eq!(callees(&cg, "interfaceResult"), Vec::<String>::new());
}

#[test]
fn a_walrus_in_a_block_that_closed_before_the_call_is_another_variable() {
    let (_dir, cg) = receiver_typing();
    for caller in ["blockShadow", "closureShadow"] {
        assert_eq!(callees(&cg, caller), ["main.go::ringLog::Write"], "{caller}");
    }
}

#[test]
fn a_parameter_named_like_an_imported_package_is_the_parameter_and_types_what_its_method_returns() {
    let (_dir, cg) = receiver_typing();
    assert_eq!(callees(&cg, "shadowPkg"), ["main.go::Holder::New", "main.go::Item::Save"]);
}

fn conversions_and_aliases() -> (tempfile::TempDir, CodeGraph) {
    index_project(&[
        (
            "web/context.go",
            "package web\n\ntype Context struct{}\n\nfunc (c *Context) MakeAuditRecord() {}\n\ntype Users []string\n\nfunc (u Users) Usernames() []string { return nil }\n",
        ),
        (
            "api4/handlers.go",
            "package api4\n\nimport (\n\t\"context\"\n\n\t\"example.com/app/web\"\n)\n\ntype Context = web.Context\n\ntype (\n\tHandler = *web.Context\n\tCtx     = context.Context\n)\n\ntype Defined web.Context\n\nfunc projectAlias(h Handler)             { h.MakeAuditRecord() }\nfunc conversion(names []string)          { list := web.Users(names); list.Usernames() }\nfunc definedType(d *Defined)             { d.MakeAuditRecord() }\nfunc stdlibReceiver(ctx context.Context) { ctx.Done() }\nfunc aliasedStdlib(ctx Ctx)              { ctx.Done() }\n",
        ),
        // Same-named types and methods elsewhere, so neither a lookup by type name nor a guess by
        // method name has a single answer to fall back on.
        (
            "decoy/decoy.go",
            "package decoy\n\ntype Context struct{}\n\nfunc (c *Context) MakeAuditRecord() {}\n\n// Named like the alias of an outside type in api4.\ntype Ctx struct{}\n\nfunc (c *Ctx) Done() {}\n\ntype Decoy struct{}\n\nfunc (d *Decoy) Done()      {}\nfunc (d *Decoy) Usernames() {}\n",
        ),
    ])
}

#[test]
fn a_receiver_bound_to_a_conversion_resolves_on_the_converted_type() {
    let (_dir, cg) = conversions_and_aliases();
    assert_eq!(callees(&cg, "conversion"), ["web/context.go::Users::Usernames"]);
}

#[test]
fn an_alias_of_a_project_type_still_resolves_on_that_type() {
    let (_dir, cg) = conversions_and_aliases();
    assert_eq!(callees(&cg, "projectAlias"), ["web/context.go::Context::MakeAuditRecord"]);
}

#[test]
fn a_defined_type_or_a_receiver_typed_outside_the_project_gets_no_edge() {
    let (_dir, cg) = conversions_and_aliases();
    for caller in ["definedType", "stdlibReceiver", "aliasedStdlib"] {
        assert_eq!(callees(&cg, caller), Vec::<String>::new(), "{caller}");
    }
}

fn what_a_call_hands_back() -> (tempfile::TempDir, CodeGraph) {
    index_project(&[
        (
            "svc/svc.go",
            "package svc\n\nimport (\n\t\"sync\"\n\n\t\"google.golang.org/grpc\"\n)\n\ntype Client struct{}\n\nfunc (c *Client) Do() {}\n\ntype monitor struct {\n\tclient func() (*Client, error)\n}\n\nfunc (m *monitor) run() {\n\tc, _ := m.client()\n\tc.Do()\n}\n\ntype Result struct{}\n\nfunc (r *Result) Done() {}\n\ntype Converter struct{}\n\nfunc (c *Converter) Convert() (*Result, error) { return &Result{}, nil }\n\nvar pool sync.Pool\n\nfunc pooled() {\n\tconv := pool.Get().(*Converter)\n\tout, _ := conv.Convert()\n\tout.Done()\n}\n\ntype Resp struct{}\n\nfunc (r *Resp) GetKind() string { return \"\" }\n\nfunc stream(s grpc.ServerStreamingClient[Resp]) {\n\tchunk, _ := s.Recv()\n\tchunk.GetKind()\n}\n",
        ),
        ("watch/watch.go", "package watch\n\ntype Watcher struct{}\n\nfunc NewWatcher() *Watcher { return &Watcher{} }\n\nfunc (w *Watcher) Stop() {}\n"),
        (
            "watch/watch_test.go",
            "package watch_test\n\nimport (\n\t\"testing\"\n\n\t. \"example.com/app/watch\"\n)\n\nfunc TestStop(t *testing.T) {\n\tw := NewWatcher()\n\tw.Stop()\n}\n",
        ),
        // Namesakes, so a guess by the method's name has somewhere wrong to go.
        ("decoy/decoy.go", "package decoy\n\ntype Decoy struct{}\n\nfunc (d *Decoy) Do()   {}\nfunc (d *Decoy) Done() {}\nfunc (d *Decoy) Stop() {}\n"),
    ])
}

#[test]
fn a_func_typed_field_is_called_for_the_result_its_type_declares() {
    let (_dir, cg) = what_a_call_hands_back();
    assert_eq!(method_callees(&cg, "run", NodeKind::Method), ["svc/svc.go::Client::Do"]);
}

#[test]
fn an_assertion_types_the_value_whatever_hands_it_out() {
    let (_dir, cg) = what_a_call_hands_back();
    assert_eq!(callees(&cg, "pooled"), ["svc/svc.go::Converter::Convert", "svc/svc.go::Result::Done"]);
}

#[test]
fn a_function_a_dot_import_brings_in_types_its_result() {
    let (_dir, cg) = what_a_call_hands_back();
    assert_eq!(callees(&cg, "TestStop"), ["watch/watch.go::Watcher::Stop"]);
}

#[test]
fn what_an_outside_generic_type_hands_back_is_left_to_name_matching_as_before() {
    let (_dir, cg) = what_a_call_hands_back();
    assert_eq!(callees(&cg, "stream"), ["svc/svc.go::Resp::GetKind"]);
}

// A Go standard method name (`String`) says nothing of its receiver, so its declaration decides: the
// type the call it is assigned from returns (upstream go-std-method-receivers, expectations updated
// by 55948436).
fn std_method_names() -> (tempfile::TempDir, CodeGraph) {
    index_project(&[
        (
            "types/id.go",
            "package types\n\ntype URLsMap map[string][]string\n\nfunc NewURLsMap() URLsMap { return URLsMap{} }\n\nfunc (m URLsMap) String() string { return \"\" }\n",
        ),
        (
            "server/version.go",
            "package server\n\ntype Version struct{}\n\nfunc ParseVersion(s string) (*Version, error) { return &Version{}, nil }\n\nfunc (v *Version) Major() string { return \"\" }\n\nfunc (v *Version) String() string { return \"\" }\n",
        ),
        (
            "server/server.go",
            "package server\n\nimport \"example.com/app/types\"\n\nfunc urls() string {\n\turlsMap := types.NewURLsMap()\n\treturn urlsMap.String()\n}\n\nfunc versionText() string {\n\ttests := []struct {\n\t\tversion string\n\t}{{version: \"1.0\"}}\n\tout := \"\"\n\tfor _, test := range tests {\n\t\tversion, _ := ParseVersion(test.version)\n\t\tout += version.Major() + version.String()\n\t}\n\treturn out\n}\n",
        ),
        // Namesakes, so a guess by the method's name has somewhere wrong to go.
        ("decoy/decoy.go", "package decoy\n\ntype Decoy struct{}\n\nfunc (d *Decoy) String() string { return \"\" }\nfunc (d *Decoy) Major() string  { return \"\" }\n"),
    ])
}

#[test]
fn a_standard_method_name_links_through_the_type_the_call_it_is_assigned_from_returns() {
    let (_dir, cg) = std_method_names();
    assert_eq!(callees(&cg, "urls"), ["types/id.go::URLsMap::String"]);
}

#[test]
fn a_value_bound_from_a_call_is_what_the_call_returns_not_the_test_tables_field_of_that_name() {
    let (_dir, cg) = std_method_names();
    assert_eq!(callees(&cg, "versionText"), ["server/version.go::Version::Major", "server/version.go::Version::String"]);
}
