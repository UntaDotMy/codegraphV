use super::state::*;
use crate::watch_policy::{watch_disabled_reason, WatchProbe};
use codegraph_extract::{
    languages::is_source_file,
    scan::config::load_extension_overrides,
    scan::{build_scope_ignore, ScopeIgnore},
};
use codegraph_types::{
    log::log_warn,
    nodepath,
    paths::{is_codegraph_data_dir, is_within_dir, real_path_preserving_spelling},
};
use indexmap::IndexMap;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::HashSet,
    io,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

pub type SyncCallback = Box<dyn FnMut(Option<Vec<String>>) -> Result<WatchSyncResult, SyncError>>;
pub type MetadataGuard = Box<dyn FnMut(&str) -> bool>;
type SyncFactory = Box<dyn FnOnce() -> SyncCallback + Send>;
type GuardFactory = Box<dyn FnOnce() -> MetadataGuard + Send>;

#[derive(Clone)]
pub struct WatchOptions {
    pub debounce_ms: u64,
    pub on_sync_complete: Option<Arc<dyn Fn(WatchSyncResult) + Send + Sync>>,
    pub on_sync_error: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_degraded: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// No OS handles; deterministic callers supply events through `ingest_event`.
    pub inert: bool,
    /// Defaults to CODEGRAPH_MAX_DIR_WATCHES or 50,000. Supplemental recursive streams cap at 256.
    pub max_dir_watches: Option<usize>,
}
impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            debounce_ms: 2000,
            on_sync_complete: None,
            on_sync_error: None,
            on_degraded: None,
            inert: false,
            max_dir_watches: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum WatchFailure {
    ResourceExhausted,
    InotifyLimit,
    Other(String),
}
fn classify(error: notify::Error) -> WatchFailure {
    match error.kind {
        notify::ErrorKind::MaxFilesWatch => WatchFailure::InotifyLimit,
        notify::ErrorKind::Io(ref e) => {
            let code = e.raw_os_error();
            let exhausted = if cfg!(windows) {
                code == Some(4)
            } else {
                matches!(code, Some(23 | 24))
            };
            let fallback = code.is_none()
                && ["emfile", "enfile", "too many open files"]
                    .iter()
                    .any(|word| error.to_string().to_lowercase().contains(word));
            if exhausted || fallback {
                WatchFailure::ResourceExhausted
            } else if cfg!(target_os = "linux") && code == Some(28) {
                WatchFailure::InotifyLimit
            } else {
                WatchFailure::Other(error.to_string())
            }
        }
        _ => {
            let message = error.to_string();
            let lower = message.to_lowercase();
            if lower.contains("emfile")
                || lower.contains("enfile")
                || lower.contains("too many open files")
            {
                WatchFailure::ResourceExhausted
            } else {
                WatchFailure::Other(message)
            }
        }
    }
}

enum Message {
    Start(mpsc::Sender<bool>),
    Stop(mpsc::Sender<()>),
    Rearm(mpsc::Sender<bool>),
    Event(String, mpsc::Sender<()>),
    Failure(WatchFailure, mpsc::Sender<()>),
    Native(u64, notify::Result<notify::Event>),
    Finished(SyncRequest, Result<WatchSyncResult, SyncError>),
    Flush(mpsc::Sender<bool>),
    Shutdown,
}
struct Shared {
    state: Mutex<WatchState>,
    worker_id: Mutex<Option<thread::ThreadId>>,
    changed: Condvar,
}
impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn notify(&self) {
        self.changed.notify_all();
    }
}

