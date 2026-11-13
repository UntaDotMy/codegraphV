//! Upstream 25144678: a Java, Kotlin, or C# supertype written as `Outer.Inner` names
//! that nested type, and a Kotlin `object : Listener { … }` is an anonymous class.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

fn index() -> (tempfile::TempDir, CodeGraph) {
    let dir = tempfile::tempdir().unwrap();
    let files: &[(&str, &str)] = &[
        (
            "java/p/Api.java",
            "package p;\npublic interface Api {\n    interface Listener { void on(); }\n}\n",
        ),
        (
            "java/p/Outer.java",
            "package p;\npublic class Outer {\n    public static class Inner { public void run() {} }\n}\n",
        ),
        (
            "java/p/Users.java",
            "package p;\nclass Named extends Outer.Inner {\n    @Override public void run() {}\n}\nclass Impl implements Api.Listener {\n    public void on() {}\n}\nclass Host {\n    Api.Listener field = new Api.Listener() {\n        public void on() {}\n    };\n}\n",
        ),
        (
            "java/a/Box.java",
            "package a;\npublic class Box { public static class Part { public void use() {} } }\n",
        ),
        (
            "java/b/Box.java",
            "package b;\npublic class Box { public static class Part { public void use() {} } }\n",
        ),
        (
            "java/c/Thing.java",
            "package c;\nimport a.Box;\nclass Thing extends Box.Part {\n    public void use() {}\n}\n",
        ),
        (
            "java/d/Stray.java",
            "package d;\nclass Stray extends Box.Part {\n    public void use() {}\n}\n",
        ),
        (
            "java/b2/Message.java",
            "package b2;\npublic class Message {\n    public static class Builder<T extends Builder<T>> {\n        public T content(String c) { return null; }\n    }\n}\n",
        ),
        (
            "java/b2/Use.java",
            "package b2;\nclass Other {\n    static class Builder {\n        Builder content(String c) { return this; }\n    }\n}\nclass Legacy {\n    static LegacyBuilder builder() { return new LegacyBuilder(); }\n    static final class LegacyBuilder extends Message.Builder<LegacyBuilder> {}\n}\nclass Use {\n    void run() { Legacy.builder().content(\"x\"); }\n}\n",
        ),
        (
            "kotlin/k/Outer.kt",
            "package k\n\nopen class KOuter {\n    open class KInner { open fun run() {} }\n    interface KListener { fun on() }\n}\n",
        ),
        (
            "kotlin/k/Users.kt",
            "package k\n\nclass KNamed : KOuter.KInner() {\n    override fun run() {}\n}\n\nclass KImpl : KOuter.KListener {\n    override fun on() {}\n}\n",
        ),
        (
            "kotlin/k/Flow.kt",
            "package k\n\ninterface Listener { fun onEvent() }\n\nabstract class Base(val n: Int) { abstract fun run() }\n\nclass Bus(private val listener: Listener) {\n    fun fire() { listener.onEvent() }\n}\n\nclass Host {\n    fun wire(): Bus = Bus(object : Listener {\n        override fun onEvent() { handled() }\n    })\n\n    fun build(): Base = object : Base(compute()) {\n        override fun run() { other() }\n    }\n\n    private val held = object : KOuter.KListener {\n        override fun on() { handled() }\n    }\n\n    fun handled() {}\n    fun other() {}\n    fun compute(): Int = 1\n}\n",
        ),
        (
            "kotlin/k/Client.kt",
            "package k\n\nclass Server {\n    fun url(path: String): String = path\n}\nclass Remote {\n    fun url(path: String): String = path\n}\ninterface Callback {\n    fun onResponse()\n}\nclass Caller(private val backup: Server) {\n    private val remote: Server = Server()\n    fun send(callback: Callback) {}\n    fun run() {\n        send(object : Callback {\n            override fun onResponse() {\n                remote.url(\"/a\")\n                backup.url(\"/b\")\n            }\n        })\n    }\n}\n",
        ),
        (
            "kotlin/k/JarApi.kt",
            "package k\n\ninterface Jar {\n    fun load()\n}\n",
        ),
        (
            "kotlin/k/Jar.kt",
            "package k\n\nenum class Override {\n    Jar,\n}\nclass JarUser {\n    fun make(): Any = object : Jar {\n        override fun load() {}\n    }\n}\n",
        ),
        (
            "kotlin/k/Wrap.kt",
            "package k\n\nabstract class Body {\n    abstract fun measure(): Long\n}\n\nfun wrap(original: Body?): Body =\n    object : Body() {\n        override fun measure(): Long = original!!.measure()\n    }\n\nclass Tasks {\n    interface Task {\n        fun ready(): Boolean\n    }\n    private val queue = ArrayList<Task>()\n    fun add() {\n        queue.add(object : Task {\n            override fun ready() = true\n        })\n    }\n    fun next(): Int = queue.indexOfFirst { it.ready() }\n}\n\ninterface ScrollListener {\n    fun onScroll(d: Int)\n}\nclass Scroller {\n    fun onScroll(d: Int) {}\n    val listener = object : ScrollListener {\n        override fun onScroll(d: Int) = this@Scroller.onScroll(d)\n    }\n}\n\nclass Pipe {\n    fun close() {}\n    val stream = object : java.io.FilterInputStream(null) {\n        override fun close() {\n            super.close()\n        }\n    }\n}\n",
        ),
        (
            "kotlin/k/Ext.kt",
            "package k\n\nfun String.produce(): Runnable = object : Runnable {\n    override fun run() {}\n    fun request(n: Long) {}\n}\nfun consume(items: List<Any>) {\n    items.forEach { it.request(1) }\n}\n",
        ),
        (
            "kotlin/k/Padding.kt",
            "package k\n\ninterface Padding { val block: Int }\n\nprivate val padding = object : Padding {\n    override val block = 1\n}\n",
        ),
        (
            "kotlin/k/Runner.kt",
            "package k\n\nfun runIt(block: () -> Unit) { block() }\n",
        ),
        (
            "java/v/Visit.java",
            "package v;\ninterface Visitor { void a(); void b(); }\nclass Walker {\n    static final Visitor VISITOR = new Visitor() {\n        public void a() { b(); }\n        public void b() {}\n    };\n}\n",
        ),
        (
            "csharp/Outer.cs",
            "namespace Shop\n{\n    public class Outer\n    {\n        public class Inner { public virtual void Run() {} }\n        public interface IListener { void On(); }\n    }\n}\n",
        ),
        (
            "csharp/Users.cs",
            "namespace Shop\n{\n    public class Named : Outer.Inner\n    {\n        public override void Run() {}\n    }\n    public class Impl : Outer.IListener\n    {\n        public void On() {}\n    }\n}\n",
        ),
    ];
    for (rel, source) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    (dir, cg)
}

