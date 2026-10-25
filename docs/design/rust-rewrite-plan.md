# Rust rewrite — plan, crate map, and gates

**Branch:** `rust-rewrite`. **Status:** in progress — see the checklist at the bottom.
**Not to be confused with** `rust-kernel-migration-plan.md`, which covers the *napi
accelerator* (`codegraph-kernel/`) that the TypeScript build loads. This plan retires
the TypeScript implementation altogether; the kernel's per-language walkers are
the head start, not the destination.

## What is being ported (measured 2026-10-22)

| Area | Lines | Notes |
|---|---:|---|
| `src/resolution/` | 74,403 | `name-matcher.ts` alone is 25,716 — the largest single unit |
| `src/extraction/` | 33,106 | `tree-sitter.ts` 8,857 + ~30 language extractors; 29 vendored `.wasm` grammars |
| `src/mcp/` | 16,865 | `tools.ts` 8,568; daemon, proxy, explore diagnostics |
| `src/ui-server/` | 12,839 | read-only JSON API + static server for the browser viewer |
| `src/db/` | 7,457 | `queries.ts` 5,244 (≈60 prepared statements), migrations v2–v16 |
| `src/graph/` | 6,758 | traversal, branch guards, dead code, named-symbol flow |
| `src/installer/` `src/upgrade/` `src/bin/` | 10,000 | agent targets, upgrade, CLI (commander) |
| `src/context/` `src/search/` `src/sync/` `src/telemetry/` | 5,900 | |
| **`src/` total** | **174,157** | |
| `__tests__/` | 202,992 | ~710 suites; they encode bug-for-bug behavior and are the spec |
| `ui/` (Svelte viewer) | 25,047 | browser code — see "Decisions" |
| `codegraph-kernel/` (already Rust) | 28,967 | walkers for 20 languages behind a napi boundary |

## Strategy: parity port, TypeScript as the oracle

1. **Behavior parity, not redesign.** Same `.codegraph/codegraph.db` schema (an index built
   by either implementation opens in the other), same node-ID scheme, same MCP tool
   output, same CLI surface. The suite and `AGENTS.md`'s retrieval-performance rules
   exist because small output changes send agents back to `Read`.
2. **Additive until the end.** All Rust lives under `crates/`; nothing in `src/` is
   edited or deleted until the Rust side passes the gates below. The TypeScript build
   stays runnable as the differential oracle the whole time.
3. **Reuse the kernel.** `codegraph-kernel/src/*` already mirrors the TypeScript extractors
   bug-for-bug for ts/js/java/python/go/c/cpp/rust/csharp/ruby/php/swift/kotlin/r/lua/luau/
   scala/dart. They are folded into `codegraph-extract` with the napi flat-buffer boundary
   replaced by native `ExtractionResult` values; their parity tests carry over.
4. **Gates, in order** (each is a command, not an opinion):
   - `cargo test --workspace` green.
   - `scripts/dump-graph.mjs` diff of a Rust-built DB against a TypeScript-built DB is
     empty on a fixture set (extraction first: nodes/files; then full: edges/refs).
   - MCP tool-output snapshots equal for `codegraph_explore` / `codegraph_node` on the same DB.
   - The TypeScript suites are ported suite-by-suite to `cargo test`; a suite counts as
     ported when its assertions run against the Rust implementation.

## Crate map

