//! Clock-driven port of the watcher scheduler. OS delivery and indexing run
//! separately, so events arriving during a sync remain visible as pending.
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub const EXHAUSTION_REASON: &str = "OS watch/file limit exhausted; auto-sync disabled. Run `codegraphV sync` (or install git sync hooks) to refresh the graph after changes.";
pub const INOTIFY_LIMIT_REASON: &str = "Linux inotify watch limit reached (fs.inotify.max_user_watches); live watching now covers only part of the project, so edits in unwatched directories will not auto-sync. Raise the limit (e.g. `sudo sysctl fs.inotify.max_user_watches=1048576`, persisted in /etc/sysctl.d) and restart, or run `codegraphV sync` (or install git sync hooks) to refresh.";
const LOCK_REASON: &str = "CodeGraphV file lock held by another process past the retry budget; auto-sync disabled. Run `codegraphV sync` once the other writer finishes (or install git sync hooks) to refresh the graph.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingFile {
    pub path: String,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub indexing: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchSyncResult {
    pub files_changed: usize,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum SyncError {
    LockUnavailable(String),
    Other(String),
}
impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LockUnavailable(s) | Self::Other(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for SyncError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRequest {
    pub id: u64,
    pub epoch: u64,
    pub started_ms: i64,
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchSnapshot {
    pub active: bool,
    pub ready: bool,
    pub syncing: bool,
    pub recovering: bool,
    pub degraded_reason: Option<String>,
    pub pending_files: Vec<PendingFile>,
    pub watched_directories: usize,
    pub partial_coverage: bool,
    pub completed_syncs: u64,
    pub revision: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Effects {
    pub completed: Option<WatchSyncResult>,
    pub error: Option<String>,
    pub degraded: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WatchState {
    pub snapshot: WatchSnapshot,
    pub deadline_ms: Option<i64>,
    pub needs_full_scan: bool,
    pub epoch: u64,
    debounce_ms: u64,
    pending: IndexMap<String, (i64, i64)>,
    pub current: Option<SyncRequest>,
    lock_retries: u32,
    failure_retries: u32,
    degraded_by_lock: bool,
    last_rearm_ms: i64,
    next_id: u64,
}

impl WatchState {
    pub fn new(debounce_ms: u64) -> Self {
        Self {
            snapshot: WatchSnapshot::default(),
            deadline_ms: None,
            needs_full_scan: false,
            epoch: 0,
            debounce_ms,
            pending: IndexMap::new(),
            current: None,
            lock_retries: 0,
            failure_retries: 0,
            degraded_by_lock: false,
            last_rearm_ms: 0,
            next_id: 0,
        }
    }
    pub fn start(&mut self) {
        self.epoch += 1;
        self.snapshot.active = true;
        self.snapshot.ready = true;
        self.snapshot.degraded_reason = None;
        self.snapshot.recovering = false;
        self.degraded_by_lock = false;
        self.lock_retries = 0;
        self.failure_retries = 0;
        self.needs_full_scan = false;
        self.pending.clear();
        self.deadline_ms = None;
        self.update();
    }
    pub fn stop(&mut self) {
        self.epoch += 1;
        self.snapshot.active = false;
        self.snapshot.ready = false;
        self.snapshot.recovering = false;
        self.snapshot.watched_directories = 0;
        self.snapshot.partial_coverage = false;
        self.pending.clear();
        self.deadline_ms = None;
        self.lock_retries = 0;
        self.failure_retries = 0;
        self.update();
    }
    pub fn record_file(&mut self, path: String, now: i64) {
        if !self.snapshot.active {
            return;
        }
        self.pending
            .entry(path)
            .and_modify(|p| p.1 = now)
            .or_insert((now, now));
        self.schedule(now);
        self.update();
    }
    pub fn request_full(&mut self, now: i64) {
        if self.snapshot.active {
            self.needs_full_scan = true;
            self.schedule(now);
        }
    }
    pub fn schedule(&mut self, now: i64) {
        let delay = if self.pending.len() <= 2 {
            self.debounce_ms.min(300).max(100)
        } else {
            self.debounce_ms
        };
        self.deadline_ms = Some(now.saturating_add(delay.min(i64::MAX as u64) as i64));
    }
    pub fn begin_sync(&mut self, now: i64) -> Option<SyncRequest> {
        self.deadline_ms = None;
        if !self.snapshot.active || self.current.is_some() {
            return None;
        }
        let paths =
            if !self.needs_full_scan && !self.pending.is_empty() && self.pending.len() <= 500 {
                Some(self.pending.keys().cloned().collect())
            } else {
                None
            };
        if paths.is_none() {
            self.needs_full_scan = false;
        }
        self.next_id += 1;
        let request = SyncRequest {
            id: self.next_id,
            epoch: self.epoch,
            started_ms: now,
            paths,
        };
        self.current = Some(request.clone());
        self.update();
        Some(request)
    }
    pub fn complete(
        &mut self,
        request: &SyncRequest,
        result: Result<WatchSyncResult, SyncError>,
        now: i64,
    ) -> Effects {
        let mut effects = Effects::default();
        if self.current.as_ref().map(|r| r.id) != Some(request.id) {
            return effects;
        }
        self.current = None;
        self.snapshot.revision += 1;
        if request.epoch != self.epoch {
            if self.snapshot.active && (!self.pending.is_empty() || self.needs_full_scan) {
                self.schedule(now);
            }
            self.update();
            return effects;
        }
        match result {
            Ok(result) => {
                if self.snapshot.recovering && request.paths.is_none() && !self.needs_full_scan {
                    self.snapshot.degraded_reason = None;
                    self.snapshot.recovering = false;
                }
                self.lock_retries = 0;
                self.failure_retries = 0;
                self.pending.retain(|_, info| info.1 > request.started_ms);
                self.snapshot.completed_syncs += 1;
                effects.completed = Some(result);
            }
            Err(error) => {
                if request.paths.is_none() {
                    self.needs_full_scan = true;
                }
                match error {
                    SyncError::LockUnavailable(_) => {
                        self.lock_retries += 1;
                        if self.lock_retries > 5 {
                            effects.degraded = self.degrade(LOCK_REASON.into(), true);
                        }
                    }
                    SyncError::Other(message) => {
                        self.lock_retries = 0;
                        self.failure_retries += 1;
                        effects.error = Some(message.clone());
                        if self.failure_retries > 5 {
                            effects.degraded=self.degrade(format!("CodeGraphV auto-sync failed {} times in a row; auto-sync disabled. Run `codegraphV sync` (or install git sync hooks) to refresh the graph after changes. Last error: {message}",self.failure_retries),false);
                        }
                    }
                }
            }
        }
        if self.snapshot.active && (!self.pending.is_empty() || self.needs_full_scan) {
            let retry = self.lock_retries.max(self.failure_retries);
            if retry > 0 {
                self.deadline_ms = Some(
                    now.saturating_add(
                        self.debounce_ms
                            .saturating_mul(1u64 << retry.saturating_sub(1).min(31))
                            .min(30_000) as i64,
                    ),
                );
            } else {
                self.schedule(now);
            }
        }
        self.update();
        effects
    }
    pub fn degrade(&mut self, reason: String, by_lock: bool) -> Option<String> {
        if self.snapshot.degraded_reason.is_some() && !self.snapshot.recovering {
            return None;
        }
        self.snapshot.degraded_reason = Some(reason.clone());
        self.degraded_by_lock = by_lock;
        self.stop();
        Some(reason)
    }
    pub fn can_rearm(&self, now: i64) -> bool {
        self.degraded_by_lock
            && !self.snapshot.active
            && (self.last_rearm_ms == 0 || now - self.last_rearm_ms >= 30_000)
    }
    pub fn rearm(&mut self, now: i64) -> bool {
        if !self.can_rearm(now) {
            return false;
        }
        let reason = self.snapshot.degraded_reason.clone();
        self.last_rearm_ms = now;
        self.start();
        self.snapshot.degraded_reason = reason;
        self.snapshot.recovering = true;
        self.request_full(now);
        true
    }
    pub fn rearm_failed(&mut self, reason: Option<String>) {
        if self.snapshot.degraded_reason.is_none() {
            self.snapshot.degraded_reason = reason;
            self.degraded_by_lock = true;
        }
    }
    fn update(&mut self) {
        self.snapshot.syncing = self.current.is_some();
        self.snapshot.pending_files = self
            .pending
            .iter()
            .map(|(path, (first, last))| PendingFile {
                path: path.clone(),
                first_seen_ms: *first,
                last_seen_ms: *last,
                indexing: self.current.as_ref().is_some_and(|r| r.started_ms >= *last),
            })
            .collect();
    }
}
