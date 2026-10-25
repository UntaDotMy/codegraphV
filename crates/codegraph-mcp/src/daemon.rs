//! Authenticated loopback transport for a shared native engine. SQLite stays
//! on one owning thread; each client keeps its own retrieval emission state.
use crate::{session::Session, ToolHandler};
use codegraph_core::{directory, utils::FileLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
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
    Path::new(root).join(".codegraph/native-daemon.json")
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
/// Proxy stdio, preserving half-close: EOF from the client drains responses
/// before returning. The daemon stays alive for other clients.
pub fn proxy(root: &str, executable: &Path) -> Result<()> {
    let mut stream = read_endpoint(root)
        .filter(|e| e.version == env!("CARGO_PKG_VERSION"))
        .and_then(|e| connect(&e).ok());
    if stream.is_none() {
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
        let mut child = command.spawn()?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if let Some(connection) = read_endpoint(root).and_then(|e| connect(&e).ok()) {
                stream = Some(connection);
                break;
            }
            if let Some(exit) = child.try_wait()? {
                if !exit.success() {
                    // Another client may have won the daemon lock.
                    if let Some(connection) = read_endpoint(root).and_then(|e| connect(&e).ok()) {
                        stream = Some(connection);
                        break;
                    }
                    return Err(format!("Native daemon exited with {exit}").into());
                }
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        // Reap the helper when it eventually leaves, while this proxy lives.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    let mut stream = stream.ok_or("Native daemon did not become ready")?;
    let mut outgoing = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut outgoing);
        let _ = outgoing.shutdown(Shutdown::Write);
    });
    io::copy(&mut stream, &mut io::stdout().lock())?;
    Ok(())
}
enum Event {
    Connected(usize, TcpStream),
    Line(usize, String),
    Closed(usize),
}
struct Client {
    session: Session,
    stream: TcpStream,
}
static NEXT_CLIENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
pub fn run(root: &str, stop: &AtomicBool, idle_timeout: Duration) -> Result<()> {
    let root = codegraph_types::paths::real_path(Path::new(root))?
        .to_string_lossy()
        .into_owned();
    if !directory::is_initialized(&root) {
        return Err("Project is not indexed".into());
    }
    let mut lock = FileLock::new(Path::new(&root).join(".codegraph/native-daemon.lock"));
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
    let _cleanup = Cleanup {
        path,
        token: token.clone(),
    };
    let (sender, receiver) = mpsc::channel();
    let mut clients = std::collections::HashMap::<usize, Client>::new();
    let mut pool = ToolHandler::new(Some(root.clone()), true);
    let mut idle = Instant::now();
    let mut quitting = false;
    while !stop.load(Ordering::Relaxed) && !quitting {
        while let Ok((mut stream, _)) = listener.accept() {
            let sender = sender.clone();
            let token = token.clone();
            std::thread::spawn(move || {
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
                    v["token"] == token && v["version"] == env!("CARGO_PKG_VERSION")
                });
                if !valid {
                    return;
                }
                if stream.write_all(b"ok\n").is_err() {
                    return;
                }
                let _ = stream.set_read_timeout(None);
                let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
                if sender.send(Event::Connected(id, stream)).is_err() {
                    return;
                }
                loop {
                    let mut line = String::new();
                    let read = reader.by_ref().take(1_048_577).read_line(&mut line);
                    match read {
                        Ok(0) | Err(_) => break,
                        Ok(_) if line.len() > 1_048_576 => break,
                        _ => {
                            if sender.send(Event::Line(id, line)).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = sender.send(Event::Closed(id));
            });
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Event::Connected(id, stream)) => {
                clients.insert(
                    id,
                    Client {
                        session: Session::new(Some(root.clone()), true),
                        stream,
                    },
                );
                idle = Instant::now();
            }
            Ok(Event::Closed(id)) => {
                clients.remove(&id);
                idle = Instant::now();
            }
            Ok(Event::Line(id, line)) => {
                let client_count = clients.len();
                let Some(client) = clients.get_mut(&id) else {
                    continue;
                };
                let control = serde_json::from_str::<Value>(&line).ok();
                let method = control.as_ref().and_then(|v| v["method"].as_str());
                let responses = if method == Some("daemon/status") {
                    vec![
                        json!({"jsonrpc":"2.0","id":control.as_ref().unwrap()["id"],"result":{"pid":endpoint.pid,"projectPath":root,"version":endpoint.version,"clients":client_count,"projects":pool.open_project_count()}}),
                    ]
                } else if method == Some("daemon/stop") {
                    quitting = true;
                    vec![
                        json!({"jsonrpc":"2.0","id":control.as_ref().unwrap()["id"],"result":{"stopped":true}}),
                    ]
                } else {
                    pool.swap_projects(&mut client.session.handler);
                    let responses = client.session.process(&line);
                    pool.swap_projects(&mut client.session.handler);
                    responses
                };
                let mut failed = false;
                for response in responses {
                    if writeln!(client.stream, "{response}").is_err() {
                        failed = true;
                        break;
                    }
                }
                if failed || client.session.exiting {
                    let _ = client.stream.shutdown(Shutdown::Both);
                    clients.remove(&id);
                }
                idle = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // A project opened for a `projectPath` goes on its own idle deadline (#2087).
        if pool.next_release_due().is_some_and(|due| due <= Instant::now()) {
            pool.release_idle(Instant::now());
        }
        if clients.is_empty() && idle.elapsed() >= idle_timeout {
            break;
        }
    }
    for (_, client) in clients {
        let _ = client.stream.shutdown(Shutdown::Both);
    }
    drop(pool);
    Ok(())
}
