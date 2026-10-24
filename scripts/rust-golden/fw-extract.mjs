#!/usr/bin/env node
/**
 * Differential check of the Rust framework `extract` hooks (crates/codegraph-resolve/src/frameworks)
 * against the TypeScript ones, file by file.
 *
 *   node scripts/rust-golden/fw-extract.mjs <fw_extract-binary> [--max-samples N] [--only fw1,fw2]
 *        <file-or-dir>...
 *
 * Every framework resolver's `extract(filePath, content)` is tried on every readable text file
 * (the hooks filter on the path themselves), on both sides. Compared per (resolver, file): the
 * nodes (minus `updatedAt`) and the references, key order ignored. Exit status 1 on any diff.
 *
 * Needs a built TS oracle (`npx tsc && npm run copy-assets`) and `cargo build -p codegraph-resolve
 * --example fw_extract`.
 */
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const { getAllFrameworkResolvers } = require(path.join(root, 'dist/resolution/frameworks/index.js'));

const args = process.argv.slice(2);
let maxSamples = 6;
let only = null;
const positional = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--max-samples') maxSamples = Number(args[++i]);
  else if (args[i] === '--only') only = new Set(args[++i].split(','));
  else positional.push(args[i]);
}
const [bin, ...inputs] = positional;
if (!bin || inputs.length === 0) {
  console.error('usage: fw-extract.mjs <fw_extract-binary> [--max-samples N] [--only fw,fw] <file-or-dir>...');
  process.exit(2);
}

const SKIP_DIRS = new Set(['node_modules', '.git', 'target', 'dist', '.codegraph', 'build', '.svelte-kit']);
const MAX_BYTES = 1024 * 1024;

function walk(dir, base, out) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    if (e.isDirectory()) {
      if (!SKIP_DIRS.has(e.name)) walk(path.join(dir, e.name), base, out);
    } else if (e.isFile()) {
      const abs = path.join(dir, e.name);
      const size = fs.statSync(abs).size;
      if (size > 0 && size <= MAX_BYTES) out.push({ abs, rel: path.relative(base, abs).split(path.sep).join('/') });
    }
  }
}

const files = [];
for (const input of inputs) {
  const stat = fs.statSync(input);
  if (stat.isDirectory()) walk(input, input, files);
  else files.push({ abs: input, rel: path.basename(input) });
}

// Text files only: a NUL byte means binary.
const text = [];
for (const f of files) {
  const buf = fs.readFileSync(f.abs);
  if (buf.includes(0)) continue;
  text.push({ ...f, content: buf.toString('utf8') });
}

function canon(v) {
  if (Array.isArray(v)) return v.map(canon);
  if (v && typeof v === 'object') {
    const out = {};
    for (const k of Object.keys(v).sort()) {
      if (v[k] !== undefined) out[k] = canon(v[k]);
    }
    return out;
  }
  return v;
}

const resolvers = getAllFrameworkResolvers().filter((r) => !only || only.has(r.name));
const expected = new Map();
for (const f of text) {
  for (const fw of resolvers) {
    if (typeof fw.extract !== 'function') continue;
    let r;
    try {
      r = fw.extract(f.rel, f.content);
    } catch {
      expected.set(`${fw.name}\0${f.rel}`, { panic: true });
      continue;
    }
    if (!r || ((r.nodes?.length ?? 0) === 0 && (r.references?.length ?? 0) === 0)) continue;
    const nodes = (r.nodes ?? []).map((n) => {
      const { updatedAt, ...rest } = n;
      return canon(rest);
    });
    expected.set(`${fw.name}\0${f.rel}`, { nodes, references: canon(r.references ?? []) });
  }
}

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-fwx-'));
const manifest = path.join(dir, 'manifest.json');
fs.writeFileSync(manifest, JSON.stringify(text.map(({ abs, rel }) => ({ abs, rel }))));
const stdout = execFileSync(bin, [manifest], { maxBuffer: 1 << 30, encoding: 'utf8' });
const actualList = JSON.parse(stdout).filter((e) => !only || only.has(e.fw));
const actual = new Map();
for (const e of actualList) {
  actual.set(`${e.fw}\0${e.rel}`, e.panic ? { panic: true } : { nodes: canon(e.nodes), references: canon(e.references) });
}

let diffs = 0;
const byFw = new Map();
const keys = new Set([...expected.keys(), ...actual.keys()]);
for (const key of [...keys].sort()) {
  const a = JSON.stringify(expected.get(key) ?? null);
  const b = JSON.stringify(actual.get(key) ?? null);
  if (a === b) continue;
  diffs++;
  const [fw, rel] = key.split('\0');
  const list = byFw.get(fw) ?? [];
  list.push({ rel, ts: expected.get(key) ?? null, rust: actual.get(key) ?? null });
  byFw.set(fw, list);
}

console.log(`${text.length} files, ${expected.size} extractions in TS, ${actual.size} in Rust, ${diffs} differ`);
for (const [fw, list] of byFw) {
  console.log(`\n== ${fw}: ${list.length} file(s) differ`);
  for (const d of list.slice(0, maxSamples)) {
    console.log(`-- ${d.rel}`);
    console.log(`   TS  : ${JSON.stringify(d.ts)?.slice(0, 700)}`);
    console.log(`   Rust: ${JSON.stringify(d.rust)?.slice(0, 700)}`);
  }
}
fs.rmSync(dir, { recursive: true, force: true });
process.exit(diffs ? 1 : 0);
