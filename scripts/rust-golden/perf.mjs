#!/usr/bin/env node
/** Comparable warm-query timings, not IPC/process-startup timings. Always run
 * verify.mjs first: performance is only useful while answers remain equal.
 * cargo build --release -p codegraph-graph --example graph_bench
 * node scripts/rust-golden/perf.mjs target/release/examples/graph_bench.exe [output.json]
 */
import { createRequire } from 'node:module';
import { spawnSync, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { performance } from 'node:perf_hooks';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { oracleRoot } from './check-upstream.mjs';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const require = createRequire(import.meta.url);
const { DatabaseConnection } = require(path.join(oracleRoot, 'dist/db/index.js'));
const { QueryBuilder } = require(path.join(oracleRoot, 'dist/db/queries.js'));
const { GraphTraverser } = require(path.join(oracleRoot, 'dist/graph/traversal.js'));
const { resolveNamedSymbolFlow } = require(path.join(oracleRoot, 'dist/graph/named-symbol-flow.js'));
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-native-perf-'));
const dbPath = path.join(work, 'codegraph.db');
const count = 6000;
const anchors = ['entryStart', 'middleBridge', 'checkpoint', 'dispatchBridge', 'finalTarget'];
const nodes = Array.from({ length: count }, (_, i) => ({
  id: `n${i}`, name: anchors[i] ?? `getUser${i % 300}`, kind: i % 3 ? 'method' : 'function',
  qualifiedName: i < anchors.length ? anchors[i] : `Service${i % 30}::getUser${i % 300}`,
  filePath: `src/service${Math.floor(i / 200)}.ts`, language: 'typescript',
  startLine: i % 200 + 1, endLine: i % 200 + 1, startColumn: 0, endColumn: 20,
  docstring: i % 5 ? '' : 'Load an authenticated user from storage', updatedAt: 1,
}));
const edges = [];
for (let i = 0; i < count - 1; i++) {
  edges.push({ source: `n${i}`, target: `n${i + 1}`, kind: 'calls', line: i % 200 + 1 });
  if (i > 5 && i % 9 === 0 && i + 17 < count) edges.push({ source: `n${i}`, target: `n${i + 17}`, kind: 'calls', line: i % 200 + 2 });
}
const writer = DatabaseConnection.initialize(dbPath);
try {
  const q = new QueryBuilder(writer.getDb());
  q.insertNodes(nodes); q.insertEdges(edges);
  for (let i = 0; i < 30; i++) q.upsertFile({ path: `src/service${i}.ts`, contentHash: 'bench', language: 'typescript', size: 4000, modifiedAt: 1, indexedAt: 1, nodeCount: 200, generated: false });
} finally { writer.close(); }
const config = { warmup: 40, samples: 7, repetitions: 25, queries: [
  { op: 'search', query: 'getUser17' }, { op: 'search', query: 'kind:method getUser' },
  { op: 'search', query: 'getUsre17' }, { op: 'bfs', id: 'n123' },
  { op: 'callers', id: 'n250' }, { op: 'impact', id: 'n250' },
  { op: 'path', id: 'n0', to: 'n20' },
  { op: 'flow', query: 'entryStart checkpoint finalTarget' }, { op: 'dominant' },
] };
const connection = DatabaseConnection.open(dbPath, { readOnly: true });
try {
  const queries = new QueryBuilder(connection.getDb());
  const t = new GraphTraverser(queries);
  const host = new Proxy(queries, { get(target, prop) {
    if (prop === 'getNode') return target.getNodeById.bind(target);
    if (prop === 'getProjectRoot') return () => work;
    if (prop === 'getCallers') return t.getCallers.bind(t);
    if (prop === 'getCallees') return t.getCallees.bind(t);
    if (prop === 'generatedFilePredicate') return target.generatedPredicateFor.bind(target);
    const value = target[prop]; return typeof value === 'function' ? value.bind(target) : value;
  } });
  const execute = q => {
    switch (q.op) {
      case 'search': return queries.searchNodes(q.query, { limit: 20 }).length;
      case 'bfs': return t.traverseBFS(q.id, { maxDepth: 3, limit: 100 }).nodes.size;
      case 'callers': return t.getCallers(q.id, 3).length;
      case 'impact': return t.getImpactRadius(q.id, 3).nodes.size;
      case 'path': return t.findPath(q.id, q.to, ['calls'])?.length ?? 0;
      case 'flow': return resolveNamedSymbolFlow(host, q.query).chains[0]?.steps.length ?? 0;
      case 'dominant': return Number(queries.getDominantFile() !== null);
      default: throw Error(`unknown benchmark ${q.op}`);
    }
  };
  const binary = path.resolve(process.argv[2] ?? 'target/release/examples/graph_bench.exe');
  const native = spawnSync(binary, [dbPath], { input: JSON.stringify(config), encoding: 'utf8', maxBuffer: 1024 * 1024, timeout: 120000, windowsHide: true });
  if (native.status !== 0) throw Error(native.stderr || native.error);
  const rust = JSON.parse(native.stdout);
  const rows = [];
  const percentile = (samples, p) => [...samples].sort((a, b) => a - b)[Math.min(samples.length - 1, Math.ceil(samples.length * p) - 1)];
  for (let i = 0; i < config.queries.length; i++) {
    const query = config.queries[i];
    for (let j = 0; j < config.warmup; j++) execute(query);
    const samplesMs = [];
    let resultCount;
    for (let sample = 0; sample < config.samples; sample++) {
      const start = performance.now();
      for (let j = 0; j < config.repetitions; j++) resultCount = execute(query);
      samplesMs.push((performance.now() - start) / config.repetitions);
    }
    if (resultCount !== rust.rows[i].resultCount) throw Error(`benchmark answer count differs for ${JSON.stringify(query)}; do not compare speed`);
    const rustMs = percentile(rust.rows[i].samplesMs, 0.5), typescriptMs = percentile(samplesMs, 0.5);
    rows.push({ ...query, resultCount, rustMedianMs: rustMs, rustP95Ms: percentile(rust.rows[i].samplesMs, 0.95), typescriptMedianMs: typescriptMs, typescriptP95Ms: percentile(samplesMs, 0.95), typescriptOverRust: typescriptMs / rustMs, rustSamplesMs: rust.rows[i].samplesMs, typescriptSamplesMs: samplesMs });
  }
  const report = { collectedAt: new Date().toISOString(), revision: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim(), rustVersion: execFileSync('rustc', ['--version'], { encoding: 'utf8' }).trim(), binarySha256: createHash('sha256').update(fs.readFileSync(binary)).digest('hex'), fixture: { nodes: nodes.length, edges: edges.length, files: 30 }, platform: `${process.platform}-${process.arch}`, cpu: os.cpus()[0]?.model, nodeVersion: process.version, config, rows,
    scope: 'Warm queries on one synthetic SQLite graph; release Rust versus the built Node engine. Excludes process startup, extraction, indexing, MCP transport and browser rendering. Counts are checked here; full answers are checked by verify.mjs.' };
  for (const row of rows) console.log(`${row.op.padEnd(8)} ${(row.query ?? '').padEnd(38)} Rust ${row.rustMedianMs.toFixed(4)} ms  TS ${row.typescriptMedianMs.toFixed(4)} ms  ratio ${row.typescriptOverRust.toFixed(2)}x`);
  if (process.argv[3]) fs.writeFileSync(path.resolve(process.argv[3]), JSON.stringify(report, null, 2) + '\n');
} finally {
  connection.close();
  fs.rmSync(work, { recursive: true, force: true });
}
