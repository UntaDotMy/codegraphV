//! Bounded native directory watching and the reference engine's debounce/retry
//! contract. The event loop never waits for indexing: a separate owner thread
//! builds and runs the supplied sync callback, including non-Send engine state.
mod runtime;
pub mod state;
pub use runtime::{FileWatcher, MetadataGuard, SyncCallback, WatchFailure, WatchOptions};
pub use state::{PendingFile, SyncError, WatchSnapshot, WatchSyncResult};
