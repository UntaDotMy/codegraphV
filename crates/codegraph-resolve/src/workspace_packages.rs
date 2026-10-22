//! JS/TS workspace (monorepo) package resolution — port of `src/resolution/workspace-packages.ts`.
//!
//! npm / yarn / bun read member packages from the root `package.json` `workspaces`
//! field, pnpm from `pnpm-workspace.yaml`; ohpm modules declare `file:` siblings in
//! `oh-package.json5`. Each member's declared `name` maps to its directory so the
//! resolver can rewrite `@scope/ui/widgets` → `packages/ui/widgets`.

use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::log::log_debug;
use codegraph_types::nodepath;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Default)]
pub struct WorkspacePackages {
    /// Member package `name` → directory relative to the project root (posix).
    pub by_name: HashMap<String, String>,
    /// Iteration order of `by_name` (JavaScript `Map` insertion order).
    pub names_in_order: Vec<String>,
    /// Member package `name` → its declared ENTRY FILE (ohpm `main`), posix, project-relative.
    pub entry_by_name: HashMap<String, String>,
}

impl WorkspacePackages {
    fn insert(&mut self, name: String, dir: String) {
        if self.by_name.insert(name.clone(), dir).is_none() {
            self.names_in_order.push(name);
        }
    }
}

/// Parse JSON-with-comments (ohpm's JSON5 manifests): comments and trailing commas tolerated.
fn parse_jsonc(text: &str) -> Option<Value> {
    use jsonc_parser::{parse_to_serde_value, ParseOptions};
    let opts = ParseOptions { allow_comments: true, allow_loose_object_property_names: true, allow_trailing_commas: true };
    parse_to_serde_value(text, &opts).ok().flatten()
}

fn read_text(path: &str) -> Option<String> {
    std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Load workspace member packages for `project_root`; `None` when the project declares none.
pub fn load_workspace_packages(project_root: &str) -> Option<WorkspacePackages> {
    let mut ws = WorkspacePackages::default();

    let patterns = read_workspace_globs(project_root);
    // What a negated glob matches is no member, wherever the glob is listed.
    let excluded: Vec<&str> = patterns.iter().filter(|p| p.starts_with('!')).map(|p| &p[1..]).collect();
    for pattern in &patterns {
        if pattern.starts_with('!') {
            continue;
        }
        for dir in expand_workspace_glob(project_root, pattern) {
            if excluded.iter().any(|x| workspace_glob_matches(x, &dir)) {
                continue;
            }
            let pkg_name = read_package_name(&nodepath::join(&[project_root, &dir]));
            // First declaration wins — workspace patterns are tried in order.
            if let Some(name) = pkg_name {
                if !ws.by_name.contains_key(&name) {
                    ws.insert(name, dir.clone());
                }
            }
        }
    }

    // HarmonyOS/OpenHarmony (ArkTS) modular projects.
    for (name, dir) in collect_ohpm_file_deps(project_root) {
        if ws.by_name.contains_key(&name) {
            continue;
        }
        let entry = read_ohpm_main(project_root, &dir);
        ws.insert(name.clone(), dir);
        if let Some(entry) = entry {
            ws.entry_by_name.insert(name, entry);
        }
    }

    if ws.by_name.is_empty() {
        return None;
    }
    log_debug("workspace packages loaded", &[("count", ws.by_name.len().to_string())]);
    Some(ws)
}

fn read_ohpm_main(project_root: &str, dir_rel: &str) -> Option<String> {
    let parsed = parse_jsonc(&read_text(&nodepath::join(&[project_root, dir_rel, OHPM_MANIFEST]))?)?;
    let main = parsed.get("main")?.as_str()?;
    let main = codegraph_types::jscompat::js_trim(main);
    if main.is_empty() {
        return None;
    }
    let entry_abs = nodepath::resolve(&[project_root, dir_rel, main]);
    let entry_rel = nodepath::relative(project_root, &entry_abs).replace('\\', "/");
    if entry_rel.starts_with("..") {
        return None;
    }
    Some(entry_rel)
}

const OHPM_MANIFEST: &str = "oh-package.json5";
const OHPM_WALK_MAX_DEPTH: usize = 6;
const OHPM_WALK_DIR_BUDGET: usize = 8000;
const OHPM_SKIP_DIRS: [&str; 9] = ["node_modules", "oh_modules", ".git", ".codegraph", ".hvigor", ".preview", "build", "dist", "out"];

/// Scan for `oh-package.json5` manifests and collect their `file:` dependencies, in the
/// order the JavaScript `Map` would iterate them. A name declared with different target
/// directories in different manifests is ambiguous and dropped.
fn collect_ohpm_file_deps(project_root: &str) -> Vec<(String, String)> {
    let mut by_name: Vec<(String, String)> = Vec::new();
    let mut ambiguous: HashSet<String> = HashSet::new();

    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((String::new(), 0));
    let mut visited = 0;
    while let Some((rel, depth)) = queue.pop_front() {
        visited += 1;
        if visited > OHPM_WALK_DIR_BUDGET {
            break;
        }
        let abs = nodepath::join(&[project_root, &rel]);
        let Ok(entries) = std::fs::read_dir(&abs) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let ft = e.file_type().ok();
            if ft.is_some_and(|t| t.is_dir()) {
                if depth >= OHPM_WALK_MAX_DEPTH {
                    continue;
                }
                if name.starts_with('.') || OHPM_SKIP_DIRS.contains(&name.as_str()) || name == "oh-package-lock.json5" {
                    continue;
                }
                queue.push_back((if rel.is_empty() { name } else { format!("{rel}/{name}") }, depth + 1));
                continue;
            }
            if name != OHPM_MANIFEST {
                continue;
            }
            for (dep, target) in read_ohpm_file_deps(&nodepath::join(&[&abs, &name])) {
                let target_abs = nodepath::resolve(&[&abs, &target]);
                let target_rel = nodepath::relative(project_root, &target_abs).replace('\\', "/");
                if target_rel.starts_with("..") {
                    continue;
                }
                match by_name.iter().position(|(n, _)| *n == dep) {
                    None => {
                        if !ambiguous.contains(&dep) {
                            by_name.push((dep, target_rel));
                        }
                    }
                    Some(i) => {
                        if by_name[i].1 != target_rel {
                            by_name.remove(i);
                            ambiguous.insert(dep);
                        }
                    }
                }
            }
        }
    }
    by_name
}

