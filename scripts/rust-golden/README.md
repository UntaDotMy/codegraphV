# Rust compatibility and performance workflow

The TypeScript engine remains a reference until the native replacement covers
all product surfaces. Preserve fixes from upstream by checking behavior against
the current source, rather than a stale `dist/` directory or old snapshots.

After updating the branch:

```text
node scripts/rust-golden/check-upstream.mjs
git log <previous-checkpoint>..HEAD --oneline
git diff <previous-checkpoint>..HEAD -- src __tests__
```

The checkpoint revision is printed by the audit. Exit status 1 means source or
native fingerprints changed, including additions/deletions. The audit groups
them by graph/search, extraction, resolution, remaining graph derivations, and
runtime/viewer. It records partial/pending status explicitly: an unported Go
resolver fix is work to preserve, not proof that the Rust resolver supports it.
Line-ending-only changes are ignored. The audit is local and never changes Git.

Once the relevant fixes have been ported/reviewed, run:

```text
node scripts/rust-golden/verify.mjs
```

This runs, in order:

1. Compile the **current** TypeScript engine and copy its schema.
2. Build the native graph, path-recognition and component-extraction probes.
3. Compare complete graph/search results on shared SQLite fixtures and refresh
   the portable vectors only after a scenario matches.
4. Compare path recognition, line anchors and exact callback traces.
5. Compare Svelte/Vue/Astro extraction, including delegated scripts, against
   current reference regressions and the viewer's real Svelte files; persist
   independent native fixtures and late-export boundary vectors.
6. Compare the grammar-free template extractors (Liquid, Razor, MyBatis XML,
   DFM) and the config languages they sit beside, including the literal inputs
   of the MyBatis robustness suite; persist their fixtures.
7. Compare native context retrieval, segment seeding, source extraction and
   markdown/JSON/tree/call-path formatting on shared databases; persist the
   reference outputs only after every comparison matches.
8. Compare git hook bytes/results and worktree diagnostics on real temporary
   repositories, including preserved user hooks and multiple indexed subprojects.
9. Compare the live watcher's clock-driven scheduler against TypeScript and
   persist its pending/retry/recovery snapshots for native replay.
10. Run `cargo test --workspace` against the persisted vectors.
11. Record revision, commands and SHA-256 source/native fingerprints on success.

The checkpoint validates implemented graph/search, Svelte/Vue/Astro extraction
and the grammar-free template extractors, plus context retrieval/formatting and
git hook/worktree utilities. It does not attest to the rest of
extraction/resolution/CLI/MCP/viewer parity. Their existing differential scripts
and regression suites remain required as each area lands. Test the new upstream
TypeScript regression files too, especially those outside the implemented Rust
surface. Never run Cargo builds concurrently with a probe executable on Windows.

The native tests do not need Node. Run `cargo test --workspace` independently
once the vectors have been recorded. Ranked search scores compare exactly;
the JSON parser enables accurate floating-point round-tripping. Optional
visibility/provenance nulls are normalized across the existing store adapters,
while node/edge identity, ordering, metadata and flow contents remain checked.

To run the context gate on its own:

```text
cargo build -p codegraph-context --example context_probe
node scripts/rust-golden/context.mjs target/debug/examples/context_probe.exe --write
cargo test -p codegraph-context -p codegraph-core
```

Omit `.exe` on Unix. This checks hybrid retrieval and graph budgets, segment
seeds, confidence, copybook entries, generated-file ordering, source extraction,
and exact markdown/JSON/tree/call-path output. The native facade tests additionally
index and incrementally sync real source files. Config-key redaction, path
traversal refusal and Windows junction escape have dedicated native coverage.
These checks do not assert MCP/CLI/viewer parity. The git utilities have their
own gate:

```text
cargo build -p codegraph-sync --example sync_probe
node scripts/rust-golden/sync.mjs target/debug/examples/sync_probe.exe
cargo test -p codegraph-sync
```

This covers hooks and worktree detection. Live watching has a separate gate:

```text
cargo build -p codegraph-sync --example watcher_probe
node scripts/rust-golden/watcher.mjs target/debug/examples/watcher_probe.exe --write
cargo test -p codegraph-sync -p codegraph-core
```

The scheduler gate compares 6,508 clock-driven snapshots, including scoped/full
sync selection, edits during sync, bounded retries and lock recovery. Large
pending sets retain samples plus a SHA-256 digest of the complete ordered list
in the portable fixtures. The native tests replay these without Node and also
exercise real OS delivery and background SQLite indexing. Windows tests cover
separate-process reads, cached-node refresh, directory junctions and retargeting.
Linux/macOS native-event validation remains pending; Docker is unavailable on
the Windows environment used for this validation run.

The reference engine may live in a separate upstream checkout. Set
`CODEGRAPH_TS_ORACLE_ROOT` to its absolute directory (with its Node dependencies
installed) before running audit/verify/perf. Reference sources and `dist/` are
then read/built there; Rust code, fixtures and checkpoints stay here. The
checkpoint records both revisions. This keeps the comparison usable after the
TypeScript implementation is eventually deleted from the native repository.

For performance:

```text
cargo build --release -p codegraph-graph --example graph_bench
node scripts/rust-golden/perf.mjs target/release/examples/graph_bench.exe report.json
```

Omit `.exe` on Unix. The executable refuses a debug build. Both engines query
the same 6,000-node, 30-file SQLite fixture. Warmup, repetitions and seven timing
samples are identical; output includes raw samples and median/p95 latency per
operation. The script checks result counts and excludes process startup/IPC.
Full answer correctness is covered by `verify.mjs` before timing.

Component extraction uses a separate release probe:

```text
cargo build --release -p codegraph-extract --example sfc_probe
node scripts/rust-golden/sfc.mjs target/release/examples/sfc_probe.exe --bench --report=sfc-report.json <corpus-dir>...
```

It compares every eligible component before timing, selects up to 100 evenly
spaced files per format, and checks observed output counts. The report retains
seven raw samples, input digests, selected file paths, corpus revisions, native
source/binary fingerprints, and the benchmark's limited scope. Each sample is
three serial extraction passes, following three warmups. Serialization, process
startup, IPC, indexing and resolution are excluded. Keep CPU-heavy builds/tests
out of a timing run. `verify.mjs <corpus-dir>...` also accepts extra corpora for
correctness checks, without timing them. Corpus files are never written into
the repository's portable fixtures.

Use these numbers to locate slow operations and re-measure an optimization;
Rust is not assumed to be faster just because it compiles natively. Preserve
prepared statements, bounded queries, batched neighbor reads, O(1) invalidation
stamps and iterative traversals. Benchmark representative real repositories
and end-to-end agent latency before making an overall speed claim: this fixture
does not measure extraction, indexing, MCP startup or browser rendering.
