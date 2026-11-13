//! Cargo workspace crate map — port of `src/resolution/frameworks/cargo-workspace.ts`.

use crate::ctx::Ctx;
use codegraph_jsre::{escape_regex, re, Re};
use codegraph_types::jscompat::js_trim;
use codegraph_types::nodepath::posix;
use indexmap::IndexMap;
use std::collections::HashSet;

const MAX_GLOB_WALK_DEPTH: usize = 5;

fn has_glob_chars(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '!'))
}

fn get_section(content: &str, section_name: &str) -> Option<String> {
    let header = format!("[{section_name}]");
    let mut in_section = false;
    let mut section_lines: Vec<&str> = Vec::new();
    for line in content.split('\n') {
        let trimmed = js_trim(line);
        if !in_section {
            if trimmed == header {
                in_section = true;
            }
            continue;
        }
        if re!(r"^\[[^\]]+\]$").is_match(trimmed) {
            break;
        }
        section_lines.push(line);
    }
    if !in_section {
        return None;
    }
    Some(section_lines.join("\n"))
}

fn extract_quoted_values(value_list: &str) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut current = String::new();
    for ch in value_list.chars() {
        let Some(q) = quote else {
            if ch == '"' || ch == '\'' {
                quote = Some(ch);
                current.clear();
            }
            continue;
        };
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == q {
            values.push(js_trim(&current).to_string());
            quote = None;
            current.clear();
            continue;
        }
        current.push(ch);
    }
    values.into_iter().filter(|v| !v.is_empty()).collect()
}

fn get_array_value(section: &str, key: &str) -> Option<String> {
    let key_regex = Re::cached(&format!(r"\b{}\b\s*=", escape_regex(key)), "m");
    let m = key_regex.find(section)?;
    let chars: Vec<char> = section.chars().collect();
    let mut i = section[..m.end()].chars().count();
    while i < chars.len() && codegraph_types::jscompat::is_js_space(chars[i]) {
        i += 1;
    }
    if chars.get(i) != Some(&'[') {
        return None;
    }
    i += 1;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 1;
    let start = i;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = in_quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_quote = Some(ch);
            i += 1;
            continue;
        }
        if ch == '[' {
            depth += 1;
            i += 1;
            continue;
        }
        if ch == ']' {
            depth -= 1;
            if depth == 0 {
                return Some(chars[start..i].iter().collect());
            }
            i += 1;
            continue;
        }
        i += 1;
    }
    None
}

fn parse_workspace_members(cargo_toml: &str) -> Vec<String> {
    let Some(workspace) = get_section(cargo_toml, "workspace").filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let Some(members) = get_array_value(&workspace, "members").filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    extract_quoted_values(&members)
}

