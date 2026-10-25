# Native graph queries

This crate ports the graph algorithms used by CodeGraph's CLI, MCP tools and
viewer. It reads the shared SQLite database directly and makes no changes to it.
It is a library layer in the Rust migration; the Rust CLI and MCP server are
still pending.

```rust
use codegraph_db::{OpenOptions, Store};
use codegraph_graph::GraphTraverser;

let store = Store::open(".codegraph/codegraph.db", OpenOptions { read_only: true })?;
let graph = GraphTraverser::new(&store);
let callers = graph.get_callers("the-indexed-node-id", 2)?;
```

`GraphTraverser` covers BFS/DFS, callers, callees, call graphs, impact, paths,
containment and usages. `GraphQueryManager` adds node context, file dependencies,
cycles, metrics and filtered subgraphs. `type_hierarchy` supplies the complete
bounded hierarchy/override derivation shared by the viewer and MCP.

`named_symbol_flow` and `symbol_lookup` use `SymbolLookupHost` for ranked symbol
search. The native `Store` implements this contract, including FTS5, LIKE/fuzzy
fallbacks, exact-name supplementation and shared relevance scoring. `StoreLookup`
also permits a custom search context, for example project path deprioritization.
Both named and directed flow modes follow recorded calls and navigation edges,
including synthesized ones, and keep the original overload and bridge rules.

Run the native tests without Node:

```text
cargo test -p codegraph-graph
```

The persisted golden tests contain TypeScript-generated results for 8,597
queries. The live comparison runs both engines against the same database and
also checks complete node fields (normalizing the existing store adapters'
absent/null optional visibility and provenance). Ranked-search results are
computed natively and checked against the TypeScript engine, including scores
and order; the flow tests also check the native search ordering they use.

To rebuild the latest oracle, refresh vectors, test the workspace and record an
upstream compatibility checkpoint, run sequentially:

```text
node scripts/rust-golden/verify.mjs
```

Check later upstream/native drift with `node scripts/rust-golden/check-upstream.mjs`.
The separate chain test exercises 3,000 nodes to check
that native depth-first walks use heap stacks rather than the thread stack.

For repeatable performance measurements (Windows; omit `.exe` on Unix):

```text
cargo build --release -p codegraph-graph --example graph_bench
node scripts/rust-golden/perf.mjs target/release/examples/graph_bench.exe report.json
```

This compares warm queries within each process on a 6,000-node synthetic graph,
checks answer counts, and reports median/p95 timings. It does not measure indexing,
MCP startup, or end-to-end agent latency. Re-run on real indexed projects before
generalizing an optimization to production workloads.
