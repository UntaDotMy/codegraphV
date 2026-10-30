//! A Java call through a field or a local resolves on the receiver's declared type, and a receiver
//! of a library type gets no edge (upstream 7973df0d, PR #1942 by danusha2345).
//!
//! The field lookup only read the tightest enclosing class and only a plain name, so
//! `this.mRepo.save()`, `Outer.this.mRepo.save()`, a field read from inside an anonymous class and
//! a field chain `mOwner.repo.save()` were all left to name-only guessing. And when the declared
//! type WAS known but was a library class (`Parcel`, `Handler`, `List`), the call fell through to
//! the same guessing, which bound it to whichever project class declares a method of that name
//! (`parcel.readInt()` → a project `VersionedParcel.readInt`).
//!
//! Two fixtures as upstream's two describes, each indexed once; a case that fails is named in the
//! report with what was expected and what the graph holds.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

struct Fixture {
    cg: CodeGraph,
    _root: tempfile::TempDir,
}

fn index(files: &[(&str, &str)]) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    Fixture { cg, _root: root }
}

impl Fixture {
    /// Qualified names of what `owner::method` calls, sorted (`owner` may name an anonymous
    /// class's supertype).
    fn callees(&self, owner: &str, method: &str) -> Vec<String> {
        let mut methods = self.cg.get_nodes_by_kind(NodeKind::Method).unwrap();
        methods.sort_by(|a, b| (&a.file_path, a.start_line).cmp(&(&b.file_path, b.start_line)));
        let suffix = format!("{owner}::{method}");
        let anon = format!("{owner}$anon");
        let Some(node) = methods.iter().find(|n| {
            n.name == method && (n.qualified_name.ends_with(&suffix) || (n.qualified_name.contains(&anon) && n.qualified_name.ends_with(&format!("::{method}"))))
        }) else {
            return vec![format!("<no method {suffix}>")];
        };
        let mut out: Vec<String> = self
            .cg
            .get_outgoing_edges(&node.id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .filter_map(|e| self.cg.get_node(&e.target).unwrap())
            .map(|n| n.qualified_name)
            .collect();
        out.sort();
        out
    }
}

#[derive(Default)]
struct Checks(Vec<String>);

impl Checks {
    fn is(&mut self, case: &str, actual: Vec<String>, expected: &[&str]) {
        if actual != expected {
            self.0.push(format!("{case}\n    expected {expected:?}\n    actual   {actual:?}"));
        }
    }

    fn finish(self) {
        assert!(self.0.is_empty(), "{} case(s) failed:\n  {}", self.0.len(), self.0.join("\n  "));
    }
}

const REPO: &str = "package p;\npublic class Repo {\n    public void save() {}\n    public void close() {}\n}\n";
const OWNER: &str = "package p;\npublic class Owner {\n    public Repo repo = new Repo();\n    public android.content.Context ctx;\n}\n";
const HOOK: &str = "package p;\npublic abstract class Hook {\n    public abstract void fire();\n}\n";
const BOX: &str = "package p;\npublic class Box<T> {\n    public void seal() {}\n}\n";
// Same-named methods on unrelated project classes: what a name-only guess lands on.
const VERSIONED_PARCEL: &str = "package p;\npublic class VersionedParcel {\n    public int readInt() { return 0; }\n    public void writeInt(int v) {}\n}\n";
// Project types nested in other packages that share a simple name with a JDK type the callers use.
const BYTES: &str = "package q;\npublic class Bytes {\n    public static class Iterator { public boolean hasNext() { return false; } }\n}\n";
const TABLE: &str = "package q;\npublic class Table {\n    public static class Entry { public Object getKey() { return null; } }\n}\n";
// A Scala type of the same simple name as a Java one, reached from a Java file through a wildcard import.
const FORM_JAVA: &str = "package p;\npublic class Form<T> {\n    public T get() { return null; }\n}\n";
const FORM_SCALA: &str = "package api\nclass Form(value: Int) {\n  def get(): Int = value\n}\n";
// A Scala trait in a file not named after it, imported explicitly.
const BODY_PARSERS_SCALA: &str = "package api.mvc\ntrait PlayBodyParsers {\n  def raw(): Int\n}\n";
// Same simple names in another package, indexed first: what an unpinned lookup would land on.
const COOKIE_B: &str = "package b;\npublic class Cookie { public String value() { return null; } }\n";
const PARSERS_B: &str = "package b;\npublic class PlayBodyParsers { public int raw() { return 0; } }\n";
const HTTP: &str = "package p;\npublic class Http {\n    public static class Cookie { public String value() { return null; } }\n}\n";
const CONTROLLER: &str = "package r;
import api.mvc.PlayBodyParsers;
import p.*;
import p.Http.Cookie;
public class Controller {
    void submit(Form<String> form) { form.get(); }
    void parse(PlayBodyParsers parsers) { parsers.raw(); }
    void written(api.mvc.PlayBodyParsers other) { other.raw(); }
    void writtenLocal() {
        api.mvc.PlayBodyParsers local = null;
        local.raw();
    }
    void cookie(Cookie crumb) { crumb.value(); }
}
";
const SCREEN: &str = "package p;

import android.os.Handler;
import android.os.Parcel;
import android.util.Log;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import q.Bytes;

class Decoy {
    void save() {}
    void post(Runnable r) {}
    void add(Object o) {}
    void put(Object k, Object v) {}
    void d(String t, String m) {}
    void getResources() {}
    void seal() {}
}
// Decoys whose names share words with the receivers, so a receiver-name
// guess prefers them over the declared type.
class StoreCache { void save() {} void close() {} }
class SharedCache { void close() {} }
class OwnerRepoCache { void save() {} }
class BoxCache { void seal() {} }

public class Screen {
    private Repo mStore;
    private Handler mHandler;
    private Owner mOwner;
    private static final Repo sShared = new Repo();

