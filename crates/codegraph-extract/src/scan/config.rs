//! Project-scoped configuration — port of `src/project-config.ts`: a committed
//! `codegraph.json` at the project root.
//!
//! Absent or malformed config is the zero-config default — no overrides, no
//! error. Invalid individual entries are warned about and skipped; an
//! unparseable project file must never break indexing.

use crate::languages::is_language_supported;
use codegraph_types::jscompat::js_trim;
use codegraph_types::log::log_warn;
use codegraph_types::Language;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// Filename of the project-scoped config, resolved relative to the project root.
pub const PROJECT_CONFIG_FILENAME: &str = "codegraph.json";

/// Parsed, validated view of a project's `codegraph.json`.
#[derive(Debug, Default, PartialEq)]
pub struct ParsedConfig {
    /// Custom file extension (`.foo`, lower-cased) → supported language.
    pub extensions: HashMap<String, Language>,
    /// Gitignored directories whose embedded git repos are indexed anyway (#622, #699).
    pub include_ignored: Vec<String>,
    /// Paths kept out of the index even when git-tracked (#999).
    pub exclude: Vec<String>,
    /// Indexed paths that must not outrank first-party code in search (#982).
    pub deprioritize: Vec<String>,
    /// First-party source forced into the index despite `.gitignore`.
    pub include: Vec<String>,
}

static CACHE: OnceLock<Mutex<HashMap<String, (SystemTime, Arc<ParsedConfig>)>>> = OnceLock::new();

fn warn(message: &str, file: &Path) {
    log_warn(message, &[("file", file.display().to_string())]);
}

/// A user-provided extension key in the `.ext` lower-case form, or none when it
/// can never match a real file extension.
fn normalize_ext_key(raw: &str) -> Option<String> {
    let mut ext = js_trim(raw).to_lowercase();
    if ext.is_empty() {
        return None;
    }
    if !ext.starts_with('.') {
        ext.insert(0, '.');
    }
    let body = &ext[1..];
    if body.is_empty() || body.contains('.') || body.contains('/') || body.contains('\\') {
        return None;
    }
    Some(ext)
}

fn extract_extensions(parsed: &Value, file: &Path) -> HashMap<String, Language> {
    let Some(Value::Object(exts)) = parsed.get("extensions") else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (raw_key, raw_val) in exts {
        let Some(key) = normalize_ext_key(raw_key) else {
            warn(&format!("Ignoring extension mapping in {PROJECT_CONFIG_FILENAME}: \"{raw_key}\" is not a valid file extension"), file);
            continue;
        };
        match raw_val.as_str().and_then(Language::parse).filter(|l| is_language_supported(*l)) {
            Some(language) => {
                out.insert(key, language);
            }
            None => {
                let shown = match raw_val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                warn(&format!("Ignoring extension \"{raw_key}\" in {PROJECT_CONFIG_FILENAME}: \"{shown}\" is not a supported language"), file);
            }
        }
    }
    out
}

/// A gitignore-style pattern list under `key`: a non-array value or a
/// non-string/blank entry warns and is skipped; entries are kept trimmed.
fn extract_pattern_list(parsed: &Value, file: &Path, key: &str) -> Vec<String> {
    let Some(raw) = parsed.get(key) else {
        return Vec::new();
    };
    let Value::Array(entries) = raw else {
        warn(&format!("Ignoring \"{key}\" in {PROJECT_CONFIG_FILENAME}: must be an array of gitignore-style patterns"), file);
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        match entry.as_str().map(js_trim).filter(|s| !s.is_empty()) {
            Some(pattern) => out.push(pattern.to_string()),
            None => warn(&format!("Ignoring a \"{key}\" entry in {PROJECT_CONFIG_FILENAME}: every pattern must be a non-empty string"), file),
        }
    }
    out
}

/// Read + parse a `codegraph.json` once. Every failure mode degrades to the
/// zero-config default.
fn parse_config(file: &Path) -> ParsedConfig {
    let Ok(raw) = std::fs::read(file).map(|b| String::from_utf8_lossy(&b).into_owned()) else {
        return ParsedConfig::default();
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            log_warn(
                &format!("Ignoring {PROJECT_CONFIG_FILENAME}: not valid JSON"),
                &[("file", file.display().to_string()), ("error", err.to_string())],
            );
            return ParsedConfig::default();
        }
    };
    if !matches!(parsed, Value::Object(_) | Value::Array(_)) {
        return ParsedConfig::default();
    }
    ParsedConfig {
        extensions: extract_extensions(&parsed, file),
        include_ignored: extract_pattern_list(&parsed, file, "includeIgnored"),
        exclude: extract_pattern_list(&parsed, file, "exclude"),
        include: extract_pattern_list(&parsed, file, "include"),
        deprioritize: extract_pattern_list(&parsed, file, "deprioritize"),
    }
}

