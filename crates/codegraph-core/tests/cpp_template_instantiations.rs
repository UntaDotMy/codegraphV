//! A C++ call through a template parameter links to that member of each class the project
//! writes as the template's argument (upstream 01a4931, #3196): rocksdb's `Txn::PutEntity(…)`
//! in `template <typename Txn> class SecondaryIndexMixin`, `Traits::Clear` in a function
//! template given two local `Traits`, and a typed test's `TypeParam::KeyGen`. No lookup at the
//! call can answer these, so they are synthesized from the instantiations, one edge per class.
use codegraph_core::CodeGraph;
use std::fs;
use std::path::Path;

/// Upstream's caps (`CPP_TEMPLATE_ARGUMENT_CAP`, `CPP_TEMPLATE_SITE_CAP`).
const ARGUMENT_CAP: usize = 40;
const SITE_CAP: usize = 1024;

fn lines(l: &[&str]) -> String {
    let mut s = l.join("\n");
    s.push('\n');
    s
}

fn write(root: &Path, files: &[(&str, String)]) {
    for (rel, content) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn indexed(files: &[(&str, String)]) -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), files);
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    source: String,
    target: String,
    target_kind: String,
    kind: String,
    line: i64,
    via: Option<String>,
    argument: Option<String>,
    registered_at: Option<String>,
    provenance: Option<String>,
    fn_ref: Option<i64>,
    source_file: String,
    target_file: String,
    target_line: i64,
}

/// Every edge the pass made, in insertion order.
fn template_edges(cg: &CodeGraph) -> Vec<Row> {
    let conn = cg.store().conn();
    let mut stmt = conn
        .prepare(
            "SELECT s.qualified_name, t.qualified_name, t.kind, e.kind, e.line, json_extract(e.metadata, '$.via'),
                json_extract(e.metadata, '$.argument'), json_extract(e.metadata, '$.registeredAt'), e.provenance,
                json_extract(e.metadata, '$.fnRef'), s.file_path, t.file_path, t.start_line
             FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
             WHERE json_extract(e.metadata, '$.synthesizedBy') = 'cpp-template-argument' ORDER BY e.id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok(Row {
                source: r.get(0)?,
                target: r.get(1)?,
                target_kind: r.get(2)?,
                kind: r.get(3)?,
                line: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                via: r.get(5)?,
                argument: r.get(6)?,
                registered_at: r.get(7)?,
                provenance: r.get(8)?,
                fn_ref: r.get(9)?,
                source_file: r.get::<_, String>(10)?.replace('\\', "/"),
                target_file: r.get::<_, String>(11)?.replace('\\', "/"),
                target_line: r.get(12)?,
            })
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// The targets the pass linked from `source`, sorted.
fn reached(cg: &CodeGraph, source: &str) -> Vec<String> {
    let mut out: Vec<String> = template_edges(cg).into_iter().filter(|e| e.source == source).map(|e| e.target).collect();
    out.sort();
    out
}

fn targets(cg: &CodeGraph) -> Vec<String> {
    template_edges(cg).into_iter().map(|e| e.target).collect()
}

fn txn() -> String {
    lines(&[
        "class BaseTxn {",
        " public:",
        "  void Commit() {}",
        "};",
        "class PlainTxn : public BaseTxn {",
        " public:",
        "  void Put(int k) {}",
        "  void Put(int k, int v) {}",
        "  static void Make() {}",
        "};",
        "class FancyTxn : public BaseTxn {",
        " public:",
        "  void Put(int k) {}",
        "  static void Make() {}",
        "};",
        "class NoPutTxn {};",
    ])
}

/// `Mixin<Txn>` with one call through its parameter, in a header.
fn mixin() -> String {
    lines(&[
        "#include \"txn.h\"",
        "template <typename Txn>",
        "class Mixin : public Txn {",
        " public:",
        "  void Add(int k) { Txn::Put(k); }",
        "  void Finish() { Txn::Commit(); }",
        "  static void Build() { Txn::Make(); }",
        "};",
    ])
}

const PUT_PLAIN: &str = "PlainTxn::Put";

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn uses(code: &str) -> (tempfile::TempDir, CodeGraph) {
    indexed(&[("txn.h", txn()), ("mixin.h", mixin()), ("use.cc", lines(&["#include \"mixin.h\"", code]))])
}

// ---- the forms of a written instantiation ----

#[test]
fn links_to_the_argument_of_the_one_instantiation_the_rocksdb_secondary_index_mixin_shape() {
    let (_d, cg) = uses("void Run() { auto* m = new Mixin<PlainTxn>(); m->Add(1); }");
    assert_eq!(reached(&cg, "Mixin::Add"), s(&[PUT_PLAIN]));
    assert_eq!(reached(&cg, "Mixin::Build"), s(&["PlainTxn::Make"]));
    let edge = template_edges(&cg).into_iter().find(|e| e.source == "Mixin::Add").unwrap();
    assert_eq!(edge.kind, "calls");
    assert_eq!(edge.via.as_deref(), Some("Txn"));
    assert_eq!(edge.argument.as_deref(), Some("PlainTxn"));
    assert_eq!(edge.registered_at.as_deref(), Some("use.cc:2"));
    assert_eq!(edge.provenance.as_deref(), Some("heuristic"));
    assert_eq!(edge.line, 5);
}

#[test]
fn chooses_the_overload_the_call_makes() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("pair.h", lines(&["#include \"txn.h\"", "template <class T>", "struct Pair {", "  void Both(int a, int b) { T::Put(a, b); }", "};"])),
        ("use.cc", lines(&["#include \"pair.h\"", "Pair<PlainTxn> pair;"])),
    ]);
    let edges = template_edges(&cg);
    assert_eq!(edges.first().map(|e| e.target.as_str()), Some(PUT_PLAIN), "{edges:#?}");
    assert_eq!(edges.iter().map(|e| e.target_line).collect::<Vec<_>>(), [8]);
}

#[test]
fn reads_every_written_form() {
    let forms = [
        ("a base clause", "class Logged : public Mixin<PlainTxn> {};"),
        ("a variable", "Mixin<PlainTxn> global;"),
        ("a member", "class Holder { Mixin<PlainTxn> held_; };"),
        ("a pointer to it", "Mixin<PlainTxn>* maybe = nullptr;"),
        ("an argument of another template", "std::vector<Mixin<PlainTxn>> many;"),
        ("a using alias", "using Aliased = Mixin<PlainTxn>;"),
        ("a typedef", "typedef Mixin<PlainTxn> Aliased;"),
        ("an explicit instantiation", "template class Mixin<PlainTxn>;"),
        ("an extern explicit instantiation", "extern template class Mixin<PlainTxn>;"),
        ("a static member call", "void Run() { Mixin<PlainTxn>::Build(); }"),
        ("the name written with a namespace", "namespace other { ::Mixin<PlainTxn> global; }"),
        ("a list broken over lines", "Mixin<\n    PlainTxn\n> global;"),
    ];
    for (form, code) in forms {
        let (_d, cg) = uses(code);
        assert_eq!(reached(&cg, "Mixin::Add"), s(&[PUT_PLAIN]), "reads {form}");
    }
}

#[test]
fn reads_past_the_arrow_of_a_trailing_return_type() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("bound.h", lines(&["#include \"txn.h\"", "template <class Fn, class T>", "struct Bound {", "  void Run() { T::Make(); }", "};"])),
        ("use.cc", lines(&["#include \"bound.h\"", "Bound<auto(int)->int, PlainTxn> bound;"])),
    ]);
    assert_eq!(reached(&cg, "Bound::Run"), s(&["PlainTxn::Make"]));
}

#[test]
fn links_an_explicit_function_template_argument() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        (
            "run.cc",
            lines(&["#include \"txn.h\"", "template <typename T>", "void Build() { T::Make(); }", "void Run() {", "  Build<PlainTxn>();", "  Build<FancyTxn>();", "}"]),
        ),
    ]);
    assert_eq!(reached(&cg, "Build"), s(&["FancyTxn::Make", "PlainTxn::Make"]));
    let mut at: Vec<String> = template_edges(&cg).into_iter().filter_map(|e| e.registered_at).collect();
    at.sort();
    assert_eq!(at, s(&["run.cc:5", "run.cc:6"]));
}

