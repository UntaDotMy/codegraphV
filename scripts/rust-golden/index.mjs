#!/usr/bin/env node
/**
 * Differential check of the Rust extraction orchestrator (crates/codegraph-core) against the
 * TypeScript one.
 *
 *   node scripts/rust-golden/index.mjs <index_project-binary> [--work <dir>] [--keep] [--sample N]
 *        [--sync] <corpus>...
 *
 * A corpus is a directory (or `name=dir`). Each one is copied twice. On one copy the TypeScript
 * engine runs `orchestrator.indexAll()` (scan, framework detection, parse, store — no
 * resolution); on the other `index_project` does the same. The `files`, `nodes`, `edges` and
 * `unresolved_refs` tables are then compared row by row, in insertion order.
 *
 * With --sync both sides then take the same edit script (a file added, one changed, one deleted)
 * and sync; the tables and the sync result are compared again.
 *
 * Needs a built TS oracle (`npx tsc && npm run copy-assets`) and
 * `cargo build -p codegraph-core --example index_project`.
 */
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { DatabaseSync } from 'node:sqlite';

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const { CodeGraph } = require(path.join(root, 'dist/index.js'));

const args = process.argv.slice(2);
let work = null;
let keep = false;
let doSync = false;
let sample = 8;
const positional = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--work') work = args[++i];
  else if (args[i] === '--keep') keep = true;
  else if (args[i] === '--sync') doSync = true;
  else if (args[i] === '--sample') sample = Number(args[++i]);
  else positional.push(args[i]);
}
const [bin, ...corpora] = positional;
if (!bin || corpora.length === 0) {
  console.error('usage: index.mjs <index_project-binary> [--work dir] [--keep] [--sync] [--sample N] <corpus>...');
  process.exit(2);
}
work ??= fs.mkdtempSync(path.join(os.tmpdir(), 'cg-index-'));
fs.mkdirSync(work, { recursive: true });

const SKIP_DIRS = new Set(['node_modules', '.git', 'target', 'dist', '.codegraph', 'build', '.svelte-kit']);

function copyCorpus(src, dest) {
  fs.rmSync(dest, { recursive: true, force: true });
  fs.cpSync(src, dest, { recursive: true, filter: (s) => !SKIP_DIRS.has(path.basename(s)) });
}

const dbPathOf = (proj) => path.join(proj, '.codegraph', 'codegraph.db');

function snapshot(proj, to) {
  for (const ext of ['-wal', '-shm']) fs.rmSync(dbPathOf(proj) + ext, { force: true });
  fs.copyFileSync(dbPathOf(proj), to);
}

// Columns that legitimately differ between two runs.
const VOLATILE = { files: ['indexed_at', 'modified_at'], nodes: ['updated_at'], edges: ['id'], unresolved_refs: ['id'] };
const ORDER = { files: 'path', nodes: 'rowid', edges: 'id', unresolved_refs: 'id' };

function readTables(dbFile) {
  const db = new DatabaseSync(dbFile, { readOnly: true });
  const out = {};
  for (const table of Object.keys(ORDER)) {
    out[table] = db
      .prepare(`SELECT * FROM ${table} ORDER BY ${ORDER[table]}`)
      .all()
      .map((r) => {
        const row = { ...r };
        for (const c of VOLATILE[table]) delete row[c];
        return row;
      });
  }
  db.close();
  return out;
}

const keyOf = {
  files: (r) => r.path,
  nodes: (r) => r.id,
  edges: (r) => `${r.source}\0${r.target}\0${r.kind}\0${r.line}\0${r.col}`,
  unresolved_refs: (r) => `${r.from_node_id}\0${r.reference_name}\0${r.reference_kind}\0${r.line}\0${r.col}`,
};

function normalize(row) {
  // Embedded JSON compared structurally, so key order and number formatting don't matter.
  const out = {};
  for (const [k, v] of Object.entries(row)) {
    if (typeof v === 'string' && (v.startsWith('{') || v.startsWith('['))) {
      try {
        out[k] = JSON.stringify(JSON.parse(v), (_k, val) => (val && typeof val === 'object' && !Array.isArray(val) ? Object.fromEntries(Object.entries(val).sort(([a], [b]) => (a < b ? -1 : 1))) : val));
        continue;
      } catch {}
    }
    out[k] = typeof v === 'bigint' ? Number(v) : v;
  }
  return out;
}