| Crate | Ports | Depends on |
|---|---|---|
| `codegraph-types` | `src/types.ts`, node-ID scheme | — |
| `codegraph-db` | `src/db/*` (schema, migrations, `QueryBuilder`, WAL healing) | types |
| `codegraph-extract` | `src/extraction/*` + `codegraph-kernel/` | types, db |
| `codegraph-resolve` | `src/resolution/*` (import resolver, name matcher, frameworks, synthesizers) | types, db |
| `codegraph-graph` | `src/graph/*` | types, db |
| `codegraph-search` | `src/search/*` | types, db |
| `codegraph-context` | `src/context/*` | graph, search |
| `codegraph-sync` | `src/sync/*` (watcher, git hooks) | extract, resolve |
| `codegraph-core` | `src/index.ts` (`CodeGraph` facade), `directory.ts`, `project-config.ts`, `utils.ts` | all of the above |
| `codegraph-mcp` | `src/mcp/*` | core |
| `codegraph-installer` | `src/installer/*`, `src/upgrade/*` | — |
| `codegraph-ui-server` | `src/ui-server/*` | core |
| `codegraph-cli` | `src/bin/*` — builds the `codegraph` binary | everything |

## Decisions made by default (revisit if wrong)

- **SQLite:** `rusqlite` with the `bundled` feature (real SQLite + FTS5 + WAL), replacing
  `node:sqlite`. No system library, no native build step for users.
- **Parsing:** native `tree-sitter` + grammar crates / vendored `parser.c`, replacing
  `web-tree-sitter` + `.wasm`. Grammars with no usable crate are vendored, as the kernel
  already does for dart/kotlin/lua/scala/swift. Revision parity is checked per grammar.
- **Concurrency:** evaluate a bounded `rayon` pool for parse fan-out at the indexing
  coordinator, replacing Node's worker lifecycle. Measure worker counts and mixed
  loads before choosing defaults; individual extractors stay synchronous. Native
  walkers retain their stack guards.
- **Viewer (`ui/`, Svelte):** the *last* item. The request to finish a full Rust port
  and remove all TypeScript includes replacing this browser surface with Rust/WASM.
  Until that replacement reaches parity, keep the existing assets available.
- **Deleting `src/`, `__tests__/`, `package.json`:** the maintainer has explicitly
  requested TypeScript removal after the port; no further confirmation is needed.
  Deletion still waits for every parity gate, so working features and regression
  coverage are not lost. Distribution (the npm thin-installer with per-platform
  packages, `install.sh`/`install.ps1`, the release workflow) is re-pointed at the Rust
  binary at the same time.

## Checklist

