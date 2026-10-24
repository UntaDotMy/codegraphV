//! JS/TS workspace (monorepo) package resolution — port of `src/resolution/workspace-packages.ts`.
//!
//! npm / yarn / bun read member packages from the root `package.json` `workspaces`
//! field, pnpm from `pnpm-workspace.yaml`; ohpm modules declare `file:` siblings in
//! `oh-package.json5`, and any package.json may depend on a sibling by path
//! (`"acme-shared": "file:../common"`, #2456). Each package's `name` maps to its
//! directory, and its manifest's declared entries (`exports`, `main`, `module`,
//! `types`) to files (#2502), so the resolver can rewrite `@scope/ui` →
//! `packages/ui/src/index.ts` and `@scope/ui/widgets` → `packages/ui/widgets`.

use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::log::log_debug;
use codegraph_types::nodepath;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Default)]
pub struct WorkspacePackages {
    /// Package `name` → directory relative to the project root (posix): the workspace members,
    /// ohpm's `file:` dependencies, and the root package.json's path dependencies.
    pub by_name: HashMap<String, String>,
    /// Iteration order of `by_name` (JavaScript `Map` insertion order).
    pub names_in_order: Vec<String>,
    /// Member package `name` → its declared ENTRY FILE (ohpm `main`), posix, project-relative.
    pub entry_by_name: HashMap<String, String>,
    /// The entries an npm/pnpm/yarn/bun package's package.json declares, by its directory.
    pub entries_by_dir: HashMap<String, PackageEntries>,
    /// The path dependencies a package.json below the root declares: its directory →
    /// (dependency name, target directory) in declaration order. Seen only from the files under
    /// that manifest; the root manifest's are in `by_name`.
    pub scoped_by_name: HashMap<String, Vec<(String, String)>>,
}

impl WorkspacePackages {
    fn insert(&mut self, name: String, dir: String) {
        if self.by_name.insert(name.clone(), dir).is_none() {
            self.names_in_order.push(name);
        }
    }
}

/// What a package.json declares a package's importers reach, as project-relative posix
/// targets, best first. A target may name build output the index does not hold.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PackageEntries {
    /// The `exports` map (`.`, `./utils`, `./schema/*` → targets), in key order. A string, an
    /// array or a conditions-only `exports` is the `.` entry.
    pub exports: Option<Vec<(String, Vec<String>)>>,
    /// The package root's `source`, `module`, `main`, `types` and `typings` fields.
    pub main: Option<Vec<String>>,
}

/// Where an import of a workspace or path-dependency package lands, before any file is probed.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceImportTargets {
    /// The entries the package declares for the import, best first.
    pub declared: Vec<String>,
    /// The package's directory plus the import's subpath (`packages/ui/widgets`), extension-less.
    pub directory: String,
    /// The package's directory.
    pub package_dir: String,
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

/// Load workspace packages for `project_root`; `None` when the project declares no workspaces
/// and no package depends on another by path.
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

    // HarmonyOS/OpenHarmony (ArkTS) modular projects, and npm path dependencies (#2456).
    let PathDeps { ohpm, mut npm } = collect_path_deps(project_root);
    for (name, dir) in ohpm {
        if ws.by_name.contains_key(&name) {
            continue;
        }
        let entry = read_ohpm_main(project_root, &dir);
        ws.insert(name.clone(), dir);
        if let Some(entry) = entry {
            ws.entry_by_name.insert(name, entry);
        }
    }
    if let Some(root_deps) = npm.remove("") {
        for (name, dir) in root_deps {
            if !ws.by_name.contains_key(&name) {
                ws.insert(name, dir);
            }
        }
    }

    if ws.by_name.is_empty() && npm.is_empty() {
        return None;
    }

    // What each npm-style package's manifest declares for its importers.
    let mut package_dirs: HashSet<String> = ws.by_name.values().cloned().collect();
    for deps in npm.values() {
        package_dirs.extend(deps.iter().map(|(_, dir)| dir.clone()));
    }
    for dir in package_dirs {
        if let Some(entries) = read_package_entries(project_root, &dir) {
            ws.entries_by_dir.insert(dir, entries);
        }
    }
    ws.scoped_by_name = npm;

    log_debug(
        "workspace packages loaded",
        &[
            ("count", ws.by_name.len().to_string()),
            ("scoped", ws.scoped_by_name.len().to_string()),
            ("entries", ws.entries_by_dir.len().to_string()),
        ],
    );
    Some(ws)
}