fn read_ohpm_file_deps(manifest_abs: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(parsed) = read_text(manifest_abs).and_then(|t| parse_jsonc(&t)) else { return out };
    let Some(Value::Object(deps)) = parsed.get("dependencies") else { return out };
    for (name, value) in deps {
        let Some(v) = value.as_str() else { continue };
        let Some(rest) = v.strip_prefix("file:") else { continue };
        let target = codegraph_types::jscompat::js_trim(rest);
        if !target.is_empty() {
            out.push((name.clone(), target.to_string()));
        }
    }
    out
}

/// Rewrite a bare workspace import to a project-relative path WITHOUT an extension.
/// `None` when no member package name matches.
pub fn resolve_workspace_import(import_path: &str, ws: &WorkspacePackages) -> Option<String> {
    // Longest matching package name wins (first on a tie, in Map order).
    let mut best: Option<&String> = None;
    for name in &ws.names_in_order {
        if import_path == name || (import_path.starts_with(name.as_str()) && import_path[name.len()..].starts_with('/')) {
            if best.is_none_or(|b| codegraph_jsre::utf16_len(name) > codegraph_jsre::utf16_len(b)) {
                best = Some(name);
            }
        }
    }
    let best = best?;
    let dir = &ws.by_name[best];
    let subpath = &import_path[best.len()..];
    if subpath.is_empty() {
        if let Some(entry) = ws.entry_by_name.get(best) {
            return Some(entry.clone());
        }
    }
    Some(re!(r"/{2,}", "g").replace_all_str(&format!("{dir}{subpath}"), "/"))
}