    void viaField() { mStore.save(); }
    void viaThis() { this.mStore.save(); }
    void viaConstant() { sShared.close(); }
    void viaChain() { mOwner.repo.save(); }
    void viaThisChain() { this.mOwner.repo.save(); }
    void viaLocalChain(Owner owner) { owner.repo.save(); }
    void libraryField() { mHandler.post(null); }
    void libraryChain() { mOwner.ctx.getResources(); }
    void libraryParam(Parcel parcel) { parcel.readInt(); parcel.writeInt(1); }
    void libraryGenericLocal() {
        List<String> items = null;
        items.add(\"x\");
        Map<String, List<Repo>> byName = null;
        byName.put(\"k\", null);
    }
    void libraryForEach(List<Parcel> parcels) {
        for (Parcel p : parcels) { p.readInt(); }
    }
    void projectGenericLocal() {
        Box<Repo> crate = make();
        crate.seal();
    }
    void iterate(List<Repo> list) {
        Iterator<Repo> it = list.iterator();
        it.hasNext();
    }
    void entries(Map<String, String> map) {
        for (Map.Entry<String, String> e : map.entrySet()) { e.getKey(); }
    }
    void nested(Bytes.Iterator bytes) { bytes.hasNext(); }
    static Box<Repo> make() { return null; }
    void staticLibrary() { Log.d(\"t\", \"m\"); }

    Runnable anonymous() {
        return new Runnable() {
            public void run() { mStore.save(); }
        };
    }

    class Inner {
        private Box<Repo> mStore;
        void viaOuter() { Screen.this.mStore.close(); }
    }
}

class Holder<T extends Hook> {
    T hook;
    void go() { hook.fire(); }
}
";

fn screen_fixture() -> Fixture {
    index(&[
        ("src/p/Repo.java", REPO),
        ("src/p/Owner.java", OWNER),
        ("src/p/Hook.java", HOOK),
        ("src/p/Box.java", BOX),
        ("src/p/VersionedParcel.java", VERSIONED_PARCEL),
        ("src/q/Bytes.java", BYTES),
        ("src/q/Table.java", TABLE),
        ("src/p/Form.java", FORM_JAVA),
        ("src/a/Form.scala", FORM_SCALA),
        ("src/api/mvc/BodyParsers.scala", BODY_PARSERS_SCALA),
        ("src/b/Cookie.java", COOKIE_B),
        ("src/b/PlayBodyParsers.java", PARSERS_B),
        ("src/p/Http.java", HTTP),
        ("src/r/Controller.java", CONTROLLER),
        ("src/p/Screen.java", SCREEN),
    ])
}

const PAUSE: &str = "package a;\npublic class Pause {\n    public static class Suite {\n        public void runDelay(String id) {}\n    }\n}\n";
const REQUEST: &str = "package a.http;\npublic class Request {\n    public String getBody() { return \"\"; }\n}\n";
// kestra's AbstractRunnerTest: `protected PauseTest.Suite pauseTest;` under
// `import io.kestra.plugin.core.flow.*;`, and RequestTest's field written
// with the project package beside an import of a library `HttpRequest`.
const RUNNER: &str = "package b;

import a.*;
import io.lib.Request;

public class Runner {
    protected Pause.Suite pause;
    private a.http.Request request;

    public void delay() {
        pause.runDelay(\"x\");
    }

    public void body() {
        request.getBody();
    }

    // spring-ai's `org.springframework.ai.chat.messages.Message message` beside
    // an import of the Anthropic SDK's `Message`: a local or loop variable
    // written with its package.
    public void localBody(java.util.List<a.http.Request> all) {
        a.http.Request first = all.get(0);
        first.getBody();
    }

    public void eachBody(java.util.List<a.http.Request> all) {
        for (a.http.Request each : all) {
            each.getBody();
        }
    }
}
";
// SystemUI's QSTileImpl<TState extends State>: a field typed by the class's type parameter,
// beside another class's same-named method.
const STATE: &str = "package a;\npublic class State {\n    public void copyTo(State other) {}\n}\n";
const OTHER_A: &str = "package a;\npublic class Other {\n    public void copyTo(Other other) {}\n}\n";
const TILE: &str = "package b;

import a.State;

public abstract class Tile<TState extends State> {
    private TState mTmpState;

    public void refresh() {
        mTmpState.copyTo(null);
    }
}
";
// SystemUI's HeadsUpManagerPhone: `AlertEntry`, nested in a class two levels up its hierarchy,
// named without an import.
const ALERTS: &str = "package a;\npublic abstract class Alerts {\n    public static class Entry {\n        public void update() {}\n    }\n}\n";
const HEADS_UP: &str = "package a;\npublic abstract class HeadsUp extends Alerts {}\n";
const STORE: &str = "package c;\npublic class Store {\n    public void update() {}\n}\n";
const PHONE: &str = "package b;

import a.HeadsUp;

public class Phone extends HeadsUp {
    public void refresh(java.util.List<Entry> entries) {
        for (Entry entry : entries) {
            entry.update();
        }
    }
}
";
// kestra: a `java.net.URI` (short enough to read as a type parameter) and a project enum, whose
// `name()` is Java's, beside project methods the receivers' names match.
const FLOW_WITH_PATH: &str = "package a;\npublic class FlowWithPath {\n    public String getPath() { return \"\"; }\n}\n";
const TYPE: &str = "package a;\npublic class Type {\n    public String name() { return \"\"; }\n}\n";
const SERVICE_TYPE: &str = "package a;\npublic enum ServiceType { WORKER, EXECUTOR }\n";
const STORAGE: &str = "package b;

import java.net.URI;
import a.ServiceType;

public class Storage {
    private URI base;
    private ServiceType kind;

    public void local(URI path, ServiceType type) {
        path.getPath();
        type.name();
    }

    public void field() {
        base.getPath();
        kind.name();
    }
}
";
// playframework's javaguide: `import play.mvc.Http.*;` names Http's nested `RequestBody`.
const WEB: &str = "package a;\npublic class Web {\n    public static class Body {\n        public String asText() { return \"\"; }\n    }\n}\n";
const PARSERS: &str = "package b;

import a.Web.*;

public class Parsers {
    public void read(Body body) {
        body.asText();
    }
}
";
// spring-modulith's ApplicationModule: `Comparable<ApplicationModule>` in the class head names the
// class, not a type parameter, and a nested class's field of that type types the call on a local
// named like it.
const APP_MODULE: &str = "package a;
public class AppModule implements Comparable<AppModule> {
    public java.util.List<String> getNames() { return null; }
    public int compareTo(AppModule other) { return 0; }

    public static class Link {
        private final AppModule target;

        Link(AppModule target) { this.target = target; }

        static Link of(AppModule source) {
            var target = source.find();
            var names = target.getNames();
            return null;
        }
    }

    public AppModule find() { return this; }
}
";
const OTHER_C: &str = "package c;\npublic class Other {\n    public java.util.List<String> getNames() { return null; }\n}\n";

fn written_fixture() -> Fixture {
    index(&[
        ("src/a/Pause.java", PAUSE),
        ("src/a/http/Request.java", REQUEST),
        ("src/b/Runner.java", RUNNER),
        ("src/a/State.java", STATE),
        ("src/a/Other.java", OTHER_A),
        ("src/b/Tile.java", TILE),
        ("src/a/Alerts.java", ALERTS),
        ("src/a/HeadsUp.java", HEADS_UP),
        ("src/c/Store.java", STORE),
        ("src/b/Phone.java", PHONE),
        ("src/a/FlowWithPath.java", FLOW_WITH_PATH),
        ("src/a/Type.java", TYPE),
        ("src/a/ServiceType.java", SERVICE_TYPE),
        ("src/b/Storage.java", STORAGE),
        ("src/a/Web.java", WEB),
        ("src/b/Parsers.java", PARSERS),
        ("src/a/AppModule.java", APP_MODULE),
        ("src/c/Other.java", OTHER_C),
    ])
}

#[test]
fn fields_and_chains_resolve_on_the_declared_type() {
    let f = screen_fixture();
    let mut c = Checks::default();
    // A field, `this.field` and a static final field.
    c.is("viaField", f.callees("Screen", "viaField"), &["p::Repo::save"]);
    c.is("viaThis", f.callees("Screen", "viaThis"), &["p::Repo::save"]);
    c.is("viaConstant", f.callees("Screen", "viaConstant"), &["p::Repo::close"]);
    // An anonymous class reads its outer class field, and `Outer.this.field` names the class.
    c.is("anonymous run", f.callees("Runnable", "run"), &["p::Repo::save"]);
    c.is("Inner.viaOuter", f.callees("Inner", "viaOuter"), &["p::Repo::close"]);
    // A chain of fields resolves through each declared type.
    c.is("viaChain", f.callees("Screen", "viaChain"), &["p::Repo::save"]);
    c.is("viaThisChain", f.callees("Screen", "viaThisChain"), &["p::Repo::save"]);
    c.is("viaLocalChain", f.callees("Screen", "viaLocalChain"), &["p::Repo::save"]);
    // A generic local of a project type resolves on that type.
    c.is("projectGenericLocal", f.callees("Screen", "projectGenericLocal"), &["p::Box::seal", "p::Screen::make"]);
    c.finish();
}

#[test]
fn a_library_type_gets_no_edge() {
    let f = screen_fixture();
    let mut c = Checks::default();
    // A field, a chain hop, a parameter or a local of a library type.
    c.is("libraryField", f.callees("Screen", "libraryField"), &[]);
    c.is("libraryChain", f.callees("Screen", "libraryChain"), &[]);
    c.is("libraryParam", f.callees("Screen", "libraryParam"), &[]);
    c.is("libraryGenericLocal", f.callees("Screen", "libraryGenericLocal"), &[]);
    c.is("libraryForEach", f.callees("Screen", "libraryForEach"), &[]);
    // A static call on an imported library class (`Log.d` beside a project `Decoy.d`).
    c.is("staticLibrary", f.callees("Screen", "staticLibrary"), &[]);
    // A JDK type is not read as a same-named type nested in another package: an explicit
    // `import java.util.Iterator`, and a qualified `Map.Entry`.
    c.is("iterate", f.callees("Screen", "iterate"), &[]);
    c.is("entries", f.callees("Screen", "entries"), &[]);

    let w = written_fixture();
    // A library type with a short name, or a project enum lacking the method, gets no guess.
    c.is("Storage.local", w.callees("Storage", "local"), &[]);
    c.is("Storage.field", w.callees("Storage", "field"), &[]);
    c.finish();
}

#[test]
fn declared_types_keep_what_the_declaration_names() {
    let f = screen_fixture();
    let mut c = Checks::default();
    // A nested project type written with its outer class resolves on it.
    c.is("nested", f.callees("Screen", "nested"), &["q::Bytes::Iterator::hasNext"]);
    // A Java receiver type prefers the Java declaration over a same-named Scala one.
    c.is("submit", f.callees("Controller", "submit"), &["p::Form::get"]);
    // An explicitly imported Scala type is a project type wherever its file is named.
    c.is("parse", f.callees("Controller", "parse"), &["PlayBodyParsers::raw"]);
    c.is("written", f.callees("Controller", "written"), &["PlayBodyParsers::raw"]);
    c.is("writtenLocal", f.callees("Controller", "writtenLocal"), &["PlayBodyParsers::raw"]);
    // An imported nested type is the one the import names.
    c.is("cookie", f.callees("Controller", "cookie"), &["p::Http::Cookie::value"]);
    // A type-parameter receiver is not treated as a library type.
    c.is("Holder.go", f.callees("Holder", "go"), &["p::Hook::fire"]);

    let w = written_fixture();
    // A field typed with its outer class, through an on-demand import, resolves on the nested type.
    c.is("Runner.delay", w.callees("Runner", "delay"), &["a::Pause::Suite::runDelay"]);
    // A field written with its package resolves on that type, whatever the file imports by the name.
    c.is("Runner.body", w.callees("Runner", "body"), &["a.http::Request::getBody"]);
    // A local or loop variable written with its package resolves on that type too.
    c.is("Runner.localBody", w.callees("Runner", "localBody"), &["a.http::Request::getBody"]);
    c.is("Runner.eachBody", w.callees("Runner", "eachBody"), &["a.http::Request::getBody"]);
    // A field typed by the class type parameter resolves on its bound.
    c.is("Tile.refresh", w.callees("Tile", "refresh"), &["a::State::copyTo"]);
    // A nested type a superclass declares is named without an import.
    c.is("Phone.refresh", w.callees("Phone", "refresh"), &["a::Alerts::Entry::update"]);
    // An on-demand import of a class names its nested types.
    c.is("Parsers.read", w.callees("Parsers", "read"), &["a::Web::Body::asText"]);
    // A class the head names as a type argument is no type parameter.
    c.is("Link.of", w.callees("Link", "of"), &["a::AppModule::find", "a::AppModule::getNames"]);
    c.finish();
}
