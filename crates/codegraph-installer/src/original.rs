//! What the original (TypeScript, npm `@colbymchenry/codegraph`) codegraph wrote, so
//! `codegraphV migrate` can find and remove it. The v0.0.1 Rust build wrote the same names.
use crate::Brand;

pub const ORIGINAL: Brand = Brand {
    server: "codegraph",
    permission: "mcp__codegraph__*",
    permission_prefix: "mcp__codegraph__",
    // Also `<path>/codegraph[.exe] prompt-hook`. The npm build wrote `codegraph.cmd prompt-hook`
    // on Windows; `npx @colbymchenry/codegraph prompt-hook` contains the first form.
    binary: BINARY,
    hook_forms: &["codegraph prompt-hook", "codegraph.cmd prompt-hook"],
    start: "<!-- CODEGRAPH_START -->",
    end: "<!-- CODEGRAPH_END -->",
};

/// The original's per-user state folder (`~/.codegraph`). Its per-project index folder is
/// `codegraph_types::paths::ORIGINAL_CODEGRAPH_DIR`.
pub const STATE_DIR: &str = ".codegraph";
/// The npm package the original was installed as.
pub const NPM_PACKAGE: &str = "@colbymchenry/codegraph";
/// The v0.0.1 Rust build's binary and its Windows install folder (`%LOCALAPPDATA%\codegraph\bin`).
pub const BINARY: &str = "codegraph";
pub const WINDOWS_INSTALL_DIR: &str = "codegraph";
