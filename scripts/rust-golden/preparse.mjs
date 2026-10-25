#!/usr/bin/env node
/**
 * `preParse` differential: the Rust hooks (crates/codegraph-extract/src/preparse)
 * against the TypeScript `EXTRACTORS[language].preParse`, file by file, byte for byte.
 *
 *   node scripts/rust-golden/preparse.mjs <preparse_dump-binary> <file-or-dir>...
 *        [--lang c,cpp,kotlin] [--max-files N] [--max-samples N] [--only-changed]
 *
 * Needs a built TS oracle (`npx tsc`). Exit code: 0 = identical on every file,
 * 1 = diffs, 2 = setup error.
 */
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const dist = (p) => pathToFileURL(path.join(ROOT, 'dist', p)).href;

const args = process.argv.slice(2);
const bin = args.shift();
const inputs = [];
let langFilter = null;
let maxFiles = Infinity;
let maxSamples = 5;
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--lang') langFilter = new Set(args[++i].split(','));
  else if (args[i] === '--max-files') maxFiles = Number(args[++i]);
  else if (args[i] === '--max-samples') maxSamples = Number(args[++i]);
  else inputs.push(args[i]);
}
if (!bin || inputs.length === 0) {
  console.error('usage: preparse.mjs <preparse_dump-binary> <file-or-dir>... [--lang a,b] [--max-files N]');
  process.exit(2);
}

const { EXTRACTORS } = await import(dist('extraction/languages/index.js'));
const { detectLanguage, isSourceFile } = await import(dist('extraction/grammars.js'));

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
  } else if (st.size < 1_500_000 && isSourceFile(p)) out.push(p);
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
  if (source.includes('�')) continue; // lossy decode differs between the two readers
  const rel = path.basename(abs);
  const language = detectLanguage(rel, source);
  if (!EXTRACTORS[language]?.preParse) continue;
  if (!['c', 'cpp', 'csharp', 'kotlin', 'swift', 'typescript', 'tsx', 'pascal'].includes(language)) continue;
  if (langFilter && !langFilter.has(language)) continue;
  files.push({ abs, rel, language, source });
}
if (files.length === 0) {
  console.error('no matching files');
  process.exit(2);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-preparse-'));
const manifest = path.join(tmp, 'manifest.json');
fs.writeFileSync(manifest, JSON.stringify(files.map((f) => ({ abs: f.abs, rel: f.rel, language: f.language }))));
const rust = JSON.parse(execFileSync(bin, [manifest], { encoding: 'utf8', maxBuffer: 1 << 30 }));

const perLang = new Map();
const diffs = [];
let tsMs = 0;
let rustMs = 0;
for (const [i, f] of files.entries()) {
  const t0 = performance.now();
  const want = EXTRACTORS[f.language].preParse(f.source, f.rel);
  tsMs += performance.now() - t0;
  rustMs += rust[i].ms;
  const stat = perLang.get(f.language) ?? { ok: 0, diff: 0, changed: 0 };
  perLang.set(f.language, stat);
  if (want !== f.source) stat.changed++;
  if (want === rust[i].out) {
    stat.ok++;
  } else {
    stat.diff++;
    diffs.push({ f, want, got: rust[i].out });
  }
}

console.log(`files: ${files.length}   (TS preParse ${tsMs.toFixed(0)} ms, Rust ${rustMs.toFixed(0)} ms)`);
for (const [lang, s] of [...perLang].sort()) {
  console.log(`  ${lang.padEnd(11)} identical ${String(s.ok).padStart(5)}   diff ${String(s.diff).padStart(4)}   (hook changed the text in ${s.changed})`);
}
for (const { f, want, got } of diffs.slice(0, maxSamples)) {
  const wl = want.split('\n');
  const gl = got.split('\n');
  let line = 0;
  while (line < wl.length && wl[line] === gl[line]) line++;
  console.log(`\n[${f.language}] ${f.abs}  (TS length ${want.length}, Rust length ${got.length}); first difference at line ${line + 1}`);
  console.log('  TS:   ' + JSON.stringify(wl[line] ?? null).slice(0, 300));
  console.log('  Rust: ' + JSON.stringify(gl[line] ?? null).slice(0, 300));
  console.log('  src:  ' + JSON.stringify(f.source.split('\n')[line] ?? null).slice(0, 300));
}
console.log(diffs.length === 0 ? '\nIDENTICAL on all compared files' : `\n${diffs.length} file(s) differ`);
process.exit(diffs.length === 0 ? 0 : 1);
