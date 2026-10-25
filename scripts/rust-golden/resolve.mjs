#!/usr/bin/env node
/**
 * Differential check of the Rust reference resolver (crates/codegraph-resolve) against the
 * TypeScript one.
 *
 *   node scripts/rust-golden/resolve.mjs <resolve_db-binary> [--work <dir>] [--keep] [--sample N]
 *        <corpus>...
 *
 * A corpus is a directory (or `name=dir`). Each one is copied to a scratch project and
 * indexed by the TypeScript engine up to — not including — reference resolution, and the
 * database is snapshotted. Then, on separate copies of that snapshot:
 *
 *   ts.db    the TypeScript resolver runs the steps `CodeGraph.indexAll` runs after extraction
 *            (initialize, runPostExtract, resolveAndPersistBatched without synthesis, the
 *            chained-call and this-member passes);
 *   rust.db  `resolve_db` does the same.
 *
 * The edges both leave and the references each leaves behind are compared row by row.
 * With --synth both sides also run the whole-graph synthesis passes (callback-synthesizer et al.).
 * Exit status 1 when any corpus differs.
 *
 * Needs a built TS oracle (`npx tsc && npm run copy-assets`) and `cargo build -p
 * codegraph-resolve --example resolve_db`.
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
let synth = false;
let sample = 12;
const positional = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--work') work = args[++i];
  else if (args[i] === '--keep') keep = true;
  else if (args[i] === '--synth') synth = true;
  else if (args[i] === '--sample') sample = Number(args[++i]);
  else positional.push(args[i]);
}
const [bin, ...corpora] = positional;
if (!bin || corpora.length === 0) {
  console.error('usage: resolve.mjs <resolve_db-binary> [--work dir] [--keep] [--sample N] <corpus>...');
  process.exit(2);
}
work ??= fs.mkdtempSync(path.join(os.tmpdir(), 'cg-resolve-'));
fs.mkdirSync(work, { recursive: true });

const SKIP_DIRS = new Set(['node_modules', '.git', 'target', 'dist', '.codegraph', 'build', '.svelte-kit']);

function copyCorpus(src, dest) {
  fs.rmSync(dest, { recursive: true, force: true });
  fs.cpSync(src, dest, {
    recursive: true,
    filter: (s) => !SKIP_DIRS.has(path.basename(s)),
  });
}

function dbPathOf(proj) {
  return path.join(proj, '.codegraph', 'codegraph.db');
}

function snapshot(proj, to) {
  for (const ext of ['-wal', '-shm']) fs.rmSync(dbPathOf(proj) + ext, { force: true });
  fs.copyFileSync(dbPathOf(proj), to);
}

function restore(proj, from) {
  for (const ext of ['-wal', '-shm']) fs.rmSync(dbPathOf(proj) + ext, { force: true });
  fs.copyFileSync(from, dbPathOf(proj));
}

async function indexWithoutResolution(proj) {
  const cg = await CodeGraph.init(proj);
  const result = await cg.orchestrator.indexAll();
  if (!result.success) throw new Error(`index failed: ${JSON.stringify(result.errors?.slice(0, 3))}`);
  cg.resolver.initialize();
  cg.resolver.runPostExtract();
  const counts = { files: result.filesIndexed, nodes: result.nodesCreated, refs: cg.queries.getUnresolvedReferencesCount() };
  cg.close();
  return counts;
}

async function resolveWithTs(proj) {
  const cg = await CodeGraph.open(proj);
  cg.resolver.initialize();
  cg.resolver.runPostExtract();
  const stats = await cg.resolver.resolveAndPersistBatched(undefined, undefined, undefined, undefined, synth);
  const chained = await cg.resolver.resolveChainedCallsViaConformance();
  const thisMember = await cg.resolver.resolveDeferredThisMemberRefs();
  const frameworks = cg.resolver.getDetectedFrameworks();
  cg.close();
  return { ...stats.stats, chained, thisMember, frameworks };
}

function resolveWithRust(proj) {
  const out = execFileSync(bin, [proj, dbPathOf(proj), ...(synth ? ['--synth'] : [])], { encoding: 'utf8', maxBuffer: 1 << 28 });
  return JSON.parse(out.trim().split('\n').pop());
}

function readEdges(dbFile) {
  const db = new DatabaseSync(dbFile, { readOnly: true });
  const rows = db
    .prepare('SELECT source, target, kind, line, col, metadata, provenance FROM edges')
    .all()
    .map((r) => ({ ...r }));
  const refs = db
    .prepare('SELECT from_node_id, reference_name, reference_kind, line, col, status, name_tail FROM unresolved_refs')
    .all()
    .map((r) => ({ ...r }));
  db.close();
  return { rows, refs };
}

const edgeKey = (r) => `${r.source}\0${r.target}\0${r.kind}\0${r.line}\0${r.col}`;
const refKey = (r) => `${r.from_node_id}\0${r.reference_name}\0${r.reference_kind}\0${r.line}\0${r.col}`;

function tally(rows, pick) {
  const t = new Map();
  for (const r of rows) {
    const k = pick(r);
    t.set(k, (t.get(k) ?? 0) + 1);
  }
  return [...t.entries()].sort((a, b) => b[1] - a[1]);
}

const resolvedBy = (r) => {
  try {
    return JSON.parse(r.metadata ?? '{}').resolvedBy ?? '-';
  } catch {
    return '?';
  }
};

function compare(name, ts, rust) {
  const tsByKey = new Map(ts.rows.map((r) => [edgeKey(r), r]));
  const rustByKey = new Map(rust.rows.map((r) => [edgeKey(r), r]));
  const onlyTs = [];
  const onlyRust = [];
  const metaDiffers = [];
  let same = 0;
  for (const [k, r] of tsByKey) {
    const o = rustByKey.get(k);
    if (!o) onlyTs.push(r);
    else if (o.metadata !== r.metadata || o.provenance !== r.provenance) metaDiffers.push([r, o]);
    else same++;
  }
  for (const [k, r] of rustByKey) if (!tsByKey.has(k)) onlyRust.push(r);

  const tsRefs = new Map(ts.refs.map((r) => [refKey(r), r]));
  const rustRefs = new Map(rust.refs.map((r) => [refKey(r), r]));
  const refOnlyTs = [...tsRefs].filter(([k]) => !rustRefs.has(k)).map(([, r]) => r);
  const refOnlyRust = [...rustRefs].filter(([k]) => !tsRefs.has(k)).map(([, r]) => r);
  const refStatusDiffers = [...tsRefs]
    .filter(([k, r]) => rustRefs.has(k) && (rustRefs.get(k).status !== r.status || rustRefs.get(k).name_tail !== r.name_tail))
    .map(([k, r]) => [r, rustRefs.get(k)]);

  const clean = !onlyTs.length && !onlyRust.length && !metaDiffers.length && !refOnlyTs.length && !refOnlyRust.length && !refStatusDiffers.length;
  const pct = ts.rows.length ? ((same / ts.rows.length) * 100).toFixed(1) : '100.0';
  console.log(
    `${clean ? 'OK  ' : 'DIFF'} ${name}: ts=${ts.rows.length} rust=${rust.rows.length} identical=${same} (${pct}%) ` +
      `onlyTS=${onlyTs.length} onlyRust=${onlyRust.length} metaDiff=${metaDiffers.length} ` +
      `refs onlyTS=${refOnlyTs.length} onlyRust=${refOnlyRust.length} statusDiff=${refStatusDiffers.length}`,
  );
  if (!clean) {
    const show = (label, rows, fmt) => {
      if (!rows.length) return;
      console.log(`  ${label}:`);
      for (const [k, n] of tally(rows, fmt).slice(0, 8)) console.log(`    ${String(n).padStart(6)}  ${k}`);
    };
    show('edges only in TS, by kind/resolvedBy', onlyTs, (r) => `${r.kind} ${resolvedBy(r)}`);
    show('edges only in Rust, by kind/resolvedBy', onlyRust, (r) => `${r.kind} ${resolvedBy(r)}`);
    show('edges with different metadata', metaDiffers.map(([a]) => a), (r) => `${r.kind} ${resolvedBy(r)}`);
    show('refs only in TS (Rust resolved/removed them)', refOnlyTs, (r) => `${r.reference_kind} ${r.status}`);
    show('refs only in Rust (Rust left them)', refOnlyRust, (r) => `${r.reference_kind} ${r.status}`);
    const dump = (label, rows, fmt) => {
      if (!rows.length) return;
      console.log(`  sample ${label}:`);
      for (const r of rows.slice(0, sample)) console.log(`    ${fmt(r)}`);
    };
    dump('edges only in TS', onlyTs, (r) => `${r.source} -[${r.kind}@${r.line}:${r.col}]-> ${r.target}  ${r.metadata}`);
    dump('edges only in Rust', onlyRust, (r) => `${r.source} -[${r.kind}@${r.line}:${r.col}]-> ${r.target}  ${r.metadata}`);
    dump('metadata diffs', metaDiffers, ([a, b]) => `${a.source} -[${a.kind}@${a.line}]-> ${a.target}\n      ts:   ${a.metadata}\n      rust: ${b.metadata}`);
    dump('refs only in Rust', refOnlyRust, (r) => `${r.from_node_id} ${r.reference_kind} ${r.reference_name}@${r.line}:${r.col} ${r.status}`);
    dump('refs only in TS', refOnlyTs, (r) => `${r.from_node_id} ${r.reference_kind} ${r.reference_name}@${r.line}:${r.col} ${r.status}`);
  }
  return clean;
}

let allClean = true;
for (const spec of corpora) {
  const eq = spec.indexOf('=');
  const name = eq > 0 ? spec.slice(0, eq) : path.basename(path.resolve(spec));
  const src = path.resolve(eq > 0 ? spec.slice(eq + 1) : spec);
  const dir = path.join(work, name);
  const proj = path.join(dir, 'proj');
  fs.mkdirSync(dir, { recursive: true });
  copyCorpus(src, proj);

  const counts = await indexWithoutResolution(proj);
  const pre = path.join(dir, 'pre.db');
  snapshot(proj, pre);

  const t0 = Date.now();
  const tsStats = await resolveWithTs(proj);
  const tsMs = Date.now() - t0;
  const tsDb = path.join(dir, 'ts.db');
  snapshot(proj, tsDb);

  restore(proj, pre);
  const t1 = Date.now();
  const rustStats = resolveWithRust(proj);
  const rustMs = Date.now() - t1;
  const rustDb = path.join(dir, 'rust.db');
  snapshot(proj, rustDb);

  console.log(
    `${name}: ${counts.files} files, ${counts.nodes} nodes, ${counts.refs} refs; ` +
      `ts ${tsMs}ms resolved=${tsStats.resolved}/${tsStats.total} fw=[${tsStats.frameworks}]; ` +
      `rust ${rustMs}ms resolved=${rustStats.resolved}/${rustStats.total} fw=[${rustStats.frameworks}]`,
  );
  allClean = compare(name, readEdges(tsDb), readEdges(rustDb)) && allClean;
}
if (!keep) {
  try {
    fs.rmSync(work, { recursive: true, force: true });
  } catch {}
} else {
  console.log(`kept ${work}`);
}
process.exit(allClean ? 0 : 1);