#[test]
fn links_a_member_function_template_and_a_class_template_method_each_to_its_own_head() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        (
            "host.h",
            lines(&["#include \"txn.h\"", "template <typename A>", "class Host {", " public:", "  template <typename B>", "  void Both() { A::Make(); B::Make(); }", "};"]),
        ),
        ("use.cc", lines(&["#include \"host.h\"", "void Run() {", "  Host<PlainTxn> h;", "  h.Both<FancyTxn>();", "}"])),
    ]);
    assert_eq!(reached(&cg, "Host::Both"), s(&["FancyTxn::Make", "PlainTxn::Make"]));
}

#[test]
fn reads_the_parameter_of_a_member_defined_outside_its_class_by_position() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        (
            "pair.h",
            lines(&[
                "#include \"txn.h\"",
                "template <int N, typename First, typename Second>",
                "class Pair {",
                " public:",
                "  void Go();",
                "  template <typename Third> void More();",
                "};",
                "template <int M, typename A, typename B>",
                "void Pair<M, A, B>::Go() { B::Make(); }",
                "template <int M, typename A, typename B>",
                "template <typename C>",
                "void Pair<M, A, B>::More() { A::Make(); C::Make(); }",
            ]),
        ),
        ("use.cc", lines(&["#include \"pair.h\"", "void Run() {", "  Pair<1, PlainTxn, FancyTxn> p;", "  p.More<NoPutTxn>();", "}"])),
    ]);
    assert_eq!(reached(&cg, "Pair::Go"), s(&["FancyTxn::Make"]));
    assert_eq!(reached(&cg, "Pair::More"), s(&["PlainTxn::Make"]));
}

#[test]
fn reads_the_parameter_of_a_class_around_the_caller_and_of_a_function_around_a_local_class() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        (
            "all.cc",
            lines(&[
                "#include \"txn.h\"",
                "template <typename T>",
                "class Outer {",
                " public:",
                "  class Inner {",
                "   public:",
                "    void Go() { T::Make(); }",
                "  };",
                "};",
                "Outer<PlainTxn> outer;",
                "template <typename T>",
                "void Run() {",
                "  struct Local {",
                "    void Go() { T::Make(); }",
                "  };",
                "  auto later = [] { T::Make(); };",
                "}",
                "void Start() { Run<FancyTxn>(); }",
            ]),
        ),
    ]);
    assert_eq!(reached(&cg, "Outer::Inner::Go"), s(&["PlainTxn::Make"]));
    assert_eq!(reached(&cg, "Run::Local::Go"), s(&["FancyTxn::Make"]));
    assert_eq!(reached(&cg, "Run"), s(&["FancyTxn::Make"]));
}

#[test]
fn reads_the_parameter_of_a_nested_class_defined_outside_its_template() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        (
            "list.h",
            lines(&["#include \"txn.h\"", "template <typename T>", "class List {", " public:", "  struct Node;", "};", "template <typename T>", "struct List<T>::Node {", "  void Go() { T::Make(); }", "};"]),
        ),
        ("use.cc", lines(&["#include \"list.h\"", "List<PlainTxn> list;"])),
    ]);
    assert_eq!(targets(&cg), s(&["PlainTxn::Make"]));
}

#[test]
fn reads_a_parameter_pack() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("pack.h", lines(&["#include \"txn.h\"", "template <int N, typename... Ts>", "struct All {", "  static void Go() { (Ts::Make(), ...); }", "};"])),
        ("use.cc", lines(&["#include \"pack.h\"", "All<1, PlainTxn, FancyTxn> all;"])),
    ]);
    assert_eq!(reached(&cg, "All::Go"), s(&["FancyTxn::Make", "PlainTxn::Make"]));
}

#[test]
fn takes_a_default_the_instantiation_does_not_write() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("def.h", lines(&["#include \"txn.h\"", "template <typename A, typename B = FancyTxn>", "struct Def {", "  void Go() { B::Commit(); B::Put(1); }", "};"])),
        ("use.cc", lines(&["#include \"def.h\"", "Def<int> plain;", "Def<int, PlainTxn> written;"])),
    ]);
    assert_eq!(reached(&cg, "Def::Go"), s(&["BaseTxn::Commit", "FancyTxn::Put", PUT_PLAIN]));
}

#[test]
fn looks_the_template_up_from_a_namespace_with_no_declaration_around_the_site() {
    let (_d, cg) = indexed(&[
        ("all.h", lines(&["namespace ns {", "struct Item { static void Go() {} };", "template <typename T> struct Box { void Run() { T::Go(); } };", "}"])),
        ("other.h", lines(&["namespace other {", "struct Item { static void Go() {} };", "}"])),
        ("use.cc", lines(&["#include \"all.h\"", "#include \"other.h\"", "namespace ns {", "template class Box<Item>;", "}"])),
    ]);
    assert_eq!(reached(&cg, "ns::Box::Run"), s(&["ns::Item::Go"]));
}

#[test]
fn leaves_a_namespace_when_it_closes_for_a_site_after_it() {
    let (_d, cg) = indexed(&[(
        "all.cc",
        lines(&[
            "struct Item { static void Make() {} };",
            "namespace a {",
            "struct Item { static void Make() {} };",
            "template <typename T> struct Box { void Run() { T::Make(); } };",
            "}",
            "template class a::Box<Item>;",
        ]),
    )]);
    assert_eq!(reached(&cg, "a::Box::Run"), s(&["Item::Make"]));
}

#[test]
fn counts_a_namespace_a_macro_opens() {
    let (_d, cg) = indexed(&[
        ("macros.h", lines(&["#define NS_BEGIN namespace ns {", "#define NS_END }"])),
        ("all.h", lines(&["#include \"macros.h\"", "NS_BEGIN", "struct Item { static void Go() {} };", "template <typename T> struct Box { void Run() { T::Go(); } };", "NS_END"])),
        ("use.cc", lines(&["#include \"all.h\"", "NS_BEGIN", "template class Box<Item>;", "NS_END"])),
    ]);
    let at: Vec<Option<String>> = template_edges(&cg).into_iter().map(|e| e.registered_at).collect();
    assert_eq!(at, [Some("use.cc:3".to_string())]);
}

#[test]
fn reads_a_function_pointer_to_a_member_through_a_parameter() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("use.cc", lines(&["#include \"txn.h\"", "template <class T>", "struct Each {", "  void Go() { Run(&T::Make); }", "  void Run(void (*fn)()) { fn(); }", "};", "Each<PlainTxn> each;"])),
    ]);
    let edges = template_edges(&cg);
    let edge = edges.first().unwrap_or_else(|| panic!("no edge"));
    assert_eq!((edge.kind.as_str(), edge.source.as_str(), edge.target.as_str(), edge.fn_ref), ("references", "Each::Go", "PlainTxn::Make", Some(1)));
}

// ---- a call on a value of the parameter's type ----

fn shard_body(code: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "shards.h",
            lines(&["struct Inner { void Deep() {} };", "struct FastShard { using Part = Inner; void Clear() {} };", "struct SlowShard { using Part = Inner; void Clear() {} };"]),
        ),
        ("cache.h", lines(&["#include \"shards.h\"", "template <typename Shard>", "class Cache {", " public:", code, " private:", "  Shard own_;", "  Shard* many_;", "};"])),
        ("use.cc", lines(&["#include \"cache.h\"", "Cache<FastShard> fast;", "Cache<SlowShard> slow;"])),
    ]
}

