//! Authenticated loopback transport for a shared native engine. SQLite stays
//! on one owning thread; each client keeps its own retrieval emission state.
use crate::{
    handoff::{self, ExeWatch, Relay},
    session::Session,
    ToolHandler,
};
use codegraph_core::{directory, utils::FileLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
/// How long a proxy waits for a daemon to start or take over the project.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Shortest gap between two daemon starts while the lock is still held by another.
const DAEMON_RESPAWN_GAP: Duration = Duration::from_millis(250);
/// Hand-offs in a row with no request answered before a proxy gives up: a request that makes
/// every new daemon exit must not restart daemons forever.
const MAX_FRUITLESS_HANDOFFS: u32 = 3;
/// How often a daemon looks at its own binary for an upgrade.
const UPGRADE_POLL: Duration = Duration::from_secs(2);
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub project_path: String,
    pub version: String,
}
fn endpoint_path(root: &str) -> PathBuf {
    Path::new(&directory::get_code_graph_dir(root)).join("native-daemon.json")
}
fn read_endpoint(root: &str) -> Option<Endpoint> {
    serde_json::from_slice(&std::fs::read(endpoint_path(root)).ok()?).ok()
}
fn connect(endpoint: &Endpoint) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], endpoint.port)),
        Duration::from_millis(500),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    writeln!(
        stream,
        "{}",
        json!({"token":endpoint.token,"version":env!("CARGO_PKG_VERSION")})
    )?;
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line)?;
    if line.trim() != "ok" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Native daemon authentication failed",
        ));
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}
pub fn status(root: &str) -> Option<Value> {
    let endpoint = read_endpoint(root)?;
    let mut stream = connect(&endpoint).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    writeln!(
        stream,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"daemon/status"})
    )
    .ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str::<Value>(&line)
        .ok()
        .and_then(|v| v.get("result").cloned())
}
pub fn stop(root: &str) -> Result<bool> {
    let Some(endpoint) = read_endpoint(root) else {
        return Ok(false);
    };
    let Ok(mut stream) = connect(&endpoint) else {
        return Ok(false);
    };
    writeln!(
        stream,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"daemon/stop"})
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str::<Value>(&line)?["result"]["stopped"] == true)
}
/// Windows gives a child every inheritable handle of its parent, so the daemon would get the
/// proxy's stdin/stdout pipes and hold the agent's pipes open after the proxy exits (BUG-89).
#[cfg(windows)]
fn keep_stdio_from_children() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    let handles = [std::io::stdin().as_raw_handle(), std::io::stdout().as_raw_handle(), std::io::stderr().as_raw_handle()];
    for handle in handles.into_iter().filter(|h| !h.is_null()) {
        // SAFETY: a handle this process owns; only its inherit flag changes.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
    }
}

fn spawn_daemon(root: &str, executable: &Path) -> io::Result<Child> {
    #[cfg(windows)]
    keep_stdio_from_children();
    let mut command = Command::new(executable);
    command
        .args(["__daemon", "--path", root])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn()
}

