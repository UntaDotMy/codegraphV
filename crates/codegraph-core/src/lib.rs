//! The CodeGraph engine core: project directory handling, file scanning, the
//! indexing orchestrator and the `CodeGraph` facade.

pub mod codegraph;
pub mod directory;
pub mod errors;
pub mod extract;
pub mod file_limits;
pub mod orchestrator;
pub mod prompt_hook;
pub mod query;
pub mod utils;
pub mod watch_policy;
mod watch;
pub use codegraph_sync::watcher::{FileWatcher,WatchOptions,PendingFile,WatchSyncResult,SyncError};

pub use codegraph::{CodeGraph, IndexOptions, IndexState, OpenOptions};
pub use codegraph_context::{BuildOutput, ContextBuilder};
pub use codegraph_sync as sync;
pub use codegraph_types::*;
