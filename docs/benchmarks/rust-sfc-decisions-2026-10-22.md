# Native component extraction: implementation and measurements

This is a parity port of the existing Svelte, Vue and Astro extractors, not a new
framework resolver. The Rust CLI, complete resolver, MCP server and viewer still
need porting. These measurements do not establish end-to-end indexing or agent
latency; the real-repository flow probes and agent A/B gates remain required
when the native runtime is ready.

## Research and implementation choices

- [Tree-sitter's multi-language parsing documentation](https://tree-sitter.github.io/tree-sitter/using-parsers/3-advanced-parsing.html)
  supports parsing included ranges. This port instead borrows script slices and
  reuses the existing native JS/TS walkers. The reference makes script-relative
  node IDs before folding line offsets; included ranges would change those IDs
  unless every walker accommodated a coordinate origin. Borrowed slices preserve
  compatibility without copying source or duplicating walkers.
- [The Rust Performance Book's profiling guidance](https://nnethercote.github.io/perf-book/profiling.html)
  recommends locating hot work before optimizing it. Inspection and controlled
  timing isolated repeated compilation of a name-specific export regex. A lazy,
  per-file set of later-exported names replaces those compilations and source
  scans. Static patterns retain JavaScript whitespace, anchors and ASCII token
  boundaries, including `$` names. Reference-generated native tests cover
  aliases, comments, Unicode whitespace, CRLF and Unicode line separators.
- [Rayon's pool documentation](https://docs.rs/rayon/latest/rayon/struct.ThreadPoolBuilder.html)
  distinguishes configurable explicit pools from a global pool initialized once.
  No pool is added to individual extractors. The future indexing coordinator
  should own a bounded pool and measure worker counts on mixed repository loads;
  spawning workers for each script would obscure extraction costs and complicate
  embedding. A specific thread count or concurrency speedup is not established here.

The common script fold is the single owner of component containment, line shifts
and instance/module call attribution. Vue's lexical scanners remain separate from
the wrapper, matching upstream's module boundaries. Native nested-template and
balanced-group scans use heap frames instead of recursive Rust calls. Active Vue
scope counts avoid rebuilding ancestor unions for every tag. Nested expression
work has a budget with a recorded extraction error on exhaustion.

## Validation and repeatability

`sfc.mjs` checks 345 synthetic/reference-regression component inputs, plus real
components in Svelte RealWorld, Flowbite Svelte, SvelteKit, Vue RealWorld,
vue-element-admin, Vue core, Astro docs and Astro. All 4,678 results match the
reference's nodes, edges, unresolved references and errors as multisets; duplicate
counts remain significant. Volatile timestamps are excluded. The portable
fixtures and native ownership/position/deep-input tests run without Node.

Repository revisions and the 100 selected benchmark files per format are retained
in the JSON reports. Format counts include cross-framework fixtures in Astro.
The viewer adds another 55 real Svelte files in the default update gate.

The first parity-correct port is recorded in
`rust-sfc-windows-baseline-2026-10-22.json`. A one-parser-per-thread experiment
(`rust-sfc-parser-reuse-windows-2026-10-22.json`) changed median native times by
about 0–2%, too little to justify retaining the cache. It was removed. The final
`rust-sfc-windows-2026-10-22.json` measures the export-scan optimization and active
Vue scope counts. These earlier experimental reports predate source/binary
fingerprint recording and describe intermediate working trees, not the foundation
commit alone; their shared Git revision is not a complete source identifier.

All timings are Windows x64 on the recorded Ryzen CPU, release builds, warm serial
extraction, three warmups, seven samples and three rounds/sample. Both engines
return equivalent outputs. JSON serialization, IPC, startup, database writes and
resolution are outside the timed region. JavaScript GC is included when triggered.
These are reproducible workload observations, not a claim that a particular crate
or Rust implementation is universally fastest.

Final median milliseconds per 100-file batch:

| Format | TypeScript reference | Native Rust | Reference/native |
|---|---:|---:|---:|
| Svelte | 32.18 | 16.60 | 1.94× |
| Astro | 13.35 | 7.67 | 1.74× |
| Vue | 59.15 | 37.06 | 1.60× |

The full update gate passes 236 native tests, 8,597 graph/search comparisons,
1,630 path/callback comparisons and 400 default component comparisons. Seven
related TypeScript regression suites pass all 38 assertions. Strict Clippy is
still limited by inherited lints in the earlier native foundation; this port
does not claim a clean workspace lint gate.
