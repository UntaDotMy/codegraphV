# Native retrieval review — 2026-10-27

The highest-confidence improvements were in the native Rust query and rendering
paths. The TypeScript implementation is the upstream reference; changing only it
would not improve the installed codegraphV executable.

## Findings and implemented changes

| Finding | Change | Expected benefit |
| --- | --- | --- |
| FTS sorted weighted `bm25()` results explicitly. | Use FTS5's ranked iterator with the same column weights, filters, limit and offset. | Less SQL work for common terms; identical scores in regression tests and the benchmark queries. |
| Explicit file paths did not consistently scope definitions and flow endpoints. | Prefer definitions in explicitly named files in both lookups; imports do not shadow remote implementations. | The displayed source and flow refer to the requested implementation, while cross-file helpers remain reachable. |
| Co-named-file scoring repeatedly scanned all overloads inside the sorting comparator. | Count distinct query-token matches per file once. | Less repeated CPU work without changing this score's ordering. |
| Rust advertised roughly 13–24K-character responses but allowed 28–38K for larger projects. | Align the defaults at 13K, 18K and 24K; preserve monotonic per-file source budgets. | Lower risk of host-side externalization or truncation. |
| Signatures, symbol lists and related-file metadata could overrun source budgets. | Bound metadata, keep complete flow paths and source lines, reserve footer and continuation space, and count source-gap markers. | More predictable responses with explicit omission notices. |
| Continuation could repeat preceding context forever, and explicit ranges could be suppressed by session history. | Honor exact requested ranges, allow explicit rereads, and return a strictly advancing range for omitted source. | Continuation works from both a persistent MCP session and fresh CLI invocations. |
| Oversized lines were reported but could not be retrieved through explore. | Return bounded character pages with Unicode positions and a content hash; preserve later definitions in the initial response. | Recover a long line exactly, with an edit detected instead of mixing two versions. |
| Whitespace splitting broke paths containing spaces. | Resolve an exact whole indexed path before tokenization. | Returned range and fragment queries work for such filenames. |
| Each edit left an obsolete content-hash entry in source-display history. | Remove prior content versions for that project/file; show edited source again. | Less retained session state during repeated edits. |
| File-source pagination repeatedly counted the entire accumulated response. | Maintain a running character count. | Linear output accounting instead of repeated full-buffer scans. |

Changes add no dependencies, network services or index migration. Existing
indexes remain usable. The build produced `target/debug/codegraphV.exe`; the
globally installed executable was not replaced.

## Audit findings

The follow-up audit first added failing regression tests and reproduced three
defects in the initial implementation: `main.rs:4-15` could repeat forever when
leading context consumed the next page, an explicitly requested line could be
hidden as already shown, and `Service.start Service.finish chosen.ts` could
display a flow through `a.ts` while showing source from `chosen.ts`. All three
now pass. Additional checks cover cross-file helpers, imports with the same
name as their implementation, and full reconstruction of oversized lines.

Agent instructions were corrected too: they no longer promise every overload
or diagnostic suggestions that the native renderer does not actually return.
They explain exact-range rereads and fragment continuation.

## Research used