fn kind_label(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Extends => "extends",
        EdgeKind::Implements => "implements",
        EdgeKind::Calls => "calls",
        EdgeKind::Instantiates => "instantiates",
        _ => "other",
    }
}

/// `kind qualified-name` of the extends/implements edges from the class named `name` in `file`.
fn supertypes(cg: &CodeGraph, file: &str, name: &str) -> Vec<String> {
    let nodes = cg.get_nodes_by_file(file).unwrap();
    let types: Vec<_> = nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Interface))
        .collect();
    let from = types
        .iter()
        .find(|n| n.name == name)
        .or_else(|| types.iter().find(|n| n.name.starts_with(name)))
        .unwrap_or_else(|| panic!("no type {name} in {file}; names: {:?}", types.iter().map(|n| n.name.as_str()).collect::<Vec<_>>()));
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&from.id).unwrap() {
        if matches!(edge.kind, EdgeKind::Extends | EdgeKind::Implements) {
            let target = cg.get_node(&edge.target).unwrap().expect("supertype target");
            out.push(format!("{} {}", kind_label(edge.kind), target.qualified_name));
        }
    }
    out.sort();
    out
}

/// Qualified names of the `kind` edges leaving the node whose qualified name is `qualified`.
fn linked(cg: &CodeGraph, qualified: &str, kind: EdgeKind) -> Vec<String> {
    let simple = qualified.rsplit("::").next().unwrap_or(qualified);
    let nodes = cg.get_nodes_by_name(simple).unwrap();
    let from = nodes
        .iter()
        .find(|n| n.qualified_name == qualified)
        .unwrap_or_else(|| panic!("no node {qualified}; candidates: {:?}", nodes.iter().map(|n| n.qualified_name.as_str()).collect::<Vec<_>>()));
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&from.id).unwrap() {
        if edge.kind == kind {
            let target = cg.get_node(&edge.target).unwrap().expect("edge target");
            out.push(target.qualified_name);
        }
    }
    out.sort();
    out
}

