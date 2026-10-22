/** Shared comparison protocol for standalone native extraction probes. */
import { spawnSync } from 'node:child_process';
import { isDeepStrictEqual } from 'node:util';

export function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') return Object.fromEntries(Object.keys(value).sort().filter(k => !['durationMs', 'updatedAt'].includes(k) && value[k] !== null && value[k] !== undefined).map(k => [k, canonical(value[k])]));
  return value;
}
export function normalize(result) {
  return Object.fromEntries(['nodes', 'edges', 'unresolvedReferences', 'errors'].map(k => [k, result[k].map(x => JSON.stringify(canonical(x))).sort()]));
}
export function compare(binary, cases, expected) {
  const native = spawnSync(binary, [], { input: cases.map(c => JSON.stringify(c)).join('\n') + '\n', encoding: 'utf8', maxBuffer: 256 * 1024 * 1024, timeout: 120000, windowsHide: true });
  if (native.status !== 0) throw Error(native.stderr || native.error);
  const actual = native.stdout.trim().split(/\r?\n/).map(s => normalize(JSON.parse(s)));
  if (actual.length !== cases.length) throw Error(`expected ${cases.length} results, got ${actual.length}`);
  for (let i = 0; i < cases.length; i++) {
    if (isDeepStrictEqual(actual[i], expected[i])) continue;
    console.error(JSON.stringify(cases[i]));
    for (const kind of Object.keys(expected[i])) {
      const expectedBag = [...expected[i][kind]], actualBag = [...actual[i][kind]];
      for (let j = expectedBag.length - 1; j >= 0; j--) {
        const index = actualBag.indexOf(expectedBag[j]);
        if (index >= 0) { expectedBag.splice(j, 1); actualBag.splice(index, 1); }
      }
      if (expectedBag.length || actualBag.length) console.error(kind, { expectedOnly: expectedBag.slice(0, 5), nativeOnly: actualBag.slice(0, 5) });
    }
    throw Error(`extraction parity mismatch at ${i}`);
  }
}
