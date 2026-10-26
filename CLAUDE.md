# CLAUDE.md

Claude Code project guidance for this repository.

Primary instructions live in the canonical agent guide — import it:

@AGENTS.md

## Claude-only notes

- Prefer the repo-root `AGENTS.md` as the source of truth for build/test/architecture/house rules. Edit that file (not this wrapper) when guidance changes.
- Claude Code can `@`-import nested guides too (e.g. `@docs/AGENTS.md`) when working on design/eval docs; Codex loads nested `AGENTS.md` automatically when the session cwd is under that directory.
- Do not reintroduce a duplicated `## CodeGraph` MCP tool-guidance block here — `src/mcp/server-instructions.ts` is the single source of truth (issue #529); the installer strips legacy marker blocks on upgrade.

## Tickets

The global ticket workflow (`~/.claude/CLAUDE.md`, tool `~/.claude/engineering/tickets.py`) applies here; DB `.engineering/tickets.sqlite3` (gitignored, so are its backups and logs). Evidence commands for `record-test --exec`: engine `npx vitest run <file>` (regression `npm test`); Rust `cargo test -p <crate>` (regression `cargo test --workspace`); Rust↔TS parity `node scripts/rust-golden/verify.mjs`. Never run Cargo builds concurrently with a probe executable on Windows.

- Evidence is bound to this repository's git state, so run the commands from the repo root and keep build output (`target/`, `dist/`, `node_modules/`) gitignored. A command that changes a tracked or untracked, non-ignored file while it runs is recorded as `error` ("source changed while the command ran"), not as pass or fail. Work outside the repo (e.g. the tool under `~/.claude`) needs `--cwd <dir>`.
- Unrelated untracked scratch files in the repo root still change the fingerprint: keep them out of the tree (use the session scratchpad) while a ticket is being verified.
- The gates bind only work done through the ticket CLI. Not detected: raw SQLite writes, file edits made without a ticket, manual evidence typed in without really checking it.