/// Condition keys an `exports` target is read through first; any other condition comes next,
/// and `types`/`typings` last.
const RESOLVING_CONDITIONS: [&str; 6] = ["import", "require", "node", "module", "module-sync", "default"];
const TYPE_CONDITIONS: [&str; 2] = ["types", "typings"];

/// An `exports` value's targets, best first: a string, a fallback array, or (nested)
/// conditions, by key order within each tier.
fn export_targets(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items.iter().flat_map(export_targets).collect(),
        Value::Object(conditions) => {
            let tier = |key: &str| {
                if RESOLVING_CONDITIONS.contains(&key) {
                    0
                } else if TYPE_CONDITIONS.contains(&key) {
                    2
                } else {
                    1
                }
            };
            (0..3)
                .flat_map(|t| conditions.iter().filter(move |(key, _)| tier(key) == t).flat_map(|(_, v)| export_targets(v)))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The package.json fields an importer of the package root reaches without `exports`.
const MAIN_FIELDS: [&str; 5] = ["source", "module", "main", "types", "typings"];

fn dedupe(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items.into_iter().filter(|s| seen.insert(s.clone())).collect()
}

/// Read a package's declared entries; `None` when its manifest declares none.
fn read_package_entries(project_root: &str, dir_rel: &str) -> Option<PackageEntries> {
    let text = read_text(&nodepath::join(&[project_root, dir_rel, "package.json"]))?;
    let pkg: Value = serde_json::from_str(&text).ok()?;
    let pkg = pkg.as_object()?;
    let in_package = |target: &str| package_target(project_root, dir_rel, target);
    let mut out = PackageEntries::default();
    if let Some(exp) = pkg.get("exports").filter(|v| !v.is_null()) {
        let subpaths = exp.as_object().is_some_and(|o| o.keys().any(|k| k.starts_with('.')));
        let entries: Vec<(String, &Value)> = if subpaths {
            exp.as_object().unwrap().iter().filter(|(k, _)| k.starts_with('.')).map(|(k, v)| (k.clone(), v)).collect()
        } else {
            vec![(".".to_string(), exp)]
        };
        let mut exports: Vec<(String, Vec<String>)> = Vec::new();
        for (key, value) in entries {
            let targets = dedupe(export_targets(value).iter().filter_map(|t| in_package(t)));
            match exports.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = targets,
                None => exports.push((key, targets)),
            }
        }
        out.exports = Some(exports);
    }
    let main = dedupe(MAIN_FIELDS.iter().filter_map(|field| pkg.get(*field)?.as_str()).filter_map(in_package));
    if !main.is_empty() {
        out.main = Some(main);
    }
    (out.exports.is_some() || out.main.is_some()).then_some(out)
}

/// A package-relative target (`./src/index.ts`) → project-relative posix, or `None` if it leaves
/// the package.
fn package_target(project_root: &str, dir_rel: &str, target: &str) -> Option<String> {
    let trimmed = js_trim(target);
    if trimmed.is_empty() {
        return None;
    }
    let package_dir = nodepath::resolve(&[project_root, dir_rel]);
    let abs = nodepath::resolve(&[&package_dir, trimmed]);
    let inside = nodepath::relative(&package_dir, &abs);
    if inside.starts_with("..") || nodepath::is_absolute(&inside) {
        return None;
    }
    Some(nodepath::relative(project_root, &abs).replace('\\', "/"))
}

fn read_ohpm_main(project_root: &str, dir_rel: &str) -> Option<String> {
    let parsed = parse_jsonc(&read_text(&nodepath::join(&[project_root, dir_rel, OHPM_MANIFEST]))?)?;
    let main = parsed.get("main")?.as_str()?;
    let main = js_trim(main);
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
const NPM_MANIFEST: &str = "package.json";
const OHPM_WALK_MAX_DEPTH: usize = 6;
const OHPM_WALK_DIR_BUDGET: usize = 8000;
const OHPM_SKIP_DIRS: [&str; 9] = ["node_modules", "oh_modules", ".git", ".codegraph", ".hvigor", ".preview", "build", "dist", "out"];

/// The path dependencies the project's manifests declare.
struct PathDeps {
    /// ohpm `file:` dependencies, in the order a JavaScript `Map` would iterate them.
    ohpm: Vec<(String, String)>,
    /// package.json `file:`/`link:`/`portal:` dependencies by the declaring manifest's directory
    /// (`""` for the root).
    npm: HashMap<String, Vec<(String, String)>>,
}

/// Scan for `oh-package.json5` manifests and collect their `file:` dependencies; a name
/// declared with different target directories in different manifests is ambiguous and dropped.
/// The same walk reads every package.json's path dependencies (#2456): only a directory of the
/// project counts, other than the root and nothing under `node_modules`.
fn collect_path_deps(project_root: &str) -> PathDeps {
    let mut ohpm: Vec<(String, String)> = Vec::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    let mut npm: HashMap<String, Vec<(String, String)>> = HashMap::new();

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
            if name == NPM_MANIFEST {
                let mut deps: Vec<(String, String)> = Vec::new();
                for (dep, target) in read_npm_path_deps(&nodepath::join(&[&abs, &name])) {
                    let target_abs = nodepath::resolve(&[&abs, &target]);
                    let target_rel = nodepath::relative(project_root, &target_abs).replace('\\', "/");
                    if target_rel.is_empty() || target_rel.starts_with("..") || nodepath::is_absolute(&target_rel) {
                        continue;
                    }
                    if re!(r"(?:^|/)node_modules(?:/|$)").is_match(&target_rel) || !std::fs::metadata(&target_abs).is_ok_and(|m| m.is_dir()) {
                        continue;
                    }
                    match deps.iter_mut().find(|(n, _)| *n == dep) {
                        Some(slot) => slot.1 = target_rel,
                        None => deps.push((dep, target_rel)),
                    }
                }
                if !deps.is_empty() {
                    npm.insert(rel.clone(), deps);
                }
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
                match ohpm.iter().position(|(n, _)| *n == dep) {
                    None => {
                        if !ambiguous.contains(&dep) {
                            ohpm.push((dep, target_rel));
                        }
                    }
                    Some(i) => {
                        if ohpm[i].1 != target_rel {
                            ohpm.remove(i);
                            ambiguous.insert(dep);
                        }
                    }
                }
            }
        }
    }
    PathDeps { ohpm, npm }
}