function diffTable(name, ts, rust) {
  const tsMap = new Map(ts.map((r) => [keyOf[name](r), normalize(r)]));
  const rustMap = new Map(rust.map((r) => [keyOf[name](r), normalize(r)]));
  const onlyTs = [...tsMap.keys()].filter((k) => !rustMap.has(k));
  const onlyRust = [...rustMap.keys()].filter((k) => !tsMap.has(k));
  const changed = [];
  for (const [k, a] of tsMap) {
    const b = rustMap.get(k);
    if (!b) continue;
    const cols = Object.keys(a).filter((c) => JSON.stringify(a[c]) !== JSON.stringify(b[c]));
    if (cols.length) changed.push({ k, cols, a, b });
  }
  // Insertion order of the keys both sides hold.
  const tsOrder = ts.map((r) => keyOf[name](r)).filter((k) => rustMap.has(k));
  const rustOrder = rust.map((r) => keyOf[name](r)).filter((k) => tsMap.has(k));
  const orderDiffers = tsOrder.some((k, i) => k !== rustOrder[i]);
  return { name, onlyTs, onlyRust, changed, orderDiffers, tsCount: ts.length, rustCount: rust.length };
}

function report(label, tables) {
  const bad = tables.filter((t) => t.onlyTs.length || t.onlyRust.length || t.changed.length || t.orderDiffers);
  const counts = tables.map((t) => `${t.name}=${t.tsCount}/${t.rustCount}`).join(' ');
  if (!bad.length) {
    console.log(`  ${label}: identical (${counts})`);
    return true;
  }
  console.log(`  ${label}: DIFF (${counts})`);
  for (const t of bad) {
    console.log(`    ${t.name}: onlyTS=${t.onlyTs.length} onlyRust=${t.onlyRust.length} changed=${t.changed.length} order=${t.orderDiffers ? 'DIFFERS' : 'same'}`);
    for (const k of t.onlyTs.slice(0, sample)) console.log(`      only TS:   ${JSON.stringify(k)}`);
    for (const k of t.onlyRust.slice(0, sample)) console.log(`      only Rust: ${JSON.stringify(k)}`);
    const byCols = new Map();
    for (const c of t.changed) byCols.set(c.cols.join(','), (byCols.get(c.cols.join(',')) ?? 0) + 1);
    for (const [cols, n] of [...byCols].slice(0, 6)) console.log(`      ${n} rows differ in: ${cols}`);
    for (const c of t.changed.slice(0, sample)) {
      for (const col of c.cols.slice(0, 2)) console.log(`      ${JSON.stringify(c.k)}.${col}\n        ts:   ${JSON.stringify(c.a[col])?.slice(0, 300)}\n        rust: ${JSON.stringify(c.b[col])?.slice(0, 300)}`);
    }
  }
  return false;
}

function compareDbs(label, tsDb, rustDb) {
  const ts = readTables(tsDb);
  const rust = readTables(rustDb);
  return report(label, Object.keys(ORDER).map((t) => diffTable(t, ts[t], rust[t])));
}

/** The edit script applied to both copies before the sync round. */
function applyEdits(proj) {
  const files = [];
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      if (SKIP_DIRS.has(e.name)) continue;
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else files.push(p);
    }
  };
  walk(proj);
  files.sort();
  const sources = files.filter((f) => /\.(ts|tsx|js|jsx|py|go|rs|java|rb|php|c|cpp|cs|swift|kt|dart|lua|scala)$/.test(f));
  const actions = [];
  if (sources[0]) {
    fs.appendFileSync(sources[0], '\n// edited by index.mjs\n');
    actions.push(`edit ${path.relative(proj, sources[0])}`);
  }
  if (sources.length > 2) {
    fs.rmSync(sources[sources.length - 1]);
    actions.push(`delete ${path.relative(proj, sources[sources.length - 1])}`);
  }
  if (sources[1]) {
    const ext = path.extname(sources[1]);
    const fresh = path.join(path.dirname(sources[1]), `__added_by_index_mjs${ext}`);
    fs.copyFileSync(sources[1], fresh);
    actions.push(`add ${path.relative(proj, fresh)}`);
  }
  return actions;
}

