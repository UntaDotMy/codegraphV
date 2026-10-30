//! A call through a Kotlin property or a receiver chain resolves on the receiver's declared type
//! (upstream 01e0ac3a, PR #1933 by danusha2345).
//!
//! Kotlin indexes a class property without a signature and a primary-constructor property
//! (`class A(private val repo: Repo)`) is no node at all, so a call through either found no
//! receiver type and fell to name-only guessing: the interface's method, or any same-named method
//! elsewhere. The declared type is read from the declaration itself, each segment of a receiver
//! chain in turn, and a receiver whose type the project does not declare (`Regex`, `Thread`, a JDK
//! class) gets no edge. `fun interface` also parses cleanly: the grammar predates it.
//!
//! One fixture as upstream's one describe, indexed once per test; a case that fails is named in
//! the report with what was expected and what the graph holds.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, Node, NodeKind};
use std::fs;

struct Fixture {
    cg: CodeGraph,
    _root: tempfile::TempDir,
}

const PROBE: &str = r#"package p
import java.io.Closeable
internal interface Probe : Closeable {
    fun probe(host: String): Int
}
internal class HttpProbe(
    private val endpoints: (String) -> List<String>,
) : Probe {
    override fun probe(host: String): Int = 1
    override fun close() {}
}
internal class OtherProbe : Closeable {
    fun probe(host: String): Int = 2
    override fun close() {}
}
class Store { fun put(v: Int) = v }
class HardwareLock {
    class Lease {
        fun close() {}
    }
}
class SessionLock {
    class Lease {
        fun close() {}
    }
}
class AAValidator { fun validate(v: String): List<String> = emptyList() }
object HardwareGate {
    fun tryBegin(): HardwareLock.Lease? = null
}
class Checker {
    fun validate(v: String): List<String> = emptyList()
    companion object {
        fun load(): Checker = Checker()
    }
}
internal fun interface Reply {
    fun send(message: String): Boolean
}

/** Events of one session, for the application layer. */
internal interface SessionEvents {
    fun onLost(reason: String)
}
class Pattern { fun find(text: String): Int = 0 }
class Conn { fun close() {} }
class Buffer(val size: Int) { fun drain(): Int = 0 }
class Pump { fun drain(): Int = 1 }
enum class Mode { ON, OFF; fun next(): Mode = this }
class Wheel { fun next(): Int = 0 }
class Worker { fun start() {}; fun join(ms: Long) {} }
class Vm {
    fun refresh() {}
    fun tick() {
        activeController?.refresh()
    }
    companion object {
        @Volatile private var activeController: Vm? = null
    }
}
"#;

const USERS: &str = r#"package p
class LateinitUser {
    private lateinit var probe: HttpProbe
    fun setUp() { probe = HttpProbe(endpoints = { listOf(it) }) }
    fun useIt() { probe.probe("a") }
    fun tearDown() { probe.close() }
}
class CtorUser(private val store: Store, val maybe: HttpProbe?) {
    fun save() { store.put(1) }
    fun check() { maybe?.probe("b") }
}
class InitUser {
    private val probe = HttpProbe(endpoints = { emptyList() })
    fun stop() { probe.close() }
}
class NestedUser(private val lease: HardwareLock.Lease) {
    fun release() { lease.close() }
}
class CallResultUser {
    private val schema = Checker.load()
    fun check() { schema.validate("x") }
    fun locked() {
        val lease = HardwareGate.tryBegin()
        lease?.close()
    }
}
class Supervisor(private val events: SessionEvents) {
    fun lost() { events.onLost("gone") }
}
class ValUser {
    private val pending = Buffer(
        size = 4,
    )
    @Volatile private var current = Mode.ON
    fun work() {
        listOf(1).forEach {
            pending.drain()
        }
    }
    fun toggle() { current.next() }
}
class Socket2 { fun cancel() {} }
class Session { fun cancel() {} }
abstract class Listener { abstract fun onOpen(socket: java.net.Socket) }
class AliasUser {
    @Volatile private var current: Session? = null
    fun stop() {
        val old = current ?: return
        old.cancel()
    }
    fun listen(): Listener = object : Listener() {
        override fun onOpen(socket: java.net.Socket) {
            socket.close()
        }
    }
}
class ThreadUser {
    fun run() {
        val serverThread = Thread {
            println("x")
        }
        serverThread.start()
        val later = lazy { 1 }
        val items = mutableListOf(1)
        items.clear()
    }
}
class LibraryUser {
    private val regex = Regex("[0-9]+")
    fun first(text: String) = regex.find(text)
    fun scheduled() {
        val scheduler = java.util.concurrent.Executors.newSingleThreadScheduledExecutor()
        scheduler.shutdown()
    }
}
"#;

