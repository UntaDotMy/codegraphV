# Implementation progress

## Current continuation (2026-11-03)

2026-11-04 continued work:

- A20: cancellation is registered before enqueue, shared across queued/active requests, and
  scoped per connection. RAII removes completed IDs; reused IDs are fresh, unknown cancellation
  IDs leave no tombstones, and pending state is capped at 33 entries per connection. Daemon
  disconnect cancels outstanding work. Stdio uses the same registry and explicitly terminates
  on queue overload rather than blocking its reader forever. Seven MCP unit tests pass,
  including deterministic stdio queued-cancel/next-ping/no-index-write coverage
  (`queued-cancel-unit-final.log`). New daemon connections fail admission when the queue is full.
  Cheap graph lookup fairness and cancellation outside SQL/adjacency work remain open.
- `e651b872`, `d2e0389b`: both sparse regressions reproduced (`sparse-before.log`). The scanner
  now excludes absent skip-worktree paths while retaining present ones and recognizes tagged
  gitlinks. Change detection records sparse pattern/config fingerprints and scans for graph/disk
  differences only when that fingerprint changes. Three real-Git tests pass: missing/present
  skip-worktree controls, widening/narrowing/disable, scoped-sync stamp preservation, linked
  worktree isolation, and normalized full-index/sync graph + pending-ref equivalence
  (`sparse-after-final.log`). No latency saving is claimed without a benchmark.
- `47a9c7d6`: reproduced reference-order-dependent Python binding results in
  `python-binding-before.log`; the memo now stores the first binding line instead of the first
  reference's boolean answer, and continuation lines inside calls cannot be assignments.
  Focused validation is in `python-binding-after.log`.
- Full workspace cancellation gate is running in `workspace-cancellation.log`. It predates
  the latest sparse/Python changes, which have separate targeted regressions; a final shared
  gate still follows. No release/merge action has occurred.

The cancellation workspace gate completed: **939 passed, zero failed**, 133 result blocks.
The binding-order follow-up passed its new test and all four receiver-rebinding controls
(`python-binding-after.log`). Sparse and binding changes still need the final shared gate.

Active cancellation continuation: request-scoped cancellation tokens now pass from daemon reader
notifications into SQLite progress callbacks during tool dispatch only. Interrupted SQL produces
honest cancellation guidance and clears source-emission history, avoiding skipped evidence on
retry. The scope is dropped before lifecycle writes/maintenance. The native DB test cancels a
billion-row recursive query and then verifies a successful query on the same connection
(`implementation-logs/cancellation-db.log`). Four MCP unit tests pass (`cancellation-build.log`).
Queued cancellation, Rust CPU-loop cancellation, stdio cancellation and fair cheap lookups remain
open; no complete A20 claim. Performance impact of the progress callback remains unmeasured.
Follow-up: incoming/outgoing adjacency reads now also check the token between short SQL calls;
the extended DB regression passes (`cancellation-db-final.log`), including recovery on the next
request. Non-adjacency CPU work still has no cancellation checkpoints. Daemon integration log:
`cancellation-daemon.log`. Full workspace and release rebuild after this cancellation change
remain required; earlier build/test passes must not be treated as covering these new changes.

Latest A20 continuation: daemon query admission uses nonblocking bounded enqueue. When full,
requests receive an explicit retry response (tool requests remain success-shaped); notifications
whose state cannot safely be dropped disconnect rather than being silently lost. The connection
reader can continue reading control frames instead of blocking on query admission. Four MCP unit
tests pass (`implementation-logs/overload-validation.log`); CLI daemon rerun is in
`overload-daemon.log`. Active-query cancellation, new-client admission during saturation and
cheap graph-query fairness remain unresolved; this is partial A20 progress, not completion.
All four CLI daemon integration tests passed, and `git diff --check` passed. Native server
guidance explains that a queue-full response means the request was not executed.

2026-11-04 follow-up: fixed an additional A21 protocol defect in `daemon.rs`: control dispatch
accepted invalid JSON-RPC envelopes, IDs and params. The duplicate unchecked event-loop dispatch
was removed. Invalid requests now reach Session error handling and cannot stop the daemon.
Three MCP library tests and four CLI daemon integration tests pass (logs:
`implementation-logs/control-validation.log`, `daemon-control-validation.log`).
A20 cancellation/fairness and the pending upstream/production backlog remain open.
The follow-up release build passed (`release-control-final.log`), and `git diff --check` passed.

The working tree at the start of this continuation already contained the nine batches below.
Those changes are preserved; their earlier test results are historical evidence, not fresh runs.
The user has authorized commit/push/tag/release **after** completion and validation; no release
action has been taken while the open gates remain.

- [x] Re-read the audit/preservation/production plans and inspect the existing implementation.
- [x] A08 follow-up: reproduced deletion of unreadable journals and loss of retry state after a
  failed restore (two failing tests in `implementation-logs/recovery-followup-before.log`).
  `transaction.rs` now retains those journals, serializes commit/recovery using an OS file lock,
  recovers version-2 journals immediately after lock release, rechecks files before each write,
  and never restores the failed destination from a stale snapshot. Rollback read failures retain
  the journal. The commit-marker failure message now honestly warns of possible recovery rollback.
- [x] A08 targeted validation: installer unit and contract suites pass, including edited settings,
  old-but-live journal exclusion, failed restore/retry, exact instruction round trips and opt-outs
  (`implementation-logs/recovery-followup-after.log`). `install.lock` intentionally persists empty;
  it must never be unlinked while other processes may hold it. Installer README updated.
- [x] Python `27c904b0` / `bea5e205`: wrapped/PEP-695 class headers reproduced as missing method
  edges (`implementation-logs/python-headers-before.log`); shared bounded header reader now handles
  comments/quotes, nested parameters and wrapped lists. Focused fixtures pass.
- [x] Python `3fca009e` / `274b43c4` / `c0b49062`: header tests exposed missing generic `extends`
  edges and external imports linking project namesakes. Native extraction unwraps one subscript;
  inheritance/receiver gates reject outside imports while retaining local bindings. Focused
  tests pass (`python-final.log`), including full-vs-sync complete graph/pending-reference
  comparisons, dotted/aliased/deep external bases, same-file rebinding, relative/fallback/star
  imports and custom-root controls. Extraction rejects computed/doubly-subscripted/keyword
  bases and tests LF/CRLF. Five core tests + one extraction test.
- [x] P01 evidence correction: `bench-before-after.md` no longer claims a per-request p95 gate
  from medians of batch averages, or answer equality from result counts. No new timing/token claim.
- [x] Fresh workspace: 920 passed, zero failed (126 result blocks); latest installer changes
  separately pass 21 unit + 19 contract tests, including killed-process lock recovery.
- [x] Explicit migrate/explore_render gates: 9 + 18 passed. Parity fingerprint and formatting
  checks executed and still exit 1; neither is a passing gate.
- [x] Release build completed (`release-continuation.log`); protocol follow-up rebuild follows
  in `release-control-final.log`. Remaining platform/parity gates stay open.
- [ ] Remaining upstream rows, real-repository/agent measurement, platform and release gates
  remain open as listed below; no broad completion or new performance claim.

Known A08 limits: version-1 journals retain the legacy age heuristic; unrelated editors do not
participate in the OS lock, so the final compare/rename interval is not an atomic editor lock.
Windows ACL/hard-link identity and actual power-loss durability remain unverified.

A20 disposition correction: the earlier "Fixed" row establishes reader-thread ping/status/stop
responses and bounded graph walks, **not** the full acceptance criterion. Cheap graph lookups
still share the synchronous SQLite loop, cancellation is not implemented, and a reader blocked
on a full event queue cannot read its next control frame. A20 remains partially addressed;
fairness/cancellation under sustained queue saturation is an open release gate.

Fresh logs are under `implementation-logs/`: `workspace-continuation.log`,
`recovery-followup-final.log`, `python-final.log`, `required-targets-continuation.log`,
`parity-continuation.log` and `fmt-continuation.log`. The workspace run preceded the final
atomic-journal publication change; the installer and required target runs validate that change.
Docker and cargo-audit are unavailable locally. Linux/macOS and advisory scanning remain
unexecuted. No commit, push, tag or publication has been performed by this continuation.