/// A manifest's `[package]` name, wherever the section writes it (after a `categories = [...]` too).
pub fn parse_package_name(cargo_toml: &str) -> Option<String> {
    let package = get_section(cargo_toml, "package").filter(|s| !s.is_empty())?;
    re!(r#"name\s*=\s*["']([^"'\n]+)["']"#)
        .captures(&package)
        .and_then(|c| c.text(1))
        .map(|s| js_trim(s).to_string())
}

/// The target roots a manifest names with a `path` of its own, as written (relative to the
/// manifest's directory): `[lib] path = "…"`, each `[[bin]] path = "…"` (ripgrep's
/// `crates/core/main.rs`), and each `[[example]]`, `[[test]]` and `[[bench]]` one.
#[derive(Debug, Default)]
pub struct TargetPaths {
    pub lib: Option<String>,
    pub bins: Vec<String>,
    /// Examples, tests and benches. Each is its own crate root.
    pub others: Vec<String>,
}

pub fn parse_target_paths(cargo_toml: &str) -> TargetPaths {
    #[derive(PartialEq, Clone, Copy)]
    enum Table {
        None,
        Lib,
        Bin,
        Other,
    }
    let mut out = TargetPaths::default();
    let mut table = Table::None;
    for raw in cargo_toml.split('\n') {
        let line = js_trim(raw.split('#').next().unwrap_or(""));
        if line.starts_with('[') {
            table = match line {
                "[lib]" => Table::Lib,
                "[[bin]]" => Table::Bin,
                _ if re!(r"^\[\[(?:example|test|bench)\]\]$").is_match(line) => Table::Other,
                _ => Table::None,
            };
            continue;
        }
        if table == Table::None {
            continue;
        }
        let at = re!(r#"^path\s*=\s*["']([^"'\n]+)["']"#)
            .captures(line)
            .and_then(|c| c.text(1).map(|s| s.replace('\\', "/")));
        let Some(at) = at.filter(|s| !s.is_empty()) else {
            continue;
        };
        match table {
            Table::Lib => out.lib = Some(at),
            Table::Bin => out.bins.push(at),
            Table::Other => out.others.push(at),
            Table::None => {}
        }
    }
    out
}

fn add_crate_alias(map: &mut IndexMap<String, String>, crate_name: &str, member_path: &str) {
    let normalized = crate_name.replace('-', "_");
    map.insert(crate_name.to_string(), member_path.to_string());
    if normalized != crate_name {
        map.insert(normalized, member_path.to_string());
    }
}

fn clean_path(member_path: &str) -> String {
    let p = member_path.replace('\\', "/");
    p.strip_suffix('/').unwrap_or(&p).to_string()
}

/// A `picomatch(…, { dot: false })` glob as a regex source (the walk never yields dot segments).
fn glob_regex(glob: &str) -> String {
    let chars: Vec<char> = glob.chars().collect();
    let mut out = String::from("^");
    let mut i = 0;
    let mut brace_depth = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    let at_start = i == 0 || chars[i - 1] == '/';
                    let followed_by_slash = chars.get(i + 2) == Some(&'/');
                    if at_start && followed_by_slash {
                        out.push_str("(?:[^/]+/)*");
                        i += 3;
                        continue;
                    }
                    if i > 0 && chars[i - 1] == '/' && i + 2 >= chars.len() {
                        // a trailing `/**` also matches the directory itself
                        out.pop();
                        out.push_str("(?:/.*)?");
                        i += 2;
                        continue;
                    }
                    out.push_str(".*");
                    i += 2;
                    continue;
                }
                out.push_str("[^/]*");
            }
            '?' => out.push_str("[^/]"),
            '[' => {
                if let Some(rel) = chars[i + 1..].iter().position(|&x| x == ']') {
                    let body: String = chars[i + 1..i + 1 + rel].iter().collect();
                    let body = if let Some(rest) = body.strip_prefix('!') {
                        format!("^{rest}")
                    } else {
                        body
                    };
                    out.push('[');
                    out.push_str(&body.replace('\\', "\\\\"));
                    out.push(']');
                    i += rel + 2;
                    continue;
                }
                out.push_str("\\[");
            }
            '{' => {
                brace_depth += 1;
                out.push_str("(?:");
            }
            '}' if brace_depth > 0 => {
                brace_depth -= 1;
                out.push(')');
            }
            ',' if brace_depth > 0 => out.push('|'),
            c => out.push_str(&escape_regex(&c.to_string())),
        }
        i += 1;
    }
    out.push('$');
    out
}