#[test]
fn reads_a_value_of_the_parameters_type_in_every_form() {
    let forms = [
        ("a parameter by reference", "void Run(Shard& shard) { shard.Clear(); }"),
        ("a parameter by const reference", "void Run(const Shard& shard) { shard.Clear(); }"),
        ("a parameter by pointer", "void Run(Shard* shard) { shard->Clear(); }"),
        ("a local", "void Run() { Shard shard; shard.Clear(); }"),
        ("a member", "void Run() { own_.Clear(); }"),
        ("a pointer member", "void Run() { many_->Clear(); }"),
    ];
    for (form, code) in forms {
        let (_d, cg) = indexed(&shard_body(code));
        assert_eq!(reached(&cg, "Cache::Run"), s(&["FastShard::Clear", "SlowShard::Clear"]), "reads {form}");
    }
}

#[test]
fn reads_a_member_of_the_parameters_type_unless_a_local_of_another_type_hides_it() {
    let (_d, cg) = indexed(&shard_body("void Run() { Inner own_; own_.Clear(); }"));
    assert_eq!(template_edges(&cg), []);
}

#[test]
fn reads_a_member_type_of_the_parameter_for_a_value() {
    let (_d, cg) = indexed(&shard_body("void Run() { typename Shard::Part part; part.Deep(); }"));
    assert_eq!(reached(&cg, "Cache::Run"), s(&["Inner::Deep"]));
}

#[test]
fn reads_an_alias_of_a_member_type() {
    let (_d, cg) = indexed(&shard_body("using Part = typename Shard::Part;\n  void Run(Part& part) { part.Deep(); }"));
    assert_eq!(reached(&cg, "Cache::Run"), s(&["Inner::Deep"]));
}

#[test]
fn keeps_out_an_arrow_on_a_value_that_is_the_arguments_own_operator() {
    let (_d, cg) = indexed(&shard_body("void Run(Shard& shard) { shard->Clear(); }"));
    assert_eq!(template_edges(&cg), []);
}

#[test]
fn keeps_out_a_variable_of_another_type() {
    let (_d, cg) = indexed(&shard_body("void Run(Other& shard) { shard.Clear(); }"));
    assert_eq!(template_edges(&cg), []);
}

#[test]
fn leaves_a_call_a_resolver_links_alone() {
    let (_d, cg) = indexed(&shard_body("void Run(Inner& inner) { inner.Deep(); }"));
    assert_eq!(template_edges(&cg), []);
    let run = cg.get_nodes_by_name("Run").unwrap().into_iter().find(|n| n.qualified_name == "Cache::Run").unwrap();
    let callees: Vec<String> = cg
        .get_outgoing_edges(&run.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == codegraph_types::EdgeKind::Calls)
        .map(|e| cg.get_node(&e.target).unwrap().unwrap().qualified_name)
        .collect();
    assert_eq!(callees, s(&["Inner::Deep"]));
}

// ---- a deduced argument ----

fn deduced(code: &str) -> (tempfile::TempDir, CodeGraph) {
    indexed(&[
        ("txn.h", txn()),
        (
            "run.cc",
            lines(&[
                "#include \"txn.h\"",
                "template <typename T> void ByValue(T x) { T::Make(); }",
                "template <typename T> void ByRef(const T& x) { T::Make(); }",
                "template <typename T> void ByPointer(T* x) { T::Make(); }",
                "template <typename T> void ByHolder(std::vector<T>* x) { T::Make(); }",
                code,
            ]),
        ),
    ])
}

#[test]
fn reads_the_type_of_a_declared_argument_whole() {
    let (_d, cg) = deduced("void Run() { PlainTxn p; ByValue(p); ByRef(p); }");
    assert_eq!(reached(&cg, "ByValue"), s(&["PlainTxn::Make"]));
    assert_eq!(reached(&cg, "ByRef"), s(&["PlainTxn::Make"]));
}

#[test]
fn reads_what_a_pointer_points_at() {
    let (_d, cg) = deduced("void Run() { FancyTxn f; ByPointer(&f); FancyTxn* q = nullptr; ByPointer(q); }");
    assert_eq!(reached(&cg, "ByPointer"), s(&["FancyTxn::Make"]));
}

#[test]
fn follows_an_argument_whose_type_is_the_callers_own_parameter() {
    let (_d, cg) = deduced("template <typename U> void Outer(U u) { ByRef(u); }\nvoid Run() { Outer<PlainTxn>(PlainTxn()); }");
    assert_eq!(reached(&cg, "ByRef"), s(&["PlainTxn::Make"]));
}

#[test]
fn gives_no_edge_for_a_literal_an_unknown_name_or_a_type_the_parameter_only_holds() {
    let (_d, cg) = deduced("void Run(int i) { ByValue(1); ByRef(nowhere); ByRef(i); std::vector<PlainTxn> v; ByHolder(&v); PlainTxn p; ByPointer(p); }");
    assert_eq!(template_edges(&cg), []);
}

#[test]
fn keeps_a_call_that_gives_a_type_beside_one_that_gives_a_literal() {
    let (_d, cg) = deduced("void Run() { ByValue(1); PlainTxn p; ByValue(p); }");
    assert_eq!(reached(&cg, "ByValue"), s(&["PlainTxn::Make"]));
}

#[test]
fn takes_the_written_argument_over_the_deduced_one() {
    let (_d, cg) = deduced("void Run() { PlainTxn p; ByValue<FancyTxn>(p); }");
    assert_eq!(reached(&cg, "ByValue"), s(&["FancyTxn::Make"]));
}

// ---- an instantiation inside another template ----

#[test]
fn follows_the_argument_to_the_outer_template_through_a_chain() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("mixin.h", mixin()),
        (
            "chain.h",
            lines(&["#include \"mixin.h\"", "template <typename U>", "class Middle : public Mixin<U> {};", "template <typename V>", "using Short = Middle<V>;", "template <typename W>", "void Outer() { Short<W> s; }"]),
        ),
        ("use.cc", lines(&["#include \"chain.h\"", "void Run() { Outer<PlainTxn>(); }", "Short<FancyTxn> direct;"])),
    ]);
    assert_eq!(reached(&cg, "Mixin::Add"), s(&["FancyTxn::Put", PUT_PLAIN]));
    let at = template_edges(&cg).into_iter().find(|e| e.target == PUT_PLAIN).and_then(|e| e.registered_at);
    assert_eq!(at.as_deref(), Some("use.cc:2"));
}

#[test]
fn links_no_call_to_itself() {
    let (_d, cg) = indexed(&[(
        "all.cc",
        lines(&["template <typename D>", "struct Base { static void Make() { D::Make(); } };", "struct Plain : public Base<Plain> {};", "struct Own : public Base<Own> { static void Make() {} };"]),
    )]);
    assert_eq!(reached(&cg, "Base::Make"), s(&["Own::Make"]));
}

#[test]
fn follows_an_argument_written_const() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("inner.h", lines(&["#include \"txn.h\"", "template <typename T> struct Inner { void Go() { T::Make(); } };", "template <typename U> struct Outer { Inner<const U> held; };"])),
        ("use.cc", lines(&["#include \"inner.h\"", "Outer<PlainTxn> outer;"])),
    ]);
    assert_eq!(reached(&cg, "Inner::Go"), s(&["PlainTxn::Make"]));
}

#[test]
fn stops_at_a_cycle_keeping_what_the_rest_of_the_walk_finds() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        (
            "loop.h",
            lines(&[
                "#include \"txn.h\"",
                "template <typename T> class B;",
                "template <typename T>",
                "class A {",
                " public:",
                "  void Go() { T::Make(); }",
                "  B<T>* next_;",
                "};",
                "template <typename T>",
                "class B {",
                "  A<T>* next_;",
                "  A<B<T>>* self_;",
                "};",
            ]),
        ),
        ("use.cc", lines(&["#include \"loop.h\"", "B<PlainTxn> start;"])),
    ]);
    assert_eq!(reached(&cg, "A::Go"), s(&["PlainTxn::Make"]));
}