fn calls(cg: &CodeGraph, qualified: &str) -> Vec<String> {
    linked(cg, qualified, EdgeKind::Calls)
}

#[test]
fn java_same_package_dotted_supertype_and_anonymous_class() {
    let (_dir, cg) = index();
    assert_eq!(supertypes(&cg, "java/p/Users.java", "Named"), vec!["extends p::Outer::Inner"]);
    assert_eq!(supertypes(&cg, "java/p/Users.java", "Impl"), vec!["implements p::Api::Listener"]);
    assert_eq!(supertypes(&cg, "java/p/Users.java", "<Listener$anon@"), vec!["implements p::Api::Listener"]);
    cg.close();
}

#[test]
fn nested_builder_content_is_not_the_namesake() {
    let (_dir, cg) = index();
    assert_eq!(supertypes(&cg, "java/b2/Use.java", "LegacyBuilder"), vec!["extends b2::Message::Builder"]);
    let calls = calls(&cg, "b2::Use::run");
    assert!(calls.contains(&"b2::Message::Builder::content".to_string()), "{calls:?}");
    assert!(!calls.contains(&"b2::Other::Builder::content".to_string()), "{calls:?}");
    cg.close();
}

#[test]
fn imported_box_part_links_and_unnamed_root_does_not() {
    let (_dir, cg) = index();
    assert_eq!(supertypes(&cg, "java/c/Thing.java", "Thing"), vec!["extends a::Box::Part"]);
    assert_eq!(supertypes(&cg, "java/d/Stray.java", "Stray"), Vec::<String>::new());
    cg.close();
}

#[test]
fn kotlin_dotted_supertype_is_the_nested_type() {
    let (_dir, cg) = index();
    assert_eq!(supertypes(&cg, "kotlin/k/Users.kt", "KNamed"), vec!["extends k::KOuter::KInner"]);
    assert_eq!(supertypes(&cg, "kotlin/k/Users.kt", "KImpl"), vec!["implements k::KOuter::KListener"]);
    cg.close();
}

