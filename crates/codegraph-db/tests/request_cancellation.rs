use codegraph_db::Store;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[test]
fn cancellation_interrupts_running_sql_and_does_not_poison_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(dir.path().join("test.db")).unwrap();
    let token = Arc::new(AtomicBool::new(false));
    let sender = token.clone();
    let child = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        sender.store(true, Ordering::Relaxed);
    });
    let start = std::time::Instant::now();
    let scope = codegraph_types::request::Scope::enter(token);
    let result = store.conn().query_row("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n", [], |row| row.get::<_, i64>(0));
    drop(scope);
    child.join().unwrap();
    assert!(result.is_err(), "SQL did not observe cancellation");
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(
        store
            .conn()
            .query_row("SELECT 42", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        42
    );
    assert!(!codegraph_types::request::cancelled());
    let scope = codegraph_types::request::Scope::enter(Arc::new(AtomicBool::new(true)));
    assert!(store.get_outgoing_edges("missing", &[], None).is_err());
    assert!(store
        .get_incoming_edges_limited("missing", &[], 10)
        .is_err());
    drop(scope);
    assert!(store
        .get_outgoing_edges("missing", &[], None)
        .unwrap()
        .is_empty());
}