#[test]
fn follows_an_argument_through_four_templates_at_most_and_stops_short_of_a_wrong_answer() {
    for (depth, expected) in [(4usize, s(&["PlainTxn::Make"])), (5, Vec::new())] {
        let mut chain: Vec<String> = (0..depth).map(|i| format!("template <typename T> class C{i} : public C{}<T> {{}};", i + 1)).collect();
        chain.reverse();
        let mut header = vec!["#include \"txn.h\"".to_string(), format!("template <typename T> class C{depth} {{ public: void Go() {{ T::Make(); }} }};")];
        header.extend(chain);
        let header: Vec<&str> = header.iter().map(String::as_str).collect();
        let (_d, cg) = indexed(&[("txn.h", txn()), ("chain.h", lines(&header)), ("use.cc", lines(&["#include \"chain.h\"", "C0<PlainTxn> deep;"]))]);
        assert_eq!(reached(&cg, &format!("C{depth}::Go")), expected, "depth {depth}");
    }
}

// ---- gtest typed tests ----

fn ribbon() -> String {
    lines(&[
        "#include <gtest/gtest.h>",
        "struct SmallGen { SmallGen(int a, int b) {} };",
        "struct BigGen { BigGen(int a, int b) {} };",
        "struct Base { using KeyGen = BigGen; static int Chance() { return 1; } };",
        "struct Small : public Base { using KeyGen = SmallGen; static int Chance() { return 2; } };",
        "struct Other : public Base {};",
        "template <typename T>",
        "class RibbonTest : public ::testing::Test {};",
        "using Listed = ::testing::Types<",
        "    Small,",
        "    Other>;",
        "TYPED_TEST_SUITE(RibbonTest, Listed);",
        "TYPED_TEST(RibbonTest, Works) {",
        "  using KeyGen = typename TypeParam::KeyGen;",
        "  KeyGen one(1, 2);",
        "  int c = TypeParam::Chance();",
        "}",
    ])
}

#[test]
fn links_type_param_to_each_listed_type_inherited_or_its_own() {
    let (_d, cg) = indexed(&[("ribbon_test.cc", ribbon())]);
    let edges = template_edges(&cg);
    let mut chance: Vec<String> = edges.iter().filter(|e| e.kind == "calls" && e.target.ends_with("Chance")).map(|e| e.target.clone()).collect();
    chance.sort();
    assert_eq!(chance, s(&["Base::Chance", "Small::Chance"]), "{edges:#?}");
    let mut made: Vec<&Row> = edges.iter().filter(|e| e.kind == "instantiates").collect();
    made.sort_by(|a, b| a.target.cmp(&b.target));
    assert_eq!(made.iter().map(|e| e.target.as_str()).collect::<Vec<_>>(), ["Base::KeyGen", "Small::KeyGen"]);
    assert!(made.iter().all(|e| e.target_kind == "type_alias"), "{made:#?}");
    let small = edges.iter().find(|e| e.target == "Small::KeyGen").unwrap();
    assert_eq!((small.via.as_deref(), small.argument.as_deref(), small.registered_at.as_deref()), (Some("TypeParam"), Some("Small"), Some("ribbon_test.cc:10")));
}

#[test]
fn links_the_constructor_of_what_each_member_alias_names() {
    let (_d, cg) = indexed(&[("ribbon_test.cc", ribbon())]);
    let mut gens: Vec<String> = template_edges(&cg).into_iter().filter(|e| e.target.ends_with("Gen") && e.target_kind == "method").map(|e| e.target).collect();
    gens.sort();
    assert_eq!(gens, s(&["BigGen::BigGen", "SmallGen::SmallGen"]));
}

#[test]
fn reads_every_typed_suite_form() {
    let forms = [
        ("TYPED_TEST_CASE", "TYPED_TEST_CASE(T1, Listed);", "TYPED_TEST(T1, Works)"),
        ("a list written in the macro", "TYPED_TEST_SUITE(T1, ::testing::Types<A>);", "TYPED_TEST(T1, Works)"),
        ("a typedef of the list", "typedef ::testing::Types<A> Spare; TYPED_TEST_SUITE(T1, Spare);", "TYPED_TEST(T1, Works)"),
        ("a parameterized suite", "INSTANTIATE_TYPED_TEST_SUITE_P(Pre, T1, Listed);", "TYPED_TEST_P(T1, Works)"),
        ("the older parameterized macro", "INSTANTIATE_TYPED_TEST_CASE_P(Pre, T1, Listed);", "TYPED_TEST_P(T1, Works)"),
    ];
    for (form, suite, test) in forms {
        let body = format!("{test} {{ TypeParam::Go(); }}");
        let (_d, cg) = indexed(&[(
            "a_test.cc",
            lines(&[
                "#include <gtest/gtest.h>",
                "struct A { static int Go() { return 1; } };",
                "template <typename T> class T1 : public ::testing::Test { public: int Own() { return T::Go(); } };",
                "using Listed = ::testing::Types<A>;",
                suite,
                &body,
            ]),
        )]);
        assert!(targets(&cg).contains(&"A::Go".to_string()), "reads {form}: {:#?}", template_edges(&cg));
        assert_eq!(reached(&cg, "T1::Own"), s(&["A::Go"]), "reads {form}");
    }
}

#[test]
fn reads_a_typed_suite_whose_tests_live_in_a_header() {
    let (_d, cg) = indexed(&[
        ("suite.h", lines(&["template <typename T> class Suite : public ::testing::Test {};", "TYPED_TEST_SUITE_P(Suite);", "TYPED_TEST_P(Suite, Works) { TypeParam::Go(); }"])),
        ("a_test.cc", lines(&["#include \"suite.h\"", "struct A { static int Go() { return 1; } };", "using Listed = ::testing::Types<A>;", "INSTANTIATE_TYPED_TEST_SUITE_P(My, Suite, Listed);"])),
    ]);
    assert_eq!(targets(&cg), s(&["A::Go"]));
}

#[test]
fn follows_type_param_into_a_helper_the_test_instantiates() {
    let (_d, cg) = indexed(&[(
        "a_test.cc",
        lines(&[
            "#include <gtest/gtest.h>",
            "struct A { static int Go() { return 1; } };",
            "template <typename T> void Helper() { T::Go(); }",
            "template <typename T> class T1 : public ::testing::Test {};",
            "TYPED_TEST_SUITE(T1, ::testing::Types<A>);",
            "TYPED_TEST(T1, Works) { Helper<TypeParam>(); }",
        ]),
    )]);
    assert_eq!(reached(&cg, "Helper"), s(&["A::Go"]));
}

// ---- what an argument is ----

#[test]
fn finds_a_class_local_to_the_function_that_writes_it_one_per_function() {
    let (_d, cg) = indexed(&[(
        "batch.cc",
        lines(&[
            "namespace ns {",
            "class Batch {",
            " public:",
            "  template <typename Traits> void Impl();",
            "  void Get();",
            "  void GetEntity();",
            "};",
            "template <typename Traits>",
            "void Batch::Impl() { Traits::Clear(); }",
            "void Batch::Get() {",
            "  struct Traits { static void Clear() {} };",
            "  Impl<Traits>();",
            "}",
            "void Batch::GetEntity() {",
            "  struct Traits { static void Clear() {} };",
            "  Impl<Traits>();",
            "}",
            "}  // namespace ns",
        ]),
    )]);
    let pairs: Vec<(String, Option<String>)> = template_edges(&cg).into_iter().map(|e| (e.source, e.registered_at)).collect();
    assert_eq!(pairs, [("ns::Batch::Impl".to_string(), Some("batch.cc:12".to_string())), ("ns::Batch::Impl".to_string(), Some("batch.cc:16".to_string()))]);
    assert_eq!(reached(&cg, "ns::Batch::Impl").len(), 2);
}

#[test]
fn follows_an_alias_of_the_class_at_namespace_scope_and_in_the_function() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("mixin.h", mixin()),
        (
            "use.cc",
            lines(&["#include \"mixin.h\"", "namespace ns { using Named = PlainTxn; }", "ns::Named* unused;", "Mixin<ns::Named> global;", "void Run() {", "  using Local = FancyTxn;", "  Mixin<Local> local;", "}"]),
        ),
    ]);
    assert_eq!(reached(&cg, "Mixin::Add"), s(&["FancyTxn::Put", PUT_PLAIN]));
}

