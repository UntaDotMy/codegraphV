//! Single-file components. Script ranges borrow the original source and reuse
//! the native TS/JS walkers. A single fold preserves component ownership and
//! original file positions for every component format.

mod script;
mod shell;
mod vue;
mod vue_options;
mod vue_template;

pub use script::{fold_script_result, sfc_file_node, ScriptFold};
pub use shell::extract_component;

pub(super) use crate::source::{now_ms, SourceIndex as Lines};
