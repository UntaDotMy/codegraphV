#!/usr/bin/env node
/**
 * Extraction differential: the Rust walkers (crates/codegraph-extract) against the
 * TypeScript wasm extractor, file by file.
 *
 *   node scripts/rust-golden/extract.mjs <extract_dump-binary> <file-or-dir>...
 *        [--lang typescript,python] [--max-files N] [--max-samples N] [--show-tolerant]
 *
 * The oracle is the TS engine with the native kernel switched OFF (CODEGRAPH_KERNEL=0), so
 * every file goes through the wasm TreeSitterExtractor — the reference behavior the
 * Rust code must reproduce. Files whose parse tree has errors are compared only
 * when `--show-tolerant` is given: the Rust engine proceeds on error trees (the
 * napi kernel deferred them), and wasm's error recovery is the one thing the two
 * parsers can legitimately disagree on.
 *
 * Needs a built TS oracle (`npx tsc && npm run copy-assets`).
 * Exit code: 0 = parity on every compared file, 1 = diffs, 2 = setup error.
 */
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

process.env.CODEGRAPH_KERNEL = '0';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const dist = (p) => pathToFileURL(path.join(ROOT, 'dist', p)).href;

const args = process.argv.slice(2);
const bin = args.shift();
const inputs = [];
let langFilter = null;
let maxFiles = Infinity;
let maxSamples = 4;
let showTolerant = false;
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--lang') langFilter = new Set(args[++i].split(','));
  else if (args[i] === '--max-files') maxFiles = Number(args[++i]);
  else if (args[i] === '--max-samples') maxSamples = Number(args[++i]);
  else if (args[i] === '--show-tolerant') showTolerant = true;
  else inputs.push(args[i]);
}
if (!bin || inputs.length === 0) {
  console.error('usage: extract.mjs <extract_dump-binary> <file-or-dir>... [--lang a,b] [--max-files N]');
  process.exit(2);
}

// Languages the Rust crate extracts natively so far.
const RUST_LANGS = new Set([
  'typescript', 'tsx', 'javascript', 'jsx', 'java', 'python', 'go', 'c', 'cpp', 'rust', 'csharp', 'ruby',
  'php', 'swift', 'kotlin', 'r', 'lua', 'luau', 'scala', 'dart', 'nix', 'solidity', 'erlang', 'terraform', 'cobol', 'arkts', 'objc', 'vbnet', 'pascal',
]);
const { extractFromSource } = await import(dist('extraction/tree-sitter.js'));
const { initGrammars, loadGrammarsForLanguages, detectLanguage, isSourceFile } = await import(dist('extraction/grammars.js'));
await initGrammars();
await loadGrammarsForLanguages([...RUST_LANGS]);

// ---------------------------------------------------------------------------
const SKIP_DIRS = new Set(['node_modules', '.git', 'dist', '.codegraph', 'target']);
function collect(p, out) {
  let st;
  try {
    st = fs.statSync(p);
  } catch {
    return;
  }
  if (st.isDirectory()) {
    if (SKIP_DIRS.has(path.basename(p))) return;
    for (const e of fs.readdirSync(p).sort()) collect(path.join(p, e), out);
  } else if (st.size < 400_000 && isSourceFile(p)) out.push(p);
}
const all = [];
for (const p of inputs) collect(path.resolve(p), all);

const files = [];
for (const abs of all) {
  if (files.length >= maxFiles) break;
  let source;
  try {
    source = fs.readFileSync(abs, 'utf8');
  } catch {
    continue;
  }
  // Skip files with invalid UTF-8 (a lossy read differs between the two readers).
  if (source.includes('�')) continue;
  const rel = path.basename(abs); // basename keeps ids independent of the corpus location
  const language = detectLanguage(rel, source);
  if (!RUST_LANGS.has(language)) continue;
  if (langFilter && !langFilter.has(language)) continue;
  files.push({ abs, rel, language, source });
}
if (files.length === 0) {
  console.error('no matching files');
  process.exit(2);
}

// ---------------------------------------------------------------------------
const canon = (v) =>
  Array.isArray(v)
    ? v.map(canon)
    : v && typeof v === 'object'
      ? Object.fromEntries(
          Object.keys(v)
            .sort()
            .filter((k) => v[k] !== undefined && v[k] !== null)
            .map((k) => [k, canon(v[k])])
        )
      : v;