/// Connect to the daemon that serves the project, starting `executable`'s when none is usable.
/// A daemon that leaves for an upgrade removes its endpoint before closing its clients, so the
/// endpoint found here is a live daemon or a crashed one's leftover. `Ok(None)`: a live daemon of
/// an older version holds the project; it predates hand-offs and keeps its lock, so the caller
/// serves in-process.
fn open_daemon(root: &str, executable: &Path) -> Result<Option<TcpStream>> {
    let own = env!("CARGO_PKG_VERSION");
    let deadline = Instant::now() + DAEMON_READY_TIMEOUT;
    let mut child: Option<Child> = None;
    let mut spawned_at: Option<Instant> = None;
    let opened = loop {
        if let Some(endpoint) = read_endpoint(root) {
            if handoff::proxy_uses_daemon(own, &endpoint.version) {
                if let Ok(stream) = connect(&endpoint) {
                    break Some(stream);
                }
            } else if codegraph_core::utils::is_process_alive(endpoint.pid) {
                break None;
            }
        }
        // Start a daemon, or another once ours lost the lock to a winner not yet published or
        // to a predecessor still leaving.
        let running = match child.as_mut() {
            Some(c) => c.try_wait()?.is_none(),
            None => false,
        };
        if !running && spawned_at.is_none_or(|at| at.elapsed() >= DAEMON_RESPAWN_GAP) {
            child = Some(spawn_daemon(root, executable)?);
            spawned_at = Some(Instant::now());
        }
        if Instant::now() >= deadline {
            return Err("Native daemon did not become ready".into());
        }
        std::thread::sleep(Duration::from_millis(40));
    };
    if let Some(mut child) = child {
        // Reap the helper when it eventually leaves, while this proxy lives.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    Ok(opened)
}

struct Link {
    relay: Relay,
    to_daemon: TcpStream,
    client_closed: bool,
}

/// Relay stdio to the project's daemon, preserving half-close: EOF from the client drains
/// responses before returning. When the daemon connection drops while the client is still
/// connected (the daemon left for an upgrade, or the link broke), the session is replayed on
/// whichever daemon now serves the project, on the same stdio connection.
/// Returns false, having read nothing, when an older daemon holds the project (serve in-process).
pub fn proxy(root: &str, executable: &Path) -> Result<bool> {
    let Some(stream) = open_daemon(root, executable)? else {
        return Ok(false);
    };
    let link = Arc::new(Mutex::new(Link { relay: Relay::new(), to_daemon: stream.try_clone()?, client_closed: false }));
    let input = link.clone();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut line = String::new();
        loop {
            line.clear();
            if matches!(stdin.read_line(&mut line), Ok(0) | Err(_)) {
                break;
            }
            // Written under the lock, so a hand-off replays this line or it reaches the
            // successor, never both.
            let mut link = input.lock().unwrap();
            link.relay.from_client(&line);
            let _ = link.to_daemon.write_all(line.as_bytes());
        }
        let mut link = input.lock().unwrap();
        link.client_closed = true;
        let _ = link.to_daemon.shutdown(Shutdown::Write);
    });
    let mut from_daemon = BufReader::new(stream);
    let mut stdout = io::stdout().lock();
    let (mut fruitless, mut answered) = (0, 0);
    loop {
        let mut line = String::new();
        if from_daemon.read_line(&mut line).unwrap_or(0) > 0 {
            if link.lock().unwrap().relay.from_daemon(&line) {
                if let Err(error) = stdout.write_all(line.as_bytes()).and_then(|_| stdout.flush()) {
                    if crate::session::client_gone(&error) {
                        return Ok(true);
                    }
                    return Err(error.into());
                }
            }
            continue;
        }
        {
            let link = link.lock().unwrap();
            if link.client_closed {
                return Ok(true);
            }
            fruitless = if link.relay.answered() == answered { fruitless + 1 } else { 0 };
            answered = link.relay.answered();
        }
        if fruitless >= MAX_FRUITLESS_HANDOFFS {
            return Err("The CodeGraphV daemon keeps exiting before it answers; reconnect the MCP server.".into());
        }
        let Some(next) = open_daemon(root, executable)? else {
            return Err("An older CodeGraphV daemon took over this project; reconnect the MCP server.".into());
        };
        {
            let mut link = link.lock().unwrap();
            link.to_daemon = next.try_clone()?;
            for replayed in link.relay.replay() {
                writeln!(link.to_daemon, "{replayed}")?;
            }
            if link.client_closed {
                let _ = link.to_daemon.shutdown(Shutdown::Write);
            }
        }
        if let Err(error) = writeln!(stdout, "{}", handoff::TOOLS_CHANGED).and_then(|_| stdout.flush()) {
            if crate::session::client_gone(&error) {
                return Ok(true);
            }
            return Err(error.into());
        }
        from_daemon = BufReader::new(next);
    }
}
enum Event {
    Connected(usize, TcpStream, std::sync::Arc<std::sync::Mutex<()>>),
    Line(usize, crate::requests::RequestFrame),
    Closed(usize),
}
struct Client {
    session: Session,
    stream: TcpStream,
    write_lock: std::sync::Arc<std::sync::Mutex<()>>,
}
static NEXT_CLIENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

