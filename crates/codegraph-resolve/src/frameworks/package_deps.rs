//! The dependencies a project declares — in its root `package.json`, in the ones one or two
//! directories down (`apps/web/package.json`, `frontend/package.json`), and in the ones deeper
//! still above JS/TS code (`src/Web/ClientApp/package.json`). Port of
//! `src/resolution/frameworks/package-deps.ts`.
//!
//! A framework detector that reads only the root misses every monorepo: proshop keeps `react` in
//! `frontend/`, a Turborepo keeps `next` in `apps/web/`, and the resolver for that framework then
//! never runs, so its routes never exist.

use crate::ctx::Ctx;
use codegraph_jsre::re;
use indexmap::IndexSet;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;

/// Nested manifests read per project, at most — a monorepo with hundreds of packages is sampled, not scanned.
const MAX_MANIFESTS: usize = 24;

/// Directories probed for a manifest below the first two levels, at most.
const MAX_DEEP_PROBES: usize = MAX_MANIFESTS * 8;

/// Cached per context, keyed by how many files are indexed. The resolver is constructed — and
/// every `detect()` runs once — BEFORE any file exists, so that first pass sees no directories to
/// probe and reads only the root manifest; re-reading when the file count changes costs one
/// manifest sweep per index.
#[derive(Default)]
pub struct DependencyCache(RefCell<Option<(usize, Rc<HashSet<String>>)>>);

/// Every dependency name declared in the manifests read (see the module comment), de-duplicated.
pub fn declared_dependencies(ctx: &Ctx) -> Rc<HashSet<String>> {
    let files = ctx.get_all_files();
    let cache = ctx.memo::<DependencyCache>();
    if let Some((count, names)) = cache.0.borrow().as_ref() {
        if *count == files.len() {
            return Rc::clone(names);
        }
    }
    let mut names: HashSet<String> = HashSet::new();
    // The index lists source files, never manifests: the candidate directories are the first one
    // or two segments of what IS indexed, probed on disk.
    let mut dirs: IndexSet<String> = IndexSet::new();
    for file in &files {
        let segs: Vec<&str> = file.split('/').collect();
        if segs.len() > 1 {
            dirs.insert(format!("{}/", segs[0]));
        }
        if segs.len() > 2 {
            dirs.insert(format!("{}/{}/", segs[0], segs[1]));
        }
        if dirs.len() > MAX_MANIFESTS * 8 {
            break;
        }
    }
    let mut manifests: Vec<String> = vec!["package.json".to_string()];
    for dir in &dirs {
        if manifests.len() > MAX_MANIFESTS {
            break;
        }
        if !dir.contains("node_modules") && ctx.file_exists(&format!("{dir}package.json")) {
            manifests.push(format!("{dir}package.json"));
        }
    }
    // An app can sit deeper: an ASP.NET solution's single-page app in `src/Web/ClientApp/`,
    // prometheus's React apps in `web/ui/mantine-ui/` under a workspace root at `web/ui/`. The
    // slots left go to the directories above JS/TS code, shallowest first.
    if manifests.len() <= MAX_MANIFESTS {
        for dir in deep_script_directories(&files) {
            if manifests.len() > MAX_MANIFESTS {
                break;
            }
            if ctx.file_exists(&format!("{dir}package.json")) {
                manifests.push(format!("{dir}package.json"));
            }
        }
    }
    for manifest in &manifests {
        let Some(content) = ctx.read_file(manifest).filter(|c| !c.is_empty()) else { continue };
        // Not JSON — a template, a broken manifest; nothing to read.
        let Ok(serde_json::Value::Object(pkg)) = serde_json::from_str::<serde_json::Value>(&content) else { continue };
        for group in ["dependencies", "devDependencies", "peerDependencies"] {
            if let Some(serde_json::Value::Object(deps)) = pkg.get(group) {
                names.extend(deps.keys().cloned());
            }
        }
    }
    let names = Rc::new(names);
    *cache.0.borrow_mut() = Some((files.len(), Rc::clone(&names)));
    names
}

/// The directories three or more levels down that hold indexed JS/TS code or a directory that
/// does, never inside `node_modules`: shallowest first, at most `MAX_DEEP_PROBES` of them. Sorted
/// within a level, so the sample does not depend on the order the files come in.
fn deep_script_directories(files: &[String]) -> Vec<String> {
    // levels[n]: the directories n levels down (`a/b/c/` is 3).
    let mut levels: Vec<BTreeSet<String>> = Vec::new();
    for file in files {
        if !re!(r"\.(?:[cm]?[jt]sx?|vue|svelte|astro)$").is_match(file) || file.contains("node_modules") {
            continue;
        }
        let mut depth = 0usize;
        for (slash, _) in file.match_indices('/') {
            depth += 1;
            if depth >= 3 {
                if levels.len() <= depth {
                    levels.resize_with(depth + 1, BTreeSet::new);
                }
                levels[depth].insert(file[..=slash].to_string());
            }
        }
    }
    let mut dirs: Vec<String> = Vec::new();
    for level in levels.iter().skip(3) {
        for dir in level {
            if dirs.len() == MAX_DEEP_PROBES {
                return dirs;
            }
            dirs.push(dir.clone());
        }
    }
    dirs
}

/// True when any of `deps` is declared anywhere the project's manifests are read.
pub fn depends_on(ctx: &Ctx, deps: &[&str]) -> bool {
    let names = declared_dependencies(ctx);
    deps.iter().any(|d| names.contains(*d))
}
