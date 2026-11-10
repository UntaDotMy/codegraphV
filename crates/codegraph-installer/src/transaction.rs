//! All-file installation transactions (A08).
//!
//! A plan is computed from the current files, preflighted as one unit, journaled, then written.
//! Any write failure rolls back every file this operation changed. A crash leaves the journal
//! behind; the next install restores only files whose content still equals what this operation
//! wrote, so a user edit made after the crash is never overwritten (A34).
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub(crate) const JOURNAL_VERSION: u32 = 2;

// Keep the lock file permanently: unlinking a locked file would let another process lock
// a different inode. The OS releases this lock even when the installer is killed.
fn lock(home: &Path) -> io::Result<std::fs::File> {
    let path = home.join(".codegraphV").join("install.lock");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock, "Another CodeGraphV installation or recovery is in progress."),
        std::fs::TryLockError::Error(error) => error,
    })?;
    Ok(file)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PlannedFile {
    pub path: PathBuf,
    /// `None` when the file did not exist at plan time.
    pub before: Option<String>,
    /// `None` deletes the file.
    pub after: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    version: u32,
    state: String,
    files: Vec<PlannedFile>,
}

pub(crate) fn journal_path(home: &Path) -> PathBuf {
    home.join(".codegraphV").join("install-journal.json")
}

pub(crate) fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn read(path: &Path) -> io::Result<String> {
    Ok(read_optional(path)?.unwrap_or_default())
}

pub(crate) fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let target = if path.is_symlink() {
        std::fs::canonicalize(path)?
    } else {
        path.to_owned()
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Carry POSIX mode / Windows read-only flags. This does not preserve Windows ACLs.
    let original_permissions = std::fs::metadata(&target).ok().map(|meta| meta.permissions());
    let mut temp = target.as_os_str().to_os_string();
    temp.push(format!(".codegraph-{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        use std::io::Write;
        if let Some(permissions) = original_permissions { file.set_permissions(permissions)?; }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn set(path: &Path, text: Option<&str>) -> io::Result<()> {
    match text {
        Some(text) => write_atomic(path, text),
        // A link is emptied through, never replaced by deleting it.
        None if path.is_symlink() => write_atomic(path, ""),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

/// Undo an in-progress journal. `before` is restored only when the file still holds exactly
/// what the interrupted operation wrote; an intervening edit is left alone and reported.
pub(crate) fn recover(home: &Path) -> io::Result<Vec<String>> {
    let path = journal_path(home);
    if !path.exists() { return Ok(Vec::new()); }
    let _lock = lock(home)?;
    let Some(raw) = read_optional(&path)? else { return Ok(Vec::new()) };
    let Ok(journal) = serde_json::from_str::<Journal>(&raw) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("Unreadable installation journal retained at {}; recovery requires inspection.", path.display())));
    };
    if !matches!(journal.version, 1 | JOURNAL_VERSION) {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("Unsupported installation journal retained at {}.", path.display())));
    }
    if journal.state == "committed" {
        std::fs::remove_file(&path)?;
        return Ok(Vec::new());
    }
    if journal.state != "in-progress" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Unknown installation journal state; retained for inspection."));
    }
    if journal.version == 1 && age(&path).is_some_and(|age| age < stale_after()) {
        // Legacy installers did not use the OS lock. Keep their conservative grace period.
        return Ok(vec![format!(
            "Another CodeGraphV installation is in progress ({}).",
            path.display()
        )]);
    }
    let mut notes = Vec::new();
    let mut failed = false;
    for file in journal.files.iter().rev() {
        let current = read_optional(&file.path)?;
        if current != file.after {
            if current != file.before {
                notes.push(format!(
                    "Left {} unchanged after an interrupted installation: it was edited since.",
                    file.path.display()
                ));
            }
            continue;
        }
        if let Err(error) = set(&file.path, file.before.as_deref()) {
            failed = true;
            notes.push(format!(
                "Could not restore {} after an interrupted installation: {error}",
                file.path.display()
            ));
        }
    }
    if failed {
        return Err(io::Error::other(format!("Recovery incomplete; journal retained at {}. {}", path.display(), notes.join(" "))));
    }
    std::fs::remove_file(&path)?;
    notes.insert(0, "Recovered an interrupted installation.".into());
    Ok(notes)
}