const MACHINES: &str = r#"package p
class Tank { fun flush() {} }
class Motor {
    val reservoir = Tank()
    val runner = Thread { }
}
class Plant(val unit: Motor)
class Crate<T> { fun seal() {} }
object Registry { val primary = Motor() }
enum class Gear { LOW, HIGH; fun shift() {} }
fun Thread.label(): String = name
"#;

// The decoy `flush` sits in the caller's own file, where a name-only guess would look first.
const CHAINS: &str = r#"package p
class Other { fun flush() {}; fun seal() {}; fun shift() {} }
class Owner(private val engine: Motor, private val site: Plant) {
    private val box = Crate<Int>()
    fun generic() { box.seal() }
    fun viaObject() { Registry.primary.reservoir.flush() }
    fun viaEnum() { Gear.LOW.shift() }
    fun go() { engine.reservoir.flush() }
    fun viaThis() { this.engine.reservoir.flush() }
    fun deep() { site.unit.reservoir.flush() }
    fun library() { engine.runner.start() }
    fun extension() { engine.runner.label() }
}
"#;

// A property a class inherits: from a base in another file (a body property and a
// primary-constructor one, two levels up), or in the same file. Variable names differ from type
// names so no capitalized-name guess can land on the right type.
const BASES: &str = r#"package p
class Ledger { fun post() {} }
class Feed { fun refresh() {} }
abstract class BaseScreen(protected val source: Feed) : java.io.Serializable {
    protected lateinit var model: Ledger
}
open class MidScreen(origin: Feed) : BaseScreen(origin)
"#;

const SCREENS: &str = r#"package p
class Cache { fun post() {}; fun refresh() {}; fun twist() {}; fun open() {} }
class Knob { fun twist() {} }
class Sink { fun drainAll() {} }
open class Panel { val widget = Knob() }
class SidePanel : Panel() {
    fun turn() { widget.twist() }
}
class HomeScreen(start: Feed) : MidScreen(start), Runnable {
    override fun run() {}
    fun show() { model.post() }
    fun pull() { source.refresh() }
    fun viaThis() { this.model.post() }
}
class LibScreen : android.app.Activity() {
    fun go() { helper.drainAll() }
}
abstract class Hook { abstract fun fire(code: Int) }
class Gate { fun open() {} }
class Rig { val gate = Gate() }
class Wiring {
    fun wire(): Hook {
        val motor = Rig()
        return object : Hook() {
            override fun fire(code: Int) { motor.gate.open() }
        }
    }
    fun siblings(): Hook {
        val latch = Gate()
        return object : Hook() {
            fun prep() { val latch = Cache() }
            override fun fire(code: Int) { latch.open() }
        }
    }
}
"#;

fn index() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let files =
        [("Probe.kt", PROBE), ("Users.kt", USERS), ("Machines.kt", MACHINES), ("Chains.kt", CHAINS), ("Bases.kt", BASES), ("Screens.kt", SCREENS)];
    for (name, content) in files {
        let path = root.path().join("src").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    Fixture { cg, _root: root }
}

impl Fixture {
    fn callables(&self) -> Vec<Node> {
        let mut nodes = self.cg.get_nodes_by_kind(NodeKind::Method).unwrap();
        nodes.extend(self.cg.get_nodes_by_kind(NodeKind::Function).unwrap());
        nodes.sort_by(|a, b| (&a.file_path, a.start_line).cmp(&(&b.file_path, b.start_line)));
        nodes
    }