/// The parsed `codegraph.json` for a project, mtime-cached: one `stat` (and at
/// most one read) while a single config file is in force. A missing or
/// malformed file yields the zero-config default.
pub fn load_parsed_config(root_dir: &Path) -> Arc<ParsedConfig> {
    static EMPTY: OnceLock<Arc<ParsedConfig>> = OnceLock::new();
    let empty = || Arc::clone(EMPTY.get_or_init(Default::default));
    let file = root_dir.join(PROJECT_CONFIG_FILENAME);
    let key = root_dir.to_string_lossy().into_owned();
    let cache = CACHE.get_or_init(Default::default);
    let mtime = match std::fs::metadata(&file).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => {
            cache.lock().unwrap().remove(&key);
            return empty();
        }
    };
    if let Some((cached_at, config)) = cache.lock().unwrap().get(&key) {
        if *cached_at == mtime {
            return Arc::clone(config);
        }
    }
    let config = Arc::new(parse_config(&file));
    cache.lock().unwrap().insert(key, (mtime, Arc::clone(&config)));
    config
}

/// The validated extension overrides (`.ext` → language).
pub fn load_extension_overrides(root_dir: &Path) -> HashMap<String, Language> {
    load_parsed_config(root_dir).extensions.clone()
}

/// `includeIgnored` patterns; empty means `.gitignore` is fully respected.
pub fn load_include_ignored_patterns(root_dir: &Path) -> Vec<String> {
    load_parsed_config(root_dir).include_ignored.clone()
}

/// `exclude` patterns: paths kept out of the index even when git-tracked.
pub fn load_exclude_patterns(root_dir: &Path) -> Vec<String> {
    load_parsed_config(root_dir).exclude.clone()
}

/// `deprioritize` patterns: indexed paths that must not outrank first-party code.
pub fn load_deprioritize_patterns(root_dir: &Path) -> Vec<String> {
    load_parsed_config(root_dir).deprioritize.clone()
}

/// `include` patterns: first-party source forced into the index.
pub fn load_include_patterns(root_dir: &Path) -> Vec<String> {
    load_parsed_config(root_dir).include.clone()
}

/// Forget cached config (after rewriting it in a test).
pub fn clear_project_config_cache() {
    CACHE.get_or_init(Default::default).lock().unwrap().clear();
}

/// Add patterns to a project's `includeIgnored` list, creating the file if
/// absent and preserving every other key. Returns the number of patterns
/// actually added (ones already present are skipped). Errs when an existing
/// config will not parse, rather than clobbering it.
pub fn add_include_ignored_patterns(root_dir: &Path, patterns: &[String]) -> Result<usize, String> {
    let file = root_dir.join(PROJECT_CONFIG_FILENAME);
    let mut config = serde_json::Map::new();
    if let Ok(bytes) = std::fs::read(&file) {
        let raw = String::from_utf8_lossy(&bytes);
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|_| format!("{PROJECT_CONFIG_FILENAME} is not valid JSON — fix it by hand, then re-run."))?;
        if let Value::Object(map) = parsed {
            config = map;
        }
    }
    let mut merged: Vec<String> = match config.get("includeIgnored") {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    };
    let mut seen: std::collections::HashSet<String> = merged.iter().cloned().collect();
    let mut added = 0;
    for p in patterns {
        if seen.insert(p.clone()) {
            merged.push(p.clone());
            added += 1;
        }
    }
    config.insert("includeIgnored".to_string(), Value::Array(merged.into_iter().map(Value::String).collect()));
    let mut text = serde_json::to_string_pretty(&Value::Object(config)).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(&file, text).map_err(|e| e.to_string())?;
    clear_project_config_cache();
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cg-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extension_keys_normalize() {
        assert_eq!(normalize_ext_key(" .FOO ").as_deref(), Some(".foo"));
        assert_eq!(normalize_ext_key("tpl").as_deref(), Some(".tpl"));
        assert_eq!(normalize_ext_key(".d.ts"), None);
        assert_eq!(normalize_ext_key("."), None);
        assert_eq!(normalize_ext_key(""), None);
        assert_eq!(normalize_ext_key("a/b"), None);
    }

    #[test]
    fn config_is_validated_not_trusted() {
        let root = temp_root("validate");
        std::fs::write(
            root.join(PROJECT_CONFIG_FILENAME),
            r#"{"extensions":{".dota_lua":"lua",".x":"klingon","a.b":"php"},"exclude":["static/"," ",3],"include":"nope","includeIgnored":["repos/"]}"#,
        )
        .unwrap();
        let cfg = load_parsed_config(&root);
        assert_eq!(cfg.extensions.len(), 1);
        assert_eq!(cfg.extensions[".dota_lua"].as_str(), "lua");
        assert_eq!(cfg.exclude, vec!["static/".to_string()]);
        assert!(cfg.include.is_empty());
        assert_eq!(cfg.include_ignored, vec!["repos/".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_broken_config_is_the_default() {
        let root = temp_root("broken");
        assert_eq!(*load_parsed_config(&root), ParsedConfig::default());
        std::fs::write(root.join(PROJECT_CONFIG_FILENAME), "{ nope").unwrap();
        assert_eq!(*load_parsed_config(&root), ParsedConfig::default());
        std::fs::write(root.join(PROJECT_CONFIG_FILENAME), "[1,2]").unwrap();
        clear_project_config_cache();
        assert_eq!(*load_parsed_config(&root), ParsedConfig::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn include_ignored_round_trip_is_idempotent() {
        let root = temp_root("add");
        assert_eq!(add_include_ignored_patterns(&root, &["a/".into(), "b/".into()]).unwrap(), 2);
        assert_eq!(add_include_ignored_patterns(&root, &["b/".into(), "c/".into()]).unwrap(), 1);
        assert_eq!(load_include_ignored_patterns(&root), vec!["a/", "b/", "c/"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