/// Workspace glob patterns from package.json + pnpm-workspace.yaml.
fn read_workspace_globs(project_root: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(text) = read_text(&nodepath::join(&[project_root, "package.json"])) {
        if let Ok(pkg) = serde_json::from_str::<Value>(&text) {
            let ws = pkg.get("workspaces");
            let list = match ws {
                Some(Value::Array(a)) => Some(a),
                Some(Value::Object(o)) => o.get("packages").and_then(Value::as_array),
                _ => None,
            };
            if let Some(list) = list {
                out.extend(list.iter().filter_map(|w| w.as_str().map(String::from)));
            }
        }
    }
    if let Some(yaml) = read_text(&nodepath::join(&[project_root, "pnpm-workspace.yaml"])) {
        out.extend(parse_pnpm_packages(&yaml));
    }
    out
}

/// Minimal pnpm-workspace.yaml `packages:` extractor.
fn parse_pnpm_packages(yaml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in re!(r"\r?\n").split(yaml) {
        if re!(r"^\s*packages\s*:").is_match(line) {
            in_packages = true;
            continue;
        }
        if in_packages {
            if let Some(item) = re!(r"^\s*-\s*(.+?)\s*$").captures(line) {
                out.push(re!(r#"^['"]|['"]$"#, "g").replace_all_str(item.text(1).unwrap(), ""));
                continue;
            }
            // A non-list, non-blank line ends the `packages:` block.
            if !codegraph_types::jscompat::js_trim(line).is_empty() && !re!(r"^\s").is_match(line) {
                in_packages = false;
            }
        }
    }
    out
}

const GLOB_SKIP_DIRS: [&str; 2] = ["node_modules", "bower_components"];
const GLOBSTAR_SKIP_DIRS: [&str; 2] = ["test", "tests"];
const GLOBSTAR_MAX_DEPTH: usize = 8;
const GLOB_DIR_BUDGET: usize = 10_000;

/// Expand a workspace glob to the member directories it matches, nearest first.
fn expand_workspace_glob(project_root: &str, pattern: &str) -> Vec<String> {
    let norm = re!(r"/+$").replace_all_str(&pattern.replace('\\', "/"), "");
    let glob = compile_glob(&norm);
    let Some(first) = glob.iter().position(|seg| seg.wildcard) else { return vec![norm] };

    let base = glob[..first].iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("/");
    let start = glob_closure(&glob, [first].into_iter().collect());
    // `tools/**` is `tools` too.
    let mut out: Vec<String> = if start.contains(&glob.len()) { vec![if base.is_empty() { ".".to_string() } else { base.clone() }] } else { Vec::new() };
    let mut level: Vec<(String, Vec<usize>)> = vec![(base, sorted(&start))];
    let mut read = 0;
    let mut depth = 0;
    while !level.is_empty() && depth <= GLOBSTAR_MAX_DEPTH {
        let mut next: Vec<(String, Vec<usize>)> = Vec::new();
        for (rel, states) in &level {
            read += 1;
            if read > GLOB_DIR_BUDGET {
                log_debug("workspace glob stopped at its folder budget", &[("pattern", pattern.to_string()), ("read", read.to_string())]);
                return out;
            }
            let dir = nodepath::join(&[project_root, rel]);
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut names: Vec<String> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let state_set: HashSet<usize> = states.iter().copied().collect();
            for name in names {
                let advanced = glob_step(&glob, &state_set, &name, true);
                if advanced.is_empty() {
                    continue;
                }
                let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                if advanced.contains(&glob.len()) {
                    out.push(child.clone());
                }
                if advanced.iter().any(|s| *s < glob.len()) {
                    next.push((child, sorted(&advanced)));
                }
            }
        }
        level = next;
        depth += 1;
    }
    out
}

fn sorted(set: &HashSet<usize>) -> Vec<usize> {
    let mut v: Vec<usize> = set.iter().copied().collect();
    v.sort_unstable();
    v
}

/// Whether a negated workspace glob's pattern matches a member directory.
fn workspace_glob_matches(pattern: &str, dir: &str) -> bool {
    let glob = compile_glob(&re!(r"/+$").replace_all_str(&pattern.replace('\\', "/"), ""));
    let mut states = glob_closure(&glob, [0usize].into_iter().collect());
    for name in dir.split('/').filter(|s| !s.is_empty() && *s != ".") {
        states = glob_step(&glob, &states, name, false);
        if states.is_empty() {
            return false;
        }
    }
    states.contains(&glob.len())
}

struct GlobSegment {
    text: String,
    /// `**`: any number of folders, none included.
    globstar: bool,
    wildcard: bool,
    re: Option<std::sync::Arc<Re>>,
}

fn compile_glob(pattern: &str) -> Vec<GlobSegment> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .map(|text| {
            if text == "**" {
                return GlobSegment { text: text.to_string(), globstar: true, wildcard: true, re: None };
            }
            if !re!(r"[*?{]").is_match(text) {
                return GlobSegment { text: text.to_string(), globstar: false, wildcard: false, re: None };
            }
            let escaped = re!(r"[.+^$()|[\]\\]", "g").replace_all(text, |c| Some(format!("\\{}", &c[0])));
            let stars = re!(r"\*+", "g").replace_all_str(&escaped, "[^/]*");
            let ques = re!(r"\?", "g").replace_all_str(&stars, "[^/]");
            let source = re!(r"\{([^{}]*)\}", "g").replace_all(&ques, |c| Some(format!("(?:{})", c[1].split(',').collect::<Vec<_>>().join("|"))));
            GlobSegment { text: text.to_string(), globstar: false, wildcard: true, re: Some(Re::cached(&format!("^{source}$"), "")) }
        })
        .collect()
}

fn glob_step(glob: &[GlobSegment], states: &HashSet<usize>, name: &str, walking: bool) -> HashSet<usize> {
    let mut next = HashSet::new();
    let hidden = walking && (name.starts_with('.') || GLOB_SKIP_DIRS.contains(&name));
    let mut ordered: Vec<usize> = states.iter().copied().collect();
    ordered.sort_unstable();
    for i in ordered {
        let Some(seg) = glob.get(i) else { continue };
        if seg.globstar {
            if !hidden && !(walking && GLOBSTAR_SKIP_DIRS.contains(&name)) {
                next.insert(i);
            }
        } else if match &seg.re {
            Some(re) => !hidden && re.is_match(name),
            None => seg.text == name,
        } {
            next.insert(i + 1);
        }
    }
    glob_closure(glob, next)
}

fn glob_closure(glob: &[GlobSegment], mut states: HashSet<usize>) -> HashSet<usize> {
    let snapshot: Vec<usize> = states.iter().copied().collect();
    for i in snapshot {
        let mut j = i;
        while glob.get(j).is_some_and(|s| s.globstar) {
            states.insert(j + 1);
            j += 1;
        }
    }
    states
}

fn read_package_name(dir_abs: &str) -> Option<String> {
    let text = read_text(&nodepath::join(&[dir_abs, "package.json"]))?;
    let pkg: Value = serde_json::from_str(&text).ok()?;
    pkg.get("name")?.as_str().filter(|n| !n.is_empty()).map(String::from)
}

#[allow(dead_code)]
fn _escape_unused() -> String {
    escape_regex("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pnpm_packages_block() {
        let yaml = "packages:\n  - 'packages/*'\n  - \"apps/*\"\n  - tools/build\nother:\n  - x\n";
        assert_eq!(parse_pnpm_packages(yaml), vec!["packages/*", "apps/*", "tools/build"]);
    }

    #[test]
    fn glob_matching() {
        assert!(workspace_glob_matches("**/example", "packages/a/example"));
        assert!(workspace_glob_matches("examples/with-*", "examples/with-nx"));
        assert!(!workspace_glob_matches("examples/with-*", "examples/other"));
    }

    #[test]
    fn longest_member_name_wins() {
        let mut ws = WorkspacePackages::default();
        ws.insert("@s/ui".into(), "packages/ui".into());
        ws.insert("@s/ui/core".into(), "packages/core".into());
        assert_eq!(resolve_workspace_import("@s/ui/core/x", &ws).as_deref(), Some("packages/core/x"));
        assert_eq!(resolve_workspace_import("@s/ui/widgets", &ws).as_deref(), Some("packages/ui/widgets"));
        assert_eq!(resolve_workspace_import("@s/uix", &ws), None);
    }
}
