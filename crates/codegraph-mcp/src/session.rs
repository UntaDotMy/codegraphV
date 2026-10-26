//! Lazy, newline-delimited MCP session. No engine is opened during initialize.
use crate::tools::{ToolHandler, INSTRUCTIONS, NO_INDEX_INSTRUCTIONS};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
pub const PROTOCOL_VERSION: &str = "2024-11-05";
pub struct Session {
    pub handler: ToolHandler,
    initialized: bool,
    roots_requested: bool,
    pub exiting: bool,
    explicit_path: bool,
}
impl Session {
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
                let result = json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"codegraphV","version":env!("CARGO_PKG_VERSION")},"instructions":codegraph_installer::upgrade::initialize_instructions(if self.handler.default_root().is_some(){INSTRUCTIONS}else{NO_INDEX_INSTRUCTIONS},codegraph_installer::upgrade::get_update_notice(env!("CARGO_PKG_VERSION")).as_deref())});
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
/// Serve newline-delimited requests from `reader`. Lines are read on their own thread, so a
/// project opened for a `projectPath` is released on its own idle deadline while the client is
/// quiet (#2087).
pub fn serve<R: BufRead + Send + 'static, W: Write>(
    reader: R,
    mut writer: W,
    path: Option<String>,
    watch: bool,
) -> io::Result<()> {
    let mut session = Session::new(path, watch);
    codegraph_installer::upgrade::check_for_update_in_background(env!("CARGO_PKG_VERSION"));
    let (sender, lines) = std::sync::mpsc::channel::<io::Result<String>>();
    std::thread::spawn(move || {
        for line in reader.lines() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    loop {
        let next = match session.handler.next_release_due() {
            Some(due) => lines.recv_timeout(due.saturating_duration_since(std::time::Instant::now())),
            None => lines.recv().map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
        };
        let line = match next {
            Ok(line) => line?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                session.handler.release_idle(std::time::Instant::now());
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        for response in session.process(&line) {
            serde_json::to_writer(&mut writer, &response)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        if session.exiting {
            break;
        }
    }
    Ok(())
}
