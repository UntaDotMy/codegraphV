//! Cooperative cancellation scoped to one request on the owning query thread.
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
thread_local! {
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}
pub struct Scope(Option<Arc<AtomicBool>>);
impl Scope {
    pub fn enter(cancel: Arc<AtomicBool>) -> Self {
        Self(CANCEL.with(|current| current.replace(Some(cancel))))
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CANCEL.with(|current| current.replace(self.0.take()));
    }
}
pub fn cancelled() -> bool {
    CANCEL.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_requests_and_unwinding_restore_the_callers_cancellation_state() {
        let cancelled_token = Arc::new(AtomicBool::new(true));
        let outer = Scope::enter(cancelled_token);
        assert!(cancelled());
        let result = std::panic::catch_unwind(|| {
            let _inner = Scope::enter(Arc::new(AtomicBool::new(false)));
            assert!(!cancelled());
            panic!("simulated request failure");
        });
        assert!(result.is_err());
        assert!(cancelled());
        assert!(
            !std::thread::spawn(cancelled).join().unwrap(),
            "other query threads must be independent"
        );
        drop(outer);
        assert!(!cancelled());
    }
}
