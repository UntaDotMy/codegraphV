# Production quality: accuracy, speed, token efficiency and reliability

This is an implementation and measurement plan, not a claim that the proposed gains are already achieved. Preserve instruction/hook behavior from `preservation-plan.md` and fix confirmed correctness bugs before optimizing their output. No production code was changed in this audit.

## Definition of success

CodeGraphV should return enough correct, current evidence for the agent to finish its task with fewer total tool calls and less total context, while remaining responsive, bounded and recoverable. A smaller individual response is not a success if it causes more follow-up calls, wrong answers or reads of the same source.

Track baseline and candidate on identical pinned repos, toolchain, hardware, host/model settings and prompts. Record commit, binary hash, configuration, corpus hash, cache state, raw samples and failures. Use current native v0.0.9 as the initial baseline; separately compare with the pinned current upstream oracle. Historical README benchmarks are context, not new measurements.

## P01 — P1: establish the measurement harness before optimization

Extend existing `scripts/bench/` and `scripts/rust-golden/` harnesses instead of creating incompatible benchmark systems. Keep correctness tests separate from timing. Never build the measured Cargo target while its executable is running on Windows.

| Axis | Measure | Required workloads |
|---|---|---|
| Retrieval correctness | Exact symbol/file hit, required source spans, flow endpoint/waypoint coverage, wrong edges, omissions and stale results | Same-name distractors, overloads, negative/missing-file queries, Unicode, generated files, dynamic dispatch, monorepos |
| Latency | Process startup, initialize, first answer, warm query, sync-to-visible latency; p50/p95 and worst case | Small/medium/large repos, cold/warm cache, busy writer, concurrent clients, slow storage |
| Resource use | Peak and steady RSS, CPU time, DB/WAL sizes, open handles, threads, queued work, cache bytes | Large generated files, repeated edits, long sessions, idle/reconnect cycles, several projects |
| Token efficiency | Full-session input/output/cache-read/cache-write usage where host exposes it; total tool calls; Read/Grep fallback | Same task and model for both arms; successful and no-answer tasks; no CLI contamination |
| Answer usefulness | Correct final answer, supporting source/paths, next action after explore, repeated source, unnecessary retrieved bytes | At least three flow prompts per representative real repo; judge correctness independently of token count |
| Lifecycle | Install/refresh/uninstall preservation, crash/restart, interrupted migration, upgrade handoff | Windows/macOS/Linux, fresh home, custom paths, active sessions and partially configured agents |

Use multiple interleaved trials, at least seven samples for deterministic microbenchmarks and repeated agent runs per repository guidance. Report raw variance and failed/time-out trials; do not drop them. Sum usage over turns with care for host accounting; do not substitute only the last-turn usage or character counts. Model/tokenizer and cached-input pricing can change, so record their versions/rates for cost comparisons.

## P02 — P1: make graph correctness and freshness the foundation

Prioritize the sync/cache and language-scope backlog in `upstream-ledger.md`. For each port add both a correct edge assertion and negative lookalikes that must not link. Distinguish static edges from heuristic ones and retain provenance/wiring evidence.

Build a full-index versus incremental-sync differential suite: add/delete/rename files; retarget exports/barrels; change tsconfig/jsconfig/go.mod/workspace/package manifests; change framework detection; alter sparse checkout; cross file-size limits. Compare normalized complete nodes/edges, unresolved references and metadata after each sequence, not just counts. Verify extraction-version changes cause a clear rebuild requirement for unchanged source.

For new framework synthesis require an end-to-end flow across every introduced boundary. Partial dispatch bridging that forces another source search is not acceptable completion. Add confidence/ambiguity diagnostics where reliable resolution is impossible instead of silently selecting a same-named method.

Acceptance: all targeted upstream regression inputs pass; no new false-positive edges on control corpora; sync converges to fresh-index behavior or explicitly reports a required rebuild. Never optimize by quietly reducing semantic coverage.

## P03 — P1: bound work and keep the service responsive

