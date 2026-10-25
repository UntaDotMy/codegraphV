# CodeGraphV — available (per-project; pass projectPath)

CodeGraphV is a SQLite knowledge graph of a codebase's symbols, edges, and
files (30+ languages): one `codegraphV_explore` call returns the verbatim, line-numbered source
of the relevant symbols PLUS the call paths between them and a blast-radius
summary — replacing a grep + Read loop with one round-trip.

This server started somewhere with no `.codegraphV/` of its own, so there is no
default project — but the tools are available and work **per project**:

- To query a project that HAS a `.codegraphV/` index (e.g. a service inside a
  monorepo, or a second repo), pass its path as `projectPath` to
  `codegraphV_explore` (and any other codegraphV tool). CodeGraphV resolves the
  nearest `.codegraphV/` at or above that path and answers from it — for as many
  projects as you like in one session.
- For a project with no `.codegraphV/`, use your built-in tools (Read/Grep/Glob)
  for that project. Indexing is the user's decision — don't run it yourself, but
  if it comes up they can run `codegraphV init` in a project to enable codegraphV
  there (a new index is picked up live, no restart).