let allClean = true;
for (const spec of corpora) {
  const eq = spec.indexOf('=');
  const name = eq > 0 ? spec.slice(0, eq) : path.basename(path.resolve(spec));
  const src = path.resolve(eq > 0 ? spec.slice(eq + 1) : spec);
  const dir = path.join(work, name);
  const tsProj = path.join(dir, 'ts');
  const rustProj = path.join(dir, 'rust');
  fs.mkdirSync(dir, { recursive: true });
  copyCorpus(src, tsProj);
  copyCorpus(src, rustProj);

  console.log(name);
  let tsResult;
  let tsMs;
  try {
    const t0 = Date.now();
    const cg = await CodeGraph.init(tsProj);
    tsResult = await cg.orchestrator.indexAll();
    tsMs = Date.now() - t0;
    cg.close();
  } catch (e) {
    console.log(`  FAIL ts: ${e.message}`);
    allClean = false;
    continue;
  }
  const tsDb = path.join(dir, 'ts.db');
  snapshot(tsProj, tsDb);

  const rustDb = path.join(dir, 'rust.db');
  fs.mkdirSync(path.dirname(dbPathOf(rustProj)), { recursive: true });
  const t1 = Date.now();
  let rustResult;
  try {
    rustResult = JSON.parse(execFileSync(bin, [rustProj, dbPathOf(rustProj)], { encoding: 'utf8', maxBuffer: 1 << 28 }).trim().split('\n').pop());
  } catch (e) {
    console.log(`  FAIL rust: ${String(e.stderr || e.message).slice(0, 500)}`);
    allClean = false;
    continue;
  }
  const rustMs = Date.now() - t1;
  fs.copyFileSync(dbPathOf(rustProj), rustDb);

  const keys = ['filesIndexed', 'filesSkipped', 'filesErrored', 'filesDiscovered', 'nodesCreated', 'edgesCreated'];
  const mismatch = keys.filter((k) => (tsResult[k] ?? undefined) !== (rustResult[k] ?? undefined));
  console.log(`  ts ${tsMs}ms files=${tsResult.filesIndexed} nodes=${tsResult.nodesCreated}; rust ${rustMs}ms files=${rustResult.filesIndexed} nodes=${rustResult.nodesCreated}${mismatch.length ? `  RESULT DIFFERS: ${mismatch.map((k) => `${k} ${tsResult[k]}/${rustResult[k]}`).join(', ')}` : ''}`);
  if (mismatch.length) allClean = false;
  allClean = compareDbs('index', tsDb, rustDb) && allClean;

  if (doSync) {
    const actions = applyEdits(tsProj);
    applyEdits(rustProj);
    const cg = await CodeGraph.open(tsProj);
    const tsSync = await cg.orchestrator.sync();
    cg.close();
    snapshot(tsProj, tsDb);
    const rustSync = JSON.parse(execFileSync(bin, [rustProj, dbPathOf(rustProj), 'sync'], { encoding: 'utf8', maxBuffer: 1 << 28 }).trim().split('\n').pop());
    fs.copyFileSync(dbPathOf(rustProj), rustDb);
    const sk = ['filesChecked', 'filesAdded', 'filesModified', 'filesRemoved', 'nodesUpdated'];
    const syncMismatch = sk.filter((k) => tsSync[k] !== rustSync[k]);
    const listDiffers = ['changedFilePaths', 'addedFilePaths', 'removedFilePaths', 'failedFilePaths'].filter((k) => JSON.stringify(tsSync[k] ?? null) !== JSON.stringify(rustSync[k] ?? null));
    const deltaDiffers = JSON.stringify([...(tsSync.definitionDelta ?? [])].sort()) !== JSON.stringify([...(rustSync.definitionDelta ?? [])].sort());
    console.log(`  sync after [${actions.join('; ')}]: ${syncMismatch.length || listDiffers.length || deltaDiffers ? `RESULT DIFFERS ${syncMismatch.map((k) => `${k} ${tsSync[k]}/${rustSync[k]}`).join(', ')} ${listDiffers.join(',')}${deltaDiffers ? ' definitionDelta' : ''}` : 'result identical'}`);
    if (syncMismatch.length || listDiffers.length || deltaDiffers) allClean = false;
    allClean = compareDbs('sync', tsDb, rustDb) && allClean;
  }
}
if (!keep) {
  try {
    fs.rmSync(work, { recursive: true, force: true });
  } catch {}
} else {
  console.log(`kept ${work}`);
}
process.exit(allClean ? 0 : 1);