fn journal(path: &Path, files: &[PlannedFile], state: &str) -> io::Result<()> {
    let text = serde_json::to_string(&Journal {
        version: JOURNAL_VERSION,
        state: state.into(),
        files: files.to_vec(),
    })?;
    write_atomic(path, &text)?;
    // The entries hold user instruction/config text; keep the journal private on POSIX.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Write the plan as one unit. The caller has already produced every `before` from disk; a
/// mismatch here means another editor or installer changed a file, and nothing is written.
pub(crate) fn commit(home: &Path, plan: &[PlannedFile], dry_run: bool) -> io::Result<Vec<String>> {
    if dry_run || plan.is_empty() {
        return Ok(Vec::new());
    }
    let _lock = lock(home)?;
    for file in plan {
        if read_optional(&file.path)? != file.before {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} changed during installation; nothing was written. Rerun after the other editor finishes.",
                    file.path.display()
                ),
            ));
        }
    }
    let path = journal_path(home);
    if path.exists() {
        // Another installer is between its journal write and its commit, or a crash left a
        // journal this call could not read. Do not interleave.
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "Another CodeGraphV installation is in progress ({}). Rerun when it finishes.",
                path.display()
            ),
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Publish the complete journal atomically under the OS lock. A crash while encoding
    // or writing it cannot expose a torn journal, and no configuration is written first.
    journal(&path, plan, "in-progress")?;
    let mut applied = Vec::new();
    for (_index, entry) in plan.iter().enumerate() {
        if entry.after == entry.before {
            continue;
        }
        #[cfg(test)]
        EDIT_BEFORE_WRITE.with(|edit| {
            if let Some((index, text)) = edit.borrow().as_ref() {
                if *index == _index { std::fs::write(&entry.path, text).unwrap(); }
            }
        });
        let unchanged = read_optional(&entry.path).and_then(|current| {
            if current == entry.before { Ok(()) } else {
                Err(io::Error::new(io::ErrorKind::AlreadyExists,
                    "File changed after installation preflight; user edit preserved"))
            }
        });
        #[cfg(test)]
        let outcome = unchanged.and_then(|()| if FAIL_WRITE_AT.with(|at| at.get()) == _index as isize {
            FAIL_WRITE_AT.with(|at| at.set(-1));
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected write failure"))
        } else {
            set(&entry.path, entry.after.as_deref())
        });
        #[cfg(not(test))]
        let outcome = unchanged.and_then(|()| set(&entry.path, entry.after.as_deref()));
        match outcome {
            Ok(()) => applied.push(entry),
            Err(error) => {
                let mut rollback_failed = false;
                for done in applied.iter().rev() {
                    match read_optional(&done.path) {
                        Ok(current) if current == done.after => {},
                        Ok(_) => continue,
                        Err(_) => { rollback_failed = true; continue; }
                    }
                    if set(&done.path, done.before.as_deref()).is_err() {
                        rollback_failed = true;
                    }
                }
                if rollback_failed {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "{} could not be written ({error}), and rollback is incomplete. The journal at {} will finish recovery on the next run.",
                            entry.path.display(),
                            path.display()
                        ),
                    ));
                }
                // Atomic writes leave the failed destination unchanged. Never restore its
                // stale snapshot: another editor may have changed it since preflight.
                let _ = std::fs::remove_file(&path);
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{} could not be written ({error}); installation writes were rolled back where unchanged; intervening user edits were preserved.",
                        entry.path.display()
                    ),
                ));
            }
        }
    }
    // Mark the journal committed before removing it. If removal then fails, recovery knows not
    // to roll back a completed installation.
    if let Err(error) = journal(&path, plan, "committed") {
        return Err(io::Error::new(
            error.kind(),
            format!(
                "Configuration was written, but the installation journal at {} could not be marked complete ({error}). Recovery may roll back unchanged installation writes on the next run.",
                path.display()
            ),
        ));
    }
    let _ = std::fs::remove_file(&path);
    Ok(Vec::new())
}

/// Compatibility grace period for version-1 journals, which had no OS lock.
pub(crate) fn stale_after() -> Duration {
    Duration::from_secs(30)
}

