#!/usr/bin/env node
/** Portable parity vectors for the pure codegraph_explore helpers (`src/mcp/tools.ts`).
 * node scripts/rust-golden/explore-helpers.mjs [--count N]
 *
 * Runs every exported helper of the TypeScript oracle (`dist/mcp/tools.js`) on
 * seeded random inputs and writes one `{fn, args, expected}` JSON line per call to
 * crates/codegraph-mcp/tests/fixtures/explore_helpers.jsonl, which
 * `cargo test -p codegraph-mcp --test explore_helpers` replays against the port.
 * Maps become entry arrays; Infinity is written as the string "Infinity".
 */
import { createRequire } from 'node:module';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { root, oracleRoot } from './check-upstream.mjs';

const require = createRequire(import.meta.url);
const T = require(path.join(oracleRoot, 'dist/mcp/tools.js'));
const args = process.argv.slice(2);
const count = Number(args[args.indexOf('--count') + 1] ?? 0) || 120;

let seed = 0x5eed1234;
const rand = () => { seed |= 0; seed = (seed + 0x6d2b79f5) | 0; let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (xs) => xs[Math.floor(rand() * xs.length)];
const chance = (p) => rand() < p;
const list = (n, f) => Array.from({ length: n }, (_, i) => f(i));

const KINDS = ['function', 'method', 'class', 'import', 'export', 'variable', 'constant', 'interface', 'property', 'struct', 'module', 'enum_member'];
const NAMES = ['run', 'poll', 'Harness', 'send', 'as_sql', 'dispatch', 'é_name', 'parse', 'run', 'Ünïcode', 'x', 'handle'];
const PATHS = ['src/a.ts', 'src/b/c.ts', 'lib/c.ts', 'c.ts', 'node/ext.ts', 'common/ext.ts', 'src/üñí/d.rs', 'tests/a_test.go', 'src/a.ts'];
const node = () => {
  const start = int(1, 400);
  return { id: `n${int(0, 1e6)}`, kind: pick(KINDS), name: pick(NAMES), qualifiedName: pick(['', 'A::run', 'SQLCompiler::as_sql', 'x::y::z', 'a.b::c', '::m']),
    filePath: pick(PATHS), language: 'typescript', startLine: start, endLine: start + int(0, 60), startColumn: 0, endColumn: 0, updatedAt: 0 };
};
const elidedRef = () => ({ name: pick(NAMES), kind: pick(KINDS), startLine: int(1, 400) });
const range = () => { const s = int(-2, 400); return { start: s, end: s + int(-3, 80) }; };
const wanted = () => { const s = int(-1, 400); const w = { name: pick(NAMES), kind: pick(KINDS), start: s, end: s + int(-2, 50),
  importance: pick([0, 3, 9, 11, 2.5, 9]), spine: chance(0.3) }; if (chance(0.6)) w.qualifiedName = pick(['A::run', 'SQLCompiler::as_sql', '1x::y', 'lone']); return w; };
const text = () => pick(['', 'x', 'fn a() {}\n  b();', 'é — ü', 'line\nline', '😀 surrogate', '- src/a.ts: run:1']);
const budget = () => T.getExploreOutputBudget(pick([10, 200, 1000, 6000, 20000]));
const queryPart = () => pick(['mod:fn/3', 'init/2', 'kind:function', 'a::b', 'src/2fa', 'x:Y', 'http://a.b', 'cowboy_stream_h:request_process/3',
  'lang:rust', 'é:x', 't:m', 'C:\\x', 'Foo', 'a:b:c', 'fn/1234', '_x@y:z', 'path:a/b']);

const plain = (v) => v instanceof Map ? [...v.entries()] : v === Infinity ? 'Infinity' : v;
const cases = {
  normalizeQuerySpelling: () => [list(int(0, 5), queryPart).join(pick([' ', ',', '(', ')', '[', ']', '/', '  ']))],
  getExploreOutputBudget: () => [pick([0, 149, 150, 499, 500, 4999, 5000, 14999, 15000, 24999, 1e6])],
  allocateExploreBudget: () => [list(int(0, 14), () => {
    const c = { path: pick([...PATHS, 'x/y.ts', 'z.go', 'q.py']), score: pick([0, -1, int(0, 120), rand() * 60, 1e-9]), worth: pick([1, 0.3, 0.5, 0.15, rand() * 1.4 - 0.2]), spine: chance(0.2) };
    if (chance(0.2)) c.pinned = true; if (chance(0.3)) c.named = true; return c;
  }), budget(), int(1, 20)],
  symbolsBetweenRanges: () => [list(int(0, 12), node), int(-1, 200), int(0, 300)],
  symbolsNotInRanges: () => [list(int(0, 10), elidedRef), list(int(0, 4), range)],
  formatGapMarker: () => [pick(PATHS), list(int(0, 9), elidedRef)],
  elidedBodyNote: () => [pick(PATHS), pick(NAMES), int(1, 300), int(1, 400)],
  joinPartsWithNamedGaps: () => {
    const parts = list(int(0, 5), () => ({ range: range(), text: text() }));
    const a = [pick(PATHS), parts, list(int(0, 20), node)];
    // Spares near one named marker's extra cost, so UTF-16 vs byte length decides the outcome.
    if (chance(0.5) && parts.length >= 2) {
      // Exactly the first named marker's UTF-16 extra: a byte-length count overshoots it.
      const named = T.formatGapMarker(a[0], T.symbolsBetweenRanges(a[2], parts[0].range.end, parts[1].range.start));
      a.push(named.length - '\n\n... (gap) ...\n\n'.length);
    } else if (chance(0.7)) a.push(pick([0, 10, 40, 100, 400, 2000, int(0, 160)]));
    return a;
  },
  biasHeaderSymbols: () => [list(int(0, 10), () => `${pick(NAMES)}(${pick(KINDS)})`), list(int(0, 6), elidedRef), int(0, 8)],
  elidedWantedSpans: () => [list(int(0, 10), wanted), list(int(0, 5), range)],
  shortestUniqueSuffixes: () => [list(int(0, 8), () => pick([...PATHS, 'a/b/ext.ts', 'ext.ts', 'b/ext.ts', 'x/a/b/ext.ts']))],
  exploreCompletenessNotes: () => [int(0, 4), list(int(0, 5), () => ({ filePath: pick(PATHS), elided: list(int(0, 4), wanted) })), list(int(0, 5), () => pick(PATHS))],
  fitExploreEpilogue: () => [{ room: pick([-10, 0, 50, 200, 400, 800, 2000, rand() * 1500]),
    noteCandidates: list(int(0, 3), () => list(int(1, 3), () => pick(['> **Complete…**', '', 'x'.repeat(int(1, 300)), 'é😀']))),
    noteYields: chance(0.5), pointerEntries: list(int(0, 6), () => `- ${pick(PATHS)}: ${pick(['s', 'é', '😀']).repeat(int(0, 120))}`), pointerOmitted: int(0, 3) }],
};

const lines = [];
for (const [fn, gen] of Object.entries(cases)) {
  for (let i = 0; i < count; i++) {
    const a = gen();
    const out = T[fn](...a);
    lines.push(JSON.stringify({ fn, args: a, expected: out && typeof out === 'object' && !Array.isArray(out) && !(out instanceof Map)
      ? Object.fromEntries(Object.entries(out).map(([k, v]) => [k, plain(v)])) : plain(out) }));
  }
}
const dest = path.join(root, 'crates/codegraph-mcp/tests/fixtures/explore_helpers.jsonl');
fs.mkdirSync(path.dirname(dest), { recursive: true });
fs.writeFileSync(dest, lines.join('\n') + '\n');
console.log(`wrote ${lines.length} vectors (${Object.keys(cases).length} helpers × ${count}) to ${path.relative(root, dest)}`);