Implement A15/A19/A20: graph node/edge/work budgets, bounded protocol frames/queues and fairness/cancellation. Bound adjacency retrieval before allocating a huge fanout. Propagate explicit truncation and continuation information through CLI, MCP and viewer.

Profile memory ownership in session source history, project pools, resolver caches and synthesis passes. Use byte-based cache limits where entry sizes vary. Invalidate on real source/config/version changes; never use an eviction strategy that returns stale lines or skips necessary continuation state.

Acceptance: a pathological query cannot monopolize all clients indefinitely; cheap lookup/ping and shutdown retain a tested latency budget under load; queues and memory plateau under a sustained bounded workload. Resource limits must not turn truncated output into a falsely complete answer.

## P04 — P2: optimize the hottest query and database paths

Measure query plans and allocation/CPU profiles first. Candidate work: repeated symbol/adjacency reads, N+1 lookups, unnecessary complete-node materialization, redundant sort/copy operations, repeated JSON schema parsing, regex compilation and source reads. Reuse prepared statements and batch bounded ID lookups. Verify indexes with SQLite query-plan inspection and include their write/storage cost.

Use generation-aware source caches keyed to project/file content identity, including replacement/rename and config invalidation. Reuse line tables for multiple ranges from one file. Avoid holding unrelated source files across an entire session. Preserve exact Unicode/UTF-16 compatibility where the reference contract requires it.

Acceptance: identical query results/source spans and ordering where contractual; statistically credible p95 improvement on the targeted workload; no material regression on small repositories or cold startup. Do not add cache complexity for an unmeasured micro-optimization.

## P05 — P2: improve indexing and live-sync throughput

Profile scan, decode, parse, persist, resolve, synthesize and checkpoint phases separately. Audit repeated whole-project scans and source copies. Share immutable detection inputs, batch DB writes, bound in-flight source bytes and schedule slow files without blocking all ready work. Tune worker count for memory bandwidth and physical cores, not just logical CPU count.

Investigate synthesis invalidation and upstream synthesis-interval work. Coalesce edits while preserving eventual convergence and honest freshness reporting. Avoid full synthesis when an exact affected subset can be proven; retain full fallback when dependency scope is uncertain. Benchmark high-churn saves, branch checkout, mass rename and dependency/config edits.

Acceptance: peak memory and end-to-end index/sync time improve on representative corpora without changed graph semantics; cancellation, failed-file accounting and WAL recovery remain correct. A faster parser with slower resolution is not an overall win.

## P06 — P2: reduce total agent tokens through more sufficient answers

1. Rank exact named files/symbols and qualified scopes ahead of lexical lookalikes; preserve overload diversity and requested waypoints.
2. Return the essential complete mechanism/body and connected flow first; avoid repeating signatures already visible in source and unrelated graph neighborhoods.
3. Make misses precise and brief. Do not answer an unindexed explicit filename with unrelated fuzzy matches. Do not invent a path through missing edges.
4. Reuse already-shown source within a session using content hashes and explicit ranges; invalidate after edits. Keep flow/state explanations needed to understand the current answer.
5. Provide deterministic continuation arguments, including oversized Unicode lines. Never shorten away needed code without saying where it continues.
6. Bound history storage without forgetting enough information to re-send large unchanged files repeatedly. Measure memory and cumulative tokens together.
7. Measure initialization guidance, tool schemas, prompt-hook injection and host-supplied AGENTS.md separately; reducing tool output does not reduce all host context.
8. Avoid arbitrary minification or removing line numbers/provenance. Retain verifiable evidence and clear truncation. A token estimate must name its tokenizer; bytes/chars alone are a proxy.

Acceptance: same or higher independently judged task success and required-source recall, lower full-session tokens and fewer fallback reads on the same prompts. Preserve the repo's sufficiency target (roughly zero Read/Grep within its expected explore-call range), treating call ranges as evaluation guidance rather than a hard refusal quota. Do not lower source budgets solely to manufacture a token-saving result.

## P07 — P2: reduce prompt-hook overhead and redundant context

