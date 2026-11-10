//! Upstream d139865f: two processes opening an index that needs a schema migration
//! must serialize, and the one that waited must skip the migration the other applied.
use codegraph_db::{migrations::get_current_version, OpenOptions, RawConnection, Store, CURRENT_SCHEMA_VERSION};
use std::time::{Duration, Instant};

const V16_INDEX: &str = "idx_unresolved_failed_module_member";

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("codegraph.db");
    let store = Store::initialize(&path).unwrap();
    store
        .conn()
        .execute_batch(&format!(
            "DROP INDEX IF EXISTS {V16_INDEX};
             DELETE FROM schema_versions WHERE version >= 16;
             INSERT OR IGNORE INTO schema_versions (version, applied_at, description) VALUES (15, 0, 'fixture');"
        ))
        .unwrap();
    (dir, path)
}

fn state(path: &std::path::Path) -> (u32, i64, bool) {
    let store = Store::open(path, OpenOptions { read_only: true, ..Default::default() }).unwrap();
    let version = get_current_version(store.conn());
    let records: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM schema_versions WHERE version = 16", [], |r| r.get(0))
        .unwrap();
    let index = store
        .conn()
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1")
        .unwrap()
        .exists([V16_INDEX])
        .unwrap();
    (version, records, index)
}

#[test]
fn a_pending_migration_still_applies_in_one_process() {
    let (_dir, path) = fixture();
    let store = Store::open(&path, OpenOptions::default()).unwrap();
    drop(store);
    assert_eq!(state(&path), (CURRENT_SCHEMA_VERSION, 1, true));
}

#[test]
fn an_open_waits_for_the_migrator_then_skips_what_it_recorded() {
    let (_dir, path) = fixture();
    // Another process is mid-migration: a raw connection (no Store::open, which would
    // migrate) holds the write lock with version 16 recorded but not committed.
    let holder = RawConnection::open(&path).unwrap();
    holder.busy_timeout(Duration::from_secs(5)).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    holder
        .execute("INSERT INTO schema_versions (version, applied_at, description) VALUES (16, 0, 'other process')", [])
        .unwrap();

    let opener_path = path.clone();
    let opener = std::thread::spawn(move || {
        let started = Instant::now();
        let result = Store::open(&opener_path, OpenOptions::default()).map(|store| {
            drop(store);
            started.elapsed()
        });
        result
    });
    std::thread::sleep(Duration::from_millis(300));
    holder.execute_batch("COMMIT").unwrap();
    drop(holder);

    let waited = opener.join().unwrap().expect("the second open must succeed");
    assert!(waited >= Duration::from_millis(200), "the open did not wait for the migrator: {waited:?}");
    // Recorded once, by the other process; the waiter did not re-run it.
    assert_eq!(state(&path), (CURRENT_SCHEMA_VERSION, 1, false));
}

#[test]
fn concurrent_openers_all_succeed_and_record_the_migration_once() {
    let (_dir, path) = fixture();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let handles: Vec<_> = (0..3)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Store::open(&path, OpenOptions::default()).map(|store| drop(store))
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().expect("every concurrent open must succeed");
    }
    let (version, records, index) = state(&path);
    assert_eq!(version, CURRENT_SCHEMA_VERSION);
    assert_eq!(records, 1, "the migration version is recorded exactly once");
    assert!(index);
}
