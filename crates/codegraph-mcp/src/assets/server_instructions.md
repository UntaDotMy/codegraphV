# CodeGraphV

A queue-full retry response means the request was not executed; retry it after the server is less busy.

Use `codegraphV_explore` first for indexed code. It returns verbatim, numbered
source and recorded call paths, including heuristic dynamic-dispatch hops.
Treat returned source as already Read; don't re-read it just to verify it.

- Name symbols or indexed files; for a flow, name both ends and useful waypoints.
  Add a file to disambiguate overloads. Matching is lexical, not semantic.
- Follow returned range/fragment arguments for more source. Explicit ranges
  reread lines; `sourceHash` detects edits during fragment continuation.
  Call counts are guidance, not a quota. On "Did you mean", use the suggested name.
- An unresolved path was not indexed; other results match remaining query terms.
  Read/Grep is appropriate for unindexed files or details the tool cannot supply.
- `projectPath` selects another indexed project. Without `.codegraphV/`, use
  other tools; mention `codegraphV init` only if useful. Never index automatically.
- `⚠ Pending edits` or `⚠ Index may be stale`: confirm affected code on disk.
  A different-worktree notice means results describe that other checkout.
  Cross-file resolution is best-effort; use the compiler and tests to validate changes.
