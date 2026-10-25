//! The engine's logger — port of the logger half of `src/errors.ts`.
//!
//! Library code reports through [`log_debug`] / [`log_warn`] / [`log_error`]; the
//! host swaps the sink with [`set_logger`] (the MCP server silences stdout-bound
//! output, tests install a recorder). The default writes `[CodeGraph] message
//! {context}` to stderr, with debug lines gated on `CODEGRAPH_DEBUG`.

use std::sync::{Arc, RwLock};

/// A context entry attached to a log line: a short key and its rendered value.
pub type LogContext<'a> = &'a [(&'a str, String)];

pub trait Logger: Send + Sync {
    fn debug(&self, message: &str, context: LogContext<'_>);
    fn warn(&self, message: &str, context: LogContext<'_>);
    fn error(&self, message: &str, context: LogContext<'_>);
}

fn render_context(context: LogContext<'_>) -> String {
    if context.is_empty() {
        return String::new();
    }
    let body: Vec<String> = context.iter().map(|(k, v)| format!("{k}: '{v}'")).collect();
    format!(" {{ {} }}", body.join(", "))
}

/// stderr logger; debug lines only when `CODEGRAPH_DEBUG` is set.
pub struct DefaultLogger;

impl Logger for DefaultLogger {
    fn debug(&self, message: &str, context: LogContext<'_>) {
        if std::env::var_os("CODEGRAPHV_DEBUG").is_some_and(|v| !v.is_empty()) {
            eprintln!("[CodeGraphV] {message}{}", render_context(context));
        }
    }
    fn warn(&self, message: &str, context: LogContext<'_>) {
        eprintln!("[CodeGraphV] {message}{}", render_context(context));
    }
    fn error(&self, message: &str, context: LogContext<'_>) {
        eprintln!("[CodeGraphV] {message}{}", render_context(context));
    }
}

/// Discards everything — useful for tests.
pub struct SilentLogger;

impl Logger for SilentLogger {
    fn debug(&self, _: &str, _: LogContext<'_>) {}
    fn warn(&self, _: &str, _: LogContext<'_>) {}
    fn error(&self, _: &str, _: LogContext<'_>) {}
}

static LOGGER: RwLock<Option<Arc<dyn Logger>>> = RwLock::new(None);

/// Replace the global logger.
pub fn set_logger(logger: Arc<dyn Logger>) {
    *LOGGER.write().unwrap() = Some(logger);
}

fn current() -> Arc<dyn Logger> {
    match LOGGER.read().unwrap().as_ref() {
        Some(l) => Arc::clone(l),
        None => Arc::new(DefaultLogger),
    }
}

pub fn log_debug(message: &str, context: LogContext<'_>) {
    current().debug(message, context);
}

pub fn log_warn(message: &str, context: LogContext<'_>) {
    current().warn(message, context);
}

pub fn log_error(message: &str, context: LogContext<'_>) {
    current().error(message, context);
}