/// Owns all native handles and joins both threads on drop. Callbacks run on the
/// watcher threads; the engine is constructed on its indexing owner thread.
pub struct FileWatcher {
    tx: mpsc::Sender<Message>,
    shared: Arc<Shared>,
    join: Option<thread::JoinHandle<()>>,
    driver_id: thread::ThreadId,
}
impl FileWatcher {
    pub fn new<F>(root: &str, sync: F, options: WatchOptions) -> io::Result<Self>
    where
        F: FnMut(Option<Vec<String>>) -> Result<WatchSyncResult, SyncError> + Send + 'static,
    {
        Self::with_factories(root, Box::new(move || Box::new(sync)), None, options)
    }
    pub fn with_factories(
        root: &str,
        factory: SyncFactory,
        guard_factory: Option<GuardFactory>,
        options: WatchOptions,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(WatchState::new(options.debounce_ms)),
            worker_id: Mutex::new(None),
            changed: Condvar::new(),
        });
        let driver_tx = tx.clone();
        let driver_shared = shared.clone();
        let root = nodepath::resolve(&[root]);
        let join = thread::Builder::new()
            .name("codegraph-watch".into())
            .spawn(move || {
                let (sync_tx, sync_rx) = mpsc::channel::<Option<SyncRequest>>();
                let result_tx = driver_tx.clone();
                let worker_shared = driver_shared.clone();
                let worker = thread::Builder::new()
                    .name("codegraph-watch-sync".into())
                    .spawn(move || {
                        *worker_shared
                            .worker_id
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = Some(thread::current().id());
                        let callback = catch_unwind(AssertUnwindSafe(factory));
                        let mut callback = callback.ok();
                        while let Ok(Some(request)) = sync_rx.recv() {
                            let result = match callback.as_mut() {
                                Some(cb) => {
                                    catch_unwind(AssertUnwindSafe(|| cb(request.paths.clone())))
                                        .unwrap_or_else(|_| {
                                            Err(SyncError::Other(
                                                "Watch sync callback panicked".into(),
                                            ))
                                        })
                                }
                                None => Err(SyncError::Other("Watch sync factory panicked".into())),
                            };
                            if result_tx.send(Message::Finished(request, result)).is_err() {
                                break;
                            }
                        }
                    });
                let Ok(worker) = worker else {
                    driver_shared
                        .lock()
                        .degrade("Could not start watch sync thread".into(), false);
                    driver_shared.notify();
                    return;
                };
                let guard =
                    guard_factory.and_then(|factory| catch_unwind(AssertUnwindSafe(factory)).ok());
                let mut driver = Driver {
                    root,
                    options,
                    shared: driver_shared,
                    tx: driver_tx,
                    sync_tx,
                    backend: None,
                    watched: IndexMap::new(),
                    known: IndexMap::new(),
                    scope: None,
                    refresh_at: None,
                    limit_warned: false,
                    cap_warned: false,
                    guard,
                };
                loop {
                    let now = now_ms();
                    let deadline = driver.shared.lock().deadline_ms;
                    let next = [deadline, driver.refresh_at].into_iter().flatten().min();
                    let message = match next {
                        Some(at) => rx
                            .recv_timeout(Duration::from_millis(
                                at.saturating_sub(now).max(0) as u64
                            ))
                            .ok(),
                        None => rx.recv().ok(),
                    };
                    if let Some(message) = message {
                        if !driver.handle(message) {
                            break;
                        }
                    }
                    driver.tick();
                    driver.shared.notify();
                }
                driver.close_backend();
                let _ = driver.sync_tx.send(None);
                let _ = worker.join();
            })?;
        let driver_id = join.thread().id();
        Ok(Self {
            tx,
            shared,
            join: Some(join),
            driver_id,
        })
    }
    pub fn start(&self) -> bool {
        if thread::current().id() == self.driver_id {
            return self.is_active();
        }
        let (tx, rx) = mpsc::channel();
        self.tx.send(Message::Start(tx)).is_ok() && rx.recv().unwrap_or(false)
    }
    pub fn stop(&self) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Message::Stop(tx)).is_ok() && thread::current().id() != self.driver_id {
            let _ = rx.recv();
        }
    }
    pub fn rearm_after_lock_contention(&self) -> bool {
        if thread::current().id() == self.driver_id {
            return false;
        }
        let (tx, rx) = mpsc::channel();
        self.tx.send(Message::Rearm(tx)).is_ok() && rx.recv().unwrap_or(false)
    }
    pub fn ingest_event(&self, path: &str) {
        let (tx, rx) = mpsc::channel();
        if self
            .tx
            .send(Message::Event(path.replace('\\', "/"), tx))
            .is_ok()
            && thread::current().id() != self.driver_id
        {
            let _ = rx.recv();
        }
    }
    pub fn ingest_failure(&self, failure: WatchFailure) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Message::Failure(failure, tx)).is_ok()
            && thread::current().id() != self.driver_id
        {
            let _ = rx.recv();
        }
    }
    pub fn flush_now(&self) -> bool {
        if thread::current().id() == self.driver_id {
            return false;
        }
        let (tx, rx) = mpsc::channel();
        self.tx.send(Message::Flush(tx)).is_ok() && rx.recv().unwrap_or(false)
    }
    pub fn snapshot(&self) -> WatchSnapshot {
        self.shared.lock().snapshot.clone()
    }
    pub fn is_active(&self) -> bool {
        self.shared.lock().snapshot.active
    }
    pub fn revision(&self) -> u64 {
        self.shared.lock().snapshot.revision
    }
    pub fn is_syncing(&self) -> bool {
        self.shared.lock().snapshot.syncing
    }
    pub fn is_degraded(&self) -> bool {
        self.shared.lock().snapshot.degraded_reason.is_some()
    }
    pub fn degraded_reason(&self) -> Option<String> {
        self.shared.lock().snapshot.degraded_reason.clone()
    }
    pub fn is_recovering(&self) -> bool {
        self.shared.lock().snapshot.recovering
    }
    pub fn get_pending_files(&self) -> Vec<PendingFile> {
        self.shared.lock().snapshot.pending_files.clone()
    }
    pub fn wait_until_ready(&self, timeout: Duration) -> io::Result<()> {
        self.wait_for(
            timeout,
            |s| s.snapshot.ready,
            "FileWatcher.waitUntilReady timed out",
        )
    }
    pub fn wait_until_idle(&self, timeout: Duration) -> io::Result<()> {
        self.wait_for(
            timeout,
            |s| s.current.is_none() && s.deadline_ms.is_none(),
            "FileWatcher.waitUntilIdle timed out",
        )
    }
    fn wait_for(
        &self,
        timeout: Duration,
        predicate: impl Fn(&WatchState) -> bool,
        message: &str,
    ) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        while !predicate(&state) {
            if thread::current().id() == self.driver_id {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Cannot wait for watcher progress from its callback",
                ));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, message));
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(())
    }
}
impl Drop for FileWatcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Message::Shutdown);
        let worker_id = *self
            .shared
            .worker_id
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if thread::current().id() != self.driver_id && Some(thread::current().id()) != worker_id {
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
fn recursive() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
}
fn real(path: &Path) -> Option<String> {
    real_path_preserving_spelling(path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

struct Driver {
    root: String,
    options: WatchOptions,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Message>,
    sync_tx: mpsc::Sender<Option<SyncRequest>>,
    backend: Option<RecommendedWatcher>,
    watched: IndexMap<String, String>,
    known: IndexMap<String, String>,
    scope: Option<ScopeIgnore>,
    refresh_at: Option<i64>,
    limit_warned: bool,
    cap_warned: bool,
    guard: Option<MetadataGuard>,
}
impl Driver {
    fn start(&mut self) -> bool {
        if self.shared.lock().snapshot.active {
            return true;
        }
        self.shared.lock().start();
        self.shared.lock().snapshot.ready = false;
        if watch_disabled_reason(&self.root, &WatchProbe::default()).is_some() {
            self.shared.lock().stop();
            return false;
        }
        self.scope = build_scope_ignore(Path::new(&self.root), None).ok();
        if !self.options.inert {
            let tx = self.tx.clone();
            let epoch = self.shared.lock().epoch;
            match notify::recommended_watcher(move |event| {
                let _ = tx.send(Message::Native(epoch, event));
            }) {
                Ok(backend) => self.backend = Some(backend),
                Err(error) => {
                    self.failure(classify(error));
                    self.shared.lock().stop();
                    return false;
                }
            }
            if recursive()
                && !self.watch_directory(
                    self.root.clone(),
                    real(Path::new(&self.root)).unwrap_or_else(|| self.root.clone()),
                    true,
                )
            {
                self.close_backend();
                self.shared.lock().stop();
                return false;
            }
            self.refresh_tree();
        }
        {
            let mut state = self.shared.lock();
            state.snapshot.ready = state.snapshot.active;
        }
        self.shared.notify();
        self.shared.lock().snapshot.active
    }
    fn close_backend(&mut self) {
        self.backend = None;
        self.watched.clear();
        self.known.clear();
        self.refresh_at = None;
        self.limit_warned = false;
        self.cap_warned = false;
    }
    fn stop(&mut self) {
        self.shared.lock().stop();
        self.close_backend();
        self.scope = None;
        self.shared.notify();
    }
    fn effects(&mut self, effects: Effects) {
        if let Some(result) = effects.completed {
            if let Some(cb) = &self.options.on_sync_complete {
                let _ = catch_unwind(AssertUnwindSafe(|| cb(result)));
            }
        }
        if let Some(error) = effects.error {
            if let Some(cb) = &self.options.on_sync_error {
                let _ = catch_unwind(AssertUnwindSafe(|| cb(error)));
            }
        }
        if let Some(reason) = effects.degraded {
            self.close_backend();
            if let Some(cb) = &self.options.on_degraded {
                let _ = catch_unwind(AssertUnwindSafe(|| cb(reason)));
            }
        }
    }
    fn failure(&mut self, failure: WatchFailure) {
        match failure {
            WatchFailure::ResourceExhausted => {
                let degraded = self.shared.lock().degrade(EXHAUSTION_REASON.into(), false);
                self.effects(Effects {
                    degraded,
                    ..Default::default()
                });
            }
            WatchFailure::InotifyLimit => {
                if !self.limit_warned {
                    self.limit_warned = true;
                    self.shared.lock().snapshot.partial_coverage = true;
                    log_warn(INOTIFY_LIMIT_REASON, &[]);
                }
            }
            WatchFailure::Other(message) => log_warn("File watcher error", &[("error", message)]),
        }
    }
    fn handle(&mut self, message: Message) -> bool {
        match message {
            Message::Start(reply) => {
                let result = self.start();
                let _ = reply.send(result);
            }
            Message::Stop(reply) => {
                self.stop();
                let _ = reply.send(());
            }
            Message::Shutdown => {
                self.stop();
                return false;
            }
            Message::Rearm(reply) => {
                let now = now_ms();
                let allowed = self.shared.lock().can_rearm(now);
                let result = if allowed {
                    let reason = self.shared.lock().snapshot.degraded_reason.clone();
                    self.close_backend();
                    // Install watches while retaining the recovery state until a full sync commits.
                    let armed = self.shared.lock().rearm(now);
                    self.shared.lock().snapshot.active = false;
                    let started = self.start();
                    if started {
                        let mut state = self.shared.lock();
                        state.snapshot.degraded_reason = reason;
                        state.snapshot.recovering = true;
                        state.request_full(now);
                    } else {
                        self.shared.lock().rearm_failed(reason);
                    }
                    armed && started
                } else {
                    false
                };
                let _ = reply.send(result);
            }
            Message::Event(path, reply) => {
                self.change(&path);
                let _ = reply.send(());
            }
            Message::Failure(failure, reply) => {
                self.failure(failure);
                let _ = reply.send(());
            }
            Message::Flush(reply) => {
                let started = self.dispatch_sync();
                let _ = reply.send(started);
            }
            Message::Finished(request, result) => {
                let effects = self.shared.lock().complete(&request, result, now_ms());
                self.effects(effects);
            }
            Message::Native(epoch, result) => {
                let current = {
                    let state = self.shared.lock();
                    epoch == state.epoch && state.snapshot.active
                };
                if !current {
                    return true;
                }
                match result {
                    Ok(event) => {
                        if matches!(event.kind, notify::EventKind::Access(_)) {
                            return true;
                        }
                        if event.need_rescan() || event.paths.is_empty() {
                            self.schedule_refresh();
                            self.shared.lock().request_full(now_ms());
                        }
                        for path in event.paths {
                            self.native_path(&path);
                        }
                    }
                    Err(error) => self.failure(classify(error)),
                }
            }
        }
        true
    }
    fn dispatch_sync(&mut self) -> bool {
        let request = self.shared.lock().begin_sync(now_ms());
        if let Some(request) = request {
            if self.sync_tx.send(Some(request)).is_err() {
                self.failure(WatchFailure::Other("Watch sync thread stopped".into()));
            }
            true
        } else {
            false
        }
    }
    fn tick(&mut self) {
        let now = now_ms();
        if self.refresh_at.is_some_and(|t| t <= now) {
            self.refresh_at = None;
            self.refresh_tree();
            self.shared.lock().request_full(now);
        }
        let due = self.shared.lock().deadline_ms.is_some_and(|t| t <= now);
        if due {
            self.dispatch_sync();
        }
    }
    fn always_ignored(rel: &str) -> bool {
        rel.split('/')
            .any(|p| p == ".git" || is_codegraph_data_dir(p))
    }
    fn ignores_dir(&self, path: &str) -> bool {
        let rel = nodepath::relative(&self.root, path).replace('\\', "/");
        if rel.is_empty() || rel == "." || rel.starts_with("..") {
            return false;
        }
        Self::always_ignored(&rel)
            || self
                .scope
                .as_ref()
                .is_some_and(|s| s.ignores(&format!("{rel}/")).unwrap_or(false))
    }
    fn schedule_refresh(&mut self) {
        if !self.options.inert && self.shared.lock().snapshot.active && self.refresh_at.is_none() {
            self.refresh_at = Some(now_ms() + 50);
        }
    }
    fn change(&mut self, rel: &str) {
        if !self.shared.lock().snapshot.active
            || rel.is_empty()
            || rel == "."
            || rel.starts_with("..")
        {
            return;
        }
        if rel == ".git/info/exclude" {
            self.refresh_scope();
            return;
        }
        if Self::always_ignored(rel) {
            return;
        }
        if rel == "codegraph.json" || rel == ".gitignore" {
            self.refresh_scope();
            return;
        }
        if self
            .scope
            .as_ref()
            .is_some_and(|s| s.ignores(rel).unwrap_or(false))
        {
            return;
        }
        if rel.ends_with("/.gitignore") {
            self.refresh_scope();
            return;
        }
        if !is_source_file(
            rel,
            Some(&load_extension_overrides(Path::new(&self.root))),
            Some(Path::new(&self.root)),
        ) {
            if std::fs::metadata(Path::new(&self.root).join(rel)).is_err() {
                self.shared.lock().request_full(now_ms());
            }
            return;
        }
        if self
            .guard
            .as_mut()
            .is_some_and(|guard| catch_unwind(AssertUnwindSafe(|| guard(rel))).unwrap_or(false))
        {
            return;
        }
        self.shared.lock().record_file(rel.to_string(), now_ms());
    }
    fn refresh_scope(&mut self) {
        self.scope = build_scope_ignore(Path::new(&self.root), None).ok();
        self.schedule_refresh();
        self.shared.lock().request_full(now_ms());
    }
    fn native_path(&mut self, path: &Path) {
        let raw = path.to_string_lossy();
        let mut full = raw.to_string();
        if !is_within_dir(&full, &self.root) {
            let mut mappings: Vec<_> = self.watched.iter().collect();
            mappings.sort_by_key(|(_, r)| std::cmp::Reverse(r.len()));
            if let Some((logical, target)) = mappings
                .into_iter()
                .find(|(_, target)| is_within_dir(&full, target))
            {
                full = nodepath::resolve(&[logical, &nodepath::relative(target, &full)]);
            }
        }
        let rel = nodepath::relative(&self.root, &full).replace('\\', "/");
        if rel.is_empty() || rel == "." {
            self.schedule_refresh();
            return;
        }
        if rel != ".git/info/exclude" && self.ignores_dir(&nodepath::dirname(&full)) {
            return;
        }
        if Path::new(&full).is_dir() {
            if !self.ignores_dir(&full) && self.known.get(&full) != real(Path::new(&full)).as_ref()
            {
                self.schedule_refresh();
            }
            return;
        }
        if self.known.contains_key(&full) && !self.ignores_dir(&full) {
            self.schedule_refresh();
        }
        self.change(&rel);
    }
    fn watch_directory(&mut self, path: String, target: String, recurse: bool) -> bool {
        if self.watched.contains_key(&path) {
            return true;
        }
        if self.limit_warned || !self.shared.lock().snapshot.active {
            return false;
        }
        let configured = self.options.max_dir_watches.unwrap_or_else(|| {
            std::env::var("CODEGRAPH_MAX_DIR_WATCHES")
                .ok()
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(50_000)
        });
        let supplemental = self.watched.len().saturating_sub(usize::from(recursive()));
        let cap = if recurse {
            configured.min(256)
        } else {
            configured
        };
        if (recurse && path != self.root && supplemental >= cap)
            || (!recurse && self.watched.len() >= cap)
        {
            if !self.cap_warned {
                self.cap_warned = true;
                log_warn("File watcher hit directory-watch cap; remaining subtrees rely on manual/periodic sync",&[("cap",cap.to_string())]);
            }
            self.shared.lock().snapshot.partial_coverage = true;
            return false;
        }
        let Some(backend) = self.backend.as_mut() else {
            return false;
        };
        match backend.watch(
            Path::new(&path),
            if recurse {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            },
        ) {
            Ok(()) => {
                self.watched.insert(path, target);
                self.shared.lock().snapshot.watched_directories = self.watched.len();
                true
            }
            Err(error) => {
                self.failure(classify(error));
                false
            }
        }
    }
    fn refresh_tree(&mut self) {
        if self.options.inert || !self.shared.lock().snapshot.active {
            return;
        }
        let mut visited = HashSet::new();
        let mut desired = IndexMap::new();
        let mut known = IndexMap::new();
        let mut stack = vec![(self.root.clone(), false)];
        while let Some((dir, linked)) = stack.pop() {
            if self.ignores_dir(&dir) {
                continue;
            }
            let Some(target) = real(Path::new(&dir)) else {
                continue;
            };
            if !visited.insert(target.clone()) {
                continue;
            }
            known.insert(dir.clone(), target.clone());
            if !recursive() || linked || dir == self.root {
                desired.insert(dir.clone(), target);
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries.into_iter().rev() {
                let path = entry.path();
                if path.is_dir() {
                    let linked = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
                    stack.push((path.to_string_lossy().into_owned(), linked));
                }
            }
        }
        self.known = known;
        let stale: Vec<_> = self
            .watched
            .iter()
            .filter(|(p, t)| desired.get(*p) != Some(*t))
            .map(|(p, _)| p.clone())
            .collect();
        for p in stale {
            if let Some(backend) = self.backend.as_mut() {
                let _ = backend.unwatch(Path::new(&p));
            }
            self.watched.shift_remove(&p);
        }
        for (dir, target) in desired {
            self.watch_directory(dir, target, recursive());
        }
        self.shared.lock().snapshot.watched_directories = self.watched.len();
    }
}
