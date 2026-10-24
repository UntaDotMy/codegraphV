# CLAUDE.md

Claude Code project guidance for this repository.

Primary instructions live in the canonical agent guide — import it:

@AGENTS.md

## Claude-only notes

- Prefer the repo-root `AGENTS.md` as the source of truth for build/test/architecture/house rules. Edit that file (not this wrapper) when guidance changes.
- Claude Code can `@`-import nested guides too (e.g. `@docs/AGENTS.md`) when working on design/eval docs; Codex loads nested `AGENTS.md` automatically when the session cwd is under that directory.
- Do not reintroduce a duplicated `## CodeGraph` MCP tool-guidance block here — `src/mcp/server-instructions.ts` is the single source of truth (issue #529); the installer strips legacy marker blocks on upgrade.

## Tickets

The global ticket workflow (`~/.claude/CLAUDE.md`, tool `~/.claude/engineering/tickets.py`) applies here; DB `.engineering/tickets.sqlite3` (gitignored). Evidence commands for `record-test --exec`: engine `npx vitest run <file>` (regression `npm test`); Rust `cargo test -p <crate>` (regression `cargo test --workspace`); Rust↔TS parity `node scripts/rust-golden/verify.mjs`. Never run Cargo builds concurrently with a probe executable on Windows.
