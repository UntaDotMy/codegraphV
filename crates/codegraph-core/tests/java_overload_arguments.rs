//! A Java call's arguments pick its overload, and a lambda's parameter is typed by the functional
//! interface it is written for (upstream be06c5e2, 6aa358cc; fixture from java-overload-arguments.test.ts).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, Node, NodeKind};
use std::fs;

const FILES: &[(&str, &str)] = &[
    ("src/n/Node.java", r#"package n;

public class Node {
    public Node before(String html) { return this; }
    public Node before(Node node) { return this; }
    public void remove(int index) {}
    public boolean remove(Object o) { return true; }
    public void pick(long id) {}
    public void pick(boolean flag) {}
    public void pick(char c) {}
    public void pick(double d) {}
    public void pick(String s) {}
    public void pick(Node node) {}
    public void pick(Node[] nodes) {}
    public void pick(int[] ids) {}
    public void log(String message) {}
    public void log(String format, Object... args) {}
    public void attach(Holder holder) {}
    public void attach(Other other) {}
    public <T> void generic(T value, String name) {}
    public <T> void generic(T value, int index) {}
}
"#),
    ("src/n/Holder.java", r#"package n;

public interface Holder {
}
"#),
    ("src/n/Other.java", r#"package n;

public class Other {
}
"#),
    ("src/n/Base.java", r#"package n;

public class Base implements Holder {
}
"#),
    ("src/n/Leaf.java", r#"package n;

public class Leaf extends Base {
}
"#),
    ("src/n/Element.java", r#"package n;

public class Element extends Node {
    private final String label = "x";
    private static final Node[] SHARED = new Node[0];

    @Override public Element before(String html) { return (Element) super.before(html); }
    @Override public Element before(Node node) { return (Element) super.before(node); }

    void removeAt(int index) { super.remove(index); }
    void removeOne(Object o) { super.remove(o); }

    void literals(Node[] nodes, int[] ids) {
        pick(1L);
        pick(true);
        pick('c');
        pick(1.5);
        pick(.5);
        pick(2d);
        pick("s");
        pick("a" + label);
        pick(new Node());
        pick(new Node[2]);
        pick(new int[] { 1 });
        pick((Node) nodes[0]);
        pick(nodes);
        pick(ids);
        pick(this);
        pick(label);
        pick(SHARED);
    }

    void chained() {
        pick(new Node().toString());
    }

    void widen(int small) {
        pick(small);
    }

    void locals() {
        final String text = "x";
        pick(text);
        for (Node each : new Node[0]) {
            pick(each);
        }
    }

    void fixedArityFirst() {
        log("only");
        log("with %s", label);
    }

    void subtypes(Leaf leaf, Other other) {
        attach(leaf);
        attach(other);
    }

    <T> void generics(T value) {
        generic(value, "n");
        generic(value, 1);
    }
}
"#),
    ("src/t/Builder.java", r#"package t;

public class Builder {
    public void process() {}
    public void other() {}
}
"#),
    ("src/t/Elsewhere.java", r#"package t;

public class Elsewhere {
    public void process() {}
    public void other() {}
}
"#),
    ("src/t/BuilderTest.java", r#"package t;

import java.util.function.BiConsumer;
import java.util.function.Consumer;

public class BuilderTest {
    void one() {
        check(new Builder(), tb -> {
            tb.process();
        });
    }

    void two() {
        both((left, right) -> {
            left.process();
            right.other();
        });
    }

    void library() {
        names(text -> text.process());
    }

    void onReceiver(java.util.List<Object> list) {
        list.forEach(item -> item.other());
    }

    private static void check(Builder tb, Consumer<Builder> exercise) { exercise.accept(tb); }
    private static void both(BiConsumer<Builder, Elsewhere> exercise) {}
    private static void names(Consumer<String> exercise) {}
}
"#),
    ("src/v/Operand.java", r#"package v;

public interface Operand {
}
"#),
    ("src/v/Expression.java", r#"package v;

public class Expression implements Operand {
}
"#),
    ("src/v/Converter.java", r#"package v;

public class Converter {
    public String convert(Operand operand) {
        return convert((Expression) operand);
    }

    private String convert(Expression expression) {
        return "";
    }

    private void hidden(Expression expression) {}

    public void hiddenOverload(Operand operand) {}
}
"#),
    ("src/v/Caller.java", r#"package v;

public class Caller {
    private static final Converter CONVERTER = new Converter();

    String fromAnotherFile(Expression e) {
        return CONVERTER.convert(e);
    }
}
"#),
    ("src/g/Message.java", r#"package g;

public class Message {
}
"#),
    ("src/g/Errors.java", r#"package g;

import java.util.Collection;

public class Errors {
    private Message merge(Message message) { return message; }
    public Errors merge(Collection<Message> messages) { return this; }
    public Errors merge(Errors more) { return this; }
    public Errors merge(Thrown thrown) { return this; }
}
"#),
    ("src/g/Thrown.java", r#"package g;

import java.util.Collection;

public class Thrown extends RuntimeException {
    public static Errors shared() { return null; }
    public static Errors shared(String name) { return null; }
    public Errors getErrors() { return null; }
    public Collection<Message> getMessages() { return null; }
    public Thrown self() { return this; }
    public Message one() { return null; }
    public Errors mixed(String name) { return null; }
    public Collection<Message> mixed(int size) { return null; }
    public Errors counted() { return null; }
    public String counted(String name) { return name; }
}
"#),
    ("src/g/Handler.java", r#"package g;

public class Handler {
    private final Thrown held = new Thrown();

    void caught(Errors errors) {
        try {
            run();
        } catch (Thrown e) {
            errors.merge(e.getErrors());
            errors.merge(e.getMessages());
        }
    }

    void onField(Errors errors) {
        errors.merge(held.getErrors());
        errors.merge(this.held.getMessages());
    }

    void onType(Errors errors) {
        errors.merge(Thrown.shared());
    }

    void longChain(Errors errors, Thrown e) {
        errors.merge(e.self().getErrors());
    }

    void overloadsDisagree(Errors errors, Thrown e) {
        errors.merge(e.mixed(make2()));
    }

    void overloadsOfItsCount(Errors errors, Thrown e) {
        errors.merge(e.counted());
    }

    void receiverless(Errors errors) {
        errors.merge(make());
    }

    private Errors make() { return null; }
    private String make2() { return null; }
    private void run() {}
}
"#),
    ("src/c/Syntax.java", r#"package c;

public class Syntax {
    public void add(Syntax... args) {}
    public void add(String format) {}
    public void value(java.util.function.Function<String, Integer> supplied) {}
    public <T> void value(T plain) {}
    public void bytes(java.nio.ByteBuffer key) {}
    public void bytes(byte[] key) {}
    public void shared(Syntax first, Runnable run) {}
    public void shared(Object first, Runnable run) {}
}
"#),
    ("src/c/Registrar.java", r#"package c;

public class Registrar {
    private void work() {}

    void varargsStays(Syntax syntax) {
        syntax.add(new Syntax());
        syntax.add(unread());
    }

    void literalIsNoFunction(Syntax syntax) {
        syntax.value(6);
        syntax.value("six");
    }

    void variableArity(Syntax syntax, byte... many) {
        syntax.bytes(many);
    }

    void arrayMarkersAfterTheName(Syntax syntax, byte other[]) {
        final byte key[] = new byte[1];
        syntax.bytes(key);
        syntax.bytes(other);
    }

    void sameParameterThere(Syntax syntax) {
        syntax.shared(new Syntax(), this::work);
    }

    private Syntax unread() { return null; }
}
"#),
    ("src/d/Native.java", r#"package d;

public class Native {
    protected final long handle_ = 0;
    public static final byte[] EMPTY = new byte[0];
}
"#),
    ("src/d/Options.java", r#"package d;

public class Options extends Native {
}
"#),
    ("src/d/Handle.java", r#"package d;

public class Handle extends Native {
}
"#),
    ("src/d/Db.java", r#"package d;

import java.util.List;

public class Db extends Native {
    public byte[] get(Handle handle, byte[] key) { return get(handle_, key); }
    public byte[] get(Options options, byte[] key) { return get(this.handle_, options.handle_, key); }
    private native byte[] get(long handle, byte[] key);
    private native byte[] get(long handle, long options, byte[] key);
    public byte[] get(Handle handle, Options options, byte[] key) { return key; }

    public void flush(Options options, /* @Nullable */ final Handle handle) {}
    public void flush(Options options, final List<Handle> handles) {}

    public void put(byte[] key) {}
    public void put(java.nio.ByteBuffer key) {}
    public void put(int size) {}
}
"#),
    ("src/d/DbTest.java", r#"package d;

import java.util.List;

public class DbTest {
    private byte[][] keys;

    void commented(Db db, Options options, List<Handle> handles) {
        db.flush(options, handles);
    }

    private int size;

    void varHidesTheField(Db db, Object anything) {
        final var size = anything;
        db.put(size);
    }

    void throughFields(Db db, byte[] key) {
        db.put(Native.EMPTY);
        db.put(keys[0]);
        db.put(key.length);
    }
}
"#),
    ("src/f/Execution.java", r#"package f;

public interface Execution {
    boolean test(Object value);
    void reset();
}
"#),
    ("src/f/TwoArguments.java", r#"package f;

@FunctionalInterface
public interface TwoArguments<T> {
    T apply(T left, T right);
}
"#),
    ("src/f/Registry.java", r#"package f;

public interface Registry<T> {
    T get(String key);
    int size();
}
"#),
    ("src/f/Plain.java", r#"package f;

public class Plain {
}
"#),
    ("src/f/Graph.java", r#"package f;

import java.util.function.BiFunction;
import java.util.function.Consumer;
import java.util.function.Function;
import java.util.function.Supplier;

public class Graph {
    public Graph defaulted(Supplier<String> supplier) { return this; }
    public Graph defaulted(Function<String, String> function) { return this; }
    public Graph defaulted(BiFunction<String, String, String> function) { return this; }
    public Graph append(String name, Execution execution) { return this; }
    public Graph append(String name, Consumer<Graph> consumer) { return this; }
    public Graph parsed(Checked<String> one) { return this; }
    public Graph parsed(TwoArguments<String> two) { return this; }
    public Graph value(int size) { return this; }
    public Graph value(String name) { return this; }
    public Graph value(Plain plain) { return this; }
    public Graph value(Object[] all) { return this; }
    public Graph value(Supplier<String> supplier) { return this; }
    public Graph open(Execution execution) { return this; }
    public Graph open(Registry<String> registry) { return this; }
    public Graph primed(Runnable run, Plain plain) { return this; }
    public Graph primed(Execution execution, Plain plain) { return this; }
}
"#),
    ("src/f/GraphTest.java", r#"package f;

public class GraphTest {
    private void work() {}

    void lambda(Graph graph) {
        graph.append("foo", child -> child.append("bar", other -> {}));
        graph.append("two", (a) -> child());
    }

    void methodReference(Graph graph) {
        graph.append("foo", this::take);
    }

    void refused(Graph graph) {
        graph.value(() -> "x");
        graph.value(GraphTest::name);
    }

    void byParameters(Graph graph) {
        graph.defaulted(() -> "x");
        graph.defaulted(s -> s);
        graph.defaulted((a, b) -> a);
    }

    void cantTell(Graph graph) {
        graph.open(x -> true);
        graph.parsed(p -> p);
    }

    void withTheOthers(Graph graph, Plain plain) {
        graph.primed(this::work, plain);
    }

    private void take(Graph graph) {}
    private static String name() { return ""; }
    private Graph child() { return null; }
}
"#),
    ("src/r/Iter.java", r#"package r;

import java.nio.ByteBuffer;

public class Iter {
    public void seek(byte[] target) {}
    public void seek(ByteBuffer target) {}
}
"#),
    ("src/r/WriteTest.java", r#"package r;

import java.nio.ByteBuffer;

public class WriteTest {
    void byTypedReceiver(Iter it, byte[] raw) {
        final ByteBuffer k2 = ByteBuffer.allocateDirect(8);
        it.seek(k2);
        it.seek(raw);
    }
}
"#),
];

struct Fixture {
    cg: CodeGraph,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in FILES {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    Fixture { cg, _dir: dir }
}

fn params(sig: &str) -> String {
    sig[sig.find('(').unwrap_or(0)..].to_string()
}

fn method(cg: &CodeGraph, pick: impl Fn(&Node) -> bool) -> Node {
    cg.get_nodes_by_kind(NodeKind::Method).unwrap().into_iter().find(|n| pick(n)).expect("method")
}

fn targets(cg: &CodeGraph, from: &Node, line: Option<u32>) -> Vec<Node> {
    cg.get_outgoing_edges(&from.id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls && (line.is_none() || e.line == line))
        .filter_map(|e| cg.get_node(&e.target).unwrap())
        .collect()
}

/// The parameter lists the call on the line holding `call_text` in method `name` reaches.
fn called_from(cg: &CodeGraph, name: &str, call_text: &str, only: Option<&str>) -> Vec<String> {
    let from = method(cg, |n| n.name == name);
    let text = FILES.iter().find(|(f, _)| *f == from.file_path).unwrap().1;
    let line = text
        .split('\n')
        .enumerate()
        .position(|(i, l)| i + 1 >= from.start_line as usize && l.contains(call_text))
        .unwrap_or_else(|| panic!("no line {call_text}"))
        + 1;
    let mut out: Vec<String> = targets(cg, &from, Some(line as u32))
        .into_iter()
        .filter(|n| only.is_none_or(|o| n.name == o))
        .map(|n| params(n.signature.as_deref().unwrap_or("")))
        .collect();
    out.sort();
    out
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_java_calls_arguments_pick_its_overload() {
    let f = fixture();
    let cg = &f.cg;
    let failed: std::cell::RefCell<Vec<String>> = Default::default();
    let check = |label: &str, got: Vec<String>, want: Vec<String>| {
        if got != want {
            failed.borrow_mut().push(format!("{label}: got {got:?}, want {want:?}"));
        }
    };
    let c = |m: &str, t: &str, o: Option<&str>| called_from(cg, m, t, o);

    // reads what a call passed as an argument returns
    check("caught getErrors", c("caught", "errors.merge(e.getErrors())", Some("merge")), v(&["(Errors more)"]));
    check("caught getMessages", c("caught", "errors.merge(e.getMessages())", Some("merge")), v(&["(Collection<Message> messages)"]));
    check("onField", c("onField", "errors.merge(held.getErrors())", Some("merge")), v(&["(Errors more)"]));
    check("onField this", c("onField", "errors.merge(this.held.getMessages())", Some("merge")), v(&["(Collection<Message> messages)"]));
    check("counted", c("overloadsOfItsCount", "errors.merge(e.counted())", Some("merge")), v(&["(Errors more)"]));
    // a type's static method only when its overloads agree
    check("onType", c("onType", "errors.merge(Thrown.shared())", Some("merge")), v(&["(Errors more)"]));
    // no call of one it can't tell apart, past a private overload of another file
    for (m, t) in [("longChain", "errors.merge(e.self()"), ("overloadsDisagree", "errors.merge(e.mixed(make2()))"), ("receiverless", "errors.merge(make())")] {
        check(t, c(m, t, Some("merge")), v(&[]));
    }
    // a variable-arity overload for an argument it can't read
    check("add new", c("varargsStays", "syntax.add(new Syntax())", Some("add")), v(&["(Syntax... args)"]));
    check("add unread", c("varargsStays", "syntax.add(unread())", Some("add")), v(&["(Syntax... args)"]));
    // a field the class inherits, and one read through a receiver
    let db_get = |sig: &str| -> Vec<String> {
        let from = method(cg, |n| n.file_path == "src/d/Db.java" && n.name == "get" && n.signature.as_deref().is_some_and(|s| s.contains(sig)));
        targets(cg, &from, None).into_iter().map(|n| params(n.signature.as_deref().unwrap_or(""))).collect()
    };
    check("Db.get handle", db_get("Handle handle, byte[] key"), v(&["(long handle, byte[] key)"]));
    check("Db.get options", db_get("(Options options, byte[] key"), v(&["(long handle, long options, byte[] key)"]));
    // a comment in a parameter, a `var` hiding a field, fields/elements/length
    check("flush", c("commented", "db.flush(options, handles)", Some("flush")), v(&["(Options options, final List<Handle> handles)"]));
    check("var", c("varHidesTheField", "db.put(size)", Some("put")), v(&["(byte[] key)"]));
    check("EMPTY", c("throughFields", "db.put(Native.EMPTY)", Some("put")), v(&["(byte[] key)"]));
    check("keys[0]", c("throughFields", "db.put(keys[0])", Some("put")), v(&["(byte[] key)"]));
    check("length", c("throughFields", "db.put(key.length)", Some("put")), v(&["(int size)"]));
    // lambdas and method references
    let foo = c("lambda", "graph.append(\"foo\", child", Some("append"));
    if !foo.contains(&"(String name, Consumer<Graph> consumer)".to_string()) || foo.contains(&"(String name, Execution execution)".to_string()) {
        failed.borrow_mut().push(format!("lambda foo: {foo:?}"));
    }
    check("lambda two", c("lambda", "graph.append(\"two\", (a)", Some("append")), v(&["(String name, Consumer<Graph> consumer)"]));
    check("method ref", c("methodReference", "graph.append(\"foo\", this::take)", Some("append")), v(&["(String name, Consumer<Graph> consumer)"]));
    check("primed", c("withTheOthers", "graph.primed(this::work, plain)", Some("primed")), v(&["(Runnable run, Plain plain)"]));
    check("value lambda", c("refused", "graph.value(() -> \"x\")", Some("value")), v(&["(Supplier<String> supplier)"]));
    check("value ref", c("refused", "graph.value(GraphTest::name)", Some("value")), v(&["(Supplier<String> supplier)"]));
    check("supplier", c("byParameters", "graph.defaulted(() -> \"x\")", Some("defaulted")), v(&["(Supplier<String> supplier)"]));
    check("function", c("byParameters", "graph.defaulted(s -> s)", Some("defaulted")), v(&["(Function<String, String> function)"]));
    check("bifunction", c("byParameters", "graph.defaulted((a, b) -> a)", Some("defaulted")), v(&["(BiFunction<String, String, String> function)"]));
    check("open", c("cantTell", "graph.open(x -> true)", Some("open")), v(&["(Execution execution)"]));
    check("parsed", c("cantTell", "graph.parsed(p -> p)", Some("parsed")), v(&["(Checked<String> one)"]));
    check("value 6", c("literalIsNoFunction", "syntax.value(6)", Some("value")), v(&["(T plain)"]));
    check("value six", c("literalIsNoFunction", "syntax.value(\"six\")", Some("value")), v(&["(T plain)"]));
    check("bytes many", c("variableArity", "syntax.bytes(many)", Some("bytes")), v(&["(byte[] key)"]));
    check("bytes key", c("arrayMarkersAfterTheName", "syntax.bytes(key)", Some("bytes")), v(&["(byte[] key)"]));
    check("bytes other", c("arrayMarkersAfterTheName", "syntax.bytes(other)", Some("bytes")), v(&["(byte[] key)"]));
    check("shared", c("sameParameterThere", "syntax.shared(new Syntax(), this::work)", Some("shared")), v(&["(Syntax first, Runnable run)"]));
    // a `super` call's parameter
    let before = |sig: &str| -> Vec<String> {
        let from = method(cg, |n| n.name == "before" && n.file_path == "src/n/Element.java" && n.signature.as_deref().is_some_and(|s| s.contains(sig)));
        targets(cg, &from, None).into_iter().map(|n| n.signature.unwrap_or_default()).collect()
    };
    check("before node", before("Node node"), v(&["Node (Node node)"]));
    check("before html", before("String html"), v(&["Node (String html)"]));
    check("removeAt", c("removeAt", "super.remove(index)", None), v(&["(int index)"]));
    check("removeOne", c("removeOne", "super.remove(o)", None), v(&["(Object o)"]));
    check("seek k2", c("byTypedReceiver", "it.seek(k2)", None), v(&["(ByteBuffer target)"]));
    check("seek raw", c("byTypedReceiver", "it.seek(raw)", None), v(&["(byte[] target)"]));
    // a private overload from another file goes, and stays in its own
    check("convert other file", c("fromAnotherFile", "CONVERTER.convert(e)", None), v(&["(Operand operand)"]));
    let public_convert = method(cg, |n| n.name == "convert" && n.signature.as_deref().is_some_and(|s| s.contains("Operand")));
    let got: Vec<String> = targets(cg, &public_convert, None).into_iter().map(|n| n.signature.unwrap_or_default()).collect();
    check("convert own file", got, v(&["String (Expression expression)"]));
    // literals, casts, new, this, parameters, fields
    for (call, p) in [
        ("pick(1L)", "(long id)"),
        ("pick(true)", "(boolean flag)"),
        ("pick('c')", "(char c)"),
        ("pick(1.5)", "(double d)"),
        ("pick(.5)", "(double d)"),
        ("pick(2d)", "(double d)"),
        ("pick(\"s\")", "(String s)"),
        ("pick(\"a\" + label)", "(String s)"),
        ("pick(new Node())", "(Node node)"),
        ("pick(new Node[2])", "(Node[] nodes)"),
        ("pick(new int[] { 1 })", "(int[] ids)"),
        ("pick((Node) nodes[0])", "(Node node)"),
        ("pick(nodes)", "(Node[] nodes)"),
        ("pick(ids)", "(int[] ids)"),
        ("pick(this)", "(Node node)"),
        ("pick(label)", "(String s)"),
        ("pick(SHARED)", "(Node[] nodes)"),
    ] {
        check(call, c("literals", call, None), v(&[p]));
    }
    check("widen", c("widen", "pick(small)", None), v(&["(long id)"]));
    check("local text", c("locals", "pick(text)", None), v(&["(String s)"]));
    check("local each", c("locals", "pick(each)", None), v(&["(Node node)"]));
    let chained = c("chained", "pick(new Node().toString())", None);
    if chained.len() != 1 {
        failed.borrow_mut().push(format!("chained: {chained:?}"));
    }
    check("log only", c("fixedArityFirst", "log(\"only\")", None), v(&["(String message)"]));
    check("log with", c("fixedArityFirst", "log(\"with %s\", label)", None), v(&["(String format, Object... args)"]));
    check("attach leaf", c("subtypes", "attach(leaf)", None), v(&["(Holder holder)"]));
    check("attach other", c("subtypes", "attach(other)", None), v(&["(Other other)"]));
    check("generic n", c("generics", "generic(value, \"n\")", None), v(&["(T value, String name)"]));
    check("generic 1", c("generics", "generic(value, 1)", None), v(&["(T value, int index)"]));
    let failed = failed.into_inner();
    assert!(failed.is_empty(), "{} case(s) failed:\n{}", failed.len(), failed.join("\n"));
    f.cg.close();
}

#[test]
fn a_java_lambdas_parameter_is_typed_by_the_functional_interface() {
    let f = fixture();
    let cg = &f.cg;
    let called = |name: &str| -> Vec<String> {
        let from = method(cg, |n| n.name == name && n.file_path == "src/t/BuilderTest.java");
        let mut q: Vec<String> = targets(cg, &from, None).into_iter().map(|n| n.qualified_name).collect();
        q.sort();
        q
    };
    assert!(called("one").contains(&"t::Builder::process".to_string()), "one: {:?}", called("one"));
    let two = called("two");
    assert!(two.contains(&"t::Builder::process".to_string()) && two.contains(&"t::Elsewhere::other".to_string()), "two: {two:?}");
    assert!(!two.contains(&"t::Elsewhere::process".to_string()), "two: {two:?}");
    assert!(!called("library").contains(&"t::Builder::process".to_string()));
    assert!(called("onReceiver").is_empty(), "onReceiver: {:?}", called("onReceiver"));
    f.cg.close();
}