fn expand_glob_member(member: &str, ctx: &Ctx) -> Vec<String> {
    let first_glob_idx = member
        .char_indices()
        .find(|(_, c)| matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '!'))
        .map_or(0, |(i, _)| i);
    let prefix = re!(r"[^/]*$").replace_first(&member[..first_glob_idx], |_| String::new());
    let static_prefix = prefix.strip_suffix('/').unwrap_or(&prefix).to_string();
    let matcher = Re::cached(&glob_regex(member), "");
    let mut matches: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    fn walk(
        dir: &str,
        depth: usize,
        ctx: &Ctx,
        matcher: &Re,
        seen: &mut HashSet<String>,
        matches: &mut Vec<String>,
    ) {
        if depth > MAX_GLOB_WALK_DEPTH {
            return;
        }
        for child in ctx.list_directories(dir) {
            if matches!(
                child.as_str(),
                "target" | "node_modules" | ".git" | "dist" | "build"
            ) || child.starts_with('.')
            {
                continue;
            }
            let rel = if dir == "." {
                child.clone()
            } else {
                format!("{dir}/{child}")
            };
            if matcher.is_match(&rel) && !seen.contains(&rel) {
                seen.insert(rel.clone());
                matches.push(rel.clone());
            }
            walk(&rel, depth + 1, ctx, matcher, seen, matches);
        }
    }
    walk(
        if static_prefix.is_empty() {
            "."
        } else {
            &static_prefix
        },
        0,
        ctx,
        &matcher,
        &mut seen,
        &mut matches,
    );
    matches
}

/// A member outside the project (`../shared`, an absolute path, or a path that
/// normalizes to one) is neither read nor listed. A folder whose name only
/// starts with two dots (`..generated`) is inside.
fn member_leaves_project(member: &str) -> bool {
    let normalized = posix::normalize(&member.replace('\\', "/"));
    let normalized = normalized.trim_end_matches('/');
    normalized == ".." || normalized.starts_with("../") || posix::is_absolute(normalized)
}

fn expand_members(members: &[String], ctx: &Ctx) -> Vec<String> {
    let mut expanded: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for member in members {
        if member_leaves_project(member) {
            continue;
        }
        let candidates = if has_glob_chars(member) {
            expand_glob_member(member, ctx)
        } else {
            vec![member.clone()]
        };
        for candidate in candidates {
            let cleaned = clean_path(&candidate);
            if seen.contains(&cleaned) {
                continue;
            }
            seen.insert(cleaned.clone());
            expanded.push(cleaned);
        }
    }
    expanded
}

/// What resolution reads from a Cargo manifest: the workspace's members, the package's name,
/// its target paths and what it depends on by name and path. A version bump is none of that.
pub fn cargo_manifest_fields(text: &str) -> String {
    let targets = parse_target_paths(text);
    let deps: Vec<_> = crate::name_matcher::rust::uses::rust_manifest_dependencies(text)
        .into_iter()
        .map(|d| serde_json::json!([format!("{:?}", d.kind), d.key, d.pkg, d.path, d.workspace]))
        .collect();
    serde_json::json!([
        parse_workspace_members(text),
        parse_package_name(text),
        [
            targets.lib,
            Some(targets.bins.join("\n")),
            Some(targets.others.join("\n"))
        ],
        deps
    ])
    .to_string()
}

/// Crate name (and its `_` spelling) → workspace member directory.
pub fn get_cargo_workspace_crate_map(ctx: &Ctx) -> IndexMap<String, String> {
    let mut result = IndexMap::new();
    let Some(root) = ctx.read_file("Cargo.toml").filter(|s| !s.is_empty()) else {
        return result;
    };
    let members = expand_members(&parse_workspace_members(&root), ctx);
    for member_path in members {
        let Some(member_toml) = ctx
            .read_file(&format!("{member_path}/Cargo.toml"))
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(name) = parse_package_name(&member_toml).filter(|n| !n.is_empty()) else {
            continue;
        };
        add_crate_alias(&mut result, &name, &member_path);
    }
    result
}

#[cfg(test)]
mod member_path_tests {
    use super::get_cargo_workspace_crate_map;
    use crate::ctx::Ctx;
    use codegraph_db::Store;
    use codegraph_types::nodepath::posix;
    use std::fs;
    use std::rc::Rc;