pub(crate) fn age(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

#[cfg(test)]
thread_local! {
    /// Index of the forward write that fails, so a test never races another test's commit.
    static FAIL_WRITE_AT: std::cell::Cell<isize> = const { std::cell::Cell::new(-1) };
    static EDIT_BEFORE_WRITE: std::cell::RefCell<Option<(usize, String)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(dir: &Path) -> (PlannedFile, PlannedFile) {
        let a = dir.join("a.json");
        let b = dir.join("b.json");
        std::fs::write(&a, "a0").unwrap();
        std::fs::write(&b, "b0").unwrap();
        (
            PlannedFile { path: a, before: Some("a0".into()), after: Some("a1".into()) },
            PlannedFile { path: b, before: Some("b0".into()), after: Some("b1".into()) },
        )
    }

    #[test]
    fn one_failed_write_restores_every_file_and_removes_the_journal() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = plan(dir.path());
        FAIL_WRITE_AT.with(|at| at.set(1));
        let error = commit(home.path(), &[a.clone(), b.clone()], false).unwrap_err();
        assert!(error.to_string().contains("injected"), "{error}");
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "a0");
        assert_eq!(std::fs::read_to_string(&b.path).unwrap(), "b0");
        assert!(!journal_path(home.path()).exists());
    }

    #[test]
    fn an_intervening_edit_aborts_before_any_write() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = plan(dir.path());
        std::fs::write(&b.path, "user edit").unwrap();
        let error = commit(home.path(), &[a.clone(), b], false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "a0");
        assert!(!journal_path(home.path()).exists());
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = plan(dir.path());
        commit(home.path(), &[a.clone(), b], true).unwrap();
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "a0");
        assert!(!journal_path(home.path()).exists());
    }

    #[test]
    fn a_recent_journal_is_a_live_installation_not_a_recovery() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = plan(dir.path());
        journal(&journal_path(home.path()), &[a.clone(), b.clone()], "in-progress").unwrap();
        let _owner = lock(home.path()).unwrap();
        assert_eq!(recover(home.path()).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "a0");
        let error = commit(home.path(), &[a, b], false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn a_stale_journal_restores_only_files_still_matching_what_it_wrote() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = plan(dir.path());
        std::fs::write(&a.path, "a1").unwrap();
        std::fs::write(&b.path, "user edit after crash").unwrap();
        let path = journal_path(home.path());
        journal(&path, &[a.clone(), b.clone()], "in-progress").unwrap();
        let old = SystemTime::now() - Duration::from_secs(300);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        let notes = recover(home.path()).unwrap();
        assert!(notes[0].contains("Recovered"), "{notes:?}");
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "a0");
        assert_eq!(std::fs::read_to_string(&b.path).unwrap(), "user edit after crash");
        assert!(notes.iter().any(|n| n.contains("Left") && n.contains("b.json")), "{notes:?}");
        assert!(!path.exists());
    }

    #[test]
    fn a_committed_journal_never_rolls_a_finished_installation_back() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = plan(dir.path());
        std::fs::write(&a.path, "a1").unwrap();
        journal(&journal_path(home.path()), &[a.clone(), b], "committed").unwrap();
        recover(home.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "a1");
        assert!(!journal_path(home.path()).exists());
    }

    #[test]
    fn unreadable_journal_is_retained_for_recovery() {
        let home = tempfile::tempdir().unwrap();
        let path = journal_path(home.path());
        write_atomic(&path, "{partial").unwrap();
        assert!(recover(home.path()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{partial");
    }

    #[test]
    fn failed_restore_keeps_the_journal_for_retry() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = plan(dir.path());
        std::fs::write(&a.path, "a1").unwrap();
        let path = journal_path(home.path());
        journal(&path, &[a.clone()], "in-progress").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(300)).unwrap();
        // A stale atomic-write temporary blocks restoration without touching user data.
        let temp = a.path.with_file_name(format!("a.json.codegraph-{}.tmp", std::process::id()));
        std::fs::write(&temp, "occupied").unwrap();
        assert!(recover(home.path()).is_err());
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "a1");
        std::fs::remove_file(temp).unwrap();
        recover(home.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "a0");
        assert!(!path.exists());
    }

    #[test]
    fn edit_after_preflight_is_not_overwritten_by_forward_write_or_rollback() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = plan(dir.path());
        EDIT_BEFORE_WRITE.with(|edit| *edit.borrow_mut() = Some((1, "new user settings".into())));
        let result = commit(home.path(), &[a.clone(), b.clone()], false);
        EDIT_BEFORE_WRITE.with(|edit| *edit.borrow_mut() = None);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&a.path).unwrap(), "a0");
        assert_eq!(read(&b.path).unwrap(), "new user settings");
    }

    #[test]
    fn lock_excludes_even_an_old_live_journal_and_recovery_is_immediate_after_release() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = plan(dir.path());
        let owner = lock(home.path()).unwrap();
        let path = journal_path(home.path());
        journal(&path, &[a.clone()], "in-progress").unwrap();
        std::fs::write(&a.path, "a1").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(300)).unwrap();
        assert_eq!(recover(home.path()).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(read(&a.path).unwrap(), "a1");
        drop(owner);
        std::fs::File::options().write(true).open(&path).unwrap()
            .set_modified(SystemTime::now()).unwrap();
        recover(home.path()).unwrap();
        assert_eq!(read(&a.path).unwrap(), "a0");
    }

    #[test]
    fn lock_owner_subprocess() {
        let Some(home) = std::env::var_os("CODEGRAPHV_TEST_INSTALL_LOCK_HOME") else { return };
        let home = PathBuf::from(home);
        let _owner = lock(&home).unwrap();
        std::fs::write(home.join("ready"), "locked").unwrap();
        loop { std::thread::sleep(Duration::from_millis(50)); }
    }

    #[test]
    fn killed_installer_releases_lock_for_immediate_recovery() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = plan(dir.path());
        journal(&journal_path(home.path()), &[a.clone()], "in-progress").unwrap();
        std::fs::write(&a.path, "a1").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "transaction::tests::lock_owner_subprocess", "--nocapture"])
            .env("CODEGRAPHV_TEST_INSTALL_LOCK_HOME", home.path())
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !home.path().join("ready").exists() && std::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() { break; }
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready = home.path().join("ready").exists();
        let refused = if ready { recover(home.path()).err().map(|e| e.kind()) } else { None };
        let _ = child.kill();
        child.wait().unwrap();
        assert!(ready, "child did not acquire installation lock");
        assert_eq!(refused, Some(io::ErrorKind::WouldBlock));
        recover(home.path()).unwrap();
        assert_eq!(read(&a.path).unwrap(), "a0");
        assert!(!journal_path(home.path()).exists());
    }
}
