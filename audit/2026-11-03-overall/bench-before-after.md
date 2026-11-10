# Before/after measurements (2026-11-03)

This is a narrow, reproducible measurement of the bounded-work changes in this batch. It is
**not** a real-agent A/B, a token measurement or a full-repository benchmark matrix; those remain
open (production-plan P01/P06). No claim below is inferred from response length.

## Setup

- Baseline revision: `8182ee40` (v0.0.9, detached worktree), release profile.
- Candidate: the working tree after the audit fixes, same toolchain (`rustc 1.99.0`), release.
- Binary under test: `crates/codegraph-graph/examples/graph_bench` (real native query APIs, in
  one process; parsing/startup/DB-open excluded). Each query runs `warmup=1`, then the stated
  samples; `repetitions` per sample.
- Hardware/OS: Windows x64, same machine and DB file for both arms, interleaved runs.
- Raw output: `bench-real-base.json`, `bench-real-head.json`, `bench-hub-base.json`,
  `bench-hub-head.json` in this directory.

### Corpus A — this repository's index

`.codegraphV/codegraph.db` (1,940 files, ~53.6k symbols, ~196k edges), read-only. Same database
for both arms; the candidate's schema changes are additive and compatible.

### Corpus B — synthetic high-fanout graph

80,004 nodes / 80,001 edges: `hubIn` has 40,000 incoming `calls` edges. Built with the product's
own schema, then `ANALYZE`. This exercises the safety budgets, not ordinary source code.

## Results — Corpus A (milliseconds per query, median of 7 samples × 200 repetitions)

| Query | Baseline | Candidate | Delta |
|---|---:|---:|---:|
| search `permission` | 0.517 | 0.563 | +8.8% |
| callers `ReferenceResolver::resolve_one` | 0.345 | 0.353 | +2.4% |
| impact `ReferenceResolver::resolve_one` | 0.322 | 0.337 | +4.7% |
| flow `resolve one` | 0.932 | 0.913 | −2.0% |
| bfs `ReferenceResolver::resolve_one` | 1.406 | 1.513 | +7.6% |

These are medians of per-batch averages, not per-request p95 measurements; they do not establish
the production plan's p95 gate. Result counts are identical (20/12/13/0/100), but counts alone
do not establish identical answers or accuracy. The search path is untouched by this batch;
a repeat of that query with 11 samples × 500
repetitions measured baseline 0.536 ms and candidate 0.528 ms in one interleaving and 0.532 vs
0.578 in another, i.e. the +8.8% above is run-to-run noise near the sub-millisecond floor, not a
reproducible regression.

## Results — Corpus B (milliseconds, median of 5 samples)

| Query | Baseline | Candidate | Delta |
|---|---:|---:|---:|
| callers `hubIn` (10,000 relationship budget) | 1,351 | 384 | **−72%** |
| relationships returned | 40,000 | 10,000 | bounded by design |

The candidate's caller walk stops at 10,000 adjacency edges and reports truncation through the
bounded API; the baseline materialized all 40,000 and returned them. The tool's caller limit is
20 by default (100 maximum), so the bound is a safety ceiling, not a reduction of the requested
answer. This is the intended trade: a pathological fanout cannot monopolize the daemon, and the
caller limit that users request is unaffected.

## An implementation note the measurement produced

The first candidate used `LIMIT ?` with a bound parameter. On SQLite that changed the plan for
outgoing adjacency (`idx_edges_identity` instead of `idx_edges_source_kind`) and doubled the cost
of even a 12-row query (raw SQL: 27 µs unlimited vs 53 µs with `LIMIT ?`; 29 µs with the same
limit inlined). The shipped code inlines the numeric budget (never user text) and keeps
`INDEXED BY idx_edges_source_kind` on the unfiltered outgoing read. The table above is after that
fix; the intermediate candidate was 2× slower on callers/bfs and was rejected.

## Reproduce

Build both revisions into separate target directories:

```powershell
# baseline
git worktree add --detach <scratch>/codegraph-base 8182ee40
$env:CARGO_TARGET_DIR = "<scratch>/base-target"
cargo build --release --locked -p codegraph-graph --example graph_bench

# candidate (from the working tree)
cargo build --release --locked -p codegraph-graph --example graph_bench
```

Run the same config through each binary:

```json
{"warmup":1,"samples":7,"repetitions":200,"queries":[
 {"op":"search","query":"permission"},
 {"op":"callers","id":"method:5699a947d9ea7d1149240e1d92b10bc0"},
 {"op":"impact","id":"method:5699a947d9ea7d1149240e1d92b10bc0"},
 {"op":"flow","query":"resolve one"},
 {"op":"bfs","id":"method:5699a947d9ea7d1149240e1d92b10bc0"}]}
```

```powershell
Get-Content config.json | & <exe> .codegraphV/codegraph.db
```

## Index time and peak RSS (candidate only)

There is no baseline index binary in this worktree (the baseline is a git worktree of the same
engine), so this is a reproducible candidate measurement, not a before/after claim.

Corpus: 2,000 generated TypeScript files (40 folders, one class + one function per file, about
300 KB) in an isolated temporary project. `codegraphV index <project> --force --quiet`, release
build, same machine, peak working set sampled every 20 ms.

| Run | Wall time | Peak RSS |
|---|---:|---:|
| first index (creates the database and FTS) | 7.1 s | not sampled |
| forced reindex, run 1 | 2.03 s | 46.6 MB |
| forced reindex, run 2 | 2.00 s | 45.5 MB |

The first run's extra time is database/schema creation; a forced reindex of the same corpus is
stable at ~2.0 s and ~46 MB. Real-repository index time and an RSS baseline remain open (A28),
as does a real-agent token A/B.
