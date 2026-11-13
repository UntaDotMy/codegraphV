//! Lazy, newline-delimited MCP session. No engine is opened during initialize.
use crate::tools::{ToolHandler, INSTRUCTIONS, NO_INDEX_INSTRUCTIONS};
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};
pub(crate) const MAX_FRAME_BYTES: usize = 1_048_576;
pub(crate) const MAX_QUEUED_FRAMES: usize = 32;

/// At most one sentinel byte beyond the limit is read, including for unterminated input.
pub(crate) fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = String::new();
    let size = reader.take((MAX_FRAME_BYTES + 1) as u64).read_line(&mut line)?;
    if size > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "MCP frame exceeds 1 MiB"));
    }
    Ok((size != 0).then_some(line))
}
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A host may show this in place of `name`. The SVG is a data URI so the server carries its
/// identity without a network fetch (upstream f4c4dae9).
const SERVER_ICON: &str = "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAzMiAzMiIgZmlsbD0ibm9uZSIgc3Ryb2tlPSIjMTYxNTBmIiBzdHJva2Utd2lkdGg9IjIiPjxyZWN0IHdpZHRoPSIzMiIgaGVpZ2h0PSIzMiIgcng9IjYiIGZpbGw9IiNmN2Y2ZjIiIHN0cm9rZT0ibm9uZSIvPjxsaW5lIHgxPSIxNiIgeTE9IjgiIHgyPSI4IiB5Mj0iMjMiLz48bGluZSB4MT0iMTYiIHkxPSI4IiB4Mj0iMjQiIHkyPSIyMyIvPjxsaW5lIHgxPSI4IiB5MT0iMjMiIHgyPSIyNCIgeTI9IjIzIi8+PGNpcmNsZSBjeD0iMTYiIGN5PSI4IiByPSIzLjQiIGZpbGw9IiMxNjE1MGYiLz48Y2lyY2xlIGN4PSI4IiBjeT0iMjMiIHI9IjMuNCIgZmlsbD0iI2Y3ZjZmMiIvPjxjaXJjbGUgY3g9IjI0IiBjeT0iMjMiIHI9IjMuNCIgZmlsbD0iI2Y3ZjZmMiIvPjwvc3ZnPg==";

