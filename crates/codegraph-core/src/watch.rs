//! Live watching over a background engine connection. Rc/SQLite owners never
//! cross threads; readers invalidate their node cache after each sync attempt.
use crate::{
    errors::{CodeGraphError, Result},
    orchestrator::FileStat,
    CodeGraph, IndexOptions, OpenOptions,
};
use codegraph_db::{OpenOptions as DbOptions, Store};
use codegraph_sync::watcher::{
    FileWatcher, PendingFile, SyncError, WatchOptions, WatchSnapshot, WatchSyncResult,
};
use std::{cell::OnceCell, path::Path, rc::Rc, time::Duration};

/// Borrowed read APIs may outlive a later read, so a replacement connection
/// remains alive until the facade next mutably reconciles/adopts it. This
/// mirrors the main engine's inode check without mutating borrowed layers.
pub(crate) struct WatchReadView {
    store: Rc<Store>,
    next: OnceCell<Box<WatchReadView>>,
}
impl WatchReadView {
    pub(crate) fn new(store: Rc<Store>) -> Self {
        Self {
            store,
            next: OnceCell::new(),
        }
    }
    pub(crate) fn followed(&self) -> bool {
        self.next.get().is_some()
    }
    pub(crate) fn current(&self) -> &Rc<Store> {
        if self.store.is_replaced_on_disk() && self.next.get().is_none() {
            if let Ok(store) = Store::open(
                self.store.path(),
                DbOptions {
                    read_only: self.store.is_read_only(),
                },
            ) {
                store.set_project_name_tokens(self.store.project_name_tokens());
                let _ = self.next.set(Box::new(Self::new(Rc::new(store))));
            }
        }
        self.next.get().map_or(&self.store, |next| next.current())
    }
}

impl CodeGraph {
    pub fn watch(&mut self, options: WatchOptions) -> Result<bool> {
        if self.is_watching() {
            return Ok(true);
        }
        self.unwatch();
        if self.store().is_read_only() {
            return Err(CodeGraphError::other(
                "Cannot auto-sync a read-only CodeGraph instance",
            ));
        }
        let root = self.project_root().to_string();
        let sync_root = root.clone();
        let factory = Box::new(move || {
            let mut engine: Option<CodeGraph> = None;
            Box::new(move |paths: Option<Vec<String>>| {
                if engine.is_none() {
                    engine = Some(
                        CodeGraph::open(&sync_root, OpenOptions::default()).map_err(map_error)?,
                    );
                }
                let engine = engine.as_mut().unwrap();
                let result = engine
                    .sync(
                        &IndexOptions {
                            paths,
                            ..Default::default()
                        },
                        None,
                    )
                    .map_err(map_error)?;
                Ok(WatchSyncResult {
                    files_changed: result.files_added
                        + result.files_modified
                        + result.files_removed,
                    duration_ms: result.duration_ms,
                })
            }) as codegraph_sync::watcher::SyncCallback
        });
        let guard = if cfg!(windows) {
            let guard_root = root.clone();
            let path = codegraph_db::database_path(Path::new(&root));
            Some(Box::new(move || {
                let mut store: Option<Store> = None;
                Box::new(move |rel: &str| {
                    if store.as_ref().is_none_or(|s| s.is_replaced_on_disk()) {
                        store = Store::open(&path, DbOptions { read_only: true }).ok();
                    }
                    let Some(store) = &store else {
                        return false;
                    };
                    let Ok(Some(file)) = store.get_file_by_path(rel) else {
                        return false;
                    };
                    let Ok(stat) = FileStat::of(&Path::new(&guard_root).join(rel)) else {
                        return false;
                    };
                    stat.size == file.size as u64
                        && stat.mtime_ms.floor() == file.modified_at as f64
                }) as codegraph_sync::watcher::MetadataGuard
            })
                as Box<
                    dyn FnOnce() -> codegraph_sync::watcher::MetadataGuard + Send,
                >)
        } else {
            None
        };
        let watcher = FileWatcher::with_factories(&root, factory, guard, options)?;
        let started = watcher.start();
        self.watcher = Some(watcher);
        self.last_watch_revision.set(0);
        Ok(started)
    }
    pub fn unwatch(&mut self) {
        let had_watcher = self.watcher.is_some();
        if let Some(watcher) = self.watcher.take() {
            watcher.stop();
            drop(watcher);
        }
        if had_watcher {
            self.reopen_if_replaced();
        }
        self.watch_readers.take();
        self.last_watch_revision.set(0);
        self.store().clear_cache();
    }
    pub fn is_watching(&self) -> bool {
        self.watcher.as_ref().is_some_and(FileWatcher::is_active)
    }
    pub fn watcher_snapshot(&self) -> WatchSnapshot {
        self.watcher
            .as_ref()
            .map(FileWatcher::snapshot)
            .unwrap_or_default()
    }
    pub fn is_watcher_degraded(&self) -> bool {
        self.watcher.as_ref().is_some_and(FileWatcher::is_degraded)
    }
    pub fn get_watcher_degraded_reason(&self) -> Option<String> {
        self.watcher.as_ref().and_then(FileWatcher::degraded_reason)
    }
    pub fn is_watcher_recovering(&self) -> bool {
        self.watcher
            .as_ref()
            .is_some_and(FileWatcher::is_recovering)
    }
    pub fn rearm_watcher_after_lock_contention(&self) -> bool {
        self.watcher
            .as_ref()
            .is_some_and(FileWatcher::rearm_after_lock_contention)
    }
    pub fn get_pending_files(&self) -> Vec<PendingFile> {
        self.watcher
            .as_ref()
            .map(FileWatcher::get_pending_files)
            .unwrap_or_default()
    }
    pub fn wait_until_watcher_ready(&self, timeout: Duration) -> std::io::Result<()> {
        self.watcher
            .as_ref()
            .map(|w| w.wait_until_ready(timeout))
            .unwrap_or(Ok(()))
    }
    pub fn ingest_watch_event(&self, relative_path: &str) {
        if let Some(watcher) = &self.watcher {
            watcher.ingest_event(relative_path);
        }
    }
    pub fn wait_until_watcher_idle(&self, timeout: Duration) -> std::io::Result<()> {
        self.watcher
            .as_ref()
            .map(|w| w.wait_until_idle(timeout))
            .unwrap_or(Ok(()))
    }
}
fn map_error(error: CodeGraphError) -> SyncError {
    match error {
        CodeGraphError::LockUnavailable(message) => SyncError::LockUnavailable(message),
        other => SyncError::Other(other.to_string()),
    }
}
