#!/usr/bin/env node
/**
 * Golden-output generator for the Rust `codegraph-search` crate.
 *
 * Runs the TypeScript implementation (the oracle, from a built `dist/`) over a
 * deterministic corpus drawn from this repository — its own file paths and
 * identifiers — and writes the results to
 * crates/codegraph-search/tests/golden/search.json. The Rust tests assert
 * equality with that file, so any drift from the TypeScript behavior fails by
 * input, not by opinion.
 *
 *   npx tsc && node scripts/rust-golden/search.mjs
 */
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const dist = (p) => require(path.join(root, 'dist', p));

const seg = dist('search/identifier-segments');
const qp = dist('search/query-parser');
const qu = dist('search/query-utils');
const gen = dist('extraction/generated-detection');

// --- deterministic corpus ----------------------------------------------------
const files = execFileSync('git', ['ls-files'], { cwd: root, encoding: 'utf8', maxBuffer: 1 << 26 })
  .split('\n').filter(Boolean);

const idents = new Set();
for (const f of files.filter((f) => f.startsWith('src/') && f.endsWith('.ts'))) {
  const text = fs.readFileSync(path.join(root, f), 'utf8');
  for (const m of text.match(/[A-Za-z_][A-Za-z0-9_]{2,}/g) ?? []) idents.add(m);
}
const identList = [...idents].sort();
const pick = (arr, n) => arr.filter((_, i) => i % Math.max(1, Math.floor(arr.length / n)) === 0).slice(0, n);
const sampleIdents = pick(identList, 1500);
// Hand-picked edge cases the repo corpus can't be trusted to contain.
const edgeIdents = ['', 'x', '42', 'HTMLParser', 'parseHTML5Doc', 'getData_getData', 'XMLHttpRequest',
  'ÜberCoolName', 'naïveBayes', 'snake_case_name', 'MAX_RETRY_COUNT', 'a.b.c', 'camelCaseWithManyHumpsInIt',
  'İstanbul', 'straße', 'ΑΒΓ', 'user2Id', 'ID', 'iOS', 'eBay', 'base64Encode'];
const allIdents = [...edgeIdents, ...sampleIdents];

let seed = 0x2545f491;
const rnd = () => { seed ^= seed << 13; seed >>>= 0; seed ^= seed >>> 17; seed ^= seed << 5; seed >>>= 0; return seed / 4294967296; };
const prose = ['how', 'does', 'the', 'cache', 'eviction', 'work', 'where', 'is', 'authentication', 'handled', 'services',
  'classes', 'caches', 'tests', 'login', 'flow', 'résolution', 'références', 'kind:function', 'lang:go', 'path:src/api',
  'name:Handler', 'TODO:', 'http://x.y', '"quoted words"', 'path:"a b"', 'order', 'state', 'machine', 'auto-scroll'];
const queries = [];
for (let i = 0; i < 1200; i++) {
  const n = 1 + Math.floor(rnd() * 4);
  const parts = [];
  for (let j = 0; j < n; j++) parts.push(rnd() < 0.5 ? prose[Math.floor(rnd() * prose.length)] : allIdents[Math.floor(rnd() * allIdents.length)]);
  queries.push(parts.join(' '));
}
queries.push('', '   ', 'kind:', ':x', 'path:"unterminated', 'a "b c" d');

const paths = [...files, 'mock_keeper.go', 'x/bank/types/tx.pb.go', 'core/data/src/main/kotlin/com/google/samples/apps/X.kt',
  'Lib/unittest/case.py', 'llvm/unittests/ADT/X.cpp', 'app/jvmTest/X.kt', 'a/b/c.min.js', 'lib/quux.g.dart'];

const out = {
  segments: allIdents.map((s) => [s, seg.splitIdentifierSegments(s)]),
  searchTerms: allIdents.map((s) => [s, qu.extractSearchTerms(s), qu.extractSearchTerms(s, { stems: false })]),
  stems: allIdents.map((s) => [s.toLowerCase(), qu.getStemVariants(s)]),
  distinctive: allIdents.map((s) => [s, qu.isDistinctiveIdentifier(s)]),
  lookup: allIdents.map((s) => [s.toLowerCase(), seg.segmentLookupVariants(s.toLowerCase())]),
  prose: queries.map((q) => [q, seg.extractProseCandidates(q), seg.extractSegmentSearchWords(q)]),
  parse: queries.map((q) => [q, qp.parseQuery(q)]),
  nameBonus: queries.slice(0, 600).map((q, i) => [allIdents[(i * 7) % allIdents.length], q, qu.nameMatchBonus(allIdents[(i * 7) % allIdents.length], q)]),
  pathScore: queries.slice(0, 600).map((q, i) => [paths[(i * 13) % paths.length], q, qu.scorePathRelevance(paths[(i * 13) % paths.length], q)]),
  pathKinds: paths.map((p) => [p, qu.isTestFile(p), qu.isTestPath(p), gen.isGeneratedFile(p)]),
  editDistance: allIdents.slice(0, 600).map((a, i) => { const b = allIdents[(i * 31 + 5) % allIdents.length].toLowerCase(); return [a.toLowerCase(), b, qu.boundedEditDistance ? 0 : qp.boundedEditDistance(a.toLowerCase(), b, 2)]; }),
  kindBonus: ['file','module','class','struct','interface','trait','protocol','function','method','property','field','variable','constant','enum','enum_member','type_alias','namespace','parameter','import','export','route','component','union'].map((k) => [k, qu.kindBonus(k)]),
};

const dest = path.join(root, 'crates', 'codegraph-search', 'tests', 'golden', 'search.json');
fs.mkdirSync(path.dirname(dest), { recursive: true });
fs.writeFileSync(dest, JSON.stringify(out));
console.log(`wrote ${dest} (${(fs.statSync(dest).size / 1024).toFixed(0)} KiB)`);
