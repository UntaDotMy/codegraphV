<!-- CODEGRAPHV_START -->
## CodeGraphV

In repositories indexed by CodeGraphV (a `.codegraphV/` directory exists at the repo root), reach for it BEFORE grep/find or reading files when you need to understand or locate code:

- **MCP tool** (when available): `codegraphV_explore` answers most code questions in one call — the relevant symbols' verbatim source plus the call paths between them, including dynamic-dispatch hops grep can't follow. Name a file or symbol in the query to read its current line-numbered source. If it's listed but deferred, load it by name via tool search.
- **Shell** (always works): `codegraphV explore "<symbol names or question>"` prints the same output.

If there is no `.codegraphV/` directory, skip CodeGraphV entirely — indexing is the user's decision.
<!-- CODEGRAPHV_END -->