#[test]
fn looks_an_argument_up_from_where_it_is_written() {
    let (_d, cg) = indexed(&[
        ("a.h", lines(&["namespace a { struct Item { static void Go() {} }; }"])),
        ("b.h", lines(&["namespace b { struct Item { static void Go() {} }; }"])),
        ("box.h", lines(&["template <typename T>", "struct Box { void Run() { T::Go(); } };"])),
        ("use.cc", lines(&["#include \"a.h\"", "#include \"b.h\"", "#include \"box.h\"", "namespace b { Box<Item> here; }"])),
    ]);
    assert_eq!(reached(&cg, "Box::Run"), s(&["b::Item::Go"]));
}

#[test]
fn takes_no_class_of_a_scope_the_site_does_not_name() {
    let (_d, cg) = indexed(&[
        ("txn.h", lines(&[&txn(), "namespace deep { struct Hidden { static void Make() {} }; }"])),
        ("box.h", lines(&["#include \"txn.h\"", "template <typename T>", "struct Box { void Run() { T::Make(); } };"])),
        ("use.cc", lines(&["#include \"box.h\"", "Box<Hidden> b;"])),
    ]);
    assert_eq!(template_edges(&cg), []);
}

#[test]
fn gives_an_outside_argument_no_edge_and_keeps_the_others() {
    let (_d, cg) = uses("#include <string>\nMixin<std::string> s;\nMixin<int> i;\nMixin<PlainTxn> p;");
    assert_eq!(reached(&cg, "Mixin::Add"), s(&[PUT_PLAIN]));
}

#[test]
fn gives_a_pointer_or_a_reference_no_edge() {
    let (_d, cg) = uses("Mixin<PlainTxn*> p;\nMixin<PlainTxn&> r;\nMixin<const FancyTxn> c;");
    assert_eq!(reached(&cg, "Mixin::Add"), s(&["FancyTxn::Put"]));
}

#[test]
fn links_a_member_a_class_inherits_and_none_it_lacks() {
    let (_d, cg) = uses("Mixin<PlainTxn> p;\nMixin<NoPutTxn> n;");
    assert_eq!(reached(&cg, "Mixin::Finish"), s(&["BaseTxn::Commit"]));
    assert_eq!(reached(&cg, "Mixin::Add"), s(&[PUT_PLAIN]));
}

#[test]
fn reads_a_member_type_of_a_parameter() {
    let (_d, cg) = indexed(&[(
        "all.cc",
        lines(&[
            "struct Key { static void Make() {} };",
            "struct Lock { static void Make() {} };",
            "struct Traits { using K = Key; struct Nested { static void Go() {} }; };",
            "template <typename T>",
            "struct User {",
            "  using Mine = typename T::K;",
            "  void Run() { Mine::Make(); T::Nested::Go(); }",
            "};",
            "User<Traits> user;",
        ]),
    )]);
    assert_eq!(reached(&cg, "User::Run"), s(&["Key::Make", "Traits::Nested::Go"]));
}

#[test]
fn reads_a_member_enum_of_a_parameter() {
    let (_d, cg) = indexed(&[
        ("item.h", lines(&["struct Item { enum Kind { One }; };", "struct Odd { enum Kind { Two }; };"])),
        ("box.h", lines(&["#include \"item.h\"", "template <typename T>", "struct Box { void Run() { using K = typename T::Kind; K kind(0); } };"])),
        ("use.cc", lines(&["#include \"box.h\"", "Box<Item> box;"])),
    ]);
    assert_eq!(reached(&cg, "Box::Run"), s(&["Item::Kind"]));
}

#[test]
fn constructs_through_a_parameter() {
    let (_d, cg) = indexed(&[(
        "all.cc",
        lines(&["struct Two { Two(int a, int b) {} };", "template <typename T>", "void Make() {", "  T value(1, 2);", "  T* made = new T(3, 4);", "}", "void Run() { Make<Two>(); }"]),
    )]);
    let edges = template_edges(&cg);
    let of = |kind: &str| edges.iter().filter(|e| e.kind == kind).map(|e| e.target.clone()).collect::<Vec<_>>();
    assert_eq!(of("instantiates"), s(&["Two"]), "{edges:#?}");
    assert_eq!(of("calls"), s(&["Two::Two"]), "{edges:#?}");
}

// ---- what is no instantiation ----

#[test]
fn reads_nothing_from_a_comment_a_string_a_raw_string_or_a_macro_body() {
    let (_d, cg) = uses(&lines(&[
        "// Mixin<PlainTxn> in a line comment",
        "/* Mixin<PlainTxn> in a block",
        "   Mixin<PlainTxn> comment */",
        "const char* s = \"Mixin<PlainTxn>\";",
        "const char* r = R\"cc(",
        "  Mixin<PlainTxn> raw;",
        ")cc\";",
        "#define USES Mixin<PlainTxn>",
        "#define CONTINUED \\",
        "  Mixin<PlainTxn>",
    ]));
    assert_eq!(template_edges(&cg), []);
}

#[test]
fn reads_no_typed_suite_from_a_macro_body() {
    let (_d, cg) = indexed(&[(
        "a_test.cc",
        lines(&[
            "#include <gtest/gtest.h>",
            "struct A { static int Go() { return 1; } };",
            "struct B { static int Go() { return 2; } };",
            "template <typename T> class T1 : public ::testing::Test {};",
            "#define NEVER TYPED_TEST_SUITE(T1, ::testing::Types<B>)",
            "TYPED_TEST_SUITE(T1, ::testing::Types<A>);",
            "TYPED_TEST(T1, Works) { TypeParam::Go(); }",
        ]),
    )]);
    assert_eq!(targets(&cg), s(&["A::Go"]));
}

#[test]
fn reads_no_longer_name_that_ends_in_the_templates() {
    let (_d, cg) = uses("template <typename T> struct SuperMixin {};\nSuperMixin<FancyTxn> other;\nMixin<PlainTxn> mine;");
    assert_eq!(reached(&cg, "Mixin::Add"), s(&[PUT_PLAIN]));
}

#[test]
fn reads_no_friend_and_no_comparison() {
    let (_d, cg) = uses("class Friendly { friend class Mixin<PlainTxn>; };\nint Mixin = 3;\nbool Less(int PlainTxn, int x) { return Mixin < PlainTxn && x > 1; }");
    assert_eq!(template_edges(&cg), []);
}

#[test]
fn keeps_a_namesake_template_out() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("mixin.h", lines(&["#include \"txn.h\"", "namespace mine {", "template <typename Txn>", "class Mixin { public: void Add(int k) { Txn::Put(k); } };", "}"])),
        ("other.h", lines(&["namespace yours {", "template <typename X> class Mixin {};", "}"])),
        ("use.cc", lines(&["#include \"mixin.h\"", "#include \"other.h\"", "yours::Mixin<PlainTxn> elsewhere;", "mine::Mixin<FancyTxn> here;"])),
    ]);
    assert_eq!(reached(&cg, "mine::Mixin::Add"), s(&["FancyTxn::Put"]));
}

#[test]
fn keeps_a_template_of_a_source_file_to_that_file() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("a.cc", lines(&["#include \"txn.h\"", "template <typename T> struct Local { void Go() { T::Make(); } };", "Local<PlainTxn> a;"])),
        ("b.cc", lines(&["#include \"txn.h\"", "template <typename T> struct Local { void Go() { T::Make(); } };", "Local<FancyTxn> b;"])),
    ]);
    let mut edges: Vec<String> = template_edges(&cg).into_iter().map(|e| format!("{} {}", e.target, e.registered_at.unwrap_or_default())).collect();
    edges.sort();
    assert_eq!(edges, s(&["FancyTxn::Make b.cc:3", "PlainTxn::Make a.cc:3"]));
}

#[test]
fn keeps_a_call_from_a_file_that_cannot_see_the_function_template_out() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("run.h", lines(&["#include \"txn.h\"", "template <typename T>", "void Run() { T::Make(); }"])),
        ("use.cc", lines(&["#include \"run.h\"", "void Use() { Run<PlainTxn>(); }"])),
        ("other.cc", lines(&["#include \"txn.h\"", "void Elsewhere() { Run<FancyTxn>(); }"])),
    ]);
    assert_eq!(reached(&cg, "Run"), s(&["PlainTxn::Make"]));
}