const key = (o) => JSON.stringify(canon(o));
function normResult(r) {
  return {
    nodes: r.nodes.map(({ updatedAt, ...n }) => key(n)),
    edges: r.edges.map(key),
    refs: r.unresolvedReferences.map(({ rowId, ...x }) => key(x)),
    errors: r.errors.map((e) => key({ ...e })),
  };
}
function multisetDiff(a, b) {
  const m = new Map();
  for (const x of a) m.set(x, (m.get(x) ?? 0) + 1);
  for (const x of b) m.set(x, (m.get(x) ?? 0) - 1);
  const onlyA = [];
  const onlyB = [];
  for (const [k, c] of m) for (let i = 0; i < Math.abs(c); i++) (c > 0 ? onlyA : onlyB).push(k);
  return { onlyA, onlyB };
}

// TS oracle (wasm path) ------------------------------------------------------
const oracle = new Array(files.length).fill(null);
let tsFailures = 0;
for (const [idx, f] of files.entries()) {
  try {
    oracle[idx] = normResult(extractFromSource(f.rel, f.source, f.language));
  } catch (e) {
    tsFailures++;
  }
}

// Rust -----------------------------------------------------------------------
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-extract-'));
const manifest = path.join(tmp, 'manifest.json');
fs.writeFileSync(manifest, JSON.stringify(files.map((f) => ({ abs: f.abs, rel: f.rel, language: f.language }))));
const run = (extra) =>
  JSON.parse(execFileSync(bin, [manifest, ...extra], { encoding: 'utf8', maxBuffer: 1 << 30 }));
const strict = run(['--defer-on-errors']);
const tolerant = showTolerant ? run([]) : null;

// Compare --------------------------------------------------------------------
const buckets = new Map();
function report(kind, sample) {
  const b = buckets.get(kind) ?? { count: 0, samples: [] };
  b.count++;
  if (b.samples.length < maxSamples) b.samples.push(sample);
  buckets.set(kind, b);
}
function compare(entry, idx, label) {
  const f = files[idx];
  const want = oracle[idx];
  if (!want) return 'skipped';
  const got = normResult(entry.result);
  let diff = false;
  for (const part of ['nodes', 'edges', 'refs', 'errors']) {
    const { onlyA, onlyB } = multisetDiff(want[part], got[part]);
    if (onlyA.length || onlyB.length) {
      diff = true;
      report(`${label}:${f.language}:${part}`, { file: f.abs, onlyTs: onlyA.slice(0, 2), onlyRust: onlyB.slice(0, 2) });
    }
  }
  return diff ? 'diff' : 'ok';
}
const tally = { ok: 0, diff: 0, deferred: 0, skipped: 0 };
const perLang = new Map();
for (const [idx, entry] of strict.entries()) {
  const lang = entry.language;
  const stat = perLang.get(lang) ?? { ok: 0, diff: 0, deferred: 0 };
  perLang.set(lang, stat);
  if (!entry.ok) {
    if (String(entry.error).startsWith('defer:')) {
      tally.deferred++;
      stat.deferred++;
      continue;
    }
    report(`rust-error:${lang}`, { file: entry.rel, error: entry.error });
    tally.diff++;
    stat.diff++;
    continue;
  }
  const r = compare(entry, idx, 'strict');
  tally[r]++;
  if (r === 'ok' || r === 'diff') stat[r]++;
}
const tolerantTally = { ok: 0, diff: 0 };
if (tolerant) {
  const deferredIdx = new Set(strict.flatMap((e, i) => (!e.ok && String(e.error).startsWith('defer:') ? [i] : [])));
  for (const [idx, entry] of tolerant.entries()) {
    if (!deferredIdx.has(idx)) continue;
    if (!entry.ok) {
      report(`tolerant-error:${entry.language}`, { file: entry.rel, error: entry.error });
      tolerantTally.diff++;
      continue;
    }
    const r = compare(entry, idx, 'tolerant');
    if (r === 'ok' || r === 'diff') tolerantTally[r]++;
  }
}

console.log(`files: ${files.length}  (TS oracle failures: ${tsFailures})`);
for (const [lang, s] of [...perLang].sort()) {
  console.log(`  ${lang.padEnd(11)} ok ${String(s.ok).padStart(5)}   diff ${String(s.diff).padStart(4)}   deferred(parse errors) ${s.deferred}`);
}
if (tolerant) console.log(`tolerant mode on the deferred files: ok ${tolerantTally.ok}, diff ${tolerantTally.diff}`);
for (const [kind, b] of [...buckets].sort((a, b) => b[1].count - a[1].count)) {
  console.log(`\n[${kind}] ${b.count} file(s)`);
  for (const s of b.samples) console.log('  ' + JSON.stringify(s).slice(0, 900));
}
const strictDiffs = [...buckets.keys()].filter((k) => k.startsWith('strict:') || k.startsWith('rust-error:')).length;
console.log(strictDiffs === 0 ? '\nPARITY on all compared files' : `\n${strictDiffs} diff categories`);
process.exit(strictDiffs === 0 ? 0 : 1);
