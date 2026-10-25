use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Daemon {
    child: Child,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            if let Some(mut stderr) = self.child.stderr.take() {
                let mut log = String::new();
                let _ = std::io::Read::read_to_string(&mut stderr, &mut log);
                eprintln!("daemon stderr: {log}");
            }
        }
    }
}
fn send(stream: &mut BufReader<TcpStream>, request: Value) -> Value {
    eprintln!("sending {request}");
    writeln!(stream.get_mut(), "{request}").unwrap();
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
fn connect(endpoint: &Value) -> BufReader<TcpStream> {
    let stream =
        TcpStream::connect(("127.0.0.1", endpoint["port"].as_u64().unwrap() as u16)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    writeln!(
        reader.get_mut(),
        "{}",
        json!({"token":endpoint["token"],"version":env!("CARGO_PKG_VERSION")})
    )
    .unwrap();
    let mut ack = String::new();
    reader.read_line(&mut ack).unwrap();
    assert_eq!(ack.trim(), "ok");
    reader
}
#[test]
fn shared_daemon_reuses_engine_and_keeps_client_source_history_separate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.ts"),
        "export function finish(){return 42;}\nexport function start(){return finish();}\n",
    )
    .unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    cg.close();
    let mut daemon = Daemon {
        child: Command::new(env!("CARGO_BIN_EXE_codegraphV"))
            .args([
                "__daemon",
                "--path",
                dir.path().to_str().unwrap(),
                "--idle-timeout",
                "20",
            ])
            .env_remove("CODEGRAPHV_MCP_TOOLS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    };
    let path = dir.path().join(".codegraphV/native-daemon.json");
    let begin = Instant::now();
    let endpoint: Value = loop {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(v) = serde_json::from_slice(&bytes) {
                break v;
            }
        }
        assert!(begin.elapsed() < Duration::from_secs(10));
        assert!(daemon.child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut first = connect(&endpoint);
    let mut second = connect(&endpoint);
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
    assert!(send(&mut first, initialize.clone()).get("result").is_some());
    assert!(send(&mut second, initialize).get("result").is_some());
    let request = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"codegraphV_explore","arguments":{"query":"start finish"}}});
    let a = send(&mut first, request.clone());
    let b = send(&mut second, request.clone());
    assert!(a["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("return 42"));
    assert!(b["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("return 42"));
    let repeat = send(&mut first, request);
    assert!(!repeat["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("1\texport function finish"));
    let status = send(
        &mut first,
        json!({"jsonrpc":"2.0","id":3,"method":"daemon/status"}),
    );
    assert_eq!(status["result"]["projects"], 1);
    assert_eq!(status["result"]["clients"], 2);
    let stop = send(
        &mut first,
        json!({"jsonrpc":"2.0","id":4,"method":"daemon/stop"}),
    );
    assert_eq!(stop["result"]["stopped"], true);
    drop(first);
    drop(second);
    let start = Instant::now();
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!path.exists());
    assert!(!dir.path().join(".codegraphV/native-daemon.lock").exists());
}

/// BUG-89: with no clients left, the daemon leaves after its idle timeout and removes the
/// endpoint and lock that would claim it is still alive.
#[test]
fn idle_daemon_exits_and_leaves_no_state() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.ts"), "export function start(){return 1;}\n").unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    cg.close();
    let mut daemon = Daemon {
        child: Command::new(env!("CARGO_BIN_EXE_codegraphV"))
            .args(["__daemon", "--path", dir.path().to_str().unwrap(), "--idle-timeout", "1"])
            .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    };
    let path = dir.path().join(".codegraphV/native-daemon.json");
    let begin = Instant::now();
    let endpoint: Value = loop {
        if let Some(v) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
            break v;
        }
        assert!(begin.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    };
    // One client comes and goes; the idle clock starts when it leaves.
    let mut client = connect(&endpoint);
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
    assert!(send(&mut client, init).get("result").is_some());
    drop(client);
    let left = Instant::now();
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(left.elapsed() < Duration::from_secs(6), "still running 6 s after its 1 s idle timeout");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!path.exists());
    assert!(!dir.path().join(".codegraphV/native-daemon.lock").exists());
}