#[test]
fn keeps_a_member_template_call_out_whatever_it_shares_a_name_with() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("get.h", lines(&["#include \"txn.h\"", "template <typename T> struct get { void Run() { T::Make(); } };"])),
        ("use.cc", lines(&["#include \"get.h\"", "void Run(Holder& h, Holder* p) { h.get<PlainTxn>(); p->get<PlainTxn>(); }"])),
    ]);
    assert_eq!(template_edges(&cg), []);
}

#[test]
fn reads_a_parameter_in_a_head_no_node_holds_as_a_parameter() {
    let (_d, cg) = indexed(&[(
        "all.h",
        lines(&[
            "struct T { static void Go() {} };",
            "template <typename U> struct Box { void Run() { U::Go(); } };",
            "template <typename T> Box<T> Make();",
            "template <typename T, typename V = Box<T>> struct Other {};",
        ]),
    )]);
    assert_eq!(template_edges(&cg), []);
}

#[test]
fn keeps_two_headers_that_declare_one_fixture_name_apart() {
    let test = |kind: &str| lines(&["template <typename T> class Shared : public ::testing::Test {};", "TYPED_TEST_SUITE_P(Shared);", &format!("TYPED_TEST_P(Shared, {kind}) {{ TypeParam::Go(); }}")]);
    let unit = |kind: &str| {
        lines(&[
            &format!("#include \"{}_test.h\"", kind.to_lowercase()),
            &format!("struct {kind}Type {{ static int Go() {{ return 1; }} }};"),
            &format!("INSTANTIATE_TYPED_TEST_SUITE_P({kind}, Shared, ::testing::Types<{kind}Type>);"),
        ])
    };
    let (_d, cg) = indexed(&[("map_test.h", test("Map")), ("set_test.h", test("Set")), ("map_test.cc", unit("Map")), ("set_test.cc", unit("Set"))]);
    let mut pairs: Vec<(String, String)> = template_edges(&cg).into_iter().map(|e| (e.source_file, e.target)).collect();
    pairs.sort();
    assert_eq!(pairs, [("map_test.h".to_string(), "MapType::Go".to_string()), ("set_test.h".to_string(), "SetType::Go".to_string())]);
}

#[test]
fn keeps_a_vendored_copy_of_a_class_out_of_a_file_that_does_not_include_it() {
    let (_d, cg) = indexed(&[
        ("include/item.h", lines(&["namespace lib { struct Item { static void Go() {} }; }"])),
        ("tests/abi/item.h", lines(&["namespace lib { struct Item { static void Go() {} }; }"])),
        ("include/box.h", lines(&["#include \"item.h\"", "namespace lib {", "template <typename T>", "struct Box { void Run() { T::Go(); } };", "}"])),
        ("tests/use.cc", lines(&["#include \"../include/box.h\"", "lib::Box<lib::Item> b;"])),
    ]);
    let edges = template_edges(&cg);
    assert_eq!(edges.iter().map(|e| e.registered_at.clone()).collect::<Vec<_>>(), [Some("tests/use.cc:2".to_string())]);
    assert_eq!(edges.iter().map(|e| e.target_file.as_str()).collect::<Vec<_>>(), ["include/item.h"]);
}

#[test]
fn a_vendored_copy_under_a_plain_namespace_is_not_what_an_alias_names_when_a_macro_hides_the_real_one() {
    let forms = [("at file scope", lines(&["using Mine = lib::Item;", "Box<Mine> b;"])), ("in a function", lines(&["void Use() {", "  using Mine = lib::Item;", "  Box<Mine> b;", "}"]))];
    for (place, code) in forms {
        let (_d, cg) = indexed(&[
            ("include/macros.h", lines(&["#define NS_BEGIN namespace lib {", "#define NS_END }"])),
            ("include/item.h", lines(&["#include \"macros.h\"", "NS_BEGIN", "struct Item { static void Go() {} };", "NS_END"])),
            ("tests/abi/item.h", lines(&["namespace lib { struct Item { static void Go() {} }; }"])),
            ("include/box.h", lines(&["#include \"item.h\"", "template <typename T>", "struct Box { void Run() { T::Go(); } };"])),
            ("tests/use.cc", lines(&["#include \"../include/box.h\"", &code])),
        ]);
        let vendored: Vec<Row> = template_edges(&cg).into_iter().filter(|e| e.target_file.starts_with("tests/")).collect();
        assert_eq!(vendored, [], "an alias {place}");
    }
}

#[test]
fn a_member_alias_names_a_class_the_file_declaring_it_can_see_not_a_vendored_copys() {
    let (_d, cg) = indexed(&[
        ("macros.h", lines(&["#define NS_BEGIN namespace lib {", "#define NS_END }"])),
        ("real/part.h", lines(&["#include \"../macros.h\"", "NS_BEGIN", "struct Part { static void Make() {} };", "NS_END"])),
        ("vendored/part.h", lines(&["namespace lib { struct Part { static void Make() {} }; }"])),
        ("real/item.h", lines(&["#include \"part.h\"", "struct Item { using P = ::lib::Part; };"])),
        ("other.h", lines(&["struct Lone { static void Make() {} };"])),
        ("box.h", lines(&["#include \"real/item.h\"", "#include \"other.h\"", "template <typename T>", "struct Box { void Run() { using P = typename T::P; P::Make(); } };"])),
        ("use.cc", lines(&["#include \"box.h\"", "Box<Item> b;"])),
    ]);
    let vendored: Vec<Row> = template_edges(&cg).into_iter().filter(|e| e.target == "lib::Part::Make" && e.target_file == "vendored/part.h").collect();
    assert_eq!(vendored, []);
}

#[test]
fn a_member_alias_of_a_vendored_copy_is_no_member_of_the_class_the_argument_names() {
    let (_d, cg) = indexed(&[
        ("real/item.h", lines(&["struct Item {};"])),
        ("vendored/item.h", lines(&["struct Item { using Spare = int; };"])),
        ("box.h", lines(&["#include \"real/item.h\"", "template <typename T>", "struct Box { void Run() { using S = typename T::Spare; S spare(0); } };"])),
        ("use.cc", lines(&["#include \"box.h\"", "Box<Item> b;"])),
    ]);
    assert_eq!(template_edges(&cg), []);
}

// ---- specializations ----

fn traits_header() -> String {
    lines(&[
        "#include \"txn.h\"",
        "template <typename T, typename U = void>",
        "struct Traits {",
        "  void Primary() { T::Make(); }",
        "};",
        "template <>",
        "struct Traits<PlainTxn> {",
        "  void Full() {}",
        "};",
        "template <typename U>",
        "struct Traits<FancyTxn, U> {",
        "  void Partial() { U::Make(); }",
        "};",
        "template <typename T>",
        "struct Traits<T*, int> {",
        "  void Pointer() { T::Make(); }",
        "};",
    ])
}

fn third_txn() -> String {
    lines(&[&txn(), "class ThirdTxn { public: static void Make() {} };"])
}

#[test]
fn an_instantiation_a_full_specialization_takes_does_not_run_the_template() {
    let (_d, cg) = indexed(&[("txn.h", third_txn()), ("traits.h", traits_header()), ("use.cc", lines(&["#include \"traits.h\"", "Traits<PlainTxn> full;", "Traits<ThirdTxn> plain;"]))]);
    assert_eq!(reached(&cg, "Traits::Primary"), s(&["ThirdTxn::Make"]));
}

#[test]
fn a_partial_specialization_reads_its_own_parameter_from_what_it_matches() {
    let (_d, cg) = indexed(&[("txn.h", third_txn()), ("traits.h", traits_header()), ("use.cc", lines(&["#include \"traits.h\"", "Traits<FancyTxn, PlainTxn> a;", "Traits<ThirdTxn*, int> b;"]))]);
    assert_eq!(reached(&cg, "<anonymous>::Partial"), s(&["PlainTxn::Make"]), "{:#?}", template_edges(&cg));
    assert_eq!(reached(&cg, "<anonymous>::Pointer"), s(&["ThirdTxn::Make"]));
    assert_eq!(reached(&cg, "Traits::Primary"), Vec::<String>::new());
}