Baseline: 8182ee403d2ca64a2516cf67f0b4f3a2bbd0fc79. Initial working tree contained the user's untracked audit package only. No real host settings are used by regression tests.

## Active preservation batch

| ID | Disposition / evidence | Files / remaining checks |
|---|---|---|
| A01 | Implemented refresh permission preservation, including narrow allow entries; explicit enable/disable retained | installer lib.rs/contracts.rs; CLI main.rs/migrate.rs. CLI/upgrade process tests pending |
| A02 | Exact legacy command recognition; misleading echo/comment/wrapper/suffix/compound commands retained | installer lib.rs/original.rs/contracts.rs; existing legacy migration tests pass |
| A03 | Shared instruction path checked for remaining configured consumers; both local Codex/OpenCode removal orders and repeated uninstall pass | installer lib.rs/contracts.rs |
| A04 | Generated hooks use direct executable plus args; existing exact shell forms migrate | installer lib.rs/contracts.rs. Special-path and real-host validation pending; current Claude exec-form documentation checked 2026-11-03 |
| A33 | Backtick/tilde fenced marker examples retained and append/refresh/remove tested | markers.rs. Ambiguous marker refusal still pending |
| A34–A35 | Pending | Exact bytes/existence and host instruction loading |

Executed: `cargo test --locked -p codegraph-installer`: 8 unit + 12 integration tests passed. An intermediate run caught missing exact legacy npx recognition; corrected and rerun. No performance gains claimed.

## Remaining checklist (not completed)

- [ ] Complete A01–A04/A33–A35 preservation matrix and process-level regressions.
- [ ] A05–A08, A11–A13: shared executable ownership, Git hooks, interruption/concurrency, opt-outs and hook work bounds.
- [ ] A15/A19/A20: bounded graph adjacency, frames/queues, cancellation and fairness.
- [ ] Upstream sync/cache group: full-index versus incremental complete graph equivalence.
- [ ] Remaining upstream ledger: verify each pinned diff and regression before assigning a disposition.
- [ ] A09–A10/A14/A16–A18 and A21–A32: remaining lifecycle, CLI, release and validation findings.
- [ ] P01 baseline harness and P02–P09: measured optimization, real-repository/agent A/B and platform reliability gates.
- [ ] Release build, full workspace, explore/migrate, parity and platform gates.

Unexecuted gates and pending items are release gaps, not passes. Existing upstream classifications remain inherited evidence until individually revalidated.

## Second batch: preservation and boundedness

- A33: production edits now refuse duplicate/nested/unmatched markers and unclosed Markdown fences before any target writes. Backtick/tilde and indented code examples are excluded from ownership.
- A34: newly appended blocks record original existence and the count of separator bytes added inside the managed block. Removal splices only those additions and preserves subsequent user edits. Legacy installations lack this history; their original missing-final-newline/existence cannot be reconstructed safely. Native contracts cover four targets and six byte/existence fixtures, plus subsequent edits.
- A35: an AGENTS.md-only local Claude project keeps that instruction path; no new CLAUDE.md is introduced. This is a filesystem regression, not live-host certification. Existing pre-shadowed installations are not automatically migrated.
- MCP configuration refresh now updates command/args field-by-field for JSON and preserves TOML table preferences. Malformed Claude hook/permission shapes fail before writing the target. A pre-write content check detects intervening edits; this is **not** a durable transaction or atomic concurrent-editor exclusion (A08 remains open).
- A07: selected-target uninstall keeps the shared binary; all-target removal checks remaining global/current-project consumers. Other projects are not inventoried, so cross-project ownership remains a release gap.
- A12/A13: configurable native hook deadline (default 2 seconds, 100–5000 ms), 1 MiB stdin cap, `install --prompt-hook preserve|on|off`. Process tests added; full gate pending.
- A15 / upstream cc863616: SQL-limited impact adjacency; 10,000 nodes, 50,000 edges, 100,000 examined edges per traversal. Aggregate impact responses also share node/edge budgets and bound definition traversals. CLI/MCP expose truncation, viewer API blast metadata carries it. Native dense/container/cycle, exact-fit, zero-work and endpoint tests pass. Real-repository differential/performance evidence remains pending; no optimization claim.
- A19: 1 MiB stdio/daemon frames; 32-entry backpressure queues; at most 32 daemon reader threads, including unauthenticated connections; bounded accept batches. Frame-limit and normal protocol tests pass. Sustained flood/RSS/host tests remain pending.
- A20: **not fixed**. The shared SQLite event loop still runs expensive requests synchronously; ingress and graph caps do not establish ping/cancellation fairness.

## Upstream correctness batch

`66449255`: reproduced in `barrel-before.log`: after retargeting a barrel, caller still reached a.ts instead of b.ts. Native extraction now compares re-export import-node signatures; changed barrels reopen importer bindings, failed refs and imported-type member references. `barrel-after.log` passes full and scoped sync for named/star re-exports, a direct-import negative control, and complete normalized node/edge/pending-reference comparisons against full index. Files: core orchestrator mod/sync, codegraph.rs, resolver module_importers.rs, core tests/sync_barrel_retarget.rs. Further upstream fixtures and real-repo validation remain open.

The first two workspace attempts exposed stale shell-hook expectations in CLI branding and installed-hook execution tests. They now assert and execute the direct-exec contract. The current full workspace run is in implementation-workspace-tests.log; do not treat intermediate failures as completed gates.

## Third batch: ownership, recovery, boundedness and upstream sync

Baseline for this batch: working tree as left by the prior batch, `cargo check --workspace --locked` clean.

| ID / upstream | Disposition | Native evidence |
|---|---|---|
| A01–A04, A33–A35 | Verified, plus removal-path completion | `cargo test -p codegraph-installer` 14 unit + 18 contracts; `git_hooks_cli`, `uninstall_cli`, `branding` 11 tests. Shared AGENTS.md installed once for Claude+Codex; removal scans both Claude instruction candidates so a shadowing CLAUDE.md cannot strand a block. |
| A05 | Fixed: `init --git-hooks` opt-in, `uninit` removes owned blocks | `crates/codegraph-cli/tests/git_hooks_cli.rs` (2 tests: user content, sibling product, custom hooksPath, idempotent re-run) |
| A06 | Fixed: `uninit` stops a live daemon, waits 5 s for the endpoint, maps a still-locked removal to a clear diagnostic; help no longer promises agent-config cleanup | same CLI test; process-level daemon contention remains a real-runtime gate |
| A07 | Verified (prior batch) | `uninstall_cli` selected-target/remaining-consumer test |
| A08 | Fixed: plan → preflight all files → journal → write → rollback; `BEGIN`-free crash recovery only restores files still equal to the written content; `create_new` journal excludes concurrent installers; dry-run writes nothing | `transaction.rs` 6 unit tests; `apply_many` merges shared paths and refuses divergent content |
| A09 | Fixed in `install-rust.sh`: unique `mktemp` stage, mkdir install lock with 10-minute stale takeover, curl→wget fallback, failed latest-tag redirect falls through, migration failure reported | `bash -n`; functional POSIX run remains a platform gate |
| A10 | Fixed: SemVer §11 prerelease ordering (`rc.2 < rc.10`, numeric < alphanumeric) | `upgrade.rs` precedence-chain test |
| A11 | Fixed: upgrade refreshes the current project's local config only when it is already configured | source; process upgrade needs network |
| A14 | Fixed: reindex advisory now says `index -f`, not `sync` | `upgrade.rs` |
| A15 | Verified: impact SQL/adjacency budgets and CLI/MCP/viewer truncation | `deep_graph` bounded-impact tests |
| A16 / 60b0d7a9 | Fixed: globs anchored at `(?:^|/)` and end-of-path; `**/` spans zero or more folders | `wire.rs` glob table test |
| A17 | Fixed: one effective limit drives query, sentinel row, warning and output; effective/requested reported | source; CLI limits documented |
| A19 | Verified: 1 MiB frames, 32-entry queues, 32 connection cap | `session` frame tests; daemon suite |
| A20 | Fixed: ping/status/stop answered on the connection reader thread under a per-client write lock; bounded BFS/DFS/caller/callee/type walks (10k adjacency, 100k work) and truncation metadata | `daemon.rs` fairness test (client still in the broad query while ping/status answer); `deep_graph` high-fanout test |
| `2d98c1b6` | Fixed: definition-delta names join the failed-ref retry, and a deletion-only/rename-only sync runs it | `sync_removed_competitor.rs` (Python delete/rename/re-added competitor, TS delete/rename, full-index equality) |
| `4b0f72b7` | Fixed: a tracked node-less file that now indexes is reported as added so its importers' file-level edges reopen | `sync_size_cap_shrink.rs` (shrink, grow/back, parse-failure record, barrel, full-index equality) |
| `80f6b505` / `66449255` | Verified | `sync_barrel_retarget.rs` 3 tests |
| `f72c9d2e` | Already native: every content-touching sync runs `run_post_extract`, which clears all Ctx caches/memos before resolution | `sync_memo_lifetime.rs` (Go build-tag swap, Vue script export) passes with an extra sync-level clear deliberately disabled. `clear_caches` was completed for aliases/Go/workspace packages. |
| `d139865f` | Fixed: migrations use `BEGIN IMMEDIATE` and re-read the version under the lock | `migration_concurrent_opens.rs` (3 tests); reproduced the UNIQUE failure when reverted |

