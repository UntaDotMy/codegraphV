#!/usr/bin/env node
/** V8 `Array.prototype.sort` vectors for `codegraph_types::jssort::js_sort_by`.
 * node scripts/rust-golden/js-sort.mjs
 *
 * Sorts seeded arrays with comparators that are NOT consistent orders (an
 * epsilon tie-break, an arbitrary pair function) — where the permutation
 * depends on the algorithm — and records the resulting index order to
 * crates/codegraph-types/tests/fixtures/js_sort.jsonl. Integer-only comparators,
 * so both sides compute identical signs.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import { root } from './check-upstream.mjs';

let seed = 0x7a11c0de;
const rand = () => { seed |= 0; seed = (seed + 0x6d2b79f5) | 0; let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));

export const COMPARATORS = {
  eps: (a, b) => (Math.abs(a.g - b.g) > 30 ? b.g - a.g : a.h - b.h),
  chaos: (a, b) => ((a.g * 7 + b.g * 13) % 5) - 2,
  total: (a, b) => a.h - b.h,
  runs: (a, b) => a.g - b.g,
  epsRuns: (a, b) => (Math.abs(a.g - b.g) > 2 ? a.g - b.g : b.h - a.h),
};

function makeArray(n) {
  const out = [];
  while (out.length < n) {
    const shape = int(0, 3);
    const len = int(1, 80);
    const start = int(0, 1000);
    for (let i = 0; i < len && out.length < n; i++) {
      const g = shape === 0 ? start + i * int(0, 3) : shape === 1 ? start - i * int(0, 3) : int(0, 1000);
      out.push({ g, h: int(0, 5), i: out.length });
    }
  }
  return out;
}

const lines = [];
for (const [name, cmp] of Object.entries(COMPARATORS)) {
  for (let k = 0; k < 60; k++) {
    const n = k < 20 ? int(0, 70) : int(64, 900);
    const items = makeArray(n);
    const sorted = [...items].sort(cmp);
    lines.push(JSON.stringify({ cmp: name, items: items.map(({ g, h }) => [g, h]), order: sorted.map((x) => x.i) }));
  }
}
const dest = path.join(root, 'crates/codegraph-types/tests/fixtures/js_sort.jsonl');
fs.mkdirSync(path.dirname(dest), { recursive: true });
fs.writeFileSync(dest, lines.join('\n') + '\n');
console.log(`wrote ${lines.length} sort vectors to ${path.relative(root, dest)}`);