// A full query queue must not park a reader: it still needs to receive ping/stop.
// Return an explicit retry response for requests; disconnect for notifications whose
// state changes cannot safely be dropped. Never silently discard a request.
fn overload_response(line: &str) -> Option<Value> {
    let request: Value = serde_json::from_str(line).ok()?;
    let id = request.get("id")?;
    Some(if request["method"] == "tools/call" {
        json!({"jsonrpc":"2.0","id":id,"result":{
            "content":[{"type":"text","text":"CodeGraphV is busy: the bounded request queue is full. Retry this request shortly."}]}})
    } else {
        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"CodeGraphV request queue full; retry shortly"}})
    })
}

/// Cheap protocol requests answered on the connection's own reader thread, so a long query in
/// the shared event loop never delays a ping, status or shutdown (A20). Returns the response
/// to write; `daemon/stop` also sets `quit`.
fn control_response(
    line: &str,
    root: &str,
    version: &str,
    clients: &std::sync::atomic::AtomicUsize,
    projects: &std::sync::atomic::AtomicUsize,
) -> Option<(String, bool)> {
    let request: Value = serde_json::from_str(line).ok()?;
    let id = request.get("id")?.clone();
    if request["jsonrpc"] != "2.0"
        || !(id.is_string() || id.is_number() || id.is_null())
        || request.get("params").is_some_and(|p| !p.is_object() && !p.is_null()) {
        return None;
    }
    match request.get("method")?.as_str()? {
        "ping" => Some((json!({"jsonrpc":"2.0","id":id,"result":{}}).to_string(), false)),
        "daemon/status" => Some((json!({"jsonrpc":"2.0","id":id,"result":{
            "pid": std::process::id(),
            "projectPath": root,
            "version": version,
            "clients": clients.load(Ordering::Relaxed),
            "projects": projects.load(Ordering::Relaxed),
        }}).to_string(), false)),
        // The reply is written before shutdown starts, so the caller always sees `stopped`.
        "daemon/stop" => Some((json!({"jsonrpc":"2.0","id":id,"result":{"stopped":true}}).to_string(), true)),
        _ => None,
    }
}

