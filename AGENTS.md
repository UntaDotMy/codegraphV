# AGENTS.md

codegraphV: Rust rewrite of CodeGraph (local code-intelligence indexer + CLI + MCP server). Production code is `crates/`. `src/`, `dist/`, `__tests__/`, `package.json` are the TypeScript reference kept only for parity checks — don't change them unless asked; their guide is `docs/typescript-reference.md` (read on demand).

## Build / test
```text
cargo build --release --locked -p codegraph-cli      # target/release/codegraphV(.exe)
cargo test --workspace --locked                      # regression suite
cargo test --locked -p <crate> [--test <file>]       # targeted
cargo test --locked -p codegraph-mcp --test explore_render
cargo test --locked -p codegraph-installer --test contracts
cargo test --locked -p codegraph-cli --test executable  # end to end through the built binary
node scripts/rust-golden/verify.mjs                  # Rust↔TS graph parity (see scripts/rust-golden/README.md)
```
- Windows: never build a Cargo target while its probe executable is running. Checkouts may be CRLF; split on `\r?\n` in tests.
- Tests use real files in `tempfile` dirs and real SQLite; no DB mocking.
- Benchmarks/gates: `scripts/bench/` (`bench.py`, `index_speed.py`, `efficiency_gate.py`, `lang_parity.py`, `gate_explore.py`). `scripts/check_names.py`: no user-visible text may use the original's names.

## Map
- Runtime data `.codegraphV/`; env vars `CODEGRAPHV_*`; MCP server `codegraphV`, tools `codegraphV_*`. `codegraph.json` keeps its name.
- CLI: `crates/codegraph-cli/src/main.rs`; Claude `UserPromptSubmit` hook: `crates/codegraph-cli/src/prompt_hook.rs` + `crates/codegraph-core/src/prompt_hook.rs` (must never break a prompt: every failure prints nothing, exit 0).
- MCP dispatch `crates/codegraph-mcp/src/tools.rs`; explore rendering `crates/codegraph-mcp/src/render.rs` + `src/explore/`.
- Agent-facing tool guidance — single source of truth: `crates/codegraph-mcp/src/assets/{server_instructions.md,server_instructions_no_root_index.md,tools.json}`. Installer's instructions-file block: `crates/codegraph-installer/src/assets/instructions.md` (names the tool + CLI only; change it only when a tool/command is renamed or removed).
- Installer `crates/codegraph-installer/` (every target change needs `tests/contracts.rs` coverage: idempotent install, sibling preservation, uninstall reverses install). Cursor needs `--path` injected into its MCP args (wrong cwd, no `rootUri`) — preserve.
- `NodeKind`/`EdgeKind`: `crates/codegraph-types/src/kinds.rs`; extractors/resolvers use those exact strings.
- Synthesizers `crates/codegraph-resolve/src/callback_synthesizer/`; frameworks `crates/codegraph-resolve/src/frameworks/`.
- Release: `.github/workflows/rust-release.yml`, notes `release-notes/vX.Y.Z.md`, installers `install-rust.{ps1,sh}`. `CHANGELOG.md` is upstream's. Don't bump versions, tag, push or publish unless asked.

## Retrieval contract (do not regress)
Goal: a flow question ("how does X reach Y", trace, impact, callers) resolves in 1 codegraphV call on small repos, 3–5 on large, with zero Read/Grep. Optimize wall-clock + tool-call count, not tokens. Judge every change by: is the answer sufficient for the agent to *stop* reading?
- **Adapt the tool, not the agent.** Server instructions and tool descriptions are low-salience; rewording them or adding tools does not move tool choice (measured; it regressed). Make `codegraphV_explore` — the tool agents already call, with a symbol-bag query — return more: the call path among named symbols (≤1 unnamed bridge; PascalCase type token biases overloads), full bodies, every overload of an ambiguous name. Fuzzy-description input can't disambiguate endpoints (why `context`/`trace` tools were cut).
- **Errors teach abandonment.** `isError: true` only for security refusals and real malfunctions. Not indexed / symbol not found / file not indexed → success-shaped response carrying guidance. Tools stay exposed at an un-indexed root (per-project instructions variant). Never index automatically.
- **Explore budget** (`explore_budget` in `render.rs`, `get_explore_budget` in `explore/helpers.rs`): calls `<500→1, <5000→2, <15000→3, <25000→4, else 5`; per-call chars/files/per-file never shrink as repo size grows (a smaller `per_file` for a larger tier forced Reads on god-files). Output never says "use Read" — steer to another explore; returned source counts as already read.
- **Dynamic dispatch: close flows end-to-end.** Partial bridging is worse than none (revealed hop → agent drills and reads). Synthesized edges: `provenance: heuristic`, `metadata.synthesizedBy`, `registeredAt`. Reactive/reconciler runtimes with no static edges stay silent — silent beats wrong.
- **New language/framework:** validate on small/medium/large real repos, ≥3 flow prompts each: deterministic probes, then agent A/B (≥2 runs/arm). Pass: ~0 Read/Grep within budget, faster than without, no control-repo regression. Method: `docs/AGENTS.md`, `docs/design/dynamic-dispatch-coverage-playbook.md`, `docs/benchmarks/call-sequence-analysis.md`. Before touching routers/frameworks/`WHEN` rules read `docs/design/framework-coverage.md` and update it in the same change.

## House rules
- Platform-specific behavior (paths, drive letters, sockets/pipes, watchers, process lifecycle) is gated per OS with `#[cfg]`/runtime checks and validated on that OS, never assumed.
- CodeGraphV gives code context, not requirements: ask the user about UX, edge cases and acceptance criteria.
- Anchor issue/PR reports to a date and version (latest `release-notes/`, `git log -1 origin/main`, current branch) before calling something unfixed.
- README images carry `?v=N`; bump N whenever the asset bytes change.

## Owner rules
- Before changing files or calling a diff final, have the plan or diff verified by the session's advisor or reviewer tool. If none exists, ask the owner. Never spawn a subagent for this check.
- Run tests and builds on GitHub Actions; use the local machine only when needed. This checkout is a fork, so `gh pr create` must pass `--repo` for this repository; to run CI on a branch, open a draft PR into `rust-rewrite`. Never merge until every leg on the head commit is green; a cancelled leg is not green.
- When a task is finished, delete what it left behind that nothing needs. Never run `git clean -X` at the repo root; it also removes `.codegraphV/`, `node_modules/` and `dist/`.
