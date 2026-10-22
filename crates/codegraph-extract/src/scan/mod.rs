//! File discovery — port of the scan half of `src/extraction/index.ts`
//! (ignore rules, `.gitignore` reading, git listing, filesystem walk) and of
//! `src/project-config.ts`.
//!
//! Which files the index holds is decided here, so everything is a port of the
//! TypeScript behavior rather than a re-derivation of gitignore semantics; see
//! [`ignore`] for the matcher those rules run on.

pub mod config;
pub mod discovery;
pub mod git;
pub mod ignore;

pub use discovery::{
    build_default_ignore, build_scope_ignore, can_trust_git_fast_path, classify_git_dir, defaults_only_ignore, discover_embedded_repo_roots,
    find_nested_git_repos, find_unindexed_ignored_repos, get_git_changed_files, get_git_head_sha, get_git_visible_files, include_static_roots,
    preload_languages_for_files, read_gitignore_patterns, scan_directory, GitChanges, GitDirKind, ScanSkipStats, ScopeIgnore, INDEXED_AT_COMMIT_KEY,
};