/// A package.json's dependencies on a path (`file:`, `link:`, `portal:`), from every dependency
/// field → (name, target).
fn read_npm_path_deps(manifest_abs: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(pkg) = read_text(manifest_abs).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return out };
    let Some(pkg) = pkg.as_object() else { return out };
    for field in ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"] {
        let Some(Value::Object(deps)) = pkg.get(field) else { continue };
        for (name, value) in deps {
            let Some(v) = value.as_str() else { continue };
            let Some(m) = re!(r"^(?:file|link|portal):(.+)$").captures(js_trim(v)) else { continue };
            let target = js_trim(m.text(1).unwrap());
            if !target.is_empty() {
                out.push((name.clone(), target.to_string()));
            }
        }
    }
    out
}

fn read_ohpm_file_deps(manifest_abs: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(parsed) = read_text(manifest_abs).and_then(|t| parse_jsonc(&t)) else { return out };
    let Some(Value::Object(deps)) = parsed.get("dependencies") else { return out };
    for (name, value) in deps {
        let Some(v) = value.as_str() else { continue };
        let Some(rest) = v.strip_prefix("file:") else { continue };
        let target = js_trim(rest);
        if !target.is_empty() {
            out.push((name.clone(), target.to_string()));
        }
    }
    out
}

/// Rewrite a bare workspace import to a project-relative path WITHOUT an extension; `from_file`
/// sees the path dependencies the manifests above it declare. `None` when no package matches.
pub fn resolve_workspace_import(import_path: &str, ws: &WorkspacePackages, from_file: Option<&str>) -> Option<String> {
    let (name, dir) = match_package(import_path, ws, from_file)?;
    let subpath = &import_path[name.len()..];
    if subpath.is_empty() {
        if let Some(entry) = ws.entry_by_name.get(name) {
            return Some(entry.clone());
        }
    }
    Some(re!(r"/{2,}", "g").replace_all_str(&format!("{dir}{subpath}"), "/"))
}

