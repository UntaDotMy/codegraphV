//! An agent's MCP connection survives the codegraphV binary being replaced under it (DEV-121).
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

fn exe_name() -> &'static str {
    if cfg!(windows) { "codegraphV.exe" } else { "codegraphV" }
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.ts"),
        "export function finish(){return 42;}\nexport function start(){return finish();}\n",
    )
    .unwrap();
    codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap().close();
    dir
}

/// An install of its own, so replacing the binary touches nothing else.
fn install() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join(exe_name());
    std::fs::copy(env!("CARGO_BIN_EXE_codegraphV"), &exe).unwrap();
    (dir, exe)
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<Value>,
    exe: PathBuf,
    root: PathBuf,
}

impl Server {
    fn start(exe: &Path, root: &Path) -> Self {
        let mut child = Command::new(exe)
            .args(["serve", "--mcp", "--path", root.to_str().unwrap()])
            .env_remove("CODEGRAPHV_DAEMON")
            .env_remove("CODEGRAPHV_MCP_TOOLS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str(&line) {
                    if sender.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        let stdin = child.stdin.take().unwrap();
        Self { child, stdin, lines, exe: exe.to_path_buf(), root: root.to_path_buf() }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Every line up to and including the first that `done` accepts.
    fn read_until(&self, timeout: Duration, done: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(v) => {
                    let stop = done(&v);
                    seen.push(v);
                    if stop {
                        return seen;
                    }
                }
                Err(_) => panic!("timed out; saw {seen:?}"),
            }
        }
    }

    fn explore(&mut self, id: u64) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"codegraphV_explore","arguments":{"query":"start finish"}}}));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new(&self.exe)
            .args(["daemon", "--stop", "--path", self.root.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn answered(id: u64) -> impl Fn(&Value) -> bool {
    move |v| v["id"] == id && v.get("method").is_none()
}

fn text(v: &Value) -> &str {
    v["result"]["content"][0]["text"].as_str().unwrap_or_default()
}

fn endpoint(root: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(root.join(".codegraphV/native-daemon.json")).ok()?).ok()
}

#[test]
fn a_session_moves_to_the_replaced_binary_on_the_same_connection() {
    let root = project();
    let (_install, exe) = install();
    let mut server = Server::start(&exe, root.path());
    server.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}));
    let init = server.read_until(Duration::from_secs(20), answered(0));
    assert_eq!(init.last().unwrap()["result"]["capabilities"]["tools"]["listChanged"], true);
    server.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    server.explore(1);
    let first = server.read_until(Duration::from_secs(20), answered(1));
    assert!(text(first.last().unwrap()).contains("return 42"), "{first:?}");
    let before = endpoint(root.path()).expect("a shared daemon serves the session")["pid"].clone();

    // Replace the binary the way the installer does: move the running one aside, copy the new one in.
    let aside = exe.with_extension("old");
    std::fs::rename(&exe, &aside).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_codegraphV"), &exe).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&exe)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(120))
        .unwrap();

    let handoff = server.read_until(Duration::from_secs(60), |v| v["method"] == "notifications/tools/list_changed");
    assert!(handoff.iter().all(|v| !v["id"].as_str().is_some_and(|id| id.starts_with("codegraphV-handoff"))), "{handoff:?}");
    server.explore(2);
    let second = server.read_until(Duration::from_secs(30), answered(2));
    assert!(text(second.last().unwrap()).contains("return 42"), "{second:?}");
    assert!(second.iter().all(|v| !v["id"].as_str().is_some_and(|id| id.starts_with("codegraphV-handoff"))), "{second:?}");
    let after = endpoint(root.path()).expect("the successor daemon published its endpoint")["pid"].clone();
    assert_ne!(before, after, "a new daemon answers after the hand-off");
    assert!(server.child.try_wait().unwrap().is_none(), "the agent's process never exited");
}

#[test]
fn a_live_older_daemon_does_not_keep_a_new_session_from_starting() {
    let root = project();
    let (_install, exe) = install();
    // A daemon of an older release still serves the project for another session; it neither
    // accepts this version nor gives up its lock.
    let me = std::process::id();
    let dir = root.path().join(".codegraphV");
    std::fs::write(dir.join("native-daemon.lock"), me.to_string()).unwrap();
    let old = json!({"pid":me,"port":9,"token":"x","projectPath":root.path(),"version":"0.0.1"});
    std::fs::write(dir.join("native-daemon.json"), old.to_string()).unwrap();

    let mut server = Server::start(&exe, root.path());
    server.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}));
    server.read_until(Duration::from_secs(20), answered(0));
    server.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    server.explore(1);
    let reply = server.read_until(Duration::from_secs(20), answered(1));
    assert!(text(reply.last().unwrap()).contains("return 42"), "{reply:?}");
    assert_eq!(endpoint(root.path()).unwrap(), old, "served in-process; the older daemon's endpoint is untouched");
}