#[test]
fn kotlin_object_expression_is_an_anonymous_class() {
    let (_dir, cg) = index();
    let mut anon: Vec<String> = cg
        .get_nodes_by_file("kotlin/k/Flow.kt")
        .unwrap()
        .into_iter()
        .filter(|n| n.kind == NodeKind::Class && n.name.contains("$anon@"))
        .map(|n| n.qualified_name)
        .collect();
    anon.sort();
    assert_eq!(
        anon,
        vec![
            "k::Host::build::<Base$anon@16>".to_string(),
            "k::Host::held::<KListener$anon@20>".to_string(),
            "k::Host::wire::<Listener$anon@12>".to_string(),
        ]
    );
    assert_eq!(supertypes(&cg, "kotlin/k/Flow.kt", "<Listener$anon@"), vec!["implements k::Listener"]);
    assert_eq!(supertypes(&cg, "kotlin/k/Flow.kt", "<Base$anon@"), vec!["extends k::Base"]);
    assert_eq!(supertypes(&cg, "kotlin/k/Flow.kt", "<KListener$anon@"), vec!["implements k::KOuter::KListener"]);
    assert_eq!(linked(&cg, "k::Host::held", EdgeKind::Instantiates), vec!["k::KOuter::KListener".to_string()]);
    assert_eq!(
        linked(&cg, "k::Host::wire", EdgeKind::Instantiates),
        vec!["k::Bus".to_string(), "k::Listener".to_string()]
    );
    assert_eq!(linked(&cg, "k::JarUser::make", EdgeKind::Instantiates), vec!["k::Jar".to_string()]);
    cg.close();
}

#[test]
fn object_expression_header_body_interface_and_enclosing_properties() {
    let (_dir, cg) = index();
    let build = calls(&cg, "k::Host::build");
    assert!(build.contains(&"k::Host::compute".to_string()), "{build:?}");
    assert_eq!(calls(&cg, "k::Host::build::<Base$anon@16>::run"), vec!["k::Host::other".to_string()]);
    assert_eq!(calls(&cg, "k::Host::wire::<Listener$anon@12>::onEvent"), vec!["k::Host::handled".to_string()]);
    assert_eq!(calls(&cg, "k::Listener::onEvent"), vec!["k::Host::wire::<Listener$anon@12>::onEvent".to_string()]);
    assert_eq!(calls(&cg, "k::Base::run"), vec!["k::Host::build::<Base$anon@16>::run".to_string()]);
    assert_eq!(
        calls(&cg, "k::Caller::run::<Callback$anon@16>::onResponse"),
        vec!["k::Server::url".to_string(), "k::Server::url".to_string()]
    );
    cg.close();
}

#[test]
fn overrides_do_not_steal_their_names() {
    let (_dir, cg) = index();
    assert_eq!(calls(&cg, "k::wrap::<Body$anon@8>::measure"), vec!["k::Body::measure".to_string()]);
    assert_eq!(calls(&cg, "k::Scroller::listener::<ScrollListener$anon@30>::onScroll"), vec!["k::Scroller::onScroll".to_string()]);
    assert_eq!(calls(&cg, "k::Pipe::stream::<FilterInputStream$anon@37>::close"), Vec::<String>::new());
    assert_eq!(calls(&cg, "k::Tasks::next"), vec!["k::Tasks::Task::ready".to_string()]);
    assert!(calls(&cg, "k::runIt").iter().all(|qn| !qn.contains("$anon@")));
    assert!(calls(&cg, "k::consume").iter().all(|qn| !qn.contains("$anon@")));
    assert_eq!(
        calls(&cg, "v::Walker::VISITOR::<Visitor$anon@4>::a"),
        vec!["v::Walker::VISITOR::<Visitor$anon@4>::b".to_string()]
    );
    cg.close();
}

#[test]
fn csharp_dotted_base_is_the_nested_type() {
    let (_dir, cg) = index();
    assert_eq!(supertypes(&cg, "csharp/Users.cs", "Named"), vec!["extends Shop::Outer::Inner"]);
    assert_eq!(supertypes(&cg, "csharp/Users.cs", "Impl"), vec!["implements Shop::Outer::IListener"]);
    cg.close();
}

#[test]
fn unnamed_root_and_anonymous_sibling_call_stay_stable() {
    let (_dir, cg) = index();
    assert_eq!(supertypes(&cg, "java/d/Stray.java", "Stray"), Vec::<String>::new());
    assert_eq!(
        calls(&cg, "v::Walker::VISITOR::<Visitor$anon@4>::a"),
        vec!["v::Walker::VISITOR::<Visitor$anon@4>::b".to_string()]
    );
    cg.close();
}
