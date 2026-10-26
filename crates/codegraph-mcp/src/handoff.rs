//! Handing an open MCP session to an upgraded engine without the agent reconnecting.
//!
//! The agent's stdio process is a proxy to the project's shared daemon. When the codegraphV
//! binary is replaced (`codegraphV upgrade`, a rebuild), the daemon notices and leaves; each
//! proxy then starts the new binary's daemon, replays its session into it and tells the agent
//! the tool list may have changed. The agent's connection never drops, which matters because
//! Claude Code marks a stdio server that exits mid-session as failed until the user reconnects.
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::SystemTime,
};

/// Sent to the agent after a hand-off: the new engine may expose different tools.
pub const TOOLS_CHANGED: &str = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;

/// The session state a proxy needs to rebuild its daemon-side session on a successor.
#[derive(Default)]
pub struct Relay {
    initialize: Option<Value>,
    initialized: Option<String>,
    /// Client requests not yet answered, by serialized id, in the order they were sent.
    pending: IndexMap<String, String>,
    /// Serialized id of the replayed initialize, whose answer belongs to the proxy.
    replayed_initialize: Option<String>,
    handoffs: u64,
    answered: u64,
}

impl Relay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a line the agent sent to the daemon.
    pub fn from_client(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else { return };
        let Some(method) = message["method"].as_str() else { return };
        match message.get("id").filter(|id| !id.is_null()) {
            Some(id) => {
                if method == "initialize" {
                    self.initialize = Some(message.get("params").cloned().unwrap_or(json!({})));
                }
                self.pending.insert(id.to_string(), line.trim_end().to_string());
            }
            None if method == "notifications/initialized" => self.initialized = Some(line.trim_end().to_string()),
            None => {}
        }
    }

    /// Account for a line the daemon sent; false when it must not reach the agent.
    pub fn from_daemon(&mut self, line: &str) -> bool {
        let Ok(message) = serde_json::from_str::<Value>(line) else { return true };
        if message.get("method").is_some() {
            return true;
        }
        let Some(id) = message.get("id").map(Value::to_string) else { return true };
        if self.replayed_initialize.as_ref() == Some(&id) {
            self.replayed_initialize = None;
            return false;
        }
        if self.pending.shift_remove(&id).is_some() {
            self.answered += 1;
        }
        true
    }

    /// How many of the client's requests have been answered so far.
    pub fn answered(&self) -> u64 {
        self.answered
    }

    /// The lines that rebuild this session on a successor daemon: the agent's initialize (under
    /// an id of the proxy's own once the agent has had its answer), its initialized
    /// notification, then every request still unanswered, in order.
    pub fn replay(&mut self) -> Vec<String> {
        self.handoffs += 1;
        let mut lines = Vec::new();
        let initialize_pending = self
            .pending
            .values()
            .any(|l| serde_json::from_str::<Value>(l).is_ok_and(|m| m["method"] == "initialize"));
        if let (Some(params), false) = (&self.initialize, initialize_pending) {
            let id = Value::String(format!("codegraphV-handoff-{}", self.handoffs));
            self.replayed_initialize = Some(id.to_string());
            lines.push(json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":params}).to_string());
            lines.extend(self.initialized.clone());
        }
        lines.extend(self.pending.values().cloned());
        lines
    }
}

/// Order two versions; anything that is not a version only equals itself.
fn order(a: &str, b: &str) -> Option<Ordering> {
    codegraph_installer::upgrade::compare_versions(a, b).or_else(|| (a == b).then_some(Ordering::Equal))
}

/// A daemon serves proxies of its own version or older: a proxy only relays bytes, so an old one
/// can ride a newer engine after a hand-off.
pub fn daemon_accepts_client(daemon: &str, client: &str) -> bool {
    matches!(order(client, daemon), Some(Ordering::Less | Ordering::Equal))
}

/// A proxy uses a daemon of its own version or newer, never an older engine.
pub fn proxy_uses_daemon(proxy: &str, daemon: &str) -> bool {
    matches!(order(daemon, proxy), Some(Ordering::Greater | Ordering::Equal))
}

type Stamp = (u64, SystemTime);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// Notices the binary a daemon was started from being replaced by one that runs.
pub struct ExeWatch {
    path: PathBuf,
    baseline: Option<Stamp>,
    /// A changed stamp seen on the previous poll; it must hold for a whole poll to count.
    candidate: Option<Stamp>,
    rejected: Option<Stamp>,
}

impl ExeWatch {
    pub fn new(path: PathBuf) -> Self {
        let baseline = stamp(&path);
        Self { path, baseline, candidate: None, rejected: None }
    }

    /// True once the binary differs from the one at start, has not changed since the previous
    /// poll (an installer may still be copying it), and `runs` accepts it. A missing file is a
    /// rename in progress, and a binary `runs` refused is not tried again until it changes.
    pub fn poll(&mut self, runs: impl FnOnce(&Path) -> bool) -> bool {
        let Some(now) = stamp(&self.path) else {
            self.candidate = None;
            return false;
        };
        if Some(now) == self.baseline || Some(now) == self.rejected {
            self.candidate = None;
            return false;
        }
        if self.candidate != Some(now) {
            self.candidate = Some(now);
            return false;
        }
        if runs(&self.path) {
            return true;
        }
        self.rejected = Some(now);
        self.candidate = None;
        false
    }
}

/// Whether `path --version` succeeds: the replacement is a working binary, not a torn copy.
pub fn binary_runs(path: &Path) -> bool {
    let mut command = Command::new(path);
    command.arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.status().is_ok_and(|s| s.success())
}