fn server_info() -> Value {
    json!({
        "name": "codegraphV",
        "title": "CodeGraphV",
        "version": env!("CARGO_PKG_VERSION"),
        "icons": [{"src": SERVER_ICON, "mimeType": "image/svg+xml"}]
    })
}
pub struct Session {
    pub handler: ToolHandler,
    initialized: bool,
    roots_requested: bool,
    pub exiting: bool,
    explicit_path: bool,
}
impl Session {
    pub(crate) fn process_frame(&mut self, frame: &crate::requests::RequestFrame) -> Vec<Value> {
        if let Some(response) = frame.cancelled_response() { return vec![response]; }
        self.handler.cancellation = frame.token.clone();
        let responses = self.process(&frame.line);
        self.handler.cancellation = None;
        responses
    }
    pub fn new(path: Option<String>, watch: bool) -> Self {
        let explicit_path = path.is_some();
        Self {
            handler: ToolHandler::new(path, watch),
            initialized: false,
            roots_requested: false,
            exiting: false,
            explicit_path,
        }
    }
    pub fn process(&mut self, line: &str) -> Vec<Value> {
        let request: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return vec![error(Value::Null, -32700, "Parse error")],
        };
        if request["id"] == "codegraph-roots" && request.get("method").is_none() {
            if !self.explicit_path {
                self.adopt_roots(&request["result"]);
            }
            return vec![];
        }
        let id = request.get("id").cloned();
        let method = request["method"].as_str();
        if request["jsonrpc"] != "2.0"
            || method.is_none()
            || id
                .as_ref()
                .is_some_and(|id| !id.is_string() && !id.is_number() && !id.is_null())
        {
            return vec![error(id.unwrap_or(Value::Null), -32600, "Invalid Request")];
        }
        let method = method.unwrap();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        if method == "exit" {
            self.exiting = true;
            return vec![];
        }
        if id.is_none() {
            if method == "notifications/roots/list_changed" && !self.explicit_path {
                self.roots_requested = true;
                return vec![
                    json!({"jsonrpc":"2.0","id":"codegraph-roots","method":"roots/list","params":{}}),
                ];
            }
            return vec![];
        }
        let id = id.unwrap();
        if !params.is_object() && !params.is_null() {
            return vec![error(id, -32602, "Invalid params")];
        }
        let result = match method {
            "initialize" => {
                self.initialized = true;
                self.handler.reset_session();
                if !self.explicit_path {
                    for key in ["rootUri", "rootPath"] {
                        if let Some(path) = params[key].as_str().and_then(file_path) {
                            self.handler.default_path = Some(path);
                            break;
                        }
                    }
                }
                let result = json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{"tools":{"listChanged":true}},"serverInfo":server_info(),"instructions":codegraph_installer::upgrade::initialize_instructions(if self.handler.default_root().is_some(){INSTRUCTIONS}else{NO_INDEX_INSTRUCTIONS},codegraph_installer::upgrade::get_update_notice(env!("CARGO_PKG_VERSION")).as_deref())});
                let mut messages = vec![response(id, result)];
                if !self.explicit_path && params["capabilities"].get("roots").is_some() {
                    self.roots_requested = true;
                    messages.push(json!({"jsonrpc":"2.0","id":"codegraph-roots","method":"roots/list","params":{}}));
                }
                return messages;
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools":self.handler.list_tools()}),
            "tools/call" => {
                if !self.initialized {
                    return vec![error(id, -32002, "Server not initialized")];
                }
                let Some(name) = params["name"].as_str() else {
                    return vec![error(id, -32602, "Tool name must be a string")];
                };
                if !serde_json::from_str::<Vec<Value>>(crate::tools::TOOLS_JSON)
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == name)
                {
                    return vec![error(id, -32602, "Unknown tool")];
                }
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                if !args.is_object() {
                    return vec![error(id, -32602, "Tool arguments must be an object")];
                }
                serde_json::to_value(self.handler.call(name, &args)).unwrap()
            }
            "shutdown" => {
                self.exiting = true;
                Value::Null
            }
            _ => return vec![error(id, -32601, "Method not found")],
        };
        vec![response(id, result)]
    }
    fn adopt_roots(&mut self, result: &Value) {
        if let Some(roots) = result["roots"].as_array() {
            let paths: Vec<_> = roots
                .iter()
                .filter_map(|r| r["uri"].as_str().and_then(file_path))
                .collect();
            if let Some(path) = paths
                .iter()
                .find(|p| {
                    codegraph_core::directory::resolve_server_root(p, true)
                        .root
                        .is_some()
                })
                .or(paths.first())
            {
                self.handler.default_path = Some(path.clone());
            }
        }
        self.roots_requested = false;
    }
}
fn file_path(path: &str) -> Option<String> {
    if path.starts_with("file:") {
        url::Url::parse(path)
            .ok()?
            .to_file_path()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    } else if path.contains("://") {
        None
    } else {
        Some(path.into())
    }
}
fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
/// The host stopped reading our standard output. Treat it as a normal end of session so the
/// engine is dropped (releasing the writer lock) instead of the process failing on EPIPE.
pub(crate) fn client_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted | io::ErrorKind::NotConnected
    ) || matches!(error.raw_os_error(), Some(109 | 232 | 10054))
}