#[test]
fn an_instantiation_no_specialization_matches_stays_with_the_template() {
    let (_d, cg) = indexed(&[("txn.h", third_txn()), ("traits.h", traits_header()), ("use.cc", lines(&["#include \"traits.h\"", "Traits<ThirdTxn, int> a;"]))]);
    assert_eq!(reached(&cg, "Traits::Primary"), s(&["ThirdTxn::Make"]));
    assert_eq!(reached(&cg, "<anonymous>::Pointer"), Vec::<String>::new());
}

#[test]
fn each_specialization_keeps_to_the_instantiations_it_takes() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        (
            "holder.h",
            lines(&[
                "#include \"txn.h\"",
                "template <typename T> struct Holder {};",
                "template <typename T> struct Holder<T*> { void Pointer() { T::Make(); } };",
                "template <typename T> struct Holder<T&> { void Ref() { T::Make(); } };",
            ]),
        ),
        ("use.cc", lines(&["#include \"holder.h\"", "Holder<PlainTxn*> a;", "Holder<FancyTxn&> b;"])),
    ]);
    assert_eq!(reached(&cg, "<anonymous>::Pointer"), s(&["PlainTxn::Make"]));
    assert_eq!(reached(&cg, "<anonymous>::Ref"), s(&["FancyTxn::Make"]));
}

#[test]
fn reads_a_member_of_a_partial_specialization_defined_outside_it() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        (
            "pair.h",
            lines(&["#include \"txn.h\"", "template <typename T, typename U>", "struct Pair { void Go(); };", "template <typename T>", "struct Pair<T, int> { void Go(); };", "template <typename T>", "void Pair<T, int>::Go() { T::Make(); }"]),
        ),
        ("use.cc", lines(&["#include \"pair.h\"", "Pair<PlainTxn, int> a;", "Pair<FancyTxn, long> b;"])),
    ]);
    assert_eq!(targets(&cg), s(&["PlainTxn::Make"]));
}

#[test]
fn a_condition_no_one_can_read_keeps_an_instantiation_out_of_the_template() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        (
            "traits.h",
            lines(&[
                "#include \"txn.h\"",
                "template <typename T, typename Enable = void>",
                "struct Traits { void Primary() { T::Make(); } };",
                "template <typename T>",
                "struct Traits<T, std::enable_if_t<sizeof(T) == 1>> { void Small() {} };",
            ]),
        ),
        ("use.cc", lines(&["#include \"traits.h\"", "Traits<PlainTxn> maybe;"])),
    ]);
    assert_eq!(reached(&cg, "Traits::Primary"), Vec::<String>::new());
}

#[test]
fn a_member_specialized_out_of_line_leaves_the_instantiation_with_the_rest() {
    let (_d, cg) = uses("template <> void Mixin<PlainTxn>::Finish() {}\nMixin<PlainTxn> p;");
    assert_eq!(reached(&cg, "Mixin::Add"), s(&[PUT_PLAIN]));
}

// ---- the cap ----

const BOX: &str = "template <typename T>\nstruct Box { void Run() { T::Go(); } };\n";

fn classes(n: usize) -> String {
    (0..n).map(|i| format!("struct C{i} {{ static void Go() {{}} }};")).collect::<Vec<_>>().join("\n")
}

fn box_uses(n: usize) -> String {
    (0..n).map(|i| format!("Box<C{i}> b{i};")).collect::<Vec<_>>().join("\n")
}

#[test]
fn links_every_class_up_to_the_cap() {
    let (_d, cg) = indexed(&[("all.cc", format!("{BOX}{}\n{}\n", classes(ARGUMENT_CAP), box_uses(ARGUMENT_CAP)))]);
    assert_eq!(reached(&cg, "Box::Run").len(), ARGUMENT_CAP);
}

#[test]
fn links_none_past_the_cap_not_some() {
    let n = ARGUMENT_CAP + 1;
    let (_d, cg) = indexed(&[("all.cc", format!("{BOX}{}\n{}\n", classes(n), box_uses(n)))]);
    assert_eq!(reached(&cg, "Box::Run"), Vec::<String>::new());
}

#[test]
fn the_cap_counts_classes_not_sites() {
    let sites = (0..ARGUMENT_CAP * 3).map(|i| format!("Box<C0> s{i};")).collect::<Vec<_>>().join("\n");
    let (_d, cg) = indexed(&[("all.cc", format!("{BOX}{}\n{sites}\n", classes(1)))]);
    assert_eq!(reached(&cg, "Box::Run"), s(&["C0::Go"]));
}

#[test]
fn the_cap_counts_a_chain_with_the_classes_it_reaches() {
    let n = ARGUMENT_CAP + 1;
    let wrapped = (0..n).map(|i| format!("Wrap<C{i}> w{i};")).collect::<Vec<_>>().join("\n");
    let (_d, cg) = indexed(&[("all.cc", format!("{BOX}template <typename U> struct Wrap : Box<U> {{}};\n{}\n{wrapped}\n", classes(n)))]);
    assert_eq!(reached(&cg, "Box::Run"), Vec::<String>::new());
}

// ---- the site cap ----

#[test]
fn reads_a_class_template_written_in_as_many_files_as_the_site_cap_allows_and_leaves_one_more_unread() {
    let mut files = vec![
        (
            "box.h".to_string(),
            lines(&[
                "struct Foo { static void Go() {} };",
                "struct Bar { static void Go() {} };",
                "template <typename T> struct Edge { void Run() { T::Go(); } };",
                "template <typename T> struct Wide { void Run() { T::Go(); } };",
                "template <typename T> struct Held { void Run() { T::Go(); } };",
                "template <typename U> struct Holder { Held<U> held; };",
            ]),
        ),
        ("direct.cc".to_string(), lines(&["#include \"box.h\"", "Held<Foo> direct;"])),
    ];
    for i in 0..=SITE_CAP {
        let edge = if i < SITE_CAP { format!("Edge<Foo> e{i};") } else { String::new() };
        files.push((format!("use/{i}.cc"), lines(&["#include \"../box.h\"", &format!("Wide<Foo> w{i};"), &format!("Holder<Bar> h{i};"), &edge])));
    }
    let files: Vec<(&str, String)> = files.iter().map(|(p, c)| (p.as_str(), c.clone())).collect();
    // (`Holder` is written in one file more than the cap allows too.)
    let (_d, cg) = indexed(&files);
    assert_eq!(reached(&cg, "Edge::Run"), s(&["Foo::Go"]));
    assert_eq!(reached(&cg, "Wide::Run"), Vec::<String>::new());
    // A parameter whose walk reaches such a template gets none of what it could read.
    assert_eq!(reached(&cg, "Held::Run"), Vec::<String>::new());
}

#[test]
fn reads_a_function_template_with_as_many_calls_as_the_site_cap_allows_and_leaves_one_more_unread() {
    let mut body = vec![
        "struct Foo { static void Go() {} };".to_string(),
        "struct Bar { static void Go() {} };".to_string(),
        "template <typename T> void Edge() { T::Go(); }".to_string(),
        "template <typename T> void Wide() { T::Go(); }".to_string(),
        "void Run() {".to_string(),
    ];
    body.extend((0..SITE_CAP).map(|_| "  Edge<Foo>();".to_string()));
    body.extend((0..=SITE_CAP).map(|_| "  Wide<Foo>();".to_string()));
    body.push("}".to_string());
    let body: Vec<&str> = body.iter().map(String::as_str).collect();
    let (_d, cg) = indexed(&[("all.cc", lines(&body))]);
    assert_eq!(reached(&cg, "Edge"), s(&["Foo::Go"]));
    assert_eq!(reached(&cg, "Wide"), Vec::<String>::new());
}

// ---- a namespace alias ----

