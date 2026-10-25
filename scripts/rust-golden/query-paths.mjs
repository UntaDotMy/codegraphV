#!/usr/bin/env node
/** Live differential check and persisted oracle vectors for path recognition.
 * cargo build -p codegraph-search --example query_paths_probe
 * node scripts/rust-golden/query-paths.mjs target/debug/examples/query_paths_probe.exe [--write]
 */
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import { isDeepStrictEqual } from 'node:util';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { oracleRoot } from './check-upstream.mjs';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const require = createRequire(import.meta.url);
const { extractQueryPaths, queryMightContainPaths } = require(path.join(oracleRoot, 'dist/search/query-paths.js'));
// Use the real regression suite's index, including framework route syntax.
const source = fs.readFileSync(path.join(oracleRoot, '__tests__/query-paths.test.ts'), 'utf8');
const indexBlock = source.match(/const INDEX = \[([\s\S]*?)\];/)[1];
const paths = [...indexBlock.matchAll(/^\s*'([^']+)'/gm)].map(m => m[1]);
paths.push('src/editor/config/editorOptions.ts', 'src/workbench/editor/editorOptions.ts', 'src/Été.ts', 'src/😀.ts', 'README', 'root.rs');
const cases = [];
const add = (query, extra = {}) => cases.push({ query, paths, ...extra });
for (const p of paths) {
  const tail = p.split('/').at(-1);
  for (const spelling of [p, tail, './' + p, p.toUpperCase(), p.replaceAll('/', '\\'), '/Users/colby/dev/repo/' + p]) {
    for (const [prefix, suffix] of [['', ''], ['`', '`'], ['(see ', ').'], ["'", "',"], ['<', '>']]) add(`why ${prefix}${spelling}${suffix} fails`);
  }
  for (const suffix of [':42', ':40-12', '#L88', '#L88-L120', '#L88-120', ':0', ':1000001', ':9999999999999999999999']) add(`read ${p}${suffix} please`);
  for (const range of ['lines 900-1003', 'line 42', 'L88-L120', 'lines 900 to 1003', 'lines 300 through 200', 'lines 300 thru 200', '88', '(L88)', 'lines 0', 'lines 1000001', 'lines 42..88', 'lines 42–88']) add(`${p} ${range} helperMethod`);
}
for (const query of [
  '', ' \t\n', 'how does the scroll pinning work', 'what reads app.isPackaged here', '--no-cache', 'foo_bar-baz',
  'does gen_server:call/2 block and/or timeout', 'non-blocking cross-call input/output', 'scripts/deploy',
  'background-image-table Source column', 'add-to-training-set training-set-page-background-images backgrounds.ts background-image-table Source column',
  '+page.svelte ChatScroller', 'generic-modal GenericModalFooter', 'user-profile UserProfileCard',
  'editorOptions.ts clampedInt cursorStyleToString', 'editorOptions.ts:1291 clampedInt',
  'editorOptions.ts EditorIntOption.clampedInt cursorStyleFromString()', 'editorOptions.ts options',
  'editorOptions.ts clampedInt src/workbench/editor/editorOptions.ts',
  'src/lib/chat-manager.ts src/lib/task-runner-manager.ts lines 42 to 88',
  'L42 of chat-manager.ts; L88 of task-runner-manager.ts',
  'src/lib/chat-manager.ts src/lib/chat-manager.ts:42',
  'absent/deep/file.ts absent/deep/file2.ts missing3.ts and/or',
  'chat-manager.ts\uFEFFlines\u00A042', 'chat-manager.ts\u0085lines 42',
]) {
  for (const extra of [{}, { maxPins: 0 }, { maxPins: 1 }, { maxMatchesPerSpan: 1 }, { maxMatchesPerSpan: 4 }, { exists: ['scripts/deploy'] }, { symbols: {} }, { symbols: { clampedInt: ['src/editor/config/editorOptions.ts'], cursorStyleToString: ['src/editor/config/editorOptions.ts'], GenericModalFooter: ['src/y/generic-modal.tsx'], UserProfileCard: ['src/c/user-profile.tsx'], ChatScroller: [paths[0]], 'EditorIntOption.clampedInt': ['src/editor/config/editorOptions.ts'] } }]) add(query, extra);
}
// Budgets, case-collisions, suffix attempts and punctuation at both ends.
for (const count of [0, 1, 2, 8, 9, 20]) add(Array.from({ length: count }, (_, i) => `src/no${i}.ts`).join(' ') + ' chat-manager.ts');
add('A.ts', { paths: ['src/a.ts', 'src/A.ts'] });
add('anything.ts', { paths: [] });
add('src/lib/chat-manager.ts lines 900-1003 flushQueue', { paths: ['src/lib/chat-manager.ts'] });
add('chat-manager.ts editorOptions.ts', { maxPins: 1 });
for (let i = 0; i < 20; i++) add('/' + 'prefix/'.repeat(i) + 'src/lib/chat-manager.ts');
const expected = cases.map(input => {
  const existsAsked = [], symbolsAsked = [];
  const opts = { maxPins: input.maxPins, maxMatchesPerSpan: input.maxMatchesPerSpan };
  if ('exists' in input) opts.existsOnDisk = p => { existsAsked.push(p); return input.exists.includes(p); };
  if ('symbols' in input) opts.symbolFiles = t => { symbolsAsked.push(t); return input.symbols[t] ?? []; };
  return { gate: queryMightContainPaths(input.query), result: extractQueryPaths(input.query, input.paths, opts), existsAsked, symbolsAsked };
});
const native = spawnSync(path.resolve(process.argv[2] ?? 'target/debug/examples/query_paths_probe.exe'), [], { input: cases.map(q => JSON.stringify(q)).join('\n') + '\n', encoding: 'utf8', maxBuffer: 32 * 1024 * 1024, timeout: 120000 });
if (native.status !== 0) throw Error(native.stderr || native.error);
const actual = native.stdout.trim().split(/\r?\n/).map(s => JSON.parse(s));
if (actual.length !== cases.length) throw Error(`expected ${cases.length} results, got ${actual.length}`);
for (let i = 0; i < cases.length; i++) {
  if (!isDeepStrictEqual(actual[i], expected[i])) {
    console.error(JSON.stringify({ input: cases[i], expected: expected[i], actual: actual[i] }, null, 2));
    throw Error(`query-paths mismatch ${i}`);
  }
}
if (process.argv.includes('--write')) {
  const dest = path.join(root, 'crates/codegraph-search/tests/fixtures/query-paths.json');
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  // Shared indexes are stored once instead of repeating their paths per query.
  const indexes = [];
  const vectors = cases.map((input, i) => {
    const key = JSON.stringify(input.paths);
    let index = indexes.findIndex(p => JSON.stringify(p) === key);
    if (index < 0) { index = indexes.length; indexes.push(input.paths); }
    const { paths: _, ...rest } = input;
    return { input: { ...rest, index }, expected: expected[i] };
  });
  fs.writeFileSync(dest, JSON.stringify({ indexes, vectors }));
}
console.log(`${cases.length} path-recognition queries and callback traces match the TypeScript oracle`);
