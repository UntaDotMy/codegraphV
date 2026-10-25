//! `codegraph upgrade` and the update notice, for the single native binary.
//!
//! Ports the version logic of src/upgrade/index.ts and the background check of
//! src/upgrade/update-check.ts. An upgrade re-runs the same installer script a first
//! install uses (install-rust.ps1 / install-rust.sh), pinned to the target release, so
//! download, checksum and PATH handling never drift between install and upgrade. The
//! network goes through `curl` (shipped with Windows 10+, macOS and Linux) rather than a TLS
//! stack in the binary.
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The GitHub repository releases come from.
pub const REPO: &str = "UntaDotMy/codegraphV";
/// The branch the installer scripts are served from.
pub const INSTALLER_BRANCH: &str = "rust-rewrite";

/// The installer script an upgrade re-runs.
pub fn installer_url(windows: bool) -> String {
    let file = if windows { "install-rust.ps1" } else { "install-rust.sh" };
    format!("https://raw.githubusercontent.com/{REPO}/{INSTALLER_BRANCH}/{file}")
}

// ---------------------------------------------------------------------------
// Versions (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Semver {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

/// `v1.2.3`, `1.2.3-rc.1`: the leading version of the trimmed string (not end-anchored, as
/// the TS regex `^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?`).
pub fn parse_semver(version: &str) -> Option<Semver> {
    fn number(rest: &mut &str) -> Option<u64> {
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let n = rest.get(..end).filter(|d| !d.is_empty())?.parse().ok()?;
        *rest = &rest[end..];
        Some(n)
    }
    let trimmed = version.trim();
    let mut rest = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let major = number(&mut rest)?;
    rest = rest.strip_prefix('.')?;
    let minor = number(&mut rest)?;
    rest = rest.strip_prefix('.')?;
    let patch = number(&mut rest)?;
    let pre = rest.strip_prefix('-').and_then(|p| {
        let end = p
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
            .unwrap_or(p.len());
        (end > 0).then(|| p[..end].to_string())
    });
    Some(Semver { major, minor, patch, pre })
}

/// `a` against `b`; None when either is not a version.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let (sa, sb) = (parse_semver(a)?, parse_semver(b)?);
    let core = (sa.major, sa.minor, sa.patch).cmp(&(sb.major, sb.minor, sb.patch));
    if core != Ordering::Equal {
        return Some(core);
    }
    // A prerelease is "less than" its release (1.0.0-rc < 1.0.0).
    Some(match (&sa.pre, &sb.pre) {
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
        (None, None) => Ordering::Equal,
    })
}

/// Whether `latest` is newer than `current`; differing unparseable strings count as an update
/// so a dev build is never stuck.
pub fn is_update_available(current: &str, latest: &str) -> bool {
    match compare_versions(latest, current) {
        Some(order) => order == Ordering::Greater,
        None => normalize_version(current) != normalize_version(latest),
    }
}

/// `0.9.9` / `v0.9.9` → `v0.9.9` (release tags are v-prefixed).
pub fn normalize_version(v: &str) -> String {
    let t = v.trim();
    if t.starts_with('v') {
        t.to_string()
    } else {
        format!("v{t}")
    }
}

/// `v0.9.9` → `0.9.9`.
pub fn strip_v(v: &str) -> String {
    let t = v.trim();
    t.strip_prefix('v').unwrap_or(t).to_string()
}