SQLite documents that `ORDER BY rank` can be faster than ordering by `bm25()`,
especially with a limit, and supports per-query rank configuration. That is the
basis for the weighted-rank change, verified against the existing scores.
[SQLite FTS5: sorting by auxiliary function results](https://www.sqlite.org/fts5.html#sorting_by_auxiliary_function_results).

Anthropic recommends meaningful, bounded tool responses and pagination or range
selection when results consume too much context. This informed the explicit
source-range continuations and bounded metadata. It does not establish a
universal character limit across agent hosts.
[Writing effective tools for agents](https://www.anthropic.com/research/writing-tools-for-agents).

Anthropic's contextual-retrieval experiments support combining contextual
information, lexical retrieval and reranking, with latency/cost tradeoffs.
Those results concern its evaluated retrieval datasets; they do not establish
that embeddings or a remote reranker will improve this AST-based code graph.
Such additions need code-specific precision and end-to-end latency evaluations
before becoming a default.
[Contextual Retrieval](https://www.anthropic.com/research/contextual-retrieval).

## Measurements

Read-only microbenchmark on this repository's existing `.codegraphV/codegraph.db`,
Windows, Python 3.13.6 / SQLite 3.50.4. Each query returns 100 complete node rows and scores.
Three warm-up rounds, then 30 measured executions per arm, alternating execution
order. The script checks exact returned tuples before measuring.

| FTS prefix | Previous median | Ranked median | Ratio |
| --- | ---: | ---: | ---: |
| `get` | 26.364 ms | 6.090 ms | 4.33× |
| `render` | 8.360 ms | 2.564 ms | 3.26× |
| `search` | 9.174 ms | 3.031 ms | 3.03× |
| `node` | 73.119 ms | 16.230 ms | 4.51× |

Reproduce with:

```powershell
python scripts/bench/fts_rank.py .codegraphV/codegraph.db
```

These are SQL microbenchmarks using Python's SQLite build, not whole-agent or
native-CLI speedups. The audit upgraded the benchmark to materialize full node
rows and hold one read snapshot for both arms. Native database tests separately
verify weighted-score equivalence, tied results, kind/language filters,
pagination, read-only operation and docstring-only FTS matches so a silent LIKE
fallback cannot make a broken ranked query look successful.

## Validation and limits

The initial database/MCP run passed 61 tests. The audit expanded validation to
search, graph, database, MCP and the executable CLI, including existing golden
oracle fixtures, protocol behavior and process lifecycle tests.

Final command: `cargo test -p codegraph-search -p codegraph-graph -p codegraph-db -p codegraph-mcp -p codegraph-cli` — **260 passed, 0 failed** on Windows.
Regression coverage includes ambiguous definitions with explicit paths, Unicode
and CRLF source, complete multi-call source recovery without duplicated lines,
edit invalidation, oversized signatures, continuation without session state,
balanced source fences, multi-file output bounds, exact range rereads, paths
containing spaces, character-page reconstruction, stale-fragment rejection,
configuration-value protection, and CLI fragment continuation.

`cargo build -p codegraph-cli`: passed. The linker emits existing tree-sitter /
wasmtime symbol-import warnings. `git diff --check` passed.

Three final CLI smoke queries on the existing repository index returned flows,
source and complete footers: `explore named_definitions` scoped to the renderer
(7,442 characters), `resolve_named_symbol_flow resolve_named_tokens` scoped to
the graph flow module (7,451), and `search_fts search_nodes_with_deprioritizer`
scoped to database search (7,423). These check the built CLI path; they are not a
multi-repository accuracy evaluation.

Character budgets are not exact token budgets; token counts vary by model and
source language. This work does not claim a measured percentage reduction in
whole-conversation tokens, cost or latency. No full language/extraction test
matrix or model-driven agent A/B was run for these retrieval-only changes.

Ordinary source is continued as complete lines. An oversized line has an
actionable fragment query, for example:

```text
codegraphV_explore(query: "src/large.ts:10-10", column: 1)
codegraphV explore "src/large.ts:10-10" --column 1
```

Follow the returned `query`, `column` and `sourceHash` arguments; CLI equivalents
are `--column` and `--source-hash`. Columns count Unicode characters, not bytes.
Only a fragment's indicated columns are represented as shown. If the source
changes, restart the line rather than joining incompatible pages. Configuration
values remain withheld on this path. A host with a smaller limit can still
truncate an otherwise valid response.

## Next improvements worth evaluating

1. Measure complete agent tasks: answer correctness, missing-source recovery,
   tool calls, wall time and summed per-turn token usage. Include small, medium
   and large real repositories and ambiguous-symbol/control prompts. Smaller
   outputs alone are not evidence of lower total task cost.
2. Evaluate model-specific token budgets: character ceilings bound response size
   but do not imply identical token use across programming languages or models.
3. Evaluate deterministic contextual ranking using file/module/type evidence
   for natural-language questions before considering embeddings. Keep exact
   symbol and flow queries as precision controls.
4. Profile full native queries and long sessions separately: SQL is only one
   component, and obsolete-hash cleanup does not bound the number of distinct
   files a session can remember.
