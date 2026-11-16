//! A file the test has just copied can be started while other test threads fork (ETXTBSY).
//!
//! A fork that runs during the copy inherits the copy's write descriptor, and the child keeps it
//! until its exec; the kernel refuses to run a file that is open for writing. The tests hit this
//! as "Text file busy" when they start a freshly copied codegraphV.
#![cfg(unix)]

mod common;

use std::{
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};

const ROUNDS: u32 = 1000;

fn system_true() -> PathBuf {
    PathBuf::from(if cfg!(target_os = "macos") { "/usr/bin/true" } else { "/bin/true" })
}

/// Stops the forking threads when dropped, also when an assertion panics.
struct Forkers {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl Drop for Forkers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn fork_in_background(threads: usize) -> Forkers {
    let stop = Arc::new(AtomicBool::new(false));
    let handles = (0..threads)
        .map(|_| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new(system_true()).status();
                }
            })
        })
        .collect();
    Forkers { stop, handles }
}

#[test]
fn a_just_copied_binary_starts_while_other_threads_fork() {
    let dir = tempfile::tempdir().unwrap();
    let _forkers = fork_in_background(4);
    for round in 0..ROUNDS {
        let copy = dir.path().join(format!("true-{round}"));
        std::fs::copy(system_true(), &copy).unwrap();
        let mut child = common::spawn_retrying(&mut Command::new(&copy))
            .unwrap_or_else(|err| panic!("round {round}: {err}"));
        let status = child.wait().unwrap();
        std::fs::remove_file(&copy).unwrap();
        assert!(status.success(), "round {round}: the copy exited with {status}");
    }
}
