//! What framework detection read, so a later sync can tell whether the answer went stale
//! (upstream 31ec9082, #2516).
//!
//! Detection reads three kinds of input. A source file in the file list it was given is covered by
//! a sync's own change lists. Any other path it read or probed (the manifests, like `package.json`,
//! that no sync reports) is kept with a signature of what was there. A folder it listed is kept
//! with the names of its sub-folders. A sync compares these against the disk before it extracts.

use std::collections::HashSet;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Everything one detection read or probed, by project-relative `/`-separated path.
#[derive(Debug, Clone, Default)]
pub struct DetectionInputs {
    /// Source files of the detection's file list that it read or probed.
    pub source_files: HashSet<String>,
    /// Every other path (a folder ends in `/`) with its [`disk_signature`] when it was first read,
    /// in the order detection reached it.
    pub disk_paths: indexmap::IndexMap<String, String>,
}

/// What is at `full`, to tell later whether it changed: a file's size and modification time, a
/// folder's sub-folders (`folder`), or `-` when nothing is there.
pub fn disk_signature(full: &Path, folder: bool) -> String {
    if folder {
        let Ok(entries) = std::fs::read_dir(full) else { return "-".to_string() };
        let mut names: Vec<String> =
            entries.filter_map(|e| e.ok()).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        return names.join("/");
    }
    match std::fs::metadata(full) {
        Ok(meta) => {
            let modified = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
            format!("{}:{modified}", meta.len())
        }
        Err(_) => "-".to_string(),
    }
}

/// The recording a detection context keeps while it runs: the file list it was given, to tell a
/// source file from another path, and what it found.
#[derive(Default)]
pub(crate) struct Recorder {
    pub(crate) listed: HashSet<String>,
    pub(crate) inputs: DetectionInputs,
}

impl Recorder {
    /// `relative` was read or probed; `full` is where it lives on disk.
    pub(crate) fn path(&mut self, relative: &str, full: &Path) {
        let relative = relative.replace('\\', "/");
        if self.listed.contains(&relative) {
            self.inputs.source_files.insert(relative);
        } else if !self.inputs.disk_paths.contains_key(&relative) {
            let signature = disk_signature(full, false);
            self.inputs.disk_paths.insert(relative, signature);
        }
    }

    /// The folder `relative` ('' for the project root) was listed; `full` is where it lives.
    pub(crate) fn folder(&mut self, relative: &str, full: &Path) {
        let key = format!("{}/", relative.replace('\\', "/").trim_matches('/'));
        if !self.inputs.disk_paths.contains_key(&key) {
            let signature = disk_signature(full, true);
            self.inputs.disk_paths.insert(key, signature);
        }
    }
}

/// Where the recorded `key` lives under `root`: a folder key ends in `/`.
pub fn recorded_path(root: &str, key: &str) -> std::path::PathBuf {
    let relative = key.trim_end_matches('/');
    if relative.is_empty() {
        Path::new(root).to_path_buf()
    } else {
        Path::new(root).join(relative)
    }
}

/// Whether anything `inputs` recorded under `root` is different on disk now.
pub fn disk_inputs_changed(root: &str, inputs: &DetectionInputs) -> bool {
    inputs.disk_paths.iter().any(|(key, recorded)| disk_signature(&recorded_path(root, key), key.ends_with('/')) != *recorded)
}
