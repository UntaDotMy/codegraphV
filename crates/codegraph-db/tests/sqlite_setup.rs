//! SQLite's global memory statistics take one process-wide lock on every allocation, which
//! stops parallel resolve workers from scaling (DEV-87). The store turns them off before SQLite
//! starts. Its own test binary, so no other test starts SQLite first.

use codegraph_db::{OpenOptions, Store};

#[test]
fn memory_statistics_are_off_once_a_store_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("codegraph.db");
    drop(Store::initialize(&path).unwrap());
    let store = Store::open(&path, OpenOptions { read_only: true }).unwrap();
    // Statistics off: SQLite stops counting, so its usage reads 0 while a connection is open.
    let used = unsafe { rusqlite::ffi::sqlite3_memory_used() };
    assert_eq!(used, 0, "SQLite still counts its memory use (statistics on)");
    drop(store);
}