pub fn run(root: &str, stop: &AtomicBool, idle_timeout: Duration) -> Result<()> {
    let root = codegraph_types::paths::real_path(Path::new(root))?
        .to_string_lossy()
        .into_owned();
    if !directory::is_initialized(&root) {
        return Err("Project is not indexed".into());
    }
    let mut lock = FileLock::new(Path::new(&directory::get_code_graph_dir(&root)).join("native-daemon.lock"));
    lock.acquire()?;
    codegraph_installer::upgrade::check_for_update_in_background(env!("CARGO_PKG_VERSION"));
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|e| e.to_string())?;
    let token = random
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    let endpoint = Endpoint {
        pid: std::process::id(),
        port: listener.local_addr()?.port(),
        token: token.clone(),
        project_path: root.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let path = endpoint_path(&root);
    let encoded = serde_json::to_vec(&endpoint)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // We hold the project-specific daemon lock, so a leftover endpoint is stale.
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let mut file = options.open(&path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    drop(file);
    struct Cleanup {
        path: PathBuf,
        token: String,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if std::fs::read(&self.path)
                .ok()
                .and_then(|s| serde_json::from_slice::<Endpoint>(&s).ok())
                .is_some_and(|e| e.token == self.token)
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
    let cleanup = Cleanup {
        path,
        token: token.clone(),
    };
    let (sender, receiver) = mpsc::sync_channel(crate::session::MAX_QUEUED_FRAMES);
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let clients_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let projects_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let quit = Arc::new(AtomicBool::new(false));
    struct CountGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for CountGuard {
        fn drop(&mut self) { self.0.fetch_sub(1, Ordering::Relaxed); }
    }
    let mut clients = std::collections::HashMap::<usize, Client>::new();
    let mut pool = ToolHandler::new(Some(root.clone()), true);
    let mut idle = Instant::now();
    let mut binary = std::env::current_exe().ok().map(ExeWatch::new);
    let mut next_upgrade_poll = Instant::now() + UPGRADE_POLL;
    while !stop.load(Ordering::Relaxed) && !quit.load(Ordering::Relaxed) {
        // A connection flood must not keep the event loop in accept forever.
        for _ in 0..32 {
            let Ok((mut stream, _)) = listener.accept() else { break; };
            if connections.try_update(Ordering::Relaxed, Ordering::Relaxed,
                |n| (n < 32).then_some(n + 1)).is_err() { continue; }
            let connections = connections.clone();
            let clients_count = clients_count.clone();
            let projects_count = projects_count.clone();
            let quit = quit.clone();
            let root = root.clone();
            let sender = sender.clone();
            let token = token.clone();
            std::thread::spawn(move || {
                struct ConnectionGuard(Arc<std::sync::atomic::AtomicUsize>);
                impl Drop for ConnectionGuard {
                    fn drop(&mut self) { self.0.fetch_sub(1, Ordering::Relaxed); }
                }
                let _guard = ConnectionGuard(connections);
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let Ok(clone) = stream.try_clone() else {
                    return;
                };
                let mut reader = BufReader::new(clone);
                let mut auth = String::new();
                if reader.by_ref().take(4096).read_line(&mut auth).is_err() {
                    return;
                }
                let valid = serde_json::from_str::<Value>(&auth).ok().is_some_and(|v| {
                    v["token"] == token
                        && handoff::daemon_accepts_client(env!("CARGO_PKG_VERSION"), v["version"].as_str().unwrap_or_default())
                });
                if !valid {
                    return;
                }
                if stream.write_all(b"ok\n").is_err() {
                    return;
                }
                // Windows keeps socket timeouts per handle: clear the auth timeout on the clone
                // this thread reads from too, or an idle client is dropped after 3 s (BUG-125).
                let _ = stream.set_read_timeout(None);
                let _ = reader.get_ref().set_read_timeout(None);
                let Ok(mut write_stream) = stream.try_clone() else {
                    return;
                };
                clients_count.fetch_add(1, Ordering::Relaxed);
                let _client_guard = CountGuard(clients_count.clone());
                let write_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
                let pending = crate::requests::PendingRequests::default();
                let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
                if sender.try_send(Event::Connected(id, stream, write_lock.clone())).is_err() {
                    return;
                }
                loop {
                    match crate::session::read_frame(&mut reader) {
                        Ok(None) | Err(_) => break,
                        Ok(Some(line)) => {
                            if pending.cancel_notification(&line) { continue; }
                            // Cheap requests are answered here, never queued behind a query.
                            if let Some((reply, stop)) = control_response(
                                &line,
                                &root,
                                env!("CARGO_PKG_VERSION"),
                                &clients_count,
                                &projects_count,
                            ) {
                                let wrote = {
                                    let _writing = write_lock.lock().unwrap_or_else(|e| e.into_inner());
                                    writeln!(write_stream, "{reply}").is_ok()
                                };
                                if stop {
                                    quit.store(true, Ordering::Relaxed);
                                }
                                if !wrote || stop {
                                    break;
                                }
                                continue;
                            }
                            let frame = match pending.frame(line.clone()) {
                                Ok(frame) => frame,
                                Err(_) => {
                                    let Some(reply) = overload_response(&line) else { break; };
                                    let _writing = write_lock.lock().unwrap_or_else(|e| e.into_inner());
                                    if writeln!(write_stream, "{reply}").is_err() { break; }
                                    continue;
                                }
                            };
                            match sender.try_send(Event::Line(id, frame)) {
                                Ok(()) => {},
                                Err(mpsc::TrySendError::Disconnected(_)) => break,
                                Err(mpsc::TrySendError::Full(Event::Line(_, frame))) => {
                                    let Some(reply) = overload_response(&frame.line) else { break; };
                                    let _writing = write_lock.lock().unwrap_or_else(|e| e.into_inner());
                                    if writeln!(write_stream, "{reply}").is_err() { break; }
                                },
                                Err(mpsc::TrySendError::Full(_)) => unreachable!(),
                            }
                        }
                    }
                }
                pending.cancel_all();
                let _ = sender.send(Event::Closed(id));
            });
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Event::Connected(id, stream, write_lock)) => {
                clients.insert(
                    id,
                    Client {
                        session: Session::new(Some(root.clone()), true),
                        stream,
                        write_lock,
                    },
                );
                projects_count.store(pool.open_project_count(), Ordering::Relaxed);
                idle = Instant::now();
            }
            Ok(Event::Closed(id)) => {
                clients.remove(&id);
                projects_count.store(pool.open_project_count(), Ordering::Relaxed);
                idle = Instant::now();
            }
            Ok(Event::Line(id, frame)) => {
                let Some(client) = clients.get_mut(&id) else {
                    continue;
                };
                let responses = {
                    pool.swap_projects(&mut client.session.handler);
                    let responses = client.session.process_frame(&frame);
                    pool.swap_projects(&mut client.session.handler);
                    responses
                };
                let mut failed = false;
                let write_lock = client.write_lock.clone();
                for response in responses {
                    let _writing = write_lock.lock().unwrap_or_else(|e| e.into_inner());
                    if writeln!(client.stream, "{response}").is_err() {
                        failed = true;
                        break;
                    }
                }
                if failed || client.session.exiting {
                    let _ = client.stream.shutdown(Shutdown::Both);
                    clients.remove(&id);
                }
                projects_count.store(pool.open_project_count(), Ordering::Relaxed);
                idle = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // A project opened for a `projectPath` goes on its own idle deadline (#2087).
        if pool.next_release_due().is_some_and(|due| due <= Instant::now()) {
            pool.release_idle(Instant::now());
            projects_count.store(pool.open_project_count(), Ordering::Relaxed);
        }
        if clients.is_empty() && idle.elapsed() >= idle_timeout {
            break;
        }
        // Replaced by an upgrade: leave, and each client's proxy hands its session to the new
        // binary's daemon.
        if next_upgrade_poll <= Instant::now() {
            next_upgrade_poll = Instant::now() + UPGRADE_POLL;
            if binary.as_mut().is_some_and(|b| b.poll(handoff::binary_runs)) {
                break;
            }
        }
    }
    // Free the project before the clients hear we left, so their successor can take it at once.
    drop(pool);
    drop(cleanup);
    drop(lock);
    for (_, client) in clients {
        let _ = client.stream.shutdown(Shutdown::Both);
    }
    Ok(())
}

#[cfg(test)]
mod control_tests {
    use super::*;
    #[test]
    fn overload_is_retryable_and_does_not_silently_drop_notifications() {
        let reply = overload_response(r#"{"jsonrpc":"2.0","id":42,"method":"tools/call"}"#).unwrap();
        assert_eq!(reply["id"], 42);
        assert!(reply["result"]["isError"].is_null());
        assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains("Retry"));
        assert!(overload_response(r#"{"jsonrpc":"2.0","method":"notifications/roots/list_changed"}"#).is_none());
        let reply = overload_response(r#"{"jsonrpc":"2.0","id":"a","method":"initialize"}"#).unwrap();
        assert_eq!(reply["error"]["code"], -32000);
    }
    #[test]
    fn malformed_control_requests_cannot_stop_the_daemon() {
        let count = std::sync::atomic::AtomicUsize::new(1);
        for request in [
            json!({"id":1,"method":"daemon/stop"}),
            json!({"jsonrpc":"1.0","id":1,"method":"daemon/stop"}),
            json!({"jsonrpc":"2.0","id":{},"method":"daemon/stop"}),
            json!({"jsonrpc":"2.0","id":1,"method":"daemon/stop","params":[]}),
            json!({"jsonrpc":"2.0","method":"daemon/stop"}),
        ] {
            assert!(control_response(&request.to_string(), ".", "test", &count, &count).is_none());
        }
        for method in ["ping", "daemon/status", "daemon/stop"] {
            let (_, stopped) = control_response(&json!({"jsonrpc":"2.0","id":1,"method":method}).to_string(),
                ".", "test", &count, &count).unwrap();
            assert_eq!(stopped, method == "daemon/stop");
        }
    }
}
