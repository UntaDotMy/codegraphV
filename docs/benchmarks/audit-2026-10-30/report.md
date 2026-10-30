# Reliability and retrieval audit — 2026-10-30

Audited the Rust codegraphV implementation at baseline `c645adc0`, including retrieval,
database lifecycle, agent configuration, migration, installation scripts and validation
tools. The TypeScript tree remains the compatibility reference. Changes are in the
working tree; no release or global installation was performed.

## Findings addressed

| Priority | Gap | Change and evidence |
|---|---|---|
| P1 | The native build bundled SQLite 3.50.2, before the WAL-reset corruption fix. | Both database consumers now share rusqlite 0.39.0 / bundled SQLite 3.51.3; a test checks the linked engine's version. |
| P1 | A Windows install could rename the working executable away and then fail to copy its replacement. | Copying finishes before the old executable moves; a failed activation restores it. Four isolated PowerShell scenarios cover copy, rename and activation failures plus success; Windows CI runs them. |
| P1 | Any existing `.codegraphV/` could cause the original index to be removed, including an empty or corrupt destination. | Cleanup requires an independent destination, a successful SQLite quick check, usable graph tables and a completed index marker. Empty, corrupt, unrelated and unfinished destinations preserve the original; Windows junction aliases are refused. |
| P1 | Migration could uninstall the old program and state after configuration or index migration failed. | Failures and configuration inspection warnings now return a nonzero status before uninstalling the original program or settings; the new configuration is preflighted before removing the old entry. |
| P1 | A failed copy could clean up a destination created by another migration after planning. | The fallback must create the destination exclusively before copying or removing it; a regression simulates that directory appearing between planning and execution. |
| P2 | Retries within one second could overwrite the first configuration backup. | Backup directories use a high-resolution timestamp, process ID and exclusive creation with collision retries. |
| P2 | Every query allocated a map containing all indexed paths. | Whole paths without whitespace resolve before map construction; plain symbol queries bypass it. Mixed queries build the map once. Unicode, whitespace and case-collision behavior remain covered. |
| P2 | Exact source continuations rebuilt symbol scopes and repeated dependency listings. | Empty symbol lists return immediately; source-only ranges omit dependency summaries while keeping source and line numbers. |
| P2 | A missing requested path could disappear from a response that matched other symbols. | Explore reports unresolved indexed paths before presenting other matches, including when there are no remaining matches. Expected misses remain successful tool results. |
| P2 | Root agent guidance pointed first to the TypeScript implementation and npm workflow. | An early Rust section identifies production sources, Cargo commands, native tool guidance, installer tests and release paths. README troubleshooting covers existing setup and query commands. |
| P3 | README response-size estimates were labeled as output tokens. | The table explicitly labels its characters/4 estimate; provider token usage and whole-session cost require separate measurement. |