    fn write(path: &std::path::Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A project-relative path leaves the root when posix normalization walks
    /// out or the path is posix-absolute. A folder named `..generated` does not.
    fn leaves_project(path: &str) -> bool {
        let normalized = posix::normalize(&path.replace('\\', "/"));
        let normalized = normalized.trim_end_matches('/');
        normalized == ".." || normalized.starts_with("../") || posix::is_absolute(normalized)
    }

    struct Sandbox {
        parent: std::path::PathBuf,
    }

    impl Sandbox {
        /// A folder of its own: the process id keeps test processes apart and the counter keeps
        /// this process's tests apart (a clock reading did not: two tests could start in one tick).
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let parent = std::env::temp_dir().join(format!("cg-rust-members-{}-{n}", std::process::id()));
            // A leftover from an earlier process that had the same id.
            let _ = fs::remove_dir_all(&parent);
            fs::create_dir_all(&parent).unwrap();
            Self { parent }
        }

        fn project(&self) -> std::path::PathBuf {
            self.parent.join("project")
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    fn map_of(sandbox: &Sandbox) -> (indexmap::IndexMap<String, String>, Vec<String>) {
        let db = sandbox.parent.join("index.db");
        let store = Rc::new(Store::initialize(&db).unwrap());
        let ctx = Ctx::new(sandbox.project().to_string_lossy().to_string(), store);
        ctx.record_detection_inputs();
        let map = get_cargo_workspace_crate_map(&ctx);
        let inputs = ctx.take_detection_inputs();
        let recorded: Vec<String> = inputs.disk_paths.keys().cloned().collect();
        (map, recorded)
    }

    /// BUG-311: two tests that start in the same clock tick must not share a sandbox (they did on
    /// a Windows CI runner: one read the other's crates, and both locked the same index).
    #[test]
    fn sandboxes_made_at_the_same_moment_are_distinct() {
        let threads = 16;
        let start = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let sandboxes: Vec<Sandbox> = (0..threads)
            .map(|_| {
                let start = std::sync::Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    Sandbox::new()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        let distinct: std::collections::HashSet<_> = sandboxes.iter().map(|s| s.parent.clone()).collect();
        assert_eq!(distinct.len(), threads, "shared sandbox folders: {:?}", sandboxes.iter().map(|s| &s.parent).collect::<Vec<_>>());
    }

    #[test]
    fn cargo_members_outside_the_project_are_not_read() {
        let sandbox = Sandbox::new();
        let project = sandbox.project();
        write(
            &project.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\", \"app\", \"../outside\", \"../vendor/*\", \"/abs/crate\", \"crates/../../escape\"]\n",
        );
        write(
            &project.join("crates/core/Cargo.toml"),
            "[package]\nname = \"core-crate\"\n",
        );
        write(
            &project.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\n",
        );
        write(
            &sandbox.parent.join("outside/Cargo.toml"),
            "[package]\nname = \"outside\"\n",
        );
        write(
            &sandbox.parent.join("vendor/x/Cargo.toml"),
            "[package]\nname = \"vendored-x\"\n",
        );
        write(
            &sandbox.parent.join("escape/Cargo.toml"),
            "[package]\nname = \"escape\"\n",
        );

        let (map, recorded) = map_of(&sandbox);
        let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["app", "core-crate", "core_crate"]);
        let escaped: Vec<&String> = recorded.iter().filter(|p| leaves_project(p)).collect();
        assert_eq!(
            escaped,
            Vec::<&String>::new(),
            "read or listed outside the project: {recorded:?}"
        );
    }

    #[test]
    fn cargo_member_folder_starting_with_two_dots_is_read() {
        let sandbox = Sandbox::new();
        let project = sandbox.project();
        write(
            &project.join("Cargo.toml"),
            "[workspace]\nmembers = [\"..generated/crate\"]\n",
        );
        write(
            &project.join("..generated/crate/Cargo.toml"),
            "[package]\nname = \"generated\"\n",
        );

        let (map, recorded) = map_of(&sandbox);
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        assert_eq!(keys, ["generated"]);
        assert!(
            recorded
                .iter()
                .any(|p| p.replace('\\', "/") == "..generated/crate/Cargo.toml"),
            "expected to read ..generated/crate/Cargo.toml, recorded {recorded:?}"
        );
    }
}