    /// The `calls` targets of `node` that no synthesizer produced, as qualified names, sorted.
    fn calls_of(&self, node: &Node) -> Vec<String> {
        let mut out: Vec<String> = self
            .cg
            .get_outgoing_edges(&node.id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .filter(|e| {
                let meta = e.metadata.as_ref();
                meta.and_then(|m| m.get("synthesizedBy")).is_none() && meta.and_then(|m| m.get("registeredAt")).is_none()
            })
            .filter_map(|e| self.cg.get_node(&e.target).unwrap())
            .map(|n| n.qualified_name)
            .collect();
        out.sort();
        out
    }

    /// Qualified names of what `owner::method` calls.
    fn callees(&self, owner: &str, method: &str) -> Vec<String> {
        let suffix = format!("{owner}::{method}");
        match self.callables().into_iter().find(|n| n.name == method && n.qualified_name.ends_with(&suffix)) {
            Some(node) => self.calls_of(&node),
            None => vec![format!("<no callable {suffix}>")],
        }
    }

    /// Callees of the `fire` override declared inside `Wiring::<outer>`.
    fn fire_in(&self, outer: &str) -> Vec<String> {
        let suffix = format!("Wiring::{outer}");
        let callables = self.callables();
        let Some(outer_node) = callables.iter().find(|n| n.name == outer && n.qualified_name.ends_with(&suffix)) else {
            return vec![format!("<no callable {suffix}>")];
        };
        match callables
            .iter()
            .find(|n| n.name == "fire" && n.file_path.ends_with("Screens.kt") && n.start_line > outer_node.start_line && n.start_line <= outer_node.end_line)
        {
            Some(node) => self.calls_of(node),
            None => vec![format!("<no fire inside {suffix}>")],
        }
    }

    /// Callees of the anonymous object's `onOpen`, not the abstract `Listener::onOpen`.
    fn on_open_in_listen(&self) -> Vec<String> {
        let callables = self.callables();
        let Some(listen) = callables.iter().find(|n| n.name == "listen" && n.qualified_name.ends_with("AliasUser::listen")) else {
            return vec!["<no callable AliasUser::listen>".to_string()];
        };
        match callables
            .iter()
            .find(|n| n.name == "onOpen" && n.file_path.ends_with("Users.kt") && n.start_line > listen.start_line && n.start_line <= listen.end_line)
        {
            Some(node) => self.calls_of(node),
            None => vec!["<no onOpen inside AliasUser::listen>".to_string()],
        }
    }

    fn node_kind(&self, qualified: &str) -> String {
        let mut found: Vec<String> = Vec::new();
        for kind in [NodeKind::Interface, NodeKind::Class, NodeKind::Function, NodeKind::Method] {
            for n in self.cg.get_nodes_by_kind(kind).unwrap() {
                if n.qualified_name == qualified {
                    found.push(kind.as_str().to_string());
                }
            }
        }
        found.join(",")
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

    fn has(&mut self, case: &str, actual: &[String], member: &str, present: bool) {
        if actual.iter().any(|a| a == member) != present {
            self.0.push(format!("{case}\n    expected {member} to be {}\n    actual   {actual:?}", if present { "present" } else { "absent" }));
        }
    }

    fn finish(self) {
        assert!(self.0.is_empty(), "{} case(s) failed:\n  {}", self.0.len(), self.0.join("\n  "));
    }
}

#[test]
fn properties_resolve_on_the_declared_type() {
    let f = index();
    let mut c = Checks::default();
    c.is("a lateinit property resolves on its concrete type, not the interface: useIt", f.callees("LateinitUser", "useIt"), &["p::HttpProbe::probe"]);
    c.is("a lateinit property resolves on its concrete type: tearDown", f.callees("LateinitUser", "tearDown"), &["p::HttpProbe::close"]);
    c.is("a primary-constructor property resolves on its type: save", f.callees("CtorUser", "save"), &["p::Store::put"]);
    c.is("a nullable primary-constructor property resolves on its type: check", f.callees("CtorUser", "check"), &["p::HttpProbe::probe"]);
    c.is("a property initialized by a constructor call: stop", f.callees("InitUser", "stop"), &["p::HttpProbe::close"]);
    c.is("a property bound to a call is typed by the callee return type: check", f.callees("CallResultUser", "check"), &["p::Checker::validate"]);
    c.is(
        "a local bound to a call is typed by the callee return type: locked",
        f.callees("CallResultUser", "locked"),
        &["p::HardwareGate::tryBegin", "p::HardwareLock::Lease::close"],
    );
    c.is("a nested type keeps its outer type: release", f.callees("NestedUser", "release"), &["p::HardwareLock::Lease::close"]);
    let work = f.callees("ValUser", "work");
    c.has("a val property, inside a lambda: Buffer::drain", &work, "p::Buffer::drain", true);
    c.has("a val property, inside a lambda: not Pump::drain", &work, "p::Pump::drain", false);
    c.is("an enum-entry var is typed: toggle", f.callees("ValUser", "toggle"), &["p::Mode::next"]);
    c.is("a companion-object property is typed: tick", f.callees("Vm", "tick"), &["p::Vm::refresh"]);
    c.is("an alias takes the aliased value type: stop", f.callees("AliasUser", "stop"), &["p::Session::cancel"]);
    c.is("a property inherited in the same file: turn", f.callees("SidePanel", "turn"), &["p::Knob::twist"]);
    c.is("a property inherited two levels up, past a library interface: show", f.callees("HomeScreen", "show"), &["p::Ledger::post"]);
    c.is("this.property inherited: viaThis", f.callees("HomeScreen", "viaThis"), &["p::Ledger::post"]);
    c.is("a primary-constructor property of the base: pull", f.callees("HomeScreen", "pull"), &["p::Feed::refresh"]);
    c.is("a local of the outer function is typed inside an anonymous object: wire", f.fire_in("wire"), &["p::Gate::open"]);
    c.is("a sibling member's same-named local is not visible from fire: siblings", f.fire_in("siblings"), &["p::Gate::open"]);
    c.finish();
}

#[test]
fn receiver_chains_are_typed_segment_by_segment() {
    let f = index();
    let mut c = Checks::default();
    c.is("engine.reservoir.flush()", f.callees("Owner", "go"), &["p::Tank::flush"]);
    c.is("this.engine.reservoir.flush()", f.callees("Owner", "viaThis"), &["p::Tank::flush"]);
    c.is("site.unit.reservoir.flush()", f.callees("Owner", "deep"), &["p::Tank::flush"]);
    c.is("Registry.primary.reservoir.flush() starts at an object", f.callees("Owner", "viaObject"), &["p::Tank::flush"]);
    c.is("Gear.LOW.shift() starts at an enum entry", f.callees("Owner", "viaEnum"), &["p::Gear::shift"]);
    c.is("a generic constructor types its property", f.callees("Owner", "generic"), &["p::Crate::seal"]);
    c.finish();
}

#[test]
fn a_library_receiver_gets_no_edge_and_a_fun_interface_parses() {
    let f = index();
    let mut c = Checks::default();
    // `Regex.find` must not bind to the project's `Pattern.find`.
    c.is("a Regex property gets no edge: first", f.callees("LibraryUser", "first"), &[]);
    c.is("a JDK call result gets no edge: scheduled", f.callees("LibraryUser", "scheduled"), &[]);
    // `Thread { … }.start()` must not bind to the project's `Worker.start`.
    c.is("a trailing-lambda constructor and library calls type a local as a library type: run", f.callees("ThreadUser", "run"), &[]);
    c.is("a library-typed parameter gets no edge: the anonymous onOpen", f.on_open_in_listen(), &[]);
    // `Thread.start` must not bind to the project's `Worker.start`.
    c.is("a receiver chain through a library type gets no edge: library", f.callees("Owner", "library"), &[]);
    c.is("the project's own extension of a library type is still reached: extension", f.callees("Owner", "extension"), &["Thread::label"]);
    // The property may come from the library base, so it is neither typed nor treated as a
    // library type.
    c.is("a receiver not found up to a library base class keeps the name-only resolution: go", f.callees("LibScreen", "go"), &["p::Sink::drainAll"]);
    c.is("a fun interface does not swallow the declaration after it: SessionEvents is an interface", vec![f.node_kind("p::SessionEvents")], &["interface"]);
    c.is("the declaration after a fun interface keeps its members: lost", f.callees("Supervisor", "lost"), &["p::SessionEvents::onLost"]);
    c.finish();
}
