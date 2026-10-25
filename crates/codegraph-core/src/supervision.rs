//! The parent watchdog of `src/mcp/ppid-watchdog.ts` (#277, #692): an MCP server (direct or the
//! daemon proxy) notices that the agent that started it has gone, even when its stdin stays
//! open (a launcher shim or a grandchild holding the pipe), and exits instead of lingering.
//!
//! Parent death looks different per OS. POSIX reparents an orphan, so the parent pid CHANGES;
//! that is the signal there (a dead original parent is not proof: a double-forked launcher may
//! legitimately exit). Windows never reparents, so the original parent's liveness is polled.
use std::time::Duration;

/// Default poll cadence.
pub const DEFAULT_POLL_MS: u64 = 5000;

/// Why the process lost its supervisor, or None while it is supervised.
pub fn supervision_lost_reason(original: u32, current: u32, windows: bool, is_alive: impl Fn(u32) -> bool) -> Option<String> {
    if current != original {
        return Some(format!("ppid {original} -> {current}"));
    }
    // pid 0/1: unknown or init, never a real Windows parent.
    if windows && original > 1 && !is_alive(original) {
        return Some(format!("parent pid {original} exited"));
    }
    None
}

/// `CODEGRAPHV_PPID_POLL_MS`: the poll interval; 0 turns the watchdog off; anything not a
/// non-negative number keeps the default.
pub fn parse_poll_ms(raw: Option<&str>) -> u64 {
    match raw.map(str::trim).filter(|s| !s.is_empty()).map(str::parse::<f64>) {
        Some(Ok(n)) if n.is_finite() && n >= 0.0 => n.floor() as u64,
        _ => DEFAULT_POLL_MS,
    }
}

/// This process's parent pid.
pub fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: getppid has no preconditions.
        Some(unsafe { libc::getppid() } as u32)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
        };
        let me = std::process::id();
        // SAFETY: a process snapshot walked with a correctly sized entry, then closed.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut found = None;
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                if entry.th32ProcessID == me {
                    found = Some(entry.th32ParentProcessID);
                    break;
                }
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            found
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Start the watchdog thread: when the parent is gone it logs the reason to stderr and exits
/// the process. Off when `CODEGRAPHV_PPID_POLL_MS=0` or the parent is unknown.
pub fn start() {
    let poll = parse_poll_ms(std::env::var("CODEGRAPHV_PPID_POLL_MS").ok().as_deref());
    let Some(original) = parent_pid().filter(|_| poll > 0) else { return };
    let _ = std::thread::Builder::new().name("codegraph-ppid-watchdog".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_millis(poll));
        let current = parent_pid().unwrap_or(original);
        if let Some(reason) = supervision_lost_reason(original, current, cfg!(windows), crate::utils::is_process_alive) {
            eprintln!("[codegraphV] The agent that started this server is gone ({reason}); exiting.");
            std::process::exit(0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reparenting_or_a_dead_windows_parent_ends_supervision() {
        let alive = |_: u32| true;
        let dead = |_: u32| false;
        assert_eq!(supervision_lost_reason(40, 40, false, dead), None, "POSIX ignores liveness");
        assert_eq!(supervision_lost_reason(40, 1, false, alive).as_deref(), Some("ppid 40 -> 1"));
        assert_eq!(supervision_lost_reason(40, 40, true, alive), None);
        assert_eq!(supervision_lost_reason(40, 40, true, dead).as_deref(), Some("parent pid 40 exited"));
        assert_eq!(supervision_lost_reason(1, 1, true, dead), None, "init / unknown is never polled");
    }

    #[test]
    fn poll_interval_parsing() {
        assert_eq!(parse_poll_ms(None), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("0")), 0);
        assert_eq!(parse_poll_ms(Some("250.7")), 250);
        assert_eq!(parse_poll_ms(Some("-5")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("soon")), DEFAULT_POLL_MS);
    }

    #[test]
    fn this_process_has_a_live_parent() {
        let parent = parent_pid().expect("a parent pid");
        assert!(parent > 0);
        assert!(crate::utils::is_process_alive(parent));
    }
}
