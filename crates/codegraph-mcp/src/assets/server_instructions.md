# CodeGraphV — code intelligence over an indexed knowledge graph

CodeGraphV is a SQLite knowledge graph of every symbol, edge, and file in the
workspace, parsed from the AST across 30+ languages (TypeScript/JavaScript,
Python, Go, Rust, Java, C#, C/C++, PHP, Ruby, Swift, Kotlin, and more). Reads
are sub-millisecond; the file watcher keeps the index about 1s behind writes.
Use it BEFORE and while editing code, not only for questions.

## One tool: codegraphV_explore — use it instead of reading files

`codegraphV_explore` takes a natural-language question or a bag of symbol/file
names and returns the **verbatim, line-numbered source** of the relevant
symbols grouped by file (the same shape `Read` gives, safe to `Edit` from),
PLUS the call path among them (including dynamic-dispatch hops such as
callbacks, React re-render and JSX children that grep can't follow) and what
depends on them. Treat the returned source as already Read. ONE call usually
answers the question; a grep/Read loop or a file-reading sub-agent repeats
work the index already did.

## How to query

- **How X works, architecture, a bug, where X is** → ask in plain words or name the symbols.
- **How X reaches Y / a flow** → name the symbols that span it (e.g. `mutateElement renderScene`); explore returns the path between them.
- **A file or symbol you will edit** → put its name or path in the query; you get its current source with callers and blast radius. An overloaded name returns every definition.
- **Need more?** Call again with more specific names. Call counts are advisory, never a quota.
- Qualified names accept dots, `::` or slashes (`AppWeb.Format.group`). Flow endpoints must match exactly; on a "Did you mean" note, query the suggested name.
- Matching is lexical, not semantic: an empty result lists word matches and may suggest names to retry with.

## Anti-patterns

- Don't re-verify results with grep: they come from a full AST parse.
- Don't grep or Read first for indexed code. Use Read/Grep only for a detail explore didn't cover, or for what isn't indexed (configs, docs).
- Don't rebuild a flow by hand: name both ends in one query.

## Freshness notices

- "⚠ Pending edits: <files>. The index is catching up." — those files were just written; Read them for exact content. Everything else in the response is fresh.
- "⚠ Index may be stale: <reason>" — the file watcher is degraded, so any file may be out of date; Read files directly to confirm anything that may have changed.
- "⚠ CodeGraphV results below come from a different git worktree" — the answer reflects that checkout; symbols changed only in yours are missing, so Read them. The user can run `codegraphV init -i` for a worktree-local index.

## Limitations

- If a project isn't indexed (no `.codegraphV/`), stop calling codegraphV for it and use built-in tools. Indexing is the user's decision: mention `codegraphV init`, never run it yourself.
- To query another indexed project (a monorepo service, a second repo), pass its path as `projectPath`.
- Cross-file resolution is best-effort name matching; ambiguous calls may return several candidates.
- It does not replace the compiler, tests or linter.
