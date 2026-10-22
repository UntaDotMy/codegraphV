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
2. Build the native graph and path-recognition probes.
3. Compare complete graph/search results on shared SQLite fixtures and refresh
   the portable vectors only after a scenario matches.
4. Compare path recognition, line anchors and exact callback traces.
5. Run `cargo test --workspace` against the persisted vectors.
6. Record revision, commands and SHA-256 source/native fingerprints on success.

The checkpoint validates graph/search; it deliberately does not attest to
extraction/resolution/CLI/MCP/viewer parity. Their existing differential scripts
and regression suites remain required as each area lands. Test the new upstream
TypeScript regression files too, especially those outside the implemented Rust
surface. Never run Cargo builds concurrently with a probe executable on Windows.

The native tests do not need Node. Run `cargo test --workspace` independently
once the vectors have been recorded. Ranked search scores compare exactly;
the JSON parser enables accurate floating-point round-tripping. Optional
visibility/provenance nulls are normalized across the existing store adapters,
while node/edge identity, ordering, metadata and flow contents remain checked.

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

Use these numbers to locate slow operations and re-measure an optimization;
Rust is not assumed to be faster just because it compiles natively. Preserve
prepared statements, bounded queries, batched neighbor reads, O(1) invalidation
stamps and iterative traversals. Benchmark representative real repositories
and end-to-end agent latency before making an overall speed claim: this fixture
does not measure extraction, indexing, MCP startup or browser rendering.