/// Where an import of a package of the project lands: what its manifest declares for the
/// import, and the directory form. `None` when no package name matches.
pub fn workspace_import_targets(import_path: &str, ws: &WorkspacePackages, from_file: Option<&str>) -> Option<WorkspaceImportTargets> {
    let (name, dir) = match_package(import_path, ws, from_file)?;
    let subpath = &import_path[name.len()..];
    let directory = re!(r"/{2,}", "g").replace_all_str(&format!("{dir}{subpath}"), "/");
    let ohpm = if subpath.is_empty() { ws.entry_by_name.get(name) } else { None };
    let declared = match ohpm {
        Some(entry) => vec![entry.clone()],
        None => declared_entries(ws.entries_by_dir.get(dir), subpath),
    };
    Some(WorkspaceImportTargets { declared, directory, package_dir: dir.to_string() })
}

/// The package an import names: a path dependency of the nearest manifest above `from_file`
/// that declares a matching name, else a workspace member or a root path dependency. The
/// longest name wins (first on a tie, in declaration order).
fn match_package<'a>(import_path: &str, ws: &'a WorkspacePackages, from_file: Option<&str>) -> Option<(&'a str, &'a str)> {
    if let Some(hit) = scoped_package(import_path, ws, from_file) {
        return Some(hit);
    }
    let name = longest_name(import_path, ws.names_in_order.iter().map(String::as_str))?;
    Some((name, ws.by_name[name].as_str()))
}

fn scoped_package<'a>(import_path: &str, ws: &'a WorkspacePackages, from_file: Option<&str>) -> Option<(&'a str, &'a str)> {
    let from_file = from_file?;
    if ws.scoped_by_name.is_empty() {
        return None;
    }
    let mut dir = from_file.replace('\\', "/");
    while !dir.is_empty() {
        dir = match dir.rfind('/') {
            Some(slash) => dir[..slash].to_string(),
            None => String::new(),
        };
        let Some(names) = ws.scoped_by_name.get(&dir) else { continue };
        if let Some(name) = longest_name(import_path, names.iter().map(|(n, _)| n.as_str())) {
            let dir = names.iter().find(|(n, _)| n == name).map(|(_, d)| d.as_str())?;
            return Some((name, dir));
        }
    }
    None
}

fn longest_name<'a>(import_path: &str, names: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut best: Option<&'a str> = None;
    for name in names {
        let matches = import_path == name || (import_path.starts_with(name) && import_path[name.len()..].starts_with('/'));
        if matches && best.is_none_or(|b| codegraph_jsre::utf16_len(name) > codegraph_jsre::utf16_len(b)) {
            best = Some(name);
        }
    }
    best
}

/// The targets a package's manifest declares for `subpath` (`""` or `/widgets`), best first.
fn declared_entries(entries: Option<&PackageEntries>, subpath: &str) -> Vec<String> {
    let Some(entries) = entries else { return Vec::new() };
    let exported = entries.exports.as_ref().map(|e| exported_targets(e, &format!(".{subpath}"))).unwrap_or_default();
    if !subpath.is_empty() {
        return exported;
    }
    dedupe(exported.into_iter().chain(entries.main.iter().flatten().cloned()))
}

