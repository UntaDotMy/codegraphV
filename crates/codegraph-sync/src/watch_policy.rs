//! Watch policy — port of `src/sync/watch-policy.ts`.
//!
//! Decides whether the live file watcher should run for a given project.
//!
//! Native recursive watching is pathologically slow on WSL2 `/mnt/*` drives (NTFS exposed over
//! the 9p/drvfs bridge): setting up the recursive watch walks the directory tree, and every
//! readdir/stat crosses the Windows boundary. Inside an MCP server this stalls the event loop
//! during startup long enough to blow past host handshake timeouts (opencode's 30s), so the tools
//! never appear (issue #199).
//!
//! This module centralizes the on/off decision so the watcher, the MCP server (for diagnostics),
//! and the installer all agree.

use codegraph_jsre::re;
use codegraph_types::nodepath;
use codegraph_types::paths::normalize_path;
use std::collections::HashMap;
use std::sync::Mutex;

static WSL_CACHE: Mutex<Option<bool>> = Mutex::new(None);
static WSL_WINDOWS_DRIVE_OVERRIDE: Mutex<Option<fn(&str) -> bool>> = Mutex::new(None);

/// Detect whether the current process is running under WSL (Windows Subsystem for Linux). Result
/// is cached after the first call.
///
/// Checks the WSL-specific env vars first (no I/O), then falls back to `/proc/version`, which
/// contains "microsoft" on WSL kernels.
pub fn detect_wsl() -> bool {
    let mut cache = WSL_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = *cache {
        return v;
    }
    let value = if !cfg!(target_os = "linux") {
        false
    } else if std::env::var_os("WSL_DISTRO_NAME").is_some_and(|v| !v.is_empty()) || std::env::var_os("WSL_INTEROP").is_some_and(|v| !v.is_empty()) {
        true
    } else {
        match std::fs::read("/proc/version") {
            Ok(bytes) => {
                let version = String::from_utf8_lossy(&bytes).to_lowercase();
                version.contains("microsoft") || version.contains("wsl")
            }
            Err(_) => false,
        }
    };
    *cache = Some(value);
    value
}

/// True for WSL Windows-drive mounts like `/mnt/c` or `/mnt/d/project`. Deliberately matches only
/// single-letter drive mounts, so genuinely fast Linux mounts such as `/mnt/wsl/...` are not
/// flagged.
pub fn is_windows_drive_mount(project_root: &str) -> bool {
    re!(r"^/mnt/[a-z](/|$)", "i").is_match(&normalize_path(project_root))
}

/// Is `p` on a Windows drive as WSL sees it (`/mnt/c/...`)? There the same tree is reachable from
/// Windows-native CodeGraph too, and the two must not share one index (#995). Always false off
/// WSL, without touching the disk.
pub fn is_wsl_windows_drive(p: &str) -> bool {
    let override_fn = *WSL_WINDOWS_DRIVE_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = override_fn {
        return f(p);
    }
    detect_wsl() && is_windows_drive_mount(&nodepath::resolve(&[p]))
}

/// Inputs that can be overridden in tests so the decision is deterministic without touching real
/// env vars or `/proc/version`.
#[derive(Debug, Clone, Default)]
pub struct WatchProbe {
    /// Defaults to the process environment.
    pub env: Option<HashMap<String, String>>,
    /// Defaults to [`detect_wsl`].
    pub is_wsl: Option<bool>,
}

/// Decide whether the file watcher should be disabled for a project, and why. Returns a short
/// human-readable reason when watching should be skipped, or `None` when it should run normally.
///
/// Precedence (first match wins):
///  1. `CODEGRAPH_NO_WATCH=1`    → off  (explicit opt-out always wins)
///  2. `CODEGRAPH_FORCE_WATCH=1` → on   (overrides auto-detection)
///  3. WSL2 + `/mnt/*` drive     → off  (recursive watching is too slow; #199)
pub fn watch_disabled_reason(project_root: &str, probe: &WatchProbe) -> Option<String> {
    let env = |key: &str| -> Option<String> {
        match &probe.env {
            Some(map) => map.get(key).cloned(),
            None => std::env::var(key).ok(),
        }
    };
    if env("CODEGRAPHV_NO_WATCH").as_deref() == Some("1") {
        return Some("CODEGRAPHV_NO_WATCH=1 is set".to_string());
    }
    if env("CODEGRAPHV_FORCE_WATCH").as_deref() == Some("1") {
        return None;
    }
    let is_wsl = probe.is_wsl.unwrap_or_else(detect_wsl);
    if is_wsl && is_windows_drive_mount(project_root) {
        return Some("project is on a WSL2 /mnt/ drive, where recursive fs.watch is too slow to be reliable".to_string());
    }
    None
}

/// Test-only: reset the cached WSL detection.
pub fn reset_wsl_cache_for_tests() {
    *WSL_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Test-only: decide [`is_wsl_windows_drive`] with `f` (`None` restores detection).
pub fn set_wsl_windows_drive_for_tests(f: Option<fn(&str) -> bool>) {
    *WSL_WINDOWS_DRIVE_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = f;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_mounts() {
        assert!(is_windows_drive_mount("/mnt/c"));
        assert!(is_windows_drive_mount("/mnt/d/project"));
        assert!(!is_windows_drive_mount("/mnt/wsl/x"));
    }

    #[test]
    fn reasons() {
        let probe = WatchProbe { env: Some(HashMap::new()), is_wsl: Some(true) };
        assert!(watch_disabled_reason("/mnt/c/p", &probe).is_some());
        let forced = WatchProbe { env: Some(HashMap::from([("CODEGRAPHV_FORCE_WATCH".to_string(), "1".to_string())])), is_wsl: Some(true) };
        assert!(watch_disabled_reason("/mnt/c/p", &forced).is_none());
        let off = WatchProbe { env: Some(HashMap::from([("CODEGRAPHV_NO_WATCH".to_string(), "1".to_string())])), is_wsl: Some(false) };
        assert_eq!(watch_disabled_reason("/home/p", &off).as_deref(), Some("CODEGRAPHV_NO_WATCH=1 is set"));
    }
}