Executed gates: `cargo test -p codegraph-installer` (all), `-p codegraph-graph` (all), `-p codegraph-db` (all), `-p codegraph-core` (all), `-p codegraph-resolve` (all), `-p codegraph-mcp` (all), `-p codegraph-cli` (all). Focused: `--test daemon`, `--test git_hooks_cli`, `--test uninstall_cli`, `--test branding`, `--test sync_*`, `--test migration_concurrent_opens`, `--test cpp_overload_reentry`, `--test jvm_cross_language_imports`. Not yet run: full workspace in one command, explore_render/migrate gates, parity fingerprint, real-host/platform runs.

Remaining checklist now: A21 protocol matrix, A22 viewer packaging (preview), A23–A32 release/ops, remaining language/framework ledger rows, and production-plan measurements.

## Fourth batch: language/framework accuracy and module-config scope

| ID / upstream | Disposition | Native evidence |
|---|---|---|
| `f72c9d2e` (Cargo scope) | Fixed: the module-config fingerprint now includes the root and member Cargo manifests (members, package, target paths, deps by name/path); a manifest-only sync re-opens Rust resolution; `refresh_module_configuration` re-reads file content and clears module memos first | `sync_module_config_cargo.rs` (path-dependency retarget with no source edit; glob member; version-only edit leaves state unchanged). The retarget test fails when the fingerprint is disabled, so it is sensitive. |
| `f72c9d2e` (C++ compile DB) | Intentionally not implemented: native `load_cpp_include_dirs` is a stub, so there is no compile-database resolution to invalidate. Recorded as a feature gap, not a sync regression. |
| `0d7480f8` | Fixed: argument-based C++ overload choice has a re-entry guard keyed by the call site; a re-entered choice takes the first overload | `cpp_overload_reentry.rs`; before the fix this test process died with 0xC00000FD (stack overflow) |
| `787eafbf` | Fixed: JVM imports resolve by qualified name across Java/Kotlin, walk nested classes, `INSTANCE`, `Companion` and Kotlin file facades (`UtilsKt` → `Utils.kt`); Kotlin extensions are in scope only by an import of the extension or its package; Java/Kotlin imports of a method (static import / top-level extension) are importable | `jvm_cross_language_imports.rs`; `kotlin_extension_on_imported_type.rs` still passes. Intentional difference: an explicit Java constructor is reached by native's `instantiates` edge to the class, not upstream's `calls` edge to the constructor node. |
| `60b0d7a9` / A16 | Fixed (prior batch) | `wire.rs` glob test |
| A17 / A18 | A17 fixed; A18: `index --quiet` implemented, `init --yes` documented as compatibility (no prompt exists), hidden `init --index` retained as the reference's deprecated no-op | CLI help/source |

Executed: `cargo test -p codegraph-core` and `-p codegraph-resolve` after each change; both green. Full workspace and release gates still pending.

## Fifth batch: more language accuracy, protocol coverage and measurement

| ID / upstream | Disposition | Evidence |
|---|---|---|
| `59eb0d28` | Fixed: a module's export index now excludes object-literal members, prefers module-level bindings for local export clauses/defaults, and recognizes explicit destructuring exports (`export const { f } = d`) so that member remains importable | `import_object_member_namesake.rs` (object before/after, clause, re-export, namespace, default, destructuring, object/class negative controls) |
| `37052b18` | Fixed: Go var/const spec nodes carry `isExported` from the first letter of their own name | `go_package_value_exported.rs` (flags, cross-package calls, hidden unresolved, in-package calls) |
| A17 SQL plan follow-up | Fixed: bounded adjacency used a bound `LIMIT ?`, which SQLite planned 2× slower than the same literal; all budgets are inlined and the unfiltered outgoing read keeps `INDEXED BY idx_edges_source_kind` | `bench-before-after.md`; corpus A candidate now within 8% of baseline (was 2× slower on callers/bfs), corpus B still −72% |
| A21 | Covered: malformed envelope/version/method/params cases, unsupported protocol version answered with the implemented version | `crates/codegraph-mcp/tests/protocol.rs` |
| A23 | Added read-only native PR CI on Linux/macOS/Windows with workspace tests, release build smoke and the Windows installer recovery script | `.github/workflows/rust-pr.yml`; not yet executed on GitHub Actions |

Measurement record: `bench-before-after.md` (baseline `8182ee40`, candidate working tree, same DB/toolchain, raw samples in `bench-*.json`). Baseline vs candidate on this repo: search +8.8% (path unchanged; repeat runs within noise), callers +2.4%, impact +4.7%, flow −2.0%, bfs +7.6%, identical result counts. High-fanout 40k-caller graph: 1,351 ms → 384 ms (−72%), 40,000 → 10,000 relationships at the safety cap. Index time/RSS, real-agent token A/B, Linux/macOS execution and agent-host testing remain unmeasured gates.

Cleanup: temporary benchmark worktree/target removed; temporary `query_micro` example removed; curated run logs in `implementation-logs/`.

| ID | Disposition now |
|---|---|
| A22 | Intentionally preview: release archives still omit viewer assets; `CODEGRAPHV_UI=1` needs assets from a build checkout. Not changed. |
| A24 | Partially fixed in the eighth batch: release archives and `SHA256SUMS` are attested; SBOM and an executed Actions run remain open. |
| A25 | Fixed: a pinned upgrade fetches the installer from the selected tag and falls back to the branch for older releases (`installer_url_for_tag` + unit test); no network run performed. |
| A26 | Not changed: the broad upstream parity checkpoint remains stale by design; per-domain fingerprints need oracle runs. |
| A27 | Not done: no coverage/fuzz campaign, no per-function reviewed ledger. |
| A28 | Partially measured: graph query before/after in `bench-before-after.md`; candidate index time/RSS added (2,000 files: 7.1 s cold, ~2.0 s reindex, ~46 MB peak). Real-repository index/RSS and real-agent token A/B remain open. |
| A29 | Known baseline: `cargo fmt --all -- --check` still reports pre-existing diffs; no workspace reformat applied (see final gate). |
| A30 | Addressed in this record: `audit/2026-11-03-overall/progress.md` is the native/reference disposition ledger; root guidance remains the build/architecture contract with its TypeScript paths marked reference-only. |
| A31 | Intentional: no arm64 Linux/Windows artifact. |
| A32 | Not run: no fresh RustSec/license/CVE scan or fuzzing; existing linker warnings remain. |