The database fix follows [SQLite's WAL-reset advisory](https://www.sqlite.org/wal.html#walreset)
and [rusqlite's 0.39.0 release](https://github.com/rusqlite/rusqlite/releases/tag/v0.39.0).
SQLite documents a rare race involving simultaneous writes/checkpoints on multiple
connections; this audit found a vulnerable bundled version, not observed corruption.
The index format and schema version remain unchanged.

## Research informing the changes

| Primary source | Relevant finding | Application here |
|---|---|---|
| [Lost in the Middle, TACL 2024](https://arxiv.org/html/2307.03172v3) | Relevant information can become harder to use in longer contexts; the value of more context is task-dependent. | Remove repeated continuation metadata while preserving requested source and existing budgets. |
| [SWE-agent, 2024](https://arxiv.org/html/2405.15793v3) | Compact actions, informative concise feedback and clear recovery improve the agent interface; extra turns also have a cost. | Keep explore as the primary tool, return actionable missing-path notices and preserve exact continuation arguments. |
| [RepoGraph, ICLR 2025](https://arxiv.org/html/2410.14684v2) | Its larger flattened neighborhoods performed worse than smaller ones; summarizing the smaller graph also lost useful information. | Keep explicit flow and verbatim source; avoid increasing graph fan-out or adding lossy source summaries to claim token savings. |
| [RepoCoder, EMNLP 2023](https://arxiv.org/html/2303.12570v3) | Iterative retrieval improves repository completion in its setting, but additional iterations introduce latency and stopping challenges. | Make existing requests and continuations cheaper; avoid introducing additional mandatory tool stages. |
| [Anthropic advanced tool use](https://www.anthropic.com/research/advanced-tool-use) | Loading tool definitions and intermediate results into context can consume substantial space; on-demand discovery reduces that overhead. | Preserve the existing single exposed primary tool by default. |
| [SQLite backup guidance](https://www.sqlite.org/backup.html) | Live database copies require SQLite-aware snapshots; `VACUUM INTO` is a supported approach. | Retain the existing SQLite-aware locked-index copy, rather than copying only the main database file. |

These papers motivate design choices; their benchmark improvements are not measurements
of codegraphV. The measured changes below do not establish a model-level token reduction
or an agent task-success improvement.

## Measured results

Windows x64, AMD Ryzen 5 2600; release builds with the same benchmark example added to
the baseline before production changes. See [raw samples and binary hashes](results.json).
Values are medians across 27 warmed samples per arm; each sample averages five calls.

| Complete explore call on this indexed checkout | Before | After | Response characters | Source lines |
|---|---:|---:|---:|---:|
| `sync index_all` | 25.686 ms | 25.741 ms | 22,076 → 22,076 | 294 → 294 |
| `crates/codegraph-core/src/codegraph.rs:667-698` | 4.580 ms | 2.414 ms | 2,793 → 2,421 | 32 → 32 |
| `crates/codegraph-core/src/codegraph.rs:667-667` | 4.279 ms | 2.194 ms | 697 → 325 | 1 → 1 |

The selected exact-source requests were **1.90× and 1.95× faster**. Their responses were
**13.3% and 53.4% smaller**, respectively, after removing dependency metadata. The named
flow's entire answer hash stayed identical, with effectively unchanged latency. These
are warm retrieval timings, not whole agent-task timings.

| Path-recognition helper only, synthetic index | Before | After |
|---|---:|---:|
| Plain symbols, 5,000 paths | 1.401 ms | 0.016 ms |
| Plain symbols, 40,000 paths | 12.122 ms | 0.156 ms |
| Mixed filename + symbol, 100 paths | 0.021 ms | 0.022 ms |
| Mixed filename + symbol, 5,000 paths | 1.415 ms | 1.445 ms |
| Mixed filename + symbol, 40,000 paths | 12.100 ms | 12.607 ms |

All helper answer hashes match. A small mixed-query overhead remains: about 0.03 ms at
5,000 paths and 0.51 ms at 40,000. This change does not make every query faster. Raw
results also include exact-path helper cases that deliberately target the last entry;
their very large speedups reflect the reverse scan finding that entry immediately,
and are not representative of complete explore latency.

## Validation and reproduction

The full native workspace suite passed on Windows: **669 passed, zero failures** across
77 test/doc-test groups. One helper is marked ignored because its parent test invokes it
in a separate process. Subsequent migration and MCP regression runs also passed,
including the added Windows junction case. Tests exercise real SQLite, real source
indexing and isolated temporary homes; they do not modify real agent configuration.
The Windows installer recovery harness also passed all four scenarios, using local
download substitutes and real file operations without touching PATH or the registry;
its temporary install path includes literal brackets. After rebuilding the TypeScript
reference, **8,597 live graph queries** and **1,630 path-recognition queries and callback
traces** matched it. Golden fixtures were not rewritten.

```text
cargo test --workspace --locked
cargo test --locked -p codegraph-cli --test migrate
cargo test --locked -p codegraph-mcp --test explore_render --test instructions
cargo build --release --locked -p codegraph-mcp --example retrieval_bench
cargo build --release --locked -p codegraph-cli --bin codegraphV
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-install-rust.ps1
node scripts/rust-golden/query-paths.mjs target/debug/examples/query_paths_probe.exe
node scripts/rust-golden/graph.mjs target/debug/examples/graph_probe.exe
```

The release benchmark example can also time any indexed project and query arguments.
For this checkout, preserve its baseline executable before rebuilding the candidate,
then run:

```text
node scripts/bench/retrieval_audit.mjs target/audit/retrieval_bench-before.exe target/release/examples/retrieval_bench.exe . target/audit/retrieval-comparison.json
```

Each arm runs three times in alternating order, with nine samples of five calls after
warm-up. Path-extraction answer hashes must match; explore source-line counts are
checked, and separate regression tests verify verbatim source and rereading ranges.
Binary hashes, sample timings and scope are retained in the raw report. Character
counts are response sizes, not tokenizer counts. Startup, indexing, IPC and model calls
are outside this benchmark.

## Remaining gaps and next priorities

1. **Filtered search recall:** path/name filters are currently applied after candidate
   truncation in `crates/codegraph-db/src/search_queries.rs`. Code inspection shows that
   a valid match outside the initial candidate set can be missed. Move filtering into
   candidate selection with Unicode-aware compatibility tests; increasing output
   limits alone would not fix the underlying recall issue.
2. **Unsupported file names:** requesting the unindexed root-level file
   `install-rust.ps1:1-200` can still fall back to unrelated lexical matches. The new
   notice covers unresolved directory-qualified paths; detecting file intent for bare
   names needs to distinguish real files from dotted symbol names without weakening
   path validation or returning another symbol's source as that file.
3. **Session history memory:** source-line history is content-aware, but has no total
   byte/file cap and survives project eviction. Add a measured memory bound with
   eviction tests that preserve explicit rereads and useful continuation behavior.
4. **Configuration rollback:** individual installer writes are atomic, but a group of
   files is not a transaction. An I/O failure after the first write can still leave
   partially updated agent configuration. Migration now preserves the original program
   and backups; automatic multi-file rollback or a restore command remains useful work.
5. **Further latency work:** explore still reads the tracked file records for each call.
   Profile this separately before introducing caches; any cache must notice external
   writes, index replacement, deletions and rollbacks.
6. **Broader evidence:** run model-level A/B evaluations with real provider token usage,
   latency, fallback reads and answer quality, and rerun native platform-sensitive
   cases on macOS/Linux. This audit ran on real Windows; no Linux/macOS result is claimed.

These are specific follow-ups, not proof that all remaining code paths are covered.