- [x] Workspace + `codegraph-types` (kinds, model, node IDs — 9 tests, TS-pinned ID vector)
- [x] `codegraph-search` pure helpers — identifier segments, query parser, query utils, generated-file detection, and query paths. Path recognition is verified against the TS oracle on 1,630 cases, including callback invocation order, bracketed routes, Windows paths, line anchors, budgets, and symbol-based ambiguity narrowing; persisted vectors run in `cargo test` without Node (`tests/query_paths.rs`). Database search/ranking and context retrieval remain separate work below.
- [~] `codegraph-db` — schema/migrations/connection lifecycle/bulk-load windows/heals/write paths/core reads done (25 tests). Verified against the TS engine: `scripts/rust-golden/xdb.mjs` (a file written by either engine reads identically in the other; `dump-graph` equal; same schema objects) and `scripts/rust-golden/migrate.mjs` (v1→v16, v9→v16, v11→v16 upgrades produce identical schema and rows). Resolver/search/explore-specific `QueryBuilder` queries are ported with their consumers.
- [~] `codegraph-extract` — **done and verified:** the kernel walkers (ts/js/py/java/go/c/cpp/rust/csharp/ruby/php/swift/kotlin/r/lua/luau/scala/dart), every `preParse` hook those languages have (C, C++, C#, Kotlin, Swift, Flow), the C++ macro-method pass, and the "parse produced no symbols" warning. Verified against the TS wasm extractor (`scripts/rust-golden/extract.mjs`, `preparse.mjs`, `tree-probe.mjs`): exact parity on Qt 6.9.1 headers (1,162 files, 528 with parse errors), Windows SDK + cargo-registry C/C++ (2,914 files, 1,633 with errors), a Unity C# tree (3,801 files), and this repo (1,305 files). Erroring files are parsed twice — UTF-8, then UTF-16 (`ts.rs`) — because web-tree-sitter reads UTF-16 and error recovery differs between the two. **File discovery is ported and verified** (`scan/`: a faithful port of node-ignore 7.0.5 — replacer pipeline hand-coded, regexes translated to PCRE2 with UTF-16-unit semantics; project config; built-in ignore defaults; `.gitignore`/`info/exclude`/`core.excludesFile` reading; embedded repos, worktrees, gitlinks; `include`/`exclude`/`includeIgnored`; git-backed listing, filesystem-walk fallback, change detection incl. `sinceCommit`). `scripts/rust-golden/ignore-diff.mjs` (328k path checks incl. 4.4k fuzzed rule sets) is identical to the real `ignore` package; `scan.mjs` + `make-scan-fixtures.mjs` (18 synthetic git/non-git fixtures: nested ignores, BOM/CRLF/non-UTF-8 `.gitignore`, embedded clones, worktrees, gitlinks, `codegraph.json` variants, sub-roots, parent-ignored roots, no-commit repos) plus this repo, a 19k-file cargo registry tree and a Unity tree match `scanDirectory`/`getGitChangedFiles`/`discoverEmbeddedRepoRoots` exactly. **Left:** the generic-extractor languages with no native walker (arkts, cobol, erlang, nix, objc, pascal + dfm, solidity, terraform, vbnet, cfml/cfscript), the remaining standalone extractors (liquid, razor, mybatis, yaml/twig/properties; Svelte/Vue/Astro are covered below), and the rest of the orchestrator (`ExtractionOrchestrator` in `src/extraction/index.ts`: hashing, rayon parse pipeline, store, sync reconcile — it needs resolution, so it lands with `codegraph-core`).
- [~] Single-file components — Svelte, Vue and Astro wrappers, shared script folding, Vue Options API members and template calls are native. `sfc.mjs` matches 4,678 extractions across reference regressions and eight real repositories; portable vectors and ownership/position/deep-input tests run without Node. The shared JS/TS walker scans later exports once per file; 2,700 reference checks pin alias/token/whitespace/line-ending behavior. Release timings and research decisions are in `docs/benchmarks/rust-sfc-decisions-2026-10-22.md`. This does not complete framework resolution or full runtime flow validation. Liquid, Razor, MyBatis and YAML/Twig/properties extraction remain pending.
- [~] `codegraph-resolve` — the resolver, language-aware name matcher, framework registry and whole-graph synthesis passes compile. Differential resolver probes exist; broad regression and real-repository validation remain before this area is complete.
- [~] `codegraph-graph` — native BFS/DFS, callers/callees, call graph, legacy hierarchy, impact, shortest paths, containment, usages, context, file dependencies, cycle detection, metrics, qualified-name wildcards, legacy unreferenced-symbol lookup, filtered subgraphs; shared bounded type hierarchy/override matches/implementation counts; synthesized-hop wording; qualified symbol lookup and grouping; Go package reader; named and directed symbol flows. `scripts/rust-golden/graph.mjs` compares **8,597 results** with the latest TS engine on the same SQLite files, including native ranked search (scores and order) and upstream #2785's dominant-file/route-file fix. These vectors run independently in `cargo test`; a separate 3,000-node chain validates native stack safety. `Store` implements `SymbolLookupHost` using native search; flow tests verify native candidate order against the TS reference, without substituting TS search results. **Left:** the richer dead-code report, AST branch guards, dynamic-boundary reporting, and COBOL copybook query derivation.
- [~] Database search/ranking — FTS5/BM25, LIKE and bounded fuzzy fallback, exact-name supplementation, field filters, kind/path/name ranking, project-name dampening, deprioritized paths, co-location-aware exact-name seeds and name substrings. Verified scores and order against TS; real-SQLite tests exercise FTS failure, read-only access and native search→flow. Dominant-file ranking memoizes against both `total_changes()` and `data_version`, bypasses the cache inside transactions, and reads generated flags in batches. Other context/retrieval-specific DB queries remain with their consumers.
- [x] `codegraph-context` — hybrid exact/prefix/text/infix/compound retrieval, segment-vocabulary seeding, confidence reporting, copybook entries, type-hierarchy expansion, traversal/diversity budgets, edge recovery, source extraction and markdown/JSON/tree/call-path output. `scripts/rust-golden/context.mjs` compares **8,196 results** against the current TypeScript engine over the same databases, including exact formatted JSON and markdown; portable fixtures run without Node. Config leaves expose keys only, path validation is shared with core, and native tests cover traversal refusal and Windows junction escape. This gate covers context retrieval, not the future MCP/CLI renderers.
- [~] `codegraph-sync`, `-core` — full/incremental indexing orchestration and the public read/query/context facade compile; native facade tests exercise real extraction, resolution, source retrieval and incremental rebinding. Segment queries preserve plural folding, co-occurrence, rarity thresholds and live-node verification. Git sync hooks and worktree detection are ported, including per-project blocks, legacy ownership, custom hook directories, shell quoting, interpreter checks, and the linked-worktree/embedded-repository distinction; `scripts/rust-golden/sync.mjs` compares **468 results and file snapshots** against TypeScript on real repositories. Windows path identity preserves Node's non-native realpath spelling (including 8.3 aliases), while security checks and nested-repository relative paths use native canonicalization. Remaining tool-specific query methods and broad runtime validation are still pending.
- [~] Live watching — native OS directory streams, bounded supplemental symlink streams, shared indexer scope, adaptive debounce, scoped/full reconciliation, pending-file status, bounded failure backoff and lock recovery are ported. `scripts/rust-golden/watcher.mjs` matches **6,508 scheduler states** against the current TypeScript watcher; portable fixtures replay without Node. `CodeGraph.watch` owns a background engine connection, drains it before close, invalidates foreground node caches after sync and follows replaced database files for readers. Real Windows tests cover edits, new directories, deletion, access-only/separate-process reads, external junction edits and link retargeting. Linux/macOS native-event validation and the POSIX replaced-inode test still need a real run; Docker is unavailable in this Windows environment, so this area is not marked complete.
- [ ] `codegraph-mcp`
- [ ] `codegraph-installer` (+ upgrade, telemetry)
- [ ] `codegraph-ui-server`, Rust/WASM viewer
- [ ] `codegraph-cli`, test port, packaging, retire TypeScript

## Following upstream updates

On 2026-10-22 the working branch was fast-forwarded from `88ebefa1` to
`0953084bc5b4db687c10417731c425873866f226`, preserving the uncommitted native port.
Six upstream fixes were reviewed:

| Update | Native disposition |
|---|---|
| #2785 dominant-file/route-file selection beyond a top-20 window | Ported and differentially tested, with cache invalidation/rollback tests |
| #2782 Go chained receiver types | Full resolver remains pending; keep the new regression suite as the specification |
| #2784 Go conversions, external test packages and build-tag twins | Resolver remains pending; the native Go walker was unchanged upstream |
| #2788 external dynamic imports | Name-matcher remains pending; do not resolve these by bare-name coincidence |
| #2787 Delphi chained receiver calls/typecasts | Generic Pascal extraction and resolution remain pending |
| #2783 oversized named-symbol rendering | MCP renderer remains pending; preserve source sufficiency and existing budgets |

`node scripts/rust-golden/check-upstream.mjs` reports changed source/native files
since the last recorded checkpoint, grouped by implemented and pending areas.
It does not fetch, switch branches, or silently mark pending features complete.
After fetching/reviewing upstream, `node scripts/rust-golden/verify.mjs` rebuilds
the latest TS oracle, builds the native probes, refreshes differential vectors,
runs component-extraction comparisons and workspace tests, and records new hashes only if every step passes.
Builds and probe runs are sequential to avoid Windows executable locking races.
See `scripts/rust-golden/README.md` for the repeatable workflow and benchmarks.