Release blockers (must not be described as complete): A22 packaged viewer, A24 provenance, A27/A28/A32 validation breadth, A29 formatting baseline, Linux/macOS and real-agent-host runs, and the unexecuted compile-database scope of `f72c9d2e`.

## Final gates executed

- `cargo test --workspace --locked`: **exit 0, 890 passed, 0 failed** across 117 result blocks (`workspace-final.log`).
- `cargo test -p codegraph-mcp --test explore_render`: 18 passed. `cargo test -p codegraph-cli --test migrate`: 9 passed.
- `cargo build --release --locked -p codegraph-cli`: exit 0; `codegraphV 0.0.9`.
- `cargo fmt --all -- --check`: exit 1, pre-existing workspace formatting debt (133,058 diff lines); no reformat applied.
- `node scripts/rust-golden/check-upstream.mjs`: exit 1, review-required stale fingerprints, not a newly observed differential failure.
- `git diff --check`: exit 0 (only Git's LF/CRLF notices).
- A08 additionally preserves an existing file's permissions across the atomic rename and keeps the journal private (0600) on POSIX.

Still not executed: Linux/macOS runs, live agent-host install/upgrade/uninstall, real-agent token A/B, real-repository differentials for the new language cases, index-time/RSS, dependency/CVE/fuzz scans, viewer packaging, provenance.

## Sixth batch: call sites, overload spread, dot-dot paths and closed stdout

| ID / upstream | Disposition | Native evidence |
|---|---|---|
| `fa03fa6b` | Fixed: `CodeGraph::get_call_sites` returns one row per caller/callee with every call site and a `recursive` flag; the old `get_callers`/`get_callees` shape is unchanged. MCP caller/callee text, the `codegraphV_node` trail and CLI JSON carry the lines (callers at call sites, callees at their declaration) and `calls itself`. Text caps one row at 10 sites, the node trail at 3, with an explicit `+N more`. | `crates/codegraph-graph/src/traversal.rs`; `crates/codegraph-mcp/tests/call_sites.rs` (3 tests: multi-site, recursion both directions, text/trail caps). `codegraphV`/`codegraphV_node` MCP + CLI suites green. |
| `9f379af7` | Fixed: `codegraphV_node` caps at 16 bodies plus the first definition of every omitted language, lists at most 20 more ordered by language then top-level folder, and replaces a bare overflow with grouped counts (`+365 more: 365 typescript in aaa_client/`) and an honest 15,000-character truncation note. A set that fits all caps reads as before with `Returning N in full`. | `crates/codegraph-mcp/src/render.rs`; `crates/codegraph-mcp/tests/node_overload_spread.rs` (402-definition fixture reproduces upstream's 17/20/365 split, plus the fits fixture). One existing protocol assertion updated from "2 definitions" to the new full-result header. |
| `a38a1ec0` | Fixed: a path leaves the project only through a `..` segment or as an absolute path. Added `nodepath::leaves_root` and replaced the prefix checks in path aliases/baseUrl, workspace package targets, project ignore check, scan discovery, watcher dir/change filtering, Python declared roots and Laravel view paths. | `crates/codegraph-core/tests/dotdot_folder.rs` (alias maps `..generated/client`, drops `../outside/client`, indexes the folder and links the call; baseUrl in `..generated`); `nodepath` unit test; full workspace check clean. |
| `e4314919` | Fixed: `session::client_gone` turns BrokenPipe/ConnectionReset/ConnectionAborted/NotConnected (and Windows 109/232, 10054) into a normal `Ok(())` for direct and proxy stdout, so RAII drops the engine and releases the writer lock instead of exiting with an error. | `session` unit test with a writer that always returns BrokenPipe; `serve` returns Ok. Real child-process pipe teardown on Windows is a platform gate. |
| `22cf7d3a` | Intentionally different: the native watcher callback runs on its own thread; a panic ends that thread, not the host process. There is no V8 last-resort handler to port. | Source inspection. |

Executed: `cargo test -p codegraph-mcp` (all, including `call_sites`, `node_overload_spread`, `protocol`), `-p codegraph-cli` (all), `-p codegraph-graph`, `-p codegraph-core`; `cargo check --workspace --all-targets --locked` clean. Workspace-wide rerun and gates follow this batch.

## Seventh batch: resolution accuracy across JS, CommonJS, Spring and Python

| ID / upstream | Disposition | Native evidence |
|---|---|---|
| `f1da755a` | Fixed: `JS_HOST_GLOBALS` / `JS_WINDOW_MEMBERS` gate calls whose receiver is the browser's own object, written directly, through `window`/`globalThis`, or as a bare name with the receiver read from the source line. A name bound/imported/taken as a parameter still links, `this` stays the instance, destructured parameters (`({ document }: Props)`) count as bindings, and a project object hung on `window` (`window.App.init()`) still resolves. | `crates/codegraph-core/tests/browser_host_globals.rs` (4 upstream fixtures); `enclosing_class_member_call` regression caught the `this` normalization and is green. |
| `8e854d11` | Fixed: `exports["getItems"]` / `module.exports['deleteItem']` name and own the function; computed (`[key]`) and template keys stay unnamed/non-functions. | `crates/codegraph-extract/tests/commonjs_exports.rs` (dotted + quoted, JS and TS, negative controls). |
| `3d19da2f` | Fixed: the route scanner now reads the method declaration the annotation sits on — package-private Java, Kotlin `fun`, generic/modifier forms, and stacked annotations with nested arguments — instead of requiring `public|private|protected`. A same-named overload is chosen by the declaration that contains the reference line, not by nearest start line. | `crates/codegraph-core/tests/spring_route_handlers.rs` (3 upstream fixtures: package-private, overload under a tall stack, Kotlin generic/internal). |
| `cd04b21a` | Fixed: Python base lists split on top-level commas and a base's type arguments are stripped, so `Repo(Base[User])` extends `Base` only. A call whose receiver names a project class resolves that class's/inherited/descendant member or gets no edge, never a type argument's namesake (`User::save`). | `crates/codegraph-core/tests/python_supertype_type_arguments.rs` (all upstream assertions: class/receiver/nested/keyed/forward bases, `Base::load`, `Pair::load`, `Mixin::save`). |

Executed: `cargo test -p codegraph-resolve -p codegraph-core` green after each change (all existing resolution tests included). These changes also exposed a pre-existing reference-line preference in `find_best_match`; it is now explicit and covered by the Spring overload test.

## Eighth batch: release provenance, docs and measurement

| ID | Disposition now |
|---|---|
| A24 | **Partially fixed**: `rust-release.yml` now grants `id-token`/`attestations` and attests every release archive plus `SHA256SUMS` with `actions/attest-build-provenance@v4` (matching the TypeScript release flow). Not executed on GitHub Actions. A generated SBOM is still missing. |
| A28 | **Partially measured**: candidate index of a 2,000-file synthetic TypeScript corpus is 7.1 s cold, ~2.0 s forced reindex, peak RSS ~46 MB (`bench-before-after.md`). No baseline index binary, no real-repository index/RSS and no real-agent token A/B. |
| A30 | **Addressed in this record**: `audit/2026-11-03-overall/progress.md` is the native/reference disposition ledger; the root AGENTS.md remains the build/architecture contract and its TypeScript paths are explicitly reference-only. No root guidance rewrite. |
| A22 | Unchanged: viewer assets are a gated preview and are not in release archives. |
| A26 | Unchanged: the broad parity checkpoint still exits 1 by design. |
| A29 | Unchanged: `cargo fmt --check` remains red at the pre-existing baseline; no workspace reformat. |
| A31 | Unchanged: no Linux/Windows arm64 artifacts. |
| A32 | Unchanged: no RustSec/license/CVE scan (tools are not installed); no fuzzing. |

Executed gates after the seventh/eighth batches:

- `cargo test --workspace --locked`: exit 0, **933 passed, 0 failed** across 132 result blocks (`workspace-final-tenth.log`).
- `cargo test -p codegraph-mcp --test explore_render`: 18 passed. `cargo test -p codegraph-cli --test migrate`: 9 passed.
- `cargo build --release --locked -p codegraph-cli`: exit 0; `codegraphV 0.0.9`.
- `cargo check --workspace --all-targets --locked`: exit 0.
- `cargo fmt --all -- --check`: exit 1, pre-existing workspace formatting debt; no reformat applied.
- `node scripts/rust-golden/check-upstream.mjs`: exit 1, review-required stale fingerprints (documented baseline).

Release blockers remain: A22 packaged viewer, A24 SBOM and an executed attestation run, A26 broad parity, A27 coverage/fuzzing, A28 real-repository/agent measurement, A29 formatting baseline, A31 arm64, A32 advisory scan, Linux/macOS and live agent-host runs, real-repository differentials for the new language cases, and the compile-database scope of `f72c9d2e`.

## Ninth batch: Liquid names and MCP identity

| Upstream | Disposition | Native evidence |
|---|---|---|
| `a0c6c1b1` | Fixed: Liquid `assign` and `capture` names accept a hyphen after the first character (but not at the end), so `product-title`, `meta-description`, `stock-level` and `promo-code` are indexed whole in braced and `{% liquid %}` bodies. | `crates/codegraph-extract/tests/liquid_hyphen.rs`; `templates.json` expected vectors updated for `bad-name`/`ghost-name` (now indexed) in LF/CRLF/emoji variants, while `tight-` stays unindexed. |
| `f4c4dae9` | Fixed: `initialize` `serverInfo` carries `title: "CodeGraphV"` and an inline `data:image/svg+xml;base64,…` icon with `mimeType`, matching the reference's no-network identity. | `crates/codegraph-mcp/tests/protocol.rs` (name/title/version/icon assertions) |

## Tenth batch: Python receiver/base scope, template defaults and serve compatibility

| Upstream | Disposition | Native evidence |
|---|---|---|
| `b700a3ee` | Fixed: a Python parameter/local/`with`/`for`/`except`/walrus/comprehension/lambda-body binding named like an imported module no longer links the module. Function-local imports resolve only inside their own function; module-level from-imports are used everywhere else. Incidental words in strings/keyword args/`case Point(other=1)` do not bind. | `crates/codegraph-core/tests/python_receiver_rebinding.rs` (all upstream assertions; `in_a_string`, `lambda_beside`, `keyword_pattern` and the scope matrix included). |
| `3fca009e` | Fixed: `python_bases` strips `Base[User]` to `Base`, so a parameterized base still links its class. | `python_supertype_type_arguments.rs` asserts `Repo(Base[User])` extends `Base`. |
| `274b43c4` / `c0b49062` | Fixed: outside-import bases resolve no project namesake, plain/parameterized/dotted/aliased; inherited members also follow already-resolved `extends` edges, so a class in a file the import path names still reaches its helper. | `crates/codegraph-core/tests/python_outside_bases.rs` (full upstream fixture: bases, class calls, receiver calls, shadow/fallback/star/relative imports). |
| `27c904b0` | Already native: `python_base_list` reads wrapped/commented/PEP 695 headers. | `crates/codegraph-core/tests/python_multiline_bases.rs` (globals, class calls, decorated classes, wrapped generic base). |
| `bea5e205` | Already native: PEP 695 parameter lists, bounds, `*Ts`/`**P` and wrapped parameter lists are skipped before the bases. | `crates/codegraph-core/tests/python_pep695_bases.rs` (all upstream assertions). |
| `ed6b88e5` | Fixed for the described defect: `export default` text inside a template/string/regex no longer names the default. `mask_js_noncode` is line-terminator aware and falls back to raw source when a construct cannot be scanned; comments stay readable for the export-list quirk. | `crates/codegraph-core/tests/default_export_template.rs` (issue/render/write/plain/unbalanced); existing `later-exports` golden still green. The larger default-import matrix beyond template text is not claimed. |
| `47526d20` | Fixed as compatibility: `serve --no-telemetry` is accepted; the native runtime has no telemetry to disable. | `crates/codegraph-cli/tests/executable.rs` spawns `serve --mcp --no-watch --no-telemetry` and checks the handshake. |

## Eleventh batch: C, C++ and Objective-C rank as one language

| Upstream | Disposition | Native evidence |
|---|---|---|
| `6d3804cf` | Fixed: a lone candidate's confidence uses language family, and ranking treats C, C++ and Objective-C as one language (one extra point for the very language, not enough to beat a nearer directory). TypeScript/TSX and Java/Kotlin stay separate for ranking. | Before the change, `Row.tsx` → `formatDate` was 0.5 and `main.cpp`/`window.m` did not link the two-definition C names. `crates/codegraph-core/tests/language_family_ranking.rs` (4) and `name_matcher::tests::c_cpp_and_objc_rank_as_one_language_and_other_families_do_not`. `cargo test -p codegraph-resolve` 166 passed. Related core tests (`language_family_ranking`, `same_module_copy`, `c_fnptr_languages`, `enclosing_class_member_call`, `cpp_overload_reentry`, `code_unit_ties`) passed. Real glfw/imgui differential not run. |

## Twelfth batch: Ruby `!`/`?` methods and calls on what `.new` made

| Upstream | Disposition | Native evidence |
|---|---|---|
| `3a4cd3bf` | Fixed: a call whose method ends in `!` or `?` enters method matching, and a call on `Const.new(...)`, `A::B.new` or `::Foo.new` is recorded as `new Const().method` and resolved to that class's instance method (or the first its ancestry declares). A missing class or method links nothing. Receiver-word scoring stays off for a `!`/`?` name on a lowercase value. `include RouteSet.new.url_helpers` is not a mixin. | Before the change, `links_bang_and_question_methods_on_a_typed_local`, `links_a_bang_class_method` and `links_calls_on_what_new_made_by_constant_lookup` found no calls. After, `crates/codegraph-core/tests/ruby_constructed_receivers.rs` (4) passed, including the undeclared-class, misread-mixin and `options.key?` guards, and `Twin.new.build` reaching the instance method while `Twin.build` stays the class method. `cargo test --locked -p codegraph-resolve --lib`: 166 passed. The upstream sync case (a parked call links once the constructed class is added) was not run. No sinatra/jekyll/rails differential. |

## Thirteenth batch: JVM dotted supertypes and Kotlin object expressions

| Upstream | Disposition | Native evidence |
|---|---|---|
| `25144678` | Fixed: a Java, Kotlin or C# supertype written as `Outer.Inner` resolves to the nested type the file can name (same file, package or namespace, or an import), choosing the closest directory when several match and linking nothing on a tie or when the file cannot name the root. A Java anonymous class keeps the written type. A Kotlin object expression is an anonymous class: its overrides are members, a constructor in its header instantiates the type it names, and a call inside it can use a property of the surrounding class. An override is not what a same-named call inside it means, and a call outside the object's reach does not land on that override. | Before the change, seven cases failed: Java `Named` and C# `Named` had no supertype, Kotlin `KNamed` extended `KOuter` instead of `KOuter.KInner`, and the Kotlin object-expression cases recorded no anonymous class. After, `crates/codegraph-core/tests/jvm_dotted_supertypes.rs` (9) passed, including the already-green unnamed-root and `Visitor.a` → `Visitor.b` guards. `cargo test --locked -p codegraph-resolve --lib`: 166 passed. Related core tests (`jvm_cross_language_imports`, `kotlin_extension_on_imported_type`, `kotlin_import_same_name`) were not re-run. No okhttp/SystemUI/Minestom differential. Sync is not part of this commit. `codegraph-kernel` was left unchanged. |

## Fourteenth batch: Ruby instance variables typed by `initialize`

| Upstream | Disposition | Native evidence |
|---|---|---|
| `cdfa31be` | Fixed: a call on `@name` written in an instance method of a class links an instance method of the one class that class's `initialize` constructs for it (`@name = Const.new`, `||=`, a namespaced constant, arguments, a block, or a statement modifier). A conditional, two classes, `.tap`, a multiple assignment, a missing assignment, a class the project does not declare, a class method, and a module method stay untyped. A missing class links nothing rather than a namesake. | Before the change, `links_the_method_of_the_class_initialize_constructs` found no calls. The four guards (ambiguous, undeclared class, class method, module) already passed. After, `crates/codegraph-core/tests/ruby_ivar_receivers.rs` (5) passed. `ruby_constructed_receivers` (4) passed. `cargo test --locked -p codegraph-resolve --lib`: 166 passed. No jekyll/activesupport/actionpack/activerecord differential. Upstream noted three activerecord links follow `NullPool.new` from `initialize` after a later setter replaces the pool. |

## Fifteenth batch: Rust calls through a type a path or a `use` names

| Upstream | Disposition | Native evidence |
|---|---|---|
| `7021de5c` | Fixed: a call through a type a path or a `use` names links that type's own function, enum variant, or the trait's own method, not a namesake in another module or crate. A `use` of a module-level `pub const` or `static` links it. A type whose module path rustfmt wraps onto the next line follows that path. A commented-out path does not. A Cargo workspace member whose normalized path leaves the project (`../outside`, an absolute path, `crates/../../escape`) is not read or listed. A folder named `..generated` is. A path dependency whose normalized directory stays inside the project still resolves. Same-module `Type::member` with no `use`, `Self::`, and a type alias stay on the previous matcher. | Before the change, `Handle::current` linked `src/driver.rs`, `Mode::Fast` and `Codec::encode` linked nothing, cross-crate `ItemPool::for_def` linked nothing, `use sim::RANK_COUNT` linked nothing, wrapped `scheduler::` / `Handle` linked the `use`, and the crate map included `outside`, `escape`, and `vendored-x`. After, `crates/codegraph-core/tests/rust_type_paths.rs` (2) passed. `cargo test --locked -p codegraph-resolve --lib`: 168 passed, including both member-path tests. `rust_module_scopes` (4) passed. No tokio/ripgrep/axum/clap/serde/dioxus/gleam differential. `codegraph-kernel` was left unchanged. |

## Sixteenth batch: Java calls on a static field

| Upstream | Disposition | Native evidence |
|---|---|---|
| `bc9291af` | Fixed: a call on a value a type holds in a static field links the method of that field's type, as the field's declaration reads with its own file's imports. The class a `new` initializer builds wins over the declared type, then an anonymous class the field holds, then the declared type. A field the type inherits counts. An enum constant's own override comes first. A Kotlin object's `INSTANCE` reaches the object's member. An interface's constants are indexed. A field of a library type links nothing. A static call on a nested type stays a call on that type. | Before the change, `r/Consts.java` indexed no constants, `ProjectInterpolators.LOCAL.getInterpolation` linked `com.lib::ProjectInterpolator`, inherited and nested field calls linked nothing, `Color.GREEN.hex` linked `s::Color::hex`, `Foo.CREATOR.createFromParcel` linked nothing, `KObj.INSTANCE.go` linked `r::Helper::go`, and `Integer.TYPE.getName` linked `s::Runner::getName`. After, `crates/codegraph-core/tests/java_static_field_receivers.rs` (3) passed. `java_receiver_type` (3) passed. `jvm_dotted_supertypes` (9) passed. No Android real-repo differential. `codegraph-kernel` was left unchanged. |

## Seventeenth batch: Rust modules a path attribute loads

| Upstream | Disposition | Native evidence |
|---|---|---|
| `cc179781` | Fixed: a module loaded with `#[path]` is the module that declares it. `use super::*` brings in that module's items, private ones included. `super::` and `crate::` start there. The path is read from the declaring file's folder, or from an inline module's own directory. A call through such a module reaches the file it names, including when two tests load one helper and when the folder is named `..generated`. An example, test or bench that `Cargo.toml` places at its own path is its own crate root. A module whose `#[cfg]` alternatives each name a file, or whose path a `cfg_attr` sets, links nothing. The later row that gives each `#[cfg]` alternative its own file of one item (`221d80c7`) is not part of this batch. | Before the change, `uses_private`, `uses_crate`, `pr_elom_rowmean`, `from_inner`, `uses_support`, `uses_support_too`, `via_gen` and the example `main` linked nothing, and `via_documented` linked `crates/core/src/documented.rs`. After, `crates/codegraph-core/tests/rust_path_attribute_modules.rs` (4) passed. `rust_type_paths` (2) passed. `rust_module_scopes` (4) passed. `member_path` (2) passed. No fast-mlsirm/dioxus/tokio differential. `codegraph-kernel` was left unchanged. |

## Eighteenth batch: Delphi chained calls and method pointers

| Upstream | Disposition | Native evidence |
|---|---|---|
| `063708ba` | Fixed: a call through a chain of fields and methods links the method of the class each step is declared to give, and links nothing when that class comes from outside the project. A constructor the class does not declare itself, such as `TLogger2.Create`, keeps the class so the next call is its method. An assignment into a field, property or variable of an event or other routine type is a reference, including `@Obj.Method`, while a plain value stays a call. A routine nested in another does not lend its local variables to the code around it. A name declared in a list (`Doc, Doc2: TDocVariantData`) reaches that type, and a pointer or `class of` alias is followed. A nested routine declared `forward` is found by the calls written before its body. | Before the change, `TLogger2.Create.Emit` linked `TDecoy.Emit` and `.Write` linked nothing, a written-out `function: Integer of object` was a call, the outer `Receiver.Run` linked `TLocal.Run`, and `Early` linked nothing. After, `crates/codegraph-core/tests/pascal_declared_chains.rs` (4) passed. Extract `units_routines_calls_and_function_values` passed. Resolve `parenless_names_and_condition_depths` passed. No SynEdit/HeidiSQL/mORMot2/DelphiMVCFramework/DWScript differential. `codegraph-kernel` was left unchanged. |

## Nineteenth batch: Python imports from a folder above a script

| Upstream | Disposition | Native evidence |
|---|---|---|
| `0f93575c` | Fixed: a `from` import of names a module in a folder above the script defines links that module, nearest folder first, and only when the file defines every imported name at module level. A module a project root provides still wins. A plain import, a star import, a missing name, a sibling folder, and a package above the script stay unlinked. Adding the module or giving it the imported name links the scripts on sync, and removing the file or the name unlinks them, matching a fresh index. The later row for a module beside a script inside a package (`0576044b`) is not part of this batch. | Before the change, `go` did not call `generate_vector` and `modules/vs/tests/with_a.py` had no import edges into `modules/vs/test.py`. `WithA` already extended `TestCase` by the class name. After, `crates/codegraph-core/tests/python_parent_folder_imports.rs` (3) passed. No redis differential. `codegraph-kernel` was left unchanged. |

## Twentieth batch: a const that hands a function on

| Upstream | Disposition | Native evidence |
|---|---|---|
| `9d3abb84` | Fixed: a top-level JavaScript or TypeScript const, let or var that only hands a function on links that function, so a flow continues through it. A conditional, `??`, `||`, a cast, a bare name, and a member of an imported namespace, object, class or stated default all link. An import of the const still calls the const. A const-alias hop does not spend the one unnamed bridge, so `getMime extname validateString` still reaches `validateString` when several classes define `extname`. A value that names data, a class, an unbound receiver, an unstated default, a package member, a call, a bind, a new, a generated file, or only another const that names no function links nothing. Sync links an alias written into a file, a target added after the alias, including among many new definitions and in a new file, and drops the link when the target is removed. The synced graph matches a fresh index. | The fixture asserts those links. There is no saved before-log from a run without the pass. After, `crates/codegraph-core/tests/const_alias_flow.rs` (27) passed. `cargo test --locked -p codegraph-resolve --lib const_alias`: 2 passed. `import_object_member_namesake` (3) passed. No vscode/vue/excalidraw differential. `codegraph-kernel` was left unchanged. |

## Twenty-first batch: a struct links the types its members are declared with

| Upstream | Disposition | Native evidence |
|---|---|---|
| `a0174cee` | Fixed: a C or C++ class, struct, union or alias references the types its members or the alias are declared with, so impact on a type includes what holds it. A value, pointer, array, tag, enum, function pointer, template argument and qualified name all link, including `std::vector<Foo>` linking `Foo` and not `std::vector`. An alias names the type it stands for, so `typedef struct redisObject robj` reaches holders of `robj *`, and an alias template is not followed through to the type it names. A function, variable or macro of that name, a builtin, a `std::` name, C's own type names, the holder's own name, `decltype`, `auto`, a template parameter and a namesake the class cannot see link nothing. In C, what the file declares or includes comes first, a nested struct is found by its own name, and a type only a source file defines is found for a header that file includes. The nearest declaration above a use wins, including both arms of an `#if`. A class local to a function is visible only inside that function. Members mint no field nodes. Adding the type's file on sync links the holder, and editing that file keeps the link. Objective-C is not part of this batch. | The fixture asserts those links. There is no saved before-log from a run without the pass. After, `crates/codegraph-core/tests/c_cpp_member_types.rs` (4) passed. `c_conditional_group` (1) passed. `cpp_overload_reentry` (1) passed. No redis/rocksdb/leveldb/fmt/jq differential. Native explore does not open a source window per edge, so the TypeScript filter that keeps these edges out of the rank walk was not ported. `codegraph-kernel` was left unchanged. |

## Twenty-second batch: a script imports a module beside it in a package

| Upstream | Disposition | Native evidence |
|---|---|---|
| `0576044b` | Fixed: a Python from-import links a module beside the script in a folder with `__init__.py` when that module defines every imported name, including `from pysim import HashTable, kLimit, run` and a dotted name such as `from sub.helper import assist`. A module the project root provides still wins. A missing name, a plain import, a standard-library name, a module that imports its own name, and a module only in a package above the file stay unlinked. Sync links the importer when the module is added or gains the name, and unlinks it when the file or the name goes away, including a dotted module and a package. An edit whose only importer is relative, or of a neighbour named after a standard-library module, matches a fresh index and does not take the forbidden link. The parent-folder import fixture still passes. | The fixture asserts those links. There is no saved before-log and no reopen spy; graph equality with a fresh index is the sync check. After, `crates/codegraph-core/tests/python_beside_package_imports.rs` (2) passed. `python_parent_folder_imports` (3) passed. No rocksdb or django differential. `codegraph-kernel` was left unchanged. |

## Twenty-third batch: a declare module answers its own package

| Upstream | Disposition | Native evidence |
|---|---|---|
| `4499d007` | Fixed: a TypeScript or JavaScript `declare module` answers its own package, a package nested under it, and a package that depends on it. A real module of the repository wins over that declaration, including in the stub's own package, and a name only the stub declares still links. A package whose module is only a declaration file stays behind the block. A sibling that does not depend, a test stub outside its own package's tests, and a `package.json` that only sets `type` stay unlinked or do not split the package. The nearest declaration the package can see wins. A relative import of the declaring file still reaches its type. A name match does not take a function a block in another package declares. Sync links importers onto a real module that appears and returns them to the stub when that file is removed. Adding a stub beside a real module leaves the importers on the real file. A single-package declaration of an unimplemented name still links. | The fixture asserts those links. There is no saved before-log and no reopen spy; graph equality with a fresh index is the sync check. The fixture does not cover a paths alias staying behind the block. After, `crates/codegraph-core/tests/ambient_package_modules.rs` (5) passed. No react, vue, astro, or vscode differential. No full workspace suite. `codegraph-kernel` was left unchanged. |

## Twenty-fourth batch: a function inside an object method is a local

| Upstream | Disposition | Native evidence |
|---|---|---|
| `9c5cf97e` | Fixed: a function declared inside a method of an object literal is a local of that block. A bare call beside it links that function, and the holder references it, when the literal is behind `satisfies` or `as const`, nested in another object, under a computed key, passed to a wrapper, or read off with `[key]`, and in a Vue component script. A call from a sibling method, from later in the file, or from another file stays unlinked. A function written directly in the literal stays a member: a bare name does not reach it, and `App.init()` does. A function nested in a method that has its own node is still called from that method. | The fixture asserts those links. There is no saved before-log. After, `crates/codegraph-core/tests/js_object_literal_locals.rs` (4) passed. No react or outline differential. A Svelte file was not given its own fixture. `codegraph-kernel` was left unchanged. |

## Twenty-fifth batch: a required module's member is that export

| Upstream | Disposition | Native evidence |
|---|---|---|
| `39fc0839` | Fixed: a member read through `require()` or a default import links the export of that name, including an assignment, a named function, a renamed function, a local function, an object member, a module that forwards another whole, a default import, and an ES module with no default or a default that is an expression. A name the module does not export links nothing. A declaration file's `export =` keeps that value. `module.exports = main` links the call of the binding to `main` and `main.extra` to `extra`. Sync links the member when the assignment appears and drops it when the assignment goes away. A stated default keeps its own member: a class static, an instance method, and a method of a typed value, ahead of a same-named export. | The fixture asserts those links. There is no saved before-log. After, `crates/codegraph-core/tests/js_default_binding_members.rs` (4) passed. No express, mocha, eslint, or react differential. `codegraph-kernel` was left unchanged. |

## Twenty-sixth batch: a Python chain links its last name

| Upstream | Disposition | Native evidence |
|---|---|---|
| `206449b2` | Fixed: a Python call through an imported class links the chain's last member when that member is on the class or a nested class. `HttpResponse.plain(self)` and `HttpResponse.get_default()` reach those methods, `HttpResponse.Inner()` constructs the nested class, and `HttpResponse.Inner.run(self)` reaches that method. A chain that leaves the class links nothing: `content.fset`, `get_default.cache_clear`, `Inner.objects.create`, and an indexed `Inner.objects.defer`. | Before the change, `HttpResponse.content.fset` linked `HttpResponse.content`. There is no saved before-log. After, `crates/codegraph-core/tests/python_dotted_chain_target.rs` (2) passed. No django, flask, or fastapi differential. `codegraph-kernel` was left unchanged. |

## Twenty-seventh batch: a type alias names the class a call uses

| Upstream | Disposition | Native evidence |
|---|---|---|
| `2393ea2f` | Fixed: a call on a value typed by a type alias links the method of the one class or interface that alias names. `BackendBridge`, `Maybe`, `Chained`, and `WithDefault` call `Bridge.send`. `Inherited` calls `Base.shutdown`. A nested function, a local, and a field typed by the alias do the same. The alias's own file decides which same-named class. A union of two types, a function type, and an array link nothing. | Before the change, `link.send` on `BackendBridge` linked nothing, and so did the nested function, the local, the field, and the alias that names a class in its own folder. There is no saved before-log. After, `crates/codegraph-core/tests/ts_alias_typed_receivers.rs` (4) passed. No react, nest, vscode, or excalidraw differential. `codegraph-kernel` was left unchanged. |

## Twenty-eighth batch: a Flow library is global to the JavaScript files Flow reads

| Upstream | Disposition | Native evidence |
|---|---|---|
| `6473f609` | Fixed: a `.js` script with a top-level `declare` statement is a Flow library. A Flow file names `ReadableStream` and `ConsoleTask` from those libraries without importing them. A TypeScript file does not take the library `ReadableStream`. A script that declares nothing, and a top-level `declare(...)` call, are not libraries. `declare module.exports` does not remove the globals. The library's own `stream: ReadableStream` stays on the library class when another file exports the same name. | Before the change, `start` linked nothing. The TypeScript file, the plain script, and the `declare()` call already linked nothing. There is no saved before-log. After, `crates/codegraph-core/tests/flow_library_globals.rs` (4) passed. No react or metro differential. `codegraph-kernel` was left unchanged. |

## Twenty-ninth batch: a package import follows the page, and stops at a monorepo's own edge

| Upstream | Disposition | Native evidence |
|---|---|---|
| `3bd99495` | Fixed: a JavaScript or TypeScript file that reads `document` or `window` imports the package entry's browser build. A server file, a comment, a string, and a property like `res.document` stay on the node build. A folder whose `package.json` or `pnpm-workspace.yaml` declares workspaces does not call the outer workspace's packages or globals. A member of the outer workspace, and files outside the nested root, still do. | Before the change, a page linked `client.node.js`, a nested root linked `packages/core/src/index.js:useThing` and `scripts/jest/jest.d.ts:expect`, and a pnpm nested root linked `useThing`. The server file, the comment and string, files outside the nested root, and an outer workspace member already linked correctly. There is no saved before-log. After, `crates/codegraph-core/tests/workspace_browser_and_nested_roots.rs` (8) passed. No react, excalidraw, vscode, ai-chatbot, pot-desktop, vben, dub, flask, or gin differential. `codegraph-kernel` was left unchanged. |

## Thirtieth batch: a PHP receiver reaches the class PHP means

| Upstream | Disposition | Native evidence |
|---|---|---|
| `14c25e37` | Fixed: a PHP call on a value named like a class reaches the class the file can name, through the traits that class uses and then the parents its own file imports. A group use counts. A name the file cannot name does not walk a namesake. The one class of a name still reaches its ancestors. A declared type reaches the method that type extends. A class written under attributes still extends the parent its head names, on a declared receiver and on `$this`. `parent::` above a parent from outside the project does not take a project trait; `$this` may. A Laravel facade hands a static call to the class `getFacadeAccessor()` names with `::class`, and a container key names no class. A local an Eloquent `create` or `find` returns is that model. A static call on a class that is not a model does not type the local that way. | Before the change, `$entity->access()` linked `Drupal\views\Plugin\views::HandlerBase::access`, `$view->render()` linked `Illuminate\Contracts\Support::Renderable::render`, `Term` `id` linked nothing, and `ParallelTesting.token` linked nothing. There is no saved before-log. After, `crates/codegraph-core/tests/php_receiver_class_ancestry.rs` (4) passed. No framework, drupal, bookstack, koel, ospos, ci3, invoiceplane, or horizon differential. `codegraph-kernel` was left unchanged. |

## Thirty-first batch: a Ruby call follows a class's real ancestors

| Upstream | Disposition | Native evidence |
|---|---|---|
| `087de4ca` | Fixed: a Ruby call follows a class's real ancestors, in the order Ruby looks a method up. The module included last is asked first. A superclass written `ActiveRecord::Base` is that class. A receiver named after a class reaches a module that class includes. A nested class is that class alone. A value of a known class answers with the instance method. A call on the class reaches what it extends and the `ClassMethods` a concern includes. A bare method that takes no arguments and hands back one class types the call, including through a module the class includes. A name the caller binds, a method that takes arguments, or one that names no class does not. A `?` or `!` method of Ruby's core classes is not a project method the receiver only shares a module word with. Other predicates still follow the receiver word. | Before the change, `mapper.get` linked nothing, `company.has_attribute?` linked `ActiveRecord::AttributeMethods::ClassMethods::has_attribute?`, `cookies.delete` linked nothing, and `bad_cookies.empty?` linked `Sinatra::Cookies::Jar::empty?`. There is no saved before-log. After, `crates/codegraph-core/tests/ruby_receiver_class_ancestry.rs` (4) passed. `ruby_ivar_receivers` (5) and `ruby_constructed_receivers` (4) passed. No rails, sinatra, mastodon, jekyll, gin, or flask differential. `codegraph-kernel` was left unchanged. |

## Thirty-second batch: a C function that writes a macro before a block still uses it

| Upstream | Disposition | Native evidence |
|---|---|---|
| `b23c8805` | Fixed: a C function that writes a macro before a block, or before the next statement, still references that macro. The name stays as `name;` only on a statement of a function body, and the arguments stay blank. An initializer, a struct or enum body, another macro's arguments, the rest of an expression, and the unbraced body of `if`, `for`, `else`, or `do` are still blanked whole. Length and a `\r\n` line end stay. A later function still keeps the name when an earlier `#if` leaves a brace open. | Before the change, `mstrTest` did not reference `TEST_CONTEXT`, `h` did not reference `XXH_ASSERT`, and `list_for_each_entry` was blanked including its name. There is no saved before-log. After, `crates/codegraph-extract/src/preparse/conly.rs` (3) passed. The other preparse tests (35, including these 3) passed. No redis, rocksdb, glfw, jq, leveldb, gin, flask, betaflight, or linux-sub differential. `codegraph-kernel` was left unchanged. |

## Thirty-third to thirty-eighth batches: remaining language accuracy

Baseline for this continuation: the working tree left by the thirty-second batch; `cargo check --workspace --all-targets --locked` clean.

| Upstream | Disposition | Native evidence |
|---|---|---|
| `cb1e13d8` | Fixed: `c_macro_renames.rs` indexes object-like `#define name value` renames over headers (16 MB budget) and a source file's own defines, asks `cppIncluders` for reachability and `is_cpp_macro_defined_at` for the certainly-defined rule, then `match_cpp_declared_type` tries the renamed type first. `cpp_macro_visibility` gained a watched-macro timeline. | `crates/codegraph-core/tests/c_macro_renames.rs` (7 tests: include, indirect C/C++, own define, later include, core flag, no-type/two-arms, block comment). Script regex in PCRE2/regex parity. |
| `79c25b76` | Fixed: ObjC member and alias type refs are extracted; `is_objc_declared_type_ref` routes them to `match_by_exact_name`, which admits only type kinds and prefers the class's own header over a category/implementation in another `.m`; extraction excludes an unnamed member type. | `crates/codegraph-core/tests/objc_member_type_refs.rs` (5 tests: member kinds, no extends/function target, `.m`-local type, sync, header preference). |
| `9191ab66` | Fixed: `match_python_element_call` handles `...<subscript>].name()` before the method parser, requiring one project Python method of the name in the file or an imported module, rejecting std/list/dict names, dunders, test doubles and chains leaving the element. | `crates/codegraph-core/tests/python_subscript_receivers.rs` (6 tests). |
| `221d80c7` | Fixed: `RustModule.choices` carries one file per cfg alternative; `rust_submodule` follows each, `rust_cfg_choice_items` returns the name exactly one holds and nothing when two do; a `#[path]` target that is missing or absent still links nothing. | `crates/codegraph-core/tests/rust_path_attribute_modules.rs` (6 tests, now including per-cfg `imp`, `unix::GlobalOrphanQueue`, `park.rs` imports). |
| `608487e2` | Fixed: Go interface comparison parses parameter/result types (`go_item_types`, `go_comparable_type`, `go_method_types`), compares them per method set, treats true aliases and type parameters as open, and leaves list-bearing types and stdlib embeddings uncompared. | `crates/codegraph-core/tests/go_implements_types.rs` (all upstream assertions). |
| `5cd8a9a8` | Fixed: a lone method guess in another file is declined when the caller's own file assigns `X.prototype.<name> = function…`; the guess remains elsewhere. | `crates/codegraph-core/tests/js_prototype_member_guess.rs`. |

Executed after these batches: `cargo test --locked -p codegraph-core --test c_macro_renames`, `objc_member_type_refs`, `python_subscript_receivers`, `rust_path_attribute_modules`, `go_implements_types`, `js_prototype_member_guess` — all green. The workspace-wide, release-build and explore/migrate gates below were rerun for the checkpoint.

## Not done at this checkpoint

The port is **not complete**. `45ea6801` (Swift closure/value overloads) is not ported. `092e6cf8` (C# type-argument/keyword-receiver/extension-method rules) is only preparatory: attribute-aware declaration/arity and extension-method helpers exist in `name_matcher/csharp.rs` but are not wired into visibility, exact matching, the ASP.NET heuristic or `matchMethodCall`, and no test covers them; behavior is unchanged. The remaining upstream rows are the perf/Rust-worker, Kotlin, Java, Ruby-parameter-flow and earlier framework/installer/CLI rows listed in [upstream-ledger.md](upstream-ledger.md), plus the audit release blockers (A22 viewer packaging, A24 SBOM and an executed attestation run, A26 parity baseline, A27 coverage/fuzz, A28 real-repository/real-agent measurement, A29 formatting baseline, A31 arm64, A32 advisory scan, Linux/macOS and live-host runs). Do not read this checkpoint as full parity.





