#!/usr/bin/env node
/** Same-index codegraph_explore parity: the TypeScript ToolHandler vs the native handler.
 * cargo build -p codegraph-mcp --example explore_probe
 * node scripts/rust-golden/explore.mjs [target/debug/examples/explore_probe.exe] [--only <repo>] [--refresh] [--show N]
 *
 * Each repository is copied once into <tmp>/cg-explore/<name> and indexed by the
 * TypeScript engine; both engines then answer every query on that same index.
 * --refresh re-copies and re-indexes. Exit 1 on any difference.
 */
import { createRequire } from 'node:module';
import { spawnSync, execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { root, oracleRoot } from './check-upstream.mjs';

const require = createRequire(import.meta.url);
const { CodeGraph } = require(path.join(oracleRoot, 'dist/index.js'));
const { ToolHandler } = require(path.join(oracleRoot, 'dist/mcp/tools.js'));

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const option = (name) => { const i = args.indexOf(name); return i >= 0 ? args[i + 1] : undefined; };
const binary = path.resolve(args.find((a) => !a.startsWith('--') && a !== option('--only') && a !== option('--show'))
  ?? 'target/debug/examples/explore_probe.exe');
const show = Number(option('--show') ?? 3);
const work = path.join(os.tmpdir(), 'cg-explore');
const registry = path.join(os.homedir(), '.cargo/registry/src/index.crates.io-1949cf8c6b5b557f');

const repos = [
  { name: 'serde_json', from: path.join(registry, 'serde_json-1.0.145'), queries: [
    'Deserializer deserialize_any parse_value', 'Value Index index_into', 'to_string Serializer serialize_str format_escaped_str',
    'from_str from_reader Deserializer::from_str', 'src/de.rs', 'src/value/index.rs:40', 'number parsing float',
    'RawValue to_raw_value', 'Map entry or_insert', 'Error syntax line column', 'nonexistent_symbol_xyz', 'json! macro',
    'src/ser.rs CompactFormatter write_string_fragment', 'PrettyFormatter begin_object_key',
  ] },
  { name: 'tokio', from: path.join(registry, 'tokio-1.53.1'), queries: [
    'spawn JoinHandle Harness poll', 'TcpListener accept TcpStream', 'Runtime block_on enter', 'mpsc channel send recv Semaphore',
    'Notify notified notify_waiters', 'src/runtime/scheduler/multi_thread/worker.rs', 'select! macro',
    'time sleep Sleep poll_elapsed TimerEntry', 'File read_to_end spawn_blocking', 'Mutex lock MutexGuard',
    'io copy AsyncRead AsyncWrite', 'budget coop poll_proceed', 'worker.rs:300', 'blocking pool spawn_blocking BlockingPool',
  ] },
  { name: 'codegraph', from: root, git: true, queries: [
    'handleExplore buildFlowFromNamedSymbols', 'ReferenceResolver resolveAll', 'ExtractionOrchestrator indexAll',
    'src/mcp/tools.ts:4300', 'getExploreBudget getExploreOutputBudget', 'CodeGraph.open DatabaseConnection',
    'FileWatcher debounce', 'installer claude target writeMcpConfig', 'named_symbol_flow resolve_named_symbol_flow',
    'explore_budget', 'how does sync work', 'callback synthesizer EventEmitter', 'src/resolution/frameworks/express.ts',
    'crates/codegraph-mcp/src/render.rs',
  ] },
];

const SKIP = new Set(['.git', 'target', 'node_modules', '.codegraph', 'dist', '.engineering']);
function copyTree(repo, dest) {
  if (repo.git) {
    const list = execFileSync('git', ['-C', repo.from, 'ls-files', '-z', '--cached', '--others', '--exclude-standard'],
      { maxBuffer: 1 << 28 }).toString('utf8').split('\0').filter(Boolean);
    for (const rel of list) {
      if (rel.split('/').some((part) => SKIP.has(part))) continue;
      const src = path.join(repo.from, rel);
      if (!fs.existsSync(src) || !fs.statSync(src).isFile()) continue;
      fs.mkdirSync(path.dirname(path.join(dest, rel)), { recursive: true });
      fs.copyFileSync(src, path.join(dest, rel));
    }
  } else {
    fs.cpSync(repo.from, dest, { recursive: true, filter: (src) => !SKIP.has(path.basename(src)) });
  }
}

async function prepare(repo) {
  const dir = path.join(work, repo.name);
  if (flag('--refresh')) fs.rmSync(dir, { recursive: true, force: true });
  if (!fs.existsSync(path.join(dir, '.codegraph'))) {
    fs.mkdirSync(dir, { recursive: true });
    copyTree(repo, dir);
    const started = Date.now();
    const cg = await CodeGraph.init(dir, { index: true });
    const stats = cg.getStats();
    cg.close();
    console.log(`indexed ${repo.name}: ${stats.fileCount} files, ${stats.nodeCount} nodes in ${Date.now() - started} ms`);
  }
  return dir;
}

function firstDifference(a, b) {
  const x = a.split('\n'); const y = b.split('\n');
  let i = 0;
  while (i < x.length && i < y.length && x[i] === y[i]) i++;
  const span = (lines) => lines.slice(Math.max(0, i - 2), i + 3).map((l, k) => `      ${Math.max(0, i - 2) + k + 1}| ${l}`).join('\n');
  return `    line ${i + 1} (ts ${x.length} lines, rust ${y.length} lines)\n    ts:\n${span(x)}\n    rust:\n${span(y)}`;
}

let total = 0; let failed = 0;
for (const repo of repos) {
  if (option('--only') && option('--only') !== repo.name) continue;
  const dir = await prepare(repo);
  const cg = await CodeGraph.open(dir);
  const handler = new ToolHandler(cg);
  const expected = [];
  for (const query of repo.queries) {
    const result = await new ToolHandler(cg).executeReadTool('codegraph_explore', { query });
    expected.push({ text: result.content.map((c) => c.text).join('\n'), isError: result.isError === true });
  }
  handler.closeAll?.();
  cg.close();
  const run = spawnSync(binary, [dir], { input: repo.queries.map((q) => JSON.stringify({ query: q })).join('\n') + '\n',
    maxBuffer: 1 << 28, encoding: 'utf8' });
  if (run.status !== 0) throw new Error(`${binary} failed (status ${run.status}): ${run.error ?? run.stderr}`);
  const actual = run.stdout.trim().split('\n').map((l) => JSON.parse(l));
  let repoFailed = 0;
  repo.queries.forEach((query, i) => {
    total++;
    const e = expected[i]; const a = actual[i];
    if (a && e.text === a.text && e.isError === a.isError) return;
    failed++; repoFailed++;
    if (repoFailed <= show) {
      console.log(`DIFF ${repo.name} :: ${JSON.stringify(query)}${e.isError !== a?.isError ? ` (isError ts=${e.isError} rust=${a?.isError})` : ''}`);
      console.log(firstDifference(e.text, a?.text ?? '<missing>'));
    }
  });
  console.log(`${repo.name}: ${repo.queries.length - repoFailed}/${repo.queries.length} identical`);
}
console.log(`explore parity: ${total - failed}/${total} identical`);
process.exit(failed ? 1 : 0);
