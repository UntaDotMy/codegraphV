//! CodeGraph error types — port of `src/errors.ts`.
//!
//! The logger of that module lives in `codegraph_types::log`.

use serde_json::{json, Map, Value};

/// The base error of every CodeGraph failure: a message, a categorizing code
/// (`FILE_ERROR`, `PARSE_ERROR`, …) and the context it was raised with.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CodeGraphError {
    /// Error reading or accessing files.
    #[error("{message}")]
    File { message: String, file_path: String, cause: Option<String> },
    /// Error parsing source code.
    #[error("{message}")]
    Parse { message: String, file_path: String, line: Option<u32>, column: Option<u32>, cause: Option<String> },
    /// Error with database operations.
    #[error("{message}")]
    Database { message: String, operation: String, cause: Option<String> },
    /// Error with search operations.
    #[error("{message}")]
    Search { message: String, query: String, cause: Option<String> },
    /// Error with vector/embedding operations.
    #[error("{message}")]
    Vector { message: String, operation: String, cause: Option<String> },
    /// Error with configuration.
    #[error("{message}")]
    Config { message: String, details: Option<Map<String, Value>> },
    /// The cross-process write lock (or a rebuild in progress) makes a sync unavailable right now;
    /// the caller keeps its pending work and retries.
    #[error("{0}")]
    LockUnavailable(String),
    /// A failure with no category of its own (the engine raised a bare `Error`).
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, CodeGraphError>;

impl CodeGraphError {
    pub fn file(message: impl Into<String>, file_path: impl Into<String>) -> Self {
        CodeGraphError::File { message: message.into(), file_path: file_path.into(), cause: None }
    }

    pub fn parse(message: impl Into<String>, file_path: impl Into<String>, line: Option<u32>, column: Option<u32>) -> Self {
        CodeGraphError::Parse { message: message.into(), file_path: file_path.into(), line, column, cause: None }
    }

    pub fn database(message: impl Into<String>, operation: impl Into<String>) -> Self {
        CodeGraphError::Database { message: message.into(), operation: operation.into(), cause: None }
    }

    pub fn search(message: impl Into<String>, query: impl Into<String>) -> Self {
        CodeGraphError::Search { message: message.into(), query: query.into(), cause: None }
    }

    pub fn config(message: impl Into<String>) -> Self {
        CodeGraphError::Config { message: message.into(), details: None }
    }

    pub fn other(message: impl Into<String>) -> Self {
        CodeGraphError::Other(message.into())
    }

    /// Error code for categorization.
    pub fn code(&self) -> &'static str {
        match self {
            CodeGraphError::File { .. } => "FILE_ERROR",
            CodeGraphError::Parse { .. } => "PARSE_ERROR",
            CodeGraphError::Database { .. } => "DATABASE_ERROR",
            CodeGraphError::Search { .. } => "SEARCH_ERROR",
            CodeGraphError::Vector { .. } => "VECTOR_ERROR",
            CodeGraphError::Config { .. } => "CONFIG_ERROR",
            CodeGraphError::LockUnavailable(_) | CodeGraphError::Other(_) => "ERROR",
        }
    }

    /// Additional context about the error.
    pub fn context(&self) -> Option<Value> {
        Some(match self {
            CodeGraphError::File { file_path, cause, .. } => json!({ "filePath": file_path, "cause": cause }),
            CodeGraphError::Parse { file_path, line, column, cause, .. } => {
                json!({ "filePath": file_path, "line": line, "column": column, "cause": cause })
            }
            CodeGraphError::Database { operation, cause, .. } => json!({ "operation": operation, "cause": cause }),
            CodeGraphError::Search { query, cause, .. } => json!({ "query": query, "cause": cause }),
            CodeGraphError::Vector { operation, cause, .. } => json!({ "operation": operation, "cause": cause }),
            CodeGraphError::Config { details, .. } => Value::Object(details.clone()?),
            CodeGraphError::LockUnavailable(_) | CodeGraphError::Other(_) => return None,
        })
    }
}

impl From<codegraph_db::DbError> for CodeGraphError {
    fn from(e: codegraph_db::DbError) -> Self {
        CodeGraphError::Database { message: e.to_string(), operation: "query".to_string(), cause: None }
    }
}

impl From<std::io::Error> for CodeGraphError {
    fn from(e: std::io::Error) -> Self {
        CodeGraphError::Other(e.to_string())
    }
}

impl From<codegraph_graph::GraphError> for CodeGraphError {
    fn from(e: codegraph_graph::GraphError) -> Self {
        match e {
            codegraph_graph::GraphError::Database(db) => db.into(),
            other => CodeGraphError::Other(other.to_string()),
        }
    }
}

/// A refused path — the caller asked for something outside the project root, or for a sensitive
/// system directory. Deliberately NOT a [`CodeGraphError`]: it is a security marker every read
/// sink tests for, not a categorized operational failure, and the MCP layer treats it as one of
/// the only two "stop trying" conditions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PathRefusalError(pub String);
