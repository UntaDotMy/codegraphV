# Native audit and upstream review — 2026-11-03

Baseline: released codegraphV **v0.0.8**, commit `bf4bfb9e`.
Release under review: **v0.0.9**. Upstream was fetched and pinned at
[`f72c9d2e`](https://github.com/colbymchenry/codegraph/commit/f72c9d2efb14454ff4f9fd7b8460abd82f19a287),
128 commits after the previous port's `8bd55fa7` checkpoint.

This audit inventories all 16 native crates and exercises the workspace's
functional tests, with focused review of retrieval, search, graph accuracy,
component extraction, protocol recovery, locking, installation and migration.
It does **not** certify every function or claim that all upstream changes have
been ported. [inventory.json](inventory.json) records the native surface;
[upstream.json](upstream.json) gives every reviewed upstream commit an explicit
ported, partial, already-native, runtime-specific, reference-only or pending status.
The old broad parity checkpoint has deliberately not been advanced.

## Why context can grow

Context is cumulative: initialization guidance, tool definitions, every returned
source page, retries and follow-up reads all contribute. A smaller single answer
can cost more overall if it omits the needed code. This repository also has a
large `AGENTS.md`; that host-supplied context is separate from MCP output.

The confirmed waste addressed here is redundant startup prose, signatures
printed both in metadata and source, and unrelated results for an unindexed
bare filename with a line range. Search false negatives and missing flow
waypoints also encourage follow-up retrieval. Those need correctness fixes,
not arbitrary reductions to source budgets.

## Changes and evidence

| Area | Finding and change | Regression coverage |
|---|---|---|
| Agent context | Shorter indexed/unindexed instructions; preserve primary-tool, continuation, freshness and indexing-consent guidance. Omit metadata signatures when the definition's source is focused. | Instruction size/essential-guidance checks; verbatim-source and signature tests. |
| File requests | `install.ps1:1-200` could turn into a fuzzy symbol search and return unrelated code. An unresolved filename range now gives a short miss. | Missing-file source test; existing continuation, Unicode fragment and source-edit tests. |
| Search | `path:`/`name:` filters ran after candidate limits, and filter-only requests ignored offset. Apply literal Unicode-aware predicates before limits in FTS, LIKE, fuzzy and exact-name supplementation. | Real SQLite fixtures with more distractors than the candidate cap, read-only reopen, FTS removal, Unicode and literal wildcard characters; live upstream ranking comparison. |
| Pasted calls | Removing punctuation joined `getUser(run)` into one nonexistent word. Punctuation now separates terms. | Native regression and upstream graph/search comparison. |
| Qualified lookup | Substring scope matching could select `LegacyFormat.Price` for `Format.Price`, including a misleading matching filename. Require scope boundaries and type-aware path qualification. | Native false-positive regression and live qualified-lookup cases. |
| Named flows | One route per node could discard a longer route through a requested waypoint. Keep non-dominated routes, prevent cycles, bound route states and share callee reads across seeds. | Shortcut/cycle regression, upstream-generated route fixtures and full graph comparison. |
| Unicode | ASCII-only flow/context tokenization lost valid identifiers. Share script-aware words and CJK boundaries; preserve accented words whole. | Unicode native flow tests, generated search vectors and live context/graph comparisons. |
| Import cycles | Old traversal reported order-dependent loops through general dependencies. Use iterative strongly connected components over imports only. | Upstream comparison over deterministic random graph topologies and persisted native vectors. The upstream `cycles` CLI surface is still pending. |
| Components | A self-closing script tag could consume the next script body or mask following template content. Handle it as a separate empty element in Vue, Svelte and Astro. | Three-language source/line/template regression; 345 live component-extraction comparisons. |
| MCP recovery | Accept a file as `projectPath` through its containing directory and existing security checks. Reject non-object tool arguments with JSON-RPC invalid params while keeping the session usable. | Native protocol, project-path and sensitive-folder tests. |
| Writer safety | Concurrent stale-lock removal could replace another contender's lock. Serialize takeover with an OS-managed guard, preserve unreadable/recent-empty locks, check PID-write failures and treat Windows access denial conservatively. | Actual Windows concurrent-contender and lock lifecycle tests; platform CI. |

The lock guard is a persistent empty sidecar, not another PID lock to reclaim.
The operating system releases its lock when the owning process exits. Keeping
the sidecar prevents different contenders from locking different replacement
inodes. The index schema and migration format are unchanged.

## Research used

These sources informed the audit's test design; their benchmark scores are not
measurements of codegraphV:

- [Agent Retrieval Bench (July 2026)](https://arxiv.org/html/2607.24882v1)
  distinguishes useful next-file retrieval from textual similarity and includes
  cases where no local answer exists. This supports testing clean misses and
  distracting names, alongside successful retrieval.
- [ContextBench (February 2026)](https://arxiv.org/html/2602.05892v1)
  measures retrieval precision, recall and efficiency through agent trajectories.
  Its distinction between explored and used context is why output characters
  alone are not claimed as task success or billed-token savings here.
- [RepoGraph](https://arxiv.org/html/2410.14684v2) studies repository graph context
  and neighborhood size. It supports retaining relevant structural paths rather
  than indiscriminately adding neighboring source.
- [SWE-agent](https://arxiv.org/html/2405.15793v3) studies the agent-computer
  interface. Clear recoverable responses and concise, informative feedback are
  relevant to the MCP fixes.
- [Lost in the Middle](https://arxiv.org/html/2307.03172v3) shows that context
  availability does not guarantee its effective use; it does not justify
  dropping essential source.
- [Anthropic's tool-design guidance](https://www.anthropic.com/engineering/writing-tools-for-agents)
  recommends measuring tool behavior and avoiding low-value output. This audit
  removes duplication while retaining source, rather than adding another tool
  an agent must learn to choose.

## Validation and measurement

The live TypeScript oracle is a separate checkout at the pinned upstream
revision, compiled from source. Native graph vectors are refreshed only after
the same-database comparison passes. Context comparison normalizes only the
intentional `codegraph`/`codegraphV` branding in guidance; graph data, source,
ordering and scores remain exact.

The executable comparison is reproducible with:

```text
node scripts/bench/mcp-session-audit.mjs <v0.0.8.exe> <v0.0.9.exe> <report.json>
```

It uses real MCP connections, three sessions per binary in alternating order,
and generated 50-, 500- and 5,000-file projects. It measures initialization text,
warm round trips and cumulative response characters. Source hashes are compared
for the initial flow and explicit range. The second flow checks session reuse;
the missing filename range checks irrelevant-context avoidance. Opening and
catch-up are outside timed calls. Binaries are not rebuilt while being measured.

### Recorded results

Workspace tests (`cargo test --workspace --locked`, log `audit-nov-verified-tests.log`
on this tree before the v0.0.9 release link): 836 passed, 0 failed, 1 ignored.
Graph comparison: 8,600 queries matched the pinned TypeScript oracle. Context
comparison: 8,578 results matched. Windows installer recovery: copy, rename,
swap, and success all passed. The optimized `codegraphV version` binary prints
`0.0.9`.

The session comparison ran on 2026-11-02 against the installed v0.0.8 binary and
`target/release/codegraphV.exe`. Raw output is
[session-audit.json](session-audit.json). Three sessions agreed on every
character count below. Source hashes for `entry deep leaf` and `flow.ts:1-3`
matched across binaries at every fixture size.

| Surface | v0.0.8 | v0.0.9 |
|---|---:|---:|
| MCP `initialize` instructions | 3,409 | 1,216 |
| `tools/list` schema | 2,330 | 1,507 |
| First flow, returned characters | 914 | 838 |
| Repeated flow, returned characters | 506 | 430 |
| `flow.ts:1-3`, returned characters | 372 | 372 |

The guidance file itself is 3,362 characters in v0.0.8 and 1,198 in v0.0.9 when
newlines are normalized. The table is what the server sent, including Windows
newlines. The 76-character drop on both flow answers is the signature that v0.0.9
no longer repeats above source that is already on the page. The exact range is
unchanged. Warm flow and range round trips stayed within 0.3 ms (about 3–10 ms).

`install.ps1:1-200` is the miss check. v0.0.8 treated it as a fuzzy name search
and returned unrelated symbols (`install` in `flow.ts`, and on the 5,000-file
fixture also `filler2847`). That answer grew from 168 to 243 characters, and its
median time grew from 11.3 ms to 61.4 ms. v0.0.9 returned the same 166-character
"no indexed file" miss at every size (median 2.7 ms, 3.1 ms, and 6.9 ms).

These are response characters and local round trips. They are not model token
counts, task success, or indexing speed.

## Remaining work

- Many upstream resolver/framework features remain pending; the commit ledger
  is the exact boundary. New framework synthesis, new CLI commands, installer
  targets and viewer features are not implied by a retrieval-focused update.
- Actual model-session input/output tokens, residual context occupancy, task
  success, and end-to-end cost have not been measured in this audit. Synthetic
  MCP timings are not an agent A/B or a real-repository indexing benchmark.
- Session source history still grows with distinct visited files. Clearing it
  arbitrarily would cause repeated source and can increase model context; a
  bounded representation needs continuation/dedup validation.
- Multi-file installer configuration changes are not one filesystem transaction.
  Migration preflight and backups preserve recovery, but full rollback across
  all files is still future work.
- The full impact graph still needs the new upstream explicit truncation budget;
  current depth limits and graph deduplication are not a total-size bound.
- The preview viewer and the original JavaScript library API remain outside
  full native parity. Passing the workspace tests is not exhaustive proof of
  language/framework accuracy on real large repositories.

## Upgrade

Run `codegraphV upgrade`, then reconnect the agent to receive the shorter server
guidance. Existing indexes remain readable. Run `codegraphV index` in affected
Vue/Svelte/Astro projects to refresh extracted script/template nodes. Query-only
search and flow fixes apply immediately to existing indexes. There is no need
to run migration again when upgrading v0.0.8.
