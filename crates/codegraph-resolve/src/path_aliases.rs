//! Project-level import-path alias loading — port of `src/resolution/path-aliases.ts`.
//!
//! Reads `compilerOptions.paths` / `baseUrl` from `tsconfig.json`, `jsconfig.json`
//! or `tsconfig.base.json`, follows `extends` chains (nearest config wins), and
//! turns the patterns into candidate project-relative paths for the import resolver.

use codegraph_jsre::re;
use codegraph_types::log::log_debug;
use codegraph_types::nodepath;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

/// A single alias pattern from `compilerOptions.paths`.
#[derive(Debug, Clone, PartialEq)]
pub struct AliasPattern {
    /// The literal prefix before `*` (or the whole pattern if no `*`).
    pub prefix: String,
    /// The literal suffix after `*` (almost always empty).
    pub suffix: String,
    pub has_wildcard: bool,
    /// Replacement templates, relative to [`AliasMap::base_url`].
    pub replacements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AliasMap {
    /// Absolute path. The directory `compilerOptions.paths` is rooted at.
    pub base_url: String,
    /// Whether the config (or one it extends) sets `compilerOptions.baseUrl`.
    pub declares_base_url: bool,
    /// Patterns ordered by specificity.
    pub patterns: Vec<AliasPattern>,
}

/// Strip JSONC comments + trailing commas so a tsconfig with the usual VS Code
/// annotations parses cleanly (the string-aware state machine of the original).
fn strip_jsonc(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_string = false;
    while i < c.len() {
        let ch = c[i];
        if in_string {
            out.push(ch);
            if ch == '\\' && i + 1 < c.len() {
                out.push(c[i + 1]);
                i += 2;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    // Trailing commas before } or ] — outside strings, so safe to run on the comment-stripped output.
    re!(r",(\s*[}\]])", "g").replace_all(&out, |c| Some(c[1].to_string()))
}

#[derive(Default)]
struct EffectiveOptions {
    base_url: Option<String>,
    paths: Option<serde_json::Map<String, Value>>,
    paths_dir: Option<String>,
}

const MAX_EXTENDS_DEPTH: usize = 32;

fn is_file(p: &str) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// Locate an `extends` target the way tsc does.
fn resolve_extends_target(spec: &str, from_dir: &str) -> Option<String> {
    if spec.starts_with("./") || spec.starts_with("../") || nodepath::is_absolute(spec) {
        let base = nodepath::resolve(&[from_dir, spec]);
        for cand in [base.clone(), format!("{base}.json"), nodepath::join(&[&base, "tsconfig.json"])] {
            if is_file(&cand) {
                return Some(cand);
            }
        }
        return None;
    }
    let mut dir = from_dir.to_string();
    loop {
        let base = nodepath::join(&[&dir, "node_modules", spec]);
        for cand in [base.clone(), format!("{base}.json"), nodepath::join(&[&base, "tsconfig.json"])] {
            if is_file(&cand) {
                return Some(cand);
            }
        }
        let parent = nodepath::dirname(&dir);
        if parent == dir {
            return None;
        }
        dir = parent;
    }
}

fn read_tsconfig_like(file_path: &str) -> Option<serde_json::Map<String, Value>> {
    let result = std::fs::read(file_path)
        .map_err(|e| e.to_string())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .and_then(|raw| serde_json::from_str::<Value>(&strip_jsonc(&raw)).map_err(|e| e.to_string()));
    match result {
        Ok(Value::Object(m)) => Some(m),
        Ok(_) => None,
        Err(err) => {
            log_debug("path-aliases: failed to parse", &[("filePath", file_path.to_string()), ("err", err)]);
            None
        }
    }
}

fn load_effective_options(file_path: &str, stack: &mut HashSet<String>, depth: usize) -> Option<EffectiveOptions> {
    let abs = nodepath::resolve(&[file_path]);
    if stack.contains(&abs) || depth > MAX_EXTENDS_DEPTH {
        log_debug("path-aliases: extends chain cycle or too deep", &[("filePath", abs), ("depth", depth.to_string())]);
        return None;
    }
    let raw = read_tsconfig_like(&abs)?;

    stack.insert(abs.clone());
    let dir = nodepath::dirname(&abs);
    let mut effective = EffectiveOptions::default();

    let parents: Vec<Value> = match raw.get("extends") {
        Some(Value::String(s)) => vec![Value::String(s.clone())],
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    for spec in parents {
        let Value::String(spec) = spec else { continue };
        let Some(target) = resolve_extends_target(&spec, &dir) else {
            log_debug("path-aliases: unresolved extends", &[("from", abs.clone()), ("spec", spec)]);
            continue;
        };
        let Some(inherited) = load_effective_options(&target, stack, depth + 1) else { continue };
        if inherited.base_url.is_some() {
            effective.base_url = inherited.base_url;
        }
        if inherited.paths.is_some() {
            effective.paths = inherited.paths;
            effective.paths_dir = inherited.paths_dir;
        }
    }
    stack.remove(&abs);

    if let Some(Value::Object(co)) = raw.get("compilerOptions") {
        // Both are relative to the file that declared them, not to whichever config started the chain.
        if let Some(Value::String(b)) = co.get("baseUrl") {
            effective.base_url = Some(nodepath::resolve(&[&dir, b]));
        }
        if let Some(Value::Object(paths)) = co.get("paths") {
            effective.paths = Some(paths.clone());
            effective.paths_dir = Some(dir);
        }
    }
    Some(effective)
}

fn split_wildcard(pattern: &str) -> (String, String, bool) {
    match pattern.find('*') {
        None => (pattern.to_string(), String::new(), false),
        Some(star) => (pattern[..star].to_string(), pattern[star + 1..].to_string(), true),
    }
}

/// Load aliases for `project_root`. `None` when no config is present or it has neither
/// usable `paths` nor a `baseUrl`.
pub fn load_project_aliases(project_root: &str) -> Option<AliasMap> {
    // `tsconfig.base.json` comes last on purpose: when a root `tsconfig.json` exists it stays
    // authoritative and reaches the base through `extends`.
    let candidates = ["tsconfig.json", "jsconfig.json", "tsconfig.base.json"];
    let mut effective: Option<EffectiveOptions> = None;
    let mut used_file: Option<&str> = None;
    for name in candidates {
        let p = nodepath::join(&[project_root, name]);
        if !Path::new(&p).exists() {
            continue;
        }
        let Some(opts) = load_effective_options(&p, &mut HashSet::new(), 0) else { continue };
        // Remember the first readable config, but keep looking: a config that contributes no
        // aliases must not shadow one that does.
        let has_paths = opts.paths.is_some();
        if effective.is_none() || has_paths {
            effective = Some(opts);
            used_file = Some(name);
        }
        if has_paths {
            break;
        }
    }
    let effective = effective?;

    let base_url = effective.base_url.clone().or(effective.paths_dir.clone()).unwrap_or_else(|| project_root.to_string());
    let declares_base_url = effective.base_url.is_some();

    let mut patterns: Vec<AliasPattern> = Vec::new();
    if let Some(paths) = &effective.paths {
        for (pattern, targets) in paths {
            let Value::Array(targets) = targets else { continue };
            if targets.is_empty() {
                continue;
            }
            let filtered: Vec<String> = targets.iter().filter_map(|t| t.as_str().map(String::from)).collect();
            if filtered.is_empty() {
                continue;
            }
            let (prefix, suffix, has_wildcard) = split_wildcard(pattern);
            patterns.push(AliasPattern { prefix, suffix, has_wildcard, replacements: filtered });
        }
    }

    // A `baseUrl` with no `paths` is still a map: the resolver reads a bare specifier from it.
    if patterns.is_empty() && !declares_base_url {
        return None;
    }

    // Specificity sort (stable, like Array.prototype.sort): longer prefix (in UTF-16 units)
    // first; literal patterns before wildcard ones of the same prefix length.
    patterns.sort_by(|a, b| {
        let (la, lb) = (codegraph_jsre::utf16_len(&a.prefix), codegraph_jsre::utf16_len(&b.prefix));
        if la != lb {
            return lb.cmp(&la);
        }
        if a.has_wildcard != b.has_wildcard {
            return if a.has_wildcard { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
        }
        std::cmp::Ordering::Equal
    });

    log_debug(
        "path-aliases loaded",
        &[
            ("file", used_file.unwrap_or("").to_string()),
            ("baseUrl", base_url.clone()),
            ("declaresBaseUrl", declares_base_url.to_string()),
            ("patternCount", patterns.len().to_string()),
        ],
    );
    Some(AliasMap { base_url, declares_base_url, patterns })
}

/// Resolve an import path through an [`AliasMap`]: the candidate project-relative paths in
/// priority order, `[]` when no alias matches. Callers still try each with the language's
/// extension list.
pub fn apply_aliases(import_path: &str, aliases: &AliasMap, project_root: &str) -> Vec<String> {
    for pat in &aliases.patterns {
        if !import_path.starts_with(&pat.prefix) {
            continue;
        }
        if !pat.suffix.is_empty() && !import_path.ends_with(&pat.suffix) {
            continue;
        }
        let mut captured = "";
        if pat.has_wildcard {
            // JS slices by UTF-16 index; prefix and suffix are prefix/suffix of the path, so bytes agree.
            captured = if import_path.len() >= pat.prefix.len() + pat.suffix.len() {
                &import_path[pat.prefix.len()..import_path.len() - pat.suffix.len()]
            } else {
                ""
            };
        } else if import_path != pat.prefix {
            continue;
        }
        let mut out = Vec::new();
        for target in &pat.replacements {
            let filled = if pat.has_wildcard { target.replacen('*', captured, 1) } else { target.clone() };
            let absolute = nodepath::resolve(&[&aliases.base_url, &filled]);
            let relative = nodepath::relative(project_root, &absolute);
            // Skip if the rewrite escapes the project root.
            if relative.starts_with("..") {
                continue;
            }
            out.push(relative.replace('\\', "/"));
        }
        return out;
    }
    Vec::new()
}

/// Where a non-relative specifier can live under `aliases`, in the order tsc tries them.
pub fn alias_and_base_url_candidates(import_path: &str, aliases: &AliasMap, project_root: &str) -> Vec<String> {
    let mut out = apply_aliases(import_path, aliases, project_root);
    if aliases.declares_base_url {
        let relative = nodepath::relative(project_root, &nodepath::resolve(&[&aliases.base_url, import_path]));
        if !relative.is_empty() && !relative.starts_with("..") && !nodepath::is_absolute(&relative) {
            out.push(relative.replace('\\', "/"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_is_stripped_but_urls_survive() {
        let s = strip_jsonc("{ // c\n \"a\": \"https://x.y/*z*/\", /* b */ \"b\": [1,], }");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["a"], "https://x.y/*z*/");
        assert_eq!(v["b"][0], 1);
    }

    #[test]
    fn wildcard_candidates() {
        let aliases = AliasMap {
            base_url: nodepath::resolve(&["proj"]),
            declares_base_url: false,
            patterns: vec![AliasPattern { prefix: "@/".into(), suffix: String::new(), has_wildcard: true, replacements: vec!["src/*".into()] }],
        };
        let root = nodepath::resolve(&["proj"]);
        assert_eq!(apply_aliases("@/a/b", &aliases, &root), vec!["src/a/b".to_string()]);
        assert!(apply_aliases("x/a", &aliases, &root).is_empty());
    }
}