/// `exports`' targets for `key`: the exact key's, else the `*` pattern's with the longest prefix,
/// its `*`s filled in.
fn exported_targets(exports: &[(String, Vec<String>)], key: &str) -> Vec<String> {
    if let Some((_, exact)) = exports.iter().find(|(k, _)| k == key) {
        return exact.clone();
    }
    let mut best: Option<(&str, &str, &Vec<String>)> = None;
    for (pattern, targets) in exports {
        let Some(star) = pattern.find('*') else { continue };
        if pattern[star + 1..].contains('*') {
            continue;
        }
        let prefix = &pattern[..star];
        let suffix = &pattern[star + 1..];
        if key.len() < prefix.len() + suffix.len() || !key.starts_with(prefix) || !key.ends_with(suffix) {
            continue;
        }
        if best.is_some_and(|(p, _, _)| utf16(prefix) <= utf16(p)) {
            continue;
        }
        best = Some((prefix, &key[prefix.len()..key.len() - suffix.len()], targets));
    }
    match best {
        Some((_, matched, targets)) => targets.iter().map(|t| t.replace('*', matched)).collect(),
        None => Vec::new(),
    }
}

fn utf16(s: &str) -> usize {
    codegraph_jsre::utf16_len(s)
}

/// The workspace map as a stable digest: every name, scope, directory and declared entry an
/// import can be resolved by (workspaceFingerprint). Sync compares it with the one the index was
/// built with to see a package.json edit (#2456, #2502).
pub fn workspace_fingerprint(ws: Option<&WorkspacePackages>) -> String {
    let Some(ws) = ws else { return "none".to_string() };
    fn js_sorted<V: Clone>(items: impl Iterator<Item = (String, V)>) -> Vec<(String, V)> {
        let mut v: Vec<(String, V)> = items.collect();
        v.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        v
    }
    let pairs = |v: Vec<(String, String)>| v.into_iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>();
    let by_name = js_sorted(ws.by_name.iter().map(|(k, v)| (k.clone(), v.clone())));
    let entry_by_name = js_sorted(ws.entry_by_name.iter().map(|(k, v)| (k.clone(), v.clone())));
    let scoped: Vec<Value> = js_sorted(ws.scoped_by_name.iter().map(|(k, v)| (k.clone(), v.clone())))
        .into_iter()
        .map(|(dir, names)| {
            // A JavaScript Map holds one value per name; the last declaration of a name is kept.
            json!([dir, pairs(js_sorted(names.into_iter()))])
        })
        .collect();
    let entries: Vec<Value> = js_sorted(ws.entries_by_dir.iter().map(|(k, v)| (k.clone(), v.clone())))
        .into_iter()
        .map(|(dir, e)| {
            let exports: Vec<Value> = js_sorted(e.exports.unwrap_or_default().into_iter()).into_iter().map(|(k, t)| json!([k, t])).collect();
            json!([dir, exports, e.main.unwrap_or_default()])
        })
        .collect();
    let mut canonical = serde_json::Map::new();
    canonical.insert("byName".into(), Value::Array(pairs(by_name)));
    canonical.insert("entryByName".into(), Value::Array(pairs(entry_by_name)));
    canonical.insert("scoped".into(), Value::Array(scoped));
    canonical.insert("entries".into(), Value::Array(entries));
    let text = serde_json::to_string(&Value::Object(canonical)).unwrap_or_default();
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The folder of the package `file` is in — a workspace member, or a path dependency's target —
/// the outermost when packages nest, or `None` (workspacePackageDirOf).
pub fn workspace_package_dir_of(ws: &WorkspacePackages, file: &str) -> Option<String> {
    let mut dirs: Vec<&String> = ws.by_name.values().collect();
    for names in ws.scoped_by_name.values() {
        dirs.extend(names.iter().map(|(_, d)| d));
    }
    let mut seen = HashSet::new();
    let mut dirs: Vec<&String> = dirs.into_iter().filter(|d| !d.is_empty() && seen.insert(*d)).collect();
    // A stable sort by length: of two nested folders the outer one is found first.
    dirs.sort_by_key(|d| utf16(d));
    dirs.into_iter().find(|dir| file.starts_with(dir.as_str()) && file[dir.len()..].starts_with('/')).cloned()
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
        assert_eq!(resolve_workspace_import("@s/ui/core/x", &ws, None).as_deref(), Some("packages/core/x"));
        assert_eq!(resolve_workspace_import("@s/ui/widgets", &ws, None).as_deref(), Some("packages/ui/widgets"));
        assert_eq!(resolve_workspace_import("@s/uix", &ws, None), None);
    }
}
