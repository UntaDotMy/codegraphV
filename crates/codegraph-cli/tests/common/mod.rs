//! Helpers shared by the CLI integration tests; each test file that needs one pulls this in with
//! `mod common;`.

use std::{
    io,
    process::{Child, Command},
    thread,
    time::Duration,
};

/// Spawns `command`, retrying while the executable is busy.
///
/// A test runs a binary it has just copied. While another test thread's fork still holds the copy's
/// write descriptor (before that child's exec), the kernel refuses to start the file with ETXTBSY
/// ("Text file busy"). The window is brief, so a few short retries clear it. Every other error is
/// returned at once.
pub fn spawn_retrying(command: &mut Command) -> io::Result<Child> {
    const ATTEMPTS: u32 = 50;
    const PAUSE: Duration = Duration::from_millis(10);
    let mut attempt = 1;
    loop {
        match command.spawn() {
            Err(err) if err.kind() == io::ErrorKind::ExecutableFileBusy && attempt < ATTEMPTS => {
                attempt += 1;
                thread::sleep(PAUSE);
            }
            result => return result,
        }
    }
}
