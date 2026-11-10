//! Bounded cancellation state shared by transport readers and the owning session thread.
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

#[derive(Clone, Default)]
pub(crate) struct PendingRequests(Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>);

pub(crate) struct RequestFrame {
    pub line: String,
    pub token: Option<Arc<AtomicBool>>,
    id: Option<Value>,
    pending: PendingRequests,
}

impl PendingRequests {
    /// Consume only a well-formed cancellation notification. Unknown/finished IDs are ignored.
    pub fn cancel_notification(&self, line: &str) -> bool {
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        if request["jsonrpc"] != "2.0"
            || request.get("id").is_some()
            || request["method"] != "notifications/cancelled"
        {
            return false;
        }
        let Some(id) = request["params"].get("requestId") else {
            return false;
        };
        if !id.is_string() && !id.is_number() && !id.is_null() {
            return false;
        }
        if let Some(token) = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id.to_string())
        {
            token.store(true, Ordering::Relaxed);
        }
        true
    }

    pub fn frame(&self, line: String) -> Result<RequestFrame, String> {
        let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| {
            (v["jsonrpc"] == "2.0" && v["method"] == "tools/call")
                .then(|| v.get("id").cloned())
                .flatten()
                .filter(|id| id.is_string() || id.is_number() || id.is_null())
        });
        let token = if let Some(id) = &id {
            let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if pending.len() >= crate::session::MAX_QUEUED_FRAMES + 1
                || pending.contains_key(&id.to_string())
            {
                return Err(
                    "Too many pending requests or a duplicate outstanding request ID".into(),
                );
            }
            let token = Arc::new(AtomicBool::new(false));
            pending.insert(id.to_string(), token.clone());
            Some(token)
        } else {
            None
        };
        Ok(RequestFrame {
            line,
            id,
            token,
            pending: self.clone(),
        })
    }

    pub fn cancel_all(&self) {
        for token in self.0.lock().unwrap_or_else(|e| e.into_inner()).values() {
            token.store(true, Ordering::Relaxed);
        }
    }
}

impl RequestFrame {
    pub fn cancelled_response(&self) -> Option<Value> {
        self.token.as_ref().filter(|token| token.load(Ordering::Relaxed)).map(|_| {
            json!({"jsonrpc":"2.0","id":self.id,"result":{
                "content":[{"type":"text","text":"Request cancelled; no complete answer was produced."}]}})
        })
    }
}

impl Drop for RequestFrame {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            self.pending
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(id: Value) -> String {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call"}).to_string()
    }
    fn cancel(id: Value) -> String {
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})
            .to_string()
    }
    #[test]
    fn queued_cancellation_is_isolated_and_ids_can_be_reused_after_completion() {
        let pending = PendingRequests::default();
        let first = pending.frame(request(json!(1))).unwrap();
        let other = pending.frame(request(json!("1"))).unwrap();
        pending.cancel_notification(&cancel(json!(1)));
        assert!(first.cancelled_response().is_some());
        assert!(other.cancelled_response().is_none());
        assert!(pending.frame(request(json!(1))).is_err());
        drop(first);
        let reused = pending.frame(request(json!(1))).unwrap();
        assert!(reused.cancelled_response().is_none());
        let different_client = PendingRequests::default();
        different_client.cancel_notification(&cancel(json!(1)));
        assert!(reused.cancelled_response().is_none());
        pending.cancel_all();
        assert!(other.cancelled_response().is_some() && reused.cancelled_response().is_some());
    }
    #[test]
    fn unknown_cancel_does_not_leave_tombstones_and_registry_is_bounded() {
        let pending = PendingRequests::default();
        for id in 0..1000 {
            pending.cancel_notification(&cancel(json!(id)));
        }
        let frames: Vec<_> = (0..=crate::session::MAX_QUEUED_FRAMES)
            .map(|id| pending.frame(request(json!(id))).unwrap())
            .collect();
        assert!(pending.frame(request(json!(9999))).is_err());
        assert!(frames
            .iter()
            .all(|frame| frame.cancelled_response().is_none()));
        drop(frames);
        assert!(pending.frame(request(json!(9999))).is_ok());
    }
}