const NS_BOX: &str = "template <typename T>\nstruct Box { void Run() { T::Make(); } };\n";

fn items(ns: &str) -> String {
    format!("namespace {ns} {{ struct Item {{ static void Make() {{}} }}; }}")
}

#[test]
fn a_namespace_alias_names_a_class_from_a_file_that_is_not_ascii() {
    let (_d, cg) = indexed(&[
        ("ns.h", lines(&["// caf\u{e9}, \u{4e2d}\u{6587}", &items("one"), &items("two"), "namespace al = one;"])),
        ("box.h", NS_BOX.to_string()),
        ("use.cc", lines(&["#include \"ns.h\"", "#include \"box.h\"", "Box<al::Item> b;"])),
    ]);
    assert_eq!(reached(&cg, "Box::Run"), s(&["one::Item::Make"]));
}

#[test]
fn a_namespace_alias_names_a_class_in_a_sync() {
    let (dir, mut cg) = indexed(&[
        ("ns.h", lines(&[&items("one"), &items("two"), "namespace al = one;"])),
        ("box.h", NS_BOX.to_string()),
        ("use.cc", lines(&["#include \"ns.h\"", "#include \"box.h\"", "Box<al::Item> b;"])),
    ]);
    write(dir.path(), &[("box.h", format!("{NS_BOX}// edited\n"))]);
    cg.sync(&Default::default(), None).unwrap();
    assert_eq!(reached(&cg, "Box::Run"), s(&["one::Item::Make"]));
}

#[test]
fn a_namespace_alias_means_what_its_first_declaration_says_in_file_order() {
    let (_d, cg) = indexed(&[
        ("ns.h", lines(&[&items("one"), &items("two")])),
        ("a.h", lines(&["namespace al = one;"])),
        ("b.h", lines(&["namespace al = two;"])),
        ("box.h", NS_BOX.to_string()),
        ("use.cc", lines(&["#include \"ns.h\"", "#include \"b.h\"", "#include \"a.h\"", "#include \"box.h\"", "Box<al::Item> b;"])),
    ]);
    assert_eq!(reached(&cg, "Box::Run"), s(&["one::Item::Make"]));
}

// ---- sync matches a fresh index ----

fn as_text(cg: &CodeGraph) -> Vec<String> {
    let mut rows: Vec<String> = template_edges(cg).into_iter().map(|e| format!("{e:?}")).collect();
    rows.sort();
    rows
}

fn sync_base() -> Vec<(&'static str, String)> {
    vec![("txn.h", txn()), ("mixin.h", mixin()), ("keep.cc", lines(&["#include \"mixin.h\"", "Mixin<PlainTxn> kept;"]))]
}

/// Index `before`, write `after`, sync, and compare with a fresh index of `after`.
fn syncs(before: &[(&'static str, String)], after: &[(&'static str, String)], removed: &[&str]) -> Vec<String> {
    let (dir, mut cg) = indexed(before);
    write(dir.path(), after);
    for file in removed {
        fs::remove_file(dir.path().join(file)).unwrap();
    }
    cg.sync(&Default::default(), None).unwrap();
    let mut merged: Vec<(&str, String)> = before.iter().filter(|(p, _)| !removed.contains(p) && !after.iter().any(|(a, _)| a == p)).cloned().collect();
    merged.extend(after.iter().cloned());
    let (_fresh_dir, fresh) = indexed(&merged);
    assert_eq!(as_text(&cg), as_text(&fresh));
    reached(&cg, "Mixin::Add")
}

fn with(mut base: Vec<(&'static str, String)>, extra: (&'static str, String)) -> Vec<(&'static str, String)> {
    base.push(extra);
    base
}

#[test]
fn sync_an_instantiation_added_in_a_third_file() {
    assert_eq!(syncs(&sync_base(), &[("added.cc", lines(&["#include \"mixin.h\"", "Mixin<FancyTxn> added;"]))], &[]), s(&["FancyTxn::Put", PUT_PLAIN]));
}

#[test]
fn sync_an_instantiation_changed_in_a_third_file() {
    let before = with(sync_base(), ("third.cc", lines(&["#include \"mixin.h\"", "Mixin<FancyTxn> third;"])));
    assert_eq!(syncs(&before, &[("third.cc", lines(&["#include \"mixin.h\"", "Mixin<NoPutTxn> third;"]))], &[]), s(&[PUT_PLAIN]));
}

#[test]
fn sync_an_instantiation_removed_from_a_third_file() {
    let before = with(sync_base(), ("third.cc", lines(&["#include \"mixin.h\"", "Mixin<FancyTxn> third;"])));
    assert_eq!(syncs(&before, &[("third.cc", lines(&["#include \"mixin.h\"", "int nothing;"]))], &[]), s(&[PUT_PLAIN]));
}

#[test]
fn sync_the_file_holding_an_instantiation_deleted() {
    let before = with(sync_base(), ("third.cc", lines(&["#include \"mixin.h\"", "Mixin<FancyTxn> third;"])));
    assert_eq!(syncs(&before, &[], &["third.cc"]), s(&[PUT_PLAIN]));
}

#[test]
fn sync_a_file_holding_nothing_but_an_instantiation() {
    let before = with(with(sync_base(), ("bare.hpp", "int nothing;\n".to_string())), ("unit.cc", lines(&["#include \"mixin.h\"", "#include \"bare.hpp\""])));
    assert_eq!(syncs(&before, &[("bare.hpp", "Mixin<FancyTxn> bare;\n".to_string())], &[]), s(&["FancyTxn::Put", PUT_PLAIN]));
    assert!(codegraph_resolve::callback_synthesizer::has_synthesis_pattern("bare.hpp", "Mixin<FancyTxn> bare;\n"));
}

#[test]
fn sync_a_member_added_to_an_argument() {
    let before = with(sync_base(), ("third.cc", lines(&["#include \"mixin.h\"", "Mixin<NoPutTxn> third;"])));
    let with_put = txn().replace("class NoPutTxn {};", "class NoPutTxn { public: void Put(int k) {} };");
    assert_eq!(syncs(&before, &[("txn.h", with_put)], &[]), s(&["NoPutTxn::Put", PUT_PLAIN]));
}

#[test]
fn sync_a_template_edited_to_call_another_member() {
    syncs(&sync_base(), &[("mixin.h", mixin().replace("Txn::Commit();", "Txn::Commit(); Txn::Make();"))], &[]);
}

#[test]
fn sync_a_typed_list_edited() {
    let test = |list: &str| {
        lines(&[
            "#include <gtest/gtest.h>",
            "struct A { static int Go() { return 1; } };",
            "struct B { static int Go() { return 2; } };",
            "template <typename T> class T1 : public ::testing::Test {};",
            &format!("TYPED_TEST_SUITE(T1, ::testing::Types<{list}>);"),
            "TYPED_TEST(T1, Works) { TypeParam::Go(); }",
        ])
    };
    let (dir, mut cg) = indexed(&[("a_test.cc", test("A"))]);
    write(dir.path(), &[("a_test.cc", test("A, B"))]);
    cg.sync(&Default::default(), None).unwrap();
    let (_f, fresh) = indexed(&[("a_test.cc", test("A, B"))]);
    assert_eq!(as_text(&cg), as_text(&fresh));
    let mut t = targets(&cg);
    t.sort();
    assert_eq!(t, s(&["A::Go", "B::Go"]));
}

// ---- determinism ----

#[test]
fn names_the_first_site_by_file_and_line_whichever_a_run_reads_first() {
    let (_d, cg) = indexed(&[
        ("txn.h", txn()),
        ("mixin.h", mixin()),
        ("b.cc", lines(&["#include \"mixin.h\"", "Mixin<FancyTxn> b;", "Mixin<PlainTxn> again;"])),
        ("a.cc", lines(&["#include \"mixin.h\"", "Mixin<PlainTxn> a;"])),
    ]);
    let at = template_edges(&cg).into_iter().find(|e| e.target == PUT_PLAIN).and_then(|e| e.registered_at);
    assert_eq!(at.as_deref(), Some("a.cc:2"));
}