Measure how often the hook is useful, wasted or duplicated by the next MCP explore. The hook currently creates its own source-history map; its injected source is not automatically the MCP session's history. Investigate a session-aware, content-hash-based handoff only if host/session identity can be trusted and retained safely. Do not mark code as already shown merely because another user/session saw it.

Add bounded stdin, explicit short timeout, fast kill switches and cheap rejection of non-user/non-code prompts (A12/A13). Compare full injection with concise symbol hints on real tasks; select policy by measured success and total tokens, not assumption. Provide transparent opt-in/out and preserve the user's choice on upgrade.

Acceptance: useful context arrives inside the chosen prompt latency budget; irrelevant prompts do negligible work; busy/corrupt/unavailable indexes fail open; hooks do not duplicate large source across turns without a measured benefit. Benchmark host default behavior as well as the optimized path.

## P08 — P1: production reliability and installation safety

Complete preservation matrix and A01–A08/A33–A35 before describing setup as non-destructive. Validate concurrent installers, transactional recovery, active-runtime uninit, version handoff, stdout closure and stale locks using real subprocesses. Record daemon lifetime and cleanup expectations; no orphaned writers, unexplained index resurrection or lost user configuration.

Use loopback-only viewer tests, traversal/symlink/junction boundaries, config redaction and malformed protocol/config fixtures. Run parser/protocol/config-editor fuzzing with bounded time/memory. Add graceful diagnostics for unsupported grammar/platform, oversized files and incomplete indexes.

Acceptance: successful rerun after interrupted operation, no user data loss, no silent half-configured success, and no state corruption under concurrent read/sync/rebuild scenarios. Crash safety is a release gate, not a performance tradeoff.

## P09 — P2: release and operational quality

Add native PR CI, pinned toolchain, formatting/lint policy, dependency/advisory/license scanning, per-platform tests and reproducible release smoke checks (A23–A32). Execute the unpacked release with an empty home and no developer toolchain; check version, init, query, MCP connect and uninstall. Include artifact provenance and verify checksums/attestations in the documented install flow.

Diagnostics should explain stale indexes, exclusions, skipped parse failures, truncation and effective resource limits. Prefer local opt-in structured timing/counters with no code/secrets; do not introduce remote telemetry to a product documented as telemetry-free. A diagnostic command should report health without mutating user projects by default.

Document supported host/platform versions, benchmark limitations, migration/reindex requirements and rollback. Keep reference/oracle dependencies out of native runtime claims. Viewer launch requires its separate packaged-assets/browser gate.

## Proposed release gates and execution order

Thresholds below are starting proposals to calibrate from the harness, not already measured service guarantees:

| Gate | Proposed criterion |
|---|---|
| Correctness | All targeted regressions and negative controls pass; no unexplained full-vs-sync graph drift; independently judged task success does not fall |
| Preservation | Every supported target/scope preservation and interruption case passes; no opted-out permission/hook re-enabled |
| Latency | No reproducible p95 regression greater than 10% on a control corpus; targeted optimizations show improvement beyond variance |
| Memory | No unbounded growth over a fixed-workload long-session soak; record peak RSS and configured cache/queue caps |
| Tokens | At equal task success, report actual cumulative usage and fallback calls; accept a token-saving claim only after repeated paired runs |
| Recovery | Forced termination, dropped connections, concurrent writer contention and interrupted upgrade/install recover predictably |
| Platforms | Windows/Linux/macOS native runtime and install tests; publish only architectures whose actual artifact was smoke-tested |

Order: **(1)** baseline harness and preservation fixes; **(2)** sync correctness and boundedness; **(3)** hot-path/index profiling and improvements; **(4)** retrieval/token and hook experiments; **(5)** real-host/platform soak plus release evidence. Parallel work can be assigned by the user's implementation agent, but integration must rerun shared correctness and lifecycle gates.

For each change, record: hypothesis, measured bottleneck, owner functions, failing fixture or baseline, proposed mechanism, correctness risk, raw before/after results, rollback strategy and final acceptance. Keep performance claims narrow when only a microbenchmark has been measured.
