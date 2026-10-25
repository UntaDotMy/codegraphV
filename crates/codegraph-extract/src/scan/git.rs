//! Running `git` the way the TypeScript scanner does (`execFileSync` with a
//! timeout and an output cap) and reading `.git` pointers.

use codegraph_types::jscompat::{is_js_space, js_trim};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const MAX_BUFFER: usize = 50 * 1024 * 1024;

fn spawn_git(cwd: &Path, args: &[&str]) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — windowsHide
    }
    cmd.spawn()
}

/// `execFileSync('git', args, { cwd, encoding: 'utf-8', timeout, maxBuffer })`:
/// stdout as text, or an error when git cannot run, exits non-zero, runs past the
/// timeout, or prints more than `max_buffer` bytes.
pub fn run_git(cwd: &Path, args: &[&str], timeout: Duration, max_buffer: usize) -> Result<String, String> {
    let mut child = spawn_git(cwd, args).map_err(|e| format!("spawn git: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let overflow = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&overflow);
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > max_buffer {
                        flag.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if overflow.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(if overflow.load(Ordering::Relaxed) { "git output exceeded maxBuffer" } else { "git timed out" }.to_string());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => break Err(format!("wait git: {e}")),
        }
    };
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    let status = status?;
    if overflow.load(Ordering::Relaxed) {
        return Err("git output exceeded maxBuffer".to_string());
    }
    if !status.success() {
        return Err(format!("git {} failed ({status}): {}", args.join(" "), String::from_utf8_lossy(&err).trim()));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The common call: 30 s, 50 MB.
pub(crate) fn git30(cwd: &Path, args: &[&str]) -> Result<String, String> {
    run_git(cwd, args, Duration::from_secs(30), MAX_BUFFER)
}

/// A short call (rev-parse, check-ignore): 5 s.
pub(crate) fn git5(cwd: &Path, args: &[&str]) -> Result<String, String> {
    run_git(cwd, args, Duration::from_secs(5), MAX_BUFFER)
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// The target of a `.git` pointer file: `/^gitdir:\s*(.+)$/m`, group 1, trimmed.
/// The leftmost line that starts with `gitdir:` and has something after it wins.
pub(crate) fn gitdir_pointer(text: &str) -> Option<String> {
    let mut starts = vec![0usize];
    for (i, c) in text.char_indices() {
        if is_line_terminator(c) {
            starts.push(i + c.len_utf8());
        }
    }
    for s in starts {
        let Some(rest) = text[s..].strip_prefix("gitdir:") else { continue };
        let chars: Vec<(usize, char)> = rest.char_indices().collect();
        let ws = chars.iter().take_while(|(_, c)| is_js_space(*c)).count();
        // `\s*` is greedy but gives characters back until `.+` has something to eat.
        for k in (0..=ws).rev() {
            let Some(&(from, first)) = chars.get(k) else { continue };
            if is_line_terminator(first) {
                continue;
            }
            let end = rest[from..].find(is_line_terminator).map_or(rest.len(), |p| from + p);
            let trimmed = js_trim(&rest[from..end]);
            return if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
        }
    }
    None
}

/// The repository GIT_DIR for `repo_root` (a `.git` directory, or the target of a
/// `.git` file pointer); none when this isn't a git checkout.
pub fn resolve_git_dir(repo_root: &Path) -> Option<PathBuf> {
    let git_path = repo_root.join(".git");
    let meta = std::fs::metadata(&git_path).ok()?;
    if meta.is_dir() {
        return Some(git_path);
    }
    if !meta.is_file() {
        return None;
    }
    let raw = gitdir_pointer(&String::from_utf8_lossy(&std::fs::read(&git_path).ok()?))?;
    let target = Path::new(&raw);
    Some(if target.is_absolute() { codegraph_types::paths::resolve_path(target) } else { codegraph_types::paths::resolve_path(repo_root.join(target)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_files() {
        assert_eq!(gitdir_pointer("gitdir: ../.git/modules/x\n").as_deref(), Some("../.git/modules/x"));
        assert_eq!(gitdir_pointer("gitdir:/a/b\r\n").as_deref(), Some("/a/b"));
        assert_eq!(gitdir_pointer("# note\ngitdir:   C:\\r\\.git  \n").as_deref(), Some("C:\\r\\.git"));
        assert_eq!(gitdir_pointer("gitdir:\n"), None);
        assert_eq!(gitdir_pointer("gitdir: \n"), None);
        assert_eq!(gitdir_pointer("nothing"), None);
        // `\s*` may swallow a line break and read the next line
        assert_eq!(gitdir_pointer("gitdir:\n/next/line\n").as_deref(), Some("/next/line"));
    }

    #[test]
    fn git_runs_and_failures_are_errors() {
        let cwd = std::env::current_dir().unwrap();
        assert!(run_git(&cwd, &["--version"], Duration::from_secs(10), MAX_BUFFER).unwrap().starts_with("git version"));
        assert!(run_git(&cwd, &["definitely-not-a-command"], Duration::from_secs(10), MAX_BUFFER).is_err());
        assert!(run_git(&cwd, &["--version"], Duration::from_secs(10), 3).is_err());
    }
}