/// The release tag in the `Location` GitHub answers `/releases/latest` with.
pub fn parse_latest_tag_from_location(location: &str) -> Option<String> {
    let at = location.find("/releases/tag/")? + "/releases/tag/".len();
    let tail = &location[at..];
    let end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
    let tag = percent_decode(tail[..end].trim());
    (!tag.is_empty()).then_some(tag)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%')
            .then(|| s.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A canonical `vX.Y.Z[-pre]` rebuilt from the parsed fields, or None. Only this may reach
/// agent-visible text: the raw value comes from the network through an on-disk cache.
pub fn canonical_version_tag(v: &str) -> Option<String> {
    let s = parse_semver(v)?;
    Some(match s.pre {
        Some(pre) => format!("v{}.{}.{}-{pre}", s.major, s.minor, s.patch),
        None => format!("v{}.{}.{}", s.major, s.minor, s.patch),
    })
}

/// The re-index reminder shown after an upgrade.
pub fn reindex_advisory() -> String {
    [
        "Your existing project indexes keep working, but were built by the previous version.",
        "To pick up this version's extraction improvements, refresh each project:",
        "  codegraphV sync        # incremental, fast",
        "  codegraphV index -f    # full rebuild",
    ]
    .join("\n")
}

// ---------------------------------------------------------------------------
// Latest release (network, through curl)
// ---------------------------------------------------------------------------

fn curl(args: &[&str]) -> Option<String> {
    let out = Command::new("curl")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The latest release tag. Primary: the `Location` of `github.com/<repo>/releases/latest` (no
/// rate limit); fallback: the API's `tag_name` (60 requests an hour per IP unauthenticated).
pub fn resolve_latest_version(timeout_secs: u64) -> Result<String, String> {
    let max_time = timeout_secs.to_string();
    let page = format!("https://github.com/{REPO}/releases/latest");
    if let Some(headers) = curl(&["-sS", "-I", "--max-time", &max_time, "-A", "codegraph-upgrade", &page]) {
        for line in headers.lines() {
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("location") {
                    if let Some(tag) = parse_latest_tag_from_location(value.trim()) {
                        return Ok(normalize_version(&tag));
                    }
                }
            }
        }
    }
    let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
    if let Some(body) = curl(&[
        "-sS",
        "--max-time",
        &max_time,
        "-A",
        "codegraph-upgrade",
        "-H",
        "Accept: application/vnd.github+json",
        &api,
    ]) {
        if let Some(tag) = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("tag_name").and_then(|t| t.as_str()).map(String::from))
            .filter(|t| !t.is_empty())
        {
            return Ok(normalize_version(&tag));
        }
    }
    Err("could not resolve the latest version from GitHub. Check your network (curl is needed), or pin a version: `codegraphV upgrade <version>`.".into())
}

// ---------------------------------------------------------------------------
// Update notice (background check, on-disk cache)
// ---------------------------------------------------------------------------

/// Re-check the release feed after this long (successful checks).
pub const UPDATE_CHECK_TTL_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
/// Back off this long after a failed check (offline, rate-limited).
pub const UPDATE_CHECK_FAILURE_BACKOFF_MS: f64 = 60.0 * 60.0 * 1000.0;
const UPDATE_CHECK_NETWORK_TIMEOUT_SECS: u64 = 5;
const NOTICE_MEMO_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckCache {
    pub last_attempt_at: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
}

fn env_truthy(raw: Option<String>) -> bool {
    raw.is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// The check is off entirely — no network call, no notice — under CODEGRAPH_NO_UPDATE_CHECK
/// or DO_NOT_TRACK.
pub fn update_check_disabled(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env_truthy(env("CODEGRAPHV_NO_UPDATE_CHECK")) || env_truthy(env("DO_NOT_TRACK"))
}

/// `~/.codegraph`, where the cache lives.
pub fn default_state_dir() -> Option<PathBuf> {
    codegraph_types::paths::home_dir().map(|h| h.join(".codegraphV"))
}

/// Not the TS `update-check.json`: that one names npm versions (v1.6.x), and a machine switched
/// from the npm codegraph still has it.
pub fn update_check_cache_path(dir: &Path) -> PathBuf {
    dir.join("update-check-rust.json")
}

pub fn read_update_check_cache(dir: &Path) -> Option<UpdateCheckCache> {
    let raw = std::fs::read_to_string(update_check_cache_path(dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_update_check_cache(dir: &Path, cache: &UpdateCheckCache) {
    // Fail silent: a read-only home must not break the server.
    let _ = std::fs::create_dir_all(dir);
    if let Ok(text) = serde_json::to_string(cache) {
        let _ = std::fs::write(update_check_cache_path(dir), text);
    }
}

/// The one sentence every surface (stderr, initialize, status) shows.
pub fn format_update_notice(current: &str, latest: &str) -> String {
    format!("CodeGraphV {latest} is available (this server is running {current}). Update with `codegraphV upgrade`.")
}

fn notice_from(cache: Option<&UpdateCheckCache>, current: &str) -> Option<String> {
    let latest = canonical_version_tag(cache?.latest.as_deref()?)?;
    let current = canonical_version_tag(current)?;
    is_update_available(&current, &latest).then(|| format_update_notice(&current, &latest))
}

fn cache_is_fresh(cache: Option<&UpdateCheckCache>, now_ms: f64) -> bool {
    let Some(cache) = cache else { return false };
    if cache.last_success_at.is_some_and(|t| now_ms - t < UPDATE_CHECK_TTL_MS) {
        return true;
    }
    now_ms - cache.last_attempt_at < UPDATE_CHECK_FAILURE_BACKOFF_MS
}

/// Bring the cache up to date (the network only past the TTL / backoff) and return the
/// notice. A failed check starts the backoff but keeps the last known release.
pub fn refresh_update_check(
    dir: &Path,
    now_ms: f64,
    current: &str,
    resolve_latest: impl FnOnce() -> Result<String, String>,
) -> Option<String> {
    let cached = read_update_check_cache(dir);
    if cache_is_fresh(cached.as_ref(), now_ms) {
        return notice_from(cached.as_ref(), current);
    }
    let next = match resolve_latest().ok().and_then(|tag| canonical_version_tag(&tag)) {
        Some(latest) => UpdateCheckCache {
            last_attempt_at: now_ms,
            last_success_at: Some(now_ms),
            latest: Some(latest),
        },
        None => UpdateCheckCache {
            last_attempt_at: now_ms,
            last_success_at: cached.as_ref().and_then(|c| c.last_success_at),
            latest: cached.and_then(|c| c.latest),
        },
    };
    write_update_check_cache(dir, &next);
    notice_from(Some(&next), current)
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

static NOTICE_MEMO: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);
static REFRESHING: AtomicBool = AtomicBool::new(false);

fn spawn_refresh(dir: PathBuf, current: String, then: impl FnOnce(Option<String>) + Send + 'static) {
    if REFRESHING.swap(true, AtomicOrdering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("codegraph-update-check".into()).spawn(move || {
        let notice = refresh_update_check(&dir, now_ms(), &current, || {
            resolve_latest_version(UPDATE_CHECK_NETWORK_TIMEOUT_SECS)
        });
        if let Ok(mut memo) = NOTICE_MEMO.lock() {
            *memo = Some((Instant::now(), notice.clone()));
        }
        REFRESHING.store(false, AtomicOrdering::SeqCst);
        then(notice);
    });
}

/// The current notice, from the cache: cheap and never blocking (memoized for a minute), so
/// it is safe on the initialize path. A stale cache kicks a background refresh for the next
/// reader.
pub fn get_update_notice(current: &str) -> Option<String> {
    if update_check_disabled(&process_env) {
        return None;
    }
    if let Ok(memo) = NOTICE_MEMO.lock() {
        if let Some((at, value)) = memo.as_ref() {
            if at.elapsed() < NOTICE_MEMO_TTL {
                return value.clone();
            }
        }
    }
    let dir = default_state_dir()?;
    let cached = read_update_check_cache(&dir);
    if !cache_is_fresh(cached.as_ref(), now_ms()) {
        spawn_refresh(dir, current.to_string(), |_| {});
    }
    let value = notice_from(cached.as_ref(), current);
    if let Ok(mut memo) = NOTICE_MEMO.lock() {
        *memo = Some((Instant::now(), value.clone()));
    }
    value
}

/// Server start-up: refresh in the background and, when an update exists, print one stderr
/// line (never stdout — that is the MCP channel). Never blocks, never fails.
pub fn check_for_update_in_background(current: &str) {
    if update_check_disabled(&process_env) {
        return;
    }
    let Some(dir) = default_state_dir() else { return };
    spawn_refresh(dir, current.to_string(), |notice| {
        if let Some(notice) = notice {
            eprintln!("[CodeGraphV] {}", notice.trim_start_matches("CodeGraphV "));
        }
    });
}

/// The notice appended to the MCP initialize instructions.
pub fn initialize_instructions(base: &str, notice: Option<&str>) -> String {
    match notice {
        None => base.to_string(),
        Some(notice) => format!(
            "{base}\n\n---\n{notice} This server keeps running the old version until the user upgrades — mention it when convenient; do not run the upgrade yourself."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parse_compare_and_normalize() {
        assert_eq!(
            parse_semver(" v1.2.3-rc.1 trailing"),
            Some(Semver { major: 1, minor: 2, patch: 3, pre: Some("rc.1".into()) })
        );
        assert_eq!(parse_semver("0.0.1"), Some(Semver { major: 0, minor: 0, patch: 1, pre: None }));
        assert_eq!(parse_semver("1.2"), None);
        assert_eq!(parse_semver("dev"), None);
        assert_eq!(compare_versions("v0.0.2", "0.0.1"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1.0.0-rc", "1.0.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("1.10.0", "1.9.9"), Some(Ordering::Greater));
        assert!(is_update_available("0.0.1", "v0.0.2"));
        assert!(!is_update_available("0.0.2", "v0.0.2"));
        assert!(!is_update_available("0.1.0", "v0.0.9"));
        assert!(is_update_available("dev", "weird"));
        assert_eq!(normalize_version(" 0.9.9"), "v0.9.9");
        assert_eq!(strip_v("v0.9.9"), "0.9.9");
        assert_eq!(canonical_version_tag("1.2.3-x <inject>").as_deref(), Some("v1.2.3-x"));
        assert_eq!(canonical_version_tag("nope"), None);
    }

    #[test]
    fn latest_tag_from_redirect_location() {
        assert_eq!(
            parse_latest_tag_from_location("https://github.com/UntaDotMy/codegraphV/releases/tag/v0.0.2").as_deref(),
            Some("v0.0.2")
        );
        assert_eq!(parse_latest_tag_from_location("https://x/releases/tag/v1.0.0?x=1").as_deref(), Some("v1.0.0"));
        assert_eq!(parse_latest_tag_from_location("https://x/releases/tag/v%31.0.0").as_deref(), Some("v1.0.0"));
        assert_eq!(parse_latest_tag_from_location("https://github.com/x/releases"), None);
    }

    #[test]
    fn update_check_ttl_backoff_and_notice() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let t0 = 1_000_000_000_000.0;
        // First check: network says v0.0.2 → notice.
        let n = refresh_update_check(d, t0, "0.0.1", || Ok("v0.0.2".into()));
        assert_eq!(n.as_deref(), Some(format_update_notice("v0.0.1", "v0.0.2").as_str()));
        // Within the TTL the network is not asked.
        let n = refresh_update_check(d, t0 + 1000.0, "0.0.1", || panic!("network called inside TTL"));
        assert!(n.is_some());
        // Past the TTL a failure keeps the known release and starts the backoff.
        let later = t0 + UPDATE_CHECK_TTL_MS + 1.0;
        assert!(refresh_update_check(d, later, "0.0.1", || Err("offline".into())).is_some());
        let cache = read_update_check_cache(d).unwrap();
        assert_eq!(cache.latest.as_deref(), Some("v0.0.2"));
        assert_eq!(cache.last_attempt_at, later);
        assert!(refresh_update_check(d, later + 1000.0, "0.0.1", || panic!("network called in backoff")).is_some());
        // Up to date: no notice. A non-version answer is a failure, never text.
        assert!(refresh_update_check(d, later + UPDATE_CHECK_TTL_MS * 2.0, "0.0.2", || Ok("v0.0.2".into())).is_none());
        let d2 = tempfile::tempdir().unwrap();
        assert!(refresh_update_check(d2.path(), t0, "0.0.1", || Ok("<script>".into())).is_none());
    }

    #[test]
    fn off_switches_and_instructions() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
        };
        assert!(update_check_disabled(&env(&[("CODEGRAPHV_NO_UPDATE_CHECK", "1")])));
        assert!(update_check_disabled(&env(&[("DO_NOT_TRACK", "true")])));
        assert!(!update_check_disabled(&env(&[("DO_NOT_TRACK", "0")])));
        assert!(!update_check_disabled(&env(&[])));
        assert_eq!(initialize_instructions("base", None), "base");
        assert!(initialize_instructions("base", Some("N.")).starts_with("base\n\n---\nN. This server keeps running"));
    }
}