fn emit<W: Write>(writer: &mut W, response: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, response)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Serve newline-delimited requests from `reader`. Lines are read on their own thread, so a
/// project opened for a `projectPath` is released on its own idle deadline while the client is
/// quiet (#2087).
pub fn serve<R: BufRead + Send + 'static, W: Write>(
    mut reader: R,
    mut writer: W,
    path: Option<String>,
    watch: bool,
) -> io::Result<()> {
    let mut session = Session::new(path, watch);
    codegraph_installer::upgrade::check_for_update_in_background(env!("CARGO_PKG_VERSION"));
    let pending = crate::requests::PendingRequests::default();
    let (sender, lines) = std::sync::mpsc::sync_channel::<io::Result<crate::requests::RequestFrame>>(MAX_QUEUED_FRAMES);
    std::thread::spawn(move || {
        loop {
            match read_frame(&mut reader) {
                Ok(Some(line)) => {
                    if pending.cancel_notification(&line) { continue; }
                    let frame = match pending.frame(line) {
                        Ok(frame) => frame,
                        Err(error) => {
                            pending.cancel_all();
                            let _ = sender.send(Err(io::Error::new(io::ErrorKind::InvalidData, error)));
                            break;
                        }
                    };
                    match sender.try_send(Ok(frame)) {
                        Ok(()) => {},
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            pending.cancel_all();
                            let _ = sender.send(Err(io::Error::new(io::ErrorKind::InvalidData, "MCP request queue full; reconnect and retry")));
                            break;
                        }
                    }
                },
                Ok(None) => break,
                Err(error) => { pending.cancel_all(); let _ = sender.send(Err(error)); break; }
            }
        }
    });
    loop {
        let next = match session.handler.next_release_due() {
            Some(due) => lines.recv_timeout(due.saturating_duration_since(std::time::Instant::now())),
            None => lines.recv().map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
        };
        let frame = match next {
            Ok(line) => line?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                session.handler.release_idle(std::time::Instant::now());
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if frame.line.trim().is_empty() {
            continue;
        }
        for response in session.process_frame(&frame) {
            if let Err(error) = emit(&mut writer, &response) {
                if client_gone(&error) {
                    return Ok(());
                }
                return Err(error);
            }
        }
        if session.exiting {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod frame_tests {
    use super::*;
    #[test]
    fn stdio_cancels_a_queued_tool_and_keeps_the_next_request_usable() {
        use std::sync::{Arc, Condvar, Mutex};
        struct Input {
            cursor: io::Cursor<Vec<u8>>,
            read_done: Arc<(Mutex<bool>, Condvar)>,
        }
        impl Read for Input {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> { self.cursor.read(bytes) }
        }
        impl BufRead for Input {
            fn fill_buf(&mut self) -> io::Result<&[u8]> { self.cursor.fill_buf() }
            fn consume(&mut self, count: usize) { self.cursor.consume(count); }
        }
        impl Drop for Input {
            fn drop(&mut self) {
                *self.read_done.0.lock().unwrap() = true;
                self.read_done.1.notify_all();
            }
        }
        struct Output {
            bytes: Vec<u8>,
            read_done: Arc<(Mutex<bool>, Condvar)>,
        }
        impl Write for Output {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let (ready, _) = self.read_done.1.wait_timeout_while(
                    self.read_done.0.lock().unwrap(), std::time::Duration::from_secs(5), |done| !*done).unwrap();
                assert!(*ready, "input reader failed to finish");
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let dir = tempfile::tempdir().unwrap();
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"codegraphV_explore","arguments":{"query":"target"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}),
            json!({"jsonrpc":"2.0","id":3,"method":"ping"}),
        ].iter().map(|v| format!("{v}\n")).collect::<String>();
        let mut output = Output { bytes: Vec::new(), read_done: done.clone() };
        serve(Input { cursor: io::Cursor::new(input.into_bytes()), read_done: done }, &mut output,
            Some(dir.path().to_string_lossy().into_owned()), false).unwrap();
        let replies: Vec<Value> = String::from_utf8(output.bytes).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[1]["id"], 2);
        assert!(replies[1]["result"]["content"][0]["text"].as_str().unwrap().contains("cancelled"));
        assert!(replies[1]["result"]["isError"].is_null());
        assert_eq!(replies[2]["id"], 3);
        assert!(replies[2]["result"].is_object());
        assert!(!dir.path().join(".codegraphV").exists());
    }
    #[test]
    fn oversized_and_unterminated_frames_stop_at_the_limit() {
        for suffix in ["", "\n"] {
            let data = format!("{}{suffix}", "x".repeat(MAX_FRAME_BYTES * 2));
            let mut reader = io::Cursor::new(data.as_bytes());
            assert_eq!(read_frame(&mut reader).unwrap_err().kind(), io::ErrorKind::InvalidData);
            assert_eq!(reader.position(), (MAX_FRAME_BYTES + 1) as u64);
        }
        let mut reader = io::Cursor::new(b"one\ntwo\n");
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("one\n"));
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("two\n"));
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    struct Gone;
    impl Write for Gone {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "client closed stdout"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_client_stdout_is_a_clean_shutdown() {
        std::env::set_var("CODEGRAPHV_NO_UPDATE_CHECK", "1");
        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"0\"}}}\n";
        assert!(serve(io::Cursor::new(request), Gone, None, false).is_ok());
    }
}
