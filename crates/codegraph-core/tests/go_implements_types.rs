//! Go wants an implementer's methods identical to the interface's (upstream 608487e2). The
//! parameter and result counts alone let a type with the right number of the wrong types
//! through; the pass now compares what the signature text can settle, each type by its name
//! with the package left off. A true alias, a type parameter and a type that holds a list of its
//! own say nothing.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }
}

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), files);
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn implementers(cg: &CodeGraph, iface: &str) -> Vec<String> {
    let node = cg
        .get_nodes_by_kind(NodeKind::Interface)
        .unwrap()
        .into_iter()
        .find(|n| n.name == iface)
        .unwrap_or_else(|| panic!("{iface} not found"));
    let mut names: Vec<String> = cg
        .get_incoming_edges(&node.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Implements)
        .map(|e| cg.get_node(&e.source).unwrap().unwrap().name)
        .collect();
    names.sort();
    names
}

#[test]
fn compares_what_the_signatures_say_of_the_types() {
    let (_dir, cg) = index(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "httpx/matcher.go",
            "package httpx\n\nimport \"net/http\"\n\ntype RequestMatcher interface {\n\tMatch(*http.Request) bool\n}\n\ntype MatchHost []string\n\nfunc (m MatchHost) Match(r *http.Request) bool { return false }\n\ntype Encode struct{}\n\nfunc (e *Encode) Match(rw *responseWriter) bool { return false }\n\ntype responseWriter struct{}\n",
        ),
        (
            "tlsx/matcher.go",
            "package tlsx\n\nimport \"crypto/tls\"\n\ntype ConnectionMatcher interface {\n\tMatch(*tls.ClientHelloInfo) bool\n}\n\ntype MatchServerName []string\n\nfunc (m MatchServerName) Match(hello *tls.ClientHelloInfo) bool { return false }\n",
        ),
        (
            "core/modules.go",
            "package core\n\ntype Context struct{}\n\ntype Provisioner interface {\n\tProvision(Context) error\n}\n\ntype Own struct{}\n\nfunc (o *Own) Provision(ctx Context) error { return nil }\n",
        ),
        (
            "plugin/plugin.go",
            "package plugin\n\nimport (\n\t\"example.com/app/core\"\n\tmod \"example.com/app/core\"\n)\n\ntype Plugin struct{}\n\nfunc (p *Plugin) Provision(ctx core.Context) error { return nil }\n\ntype Renamed struct{}\n\nfunc (r *Renamed) Provision(ctx mod.Context) error { return nil }\n",
        ),
        (
            "lists/lists.go",
            "package lists\n\ntype Copier interface {\n\tCopy(string, string) (int, error)\n}\n\ntype Grouped struct{}\n\nfunc (g *Grouped) Copy(from, to string) (n int, err error) { return 0, nil }\n\ntype Bytes struct{}\n\nfunc (b *Bytes) Copy(from []byte, to string) (int, error) { return 0, nil }\n",
        ),
        (
            "spelled/spelled.go",
            "package spelled\n\ntype Sink interface {\n\tPut(v any, b []byte)\n}\n\ntype Old struct{}\n\nfunc (o *Old) Put(v interface{}, b []uint8) {}\n",
        ),
        (
            "aliased/aliased.go",
            "package aliased\n\ntype Handle = uint64\n\ntype Store interface {\n\tGet(id uint64) string\n}\n\ntype ByAlias struct{}\n\nfunc (a *ByAlias) Get(id Handle) string { return \"\" }\n\ntype Other uint64\n\ntype ByDefined struct{}\n\nfunc (d *ByDefined) Get(id Other) string { return \"\" }\n",
        ),
        (
            "generic/generic.go",
            "package generic\n\ntype Keyed interface {\n\tKey() string\n\tValue() int\n}\n\ntype Pair[K comparable, V any] struct{}\n\nfunc (p *Pair[K, V]) Key() K   { var k K; return k }\nfunc (p *Pair[K, V]) Value() V { var v V; return v }\n\ntype Tagged[T any] struct{}\n\nfunc (t *Tagged[T]) Key() T       { var k T; return k }\nfunc (t *Tagged[T]) Value() int64 { return 0 }\n\ntype Getter[T any] interface {\n\tGet() T\n}\n\ntype Text struct{}\n\nfunc (t *Text) Get() string { return \"\" }\n",
        ),
        (
            "promoted/promoted.go",
            "package promoted\n\ntype Base[T any] struct{}\n\nfunc (b *Base[T]) Load() T { var v T; return v }\n\ntype Loader interface {\n\tLoad() string\n}\n\ntype Holder struct {\n\tBase[string]\n}\n",
        ),
        (
            "funcs/funcs.go",
            "package funcs\n\nimport \"context\"\n\ntype Runner interface {\n\tRun(fn func(context.Context) error)\n}\n\ntype Job struct{}\n\nfunc (j *Job) Run(fn func(ctx context.Context) error) {}\n",
        ),
        (
            "pb/rpc_grpc.pb.go",
            "package pb\n\nimport (\n\t\"context\"\n\n\t\"google.golang.org/grpc\"\n)\n\ntype RangeRequest struct{}\ntype RangeResponse struct{}\n\ntype KVClient interface {\n\tRange(ctx context.Context, in *RangeRequest, opts ...grpc.CallOption) (*RangeResponse, error)\n}\n",
        ),
        (
            "server/kv.go",
            "package server\n\nimport (\n\t\"context\"\n\n\tpb \"example.com/app/pb\"\n)\n\ntype kvServer struct{}\n\nfunc (s *kvServer) Range(ctx context.Context, r *pb.RangeRequest) (*pb.RangeResponse, error) {\n\treturn nil, nil\n}\n",
        ),
    ]);
    assert_eq!(implementers(&cg, "RequestMatcher"), ["MatchHost"]);
    assert_eq!(implementers(&cg, "ConnectionMatcher"), ["MatchServerName"]);
    assert_eq!(implementers(&cg, "Provisioner"), ["Own", "Plugin", "Renamed"]);
    assert_eq!(implementers(&cg, "Copier"), ["Grouped"]);
    assert_eq!(implementers(&cg, "Sink"), ["Old"]);
    assert_eq!(implementers(&cg, "Store"), ["ByAlias"]);
    assert_eq!(implementers(&cg, "Keyed"), ["Pair"]);
    assert_eq!(implementers(&cg, "Getter"), ["Text"]);
    assert_eq!(implementers(&cg, "Loader"), ["Base", "Holder"]);
    assert_eq!(implementers(&cg, "Runner"), ["Job"]);
    assert_eq!(implementers(&cg, "KVClient"), ["kvServer"]);
}
