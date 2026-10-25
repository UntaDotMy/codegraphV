#!/usr/bin/env node
/** Repeatable latest-oracle gate. Keep build, probe execution and cargo test
 * SEQUENTIAL: rebuilding a live .exe races Windows' executable file locks. */
import { spawnSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { root, oracleRoot, recordValidated } from './check-upstream.mjs';

const commands = [];
function run(command, args, cwd = root) {
  const display = [command === process.execPath ? 'node' : command, ...args.map(a => a.startsWith(root + path.sep) ? path.relative(root, a).replaceAll('\\', '/') : a)].join(' ');
  console.log(`\n${display}`);
  const result = spawnSync(command, args, { cwd, stdio: 'inherit', windowsHide: true });
  if (result.error) throw result.error;
  if (result.status !== 0) throw Error(`${display} failed (${result.status ?? result.signal})`);
  commands.push(display);
}
run(process.execPath, [path.join(oracleRoot, 'node_modules/typescript/bin/tsc')], oracleRoot);
// Graph/search require the exact schema shipped with the freshly built oracle.
fs.mkdirSync(path.join(oracleRoot, 'dist/db'), { recursive: true });
fs.copyFileSync(path.join(oracleRoot, 'src/db/schema.sql'), path.join(oracleRoot, 'dist/db/schema.sql'));
run('cargo', ['build', '-p', 'codegraph-graph', '--example', 'graph_probe']);
run('cargo', ['build', '-p', 'codegraph-search', '--example', 'query_paths_probe']);
run('cargo', ['build', '-p', 'codegraph-extract', '--example', 'sfc_probe']);
run('cargo', ['build', '-p', 'codegraph-context', '--example', 'context_probe']);
run('cargo', ['build', '-p', 'codegraph-sync', '--example', 'sync_probe']);
run('cargo', ['build', '-p', 'codegraph-sync', '--example', 'watcher_probe']);
const suffix = process.platform === 'win32' ? '.exe' : '';
run(process.execPath, ['scripts/rust-golden/graph.mjs', `target/debug/examples/graph_probe${suffix}`, '--write']);
run(process.execPath, ['scripts/rust-golden/query-paths.mjs', `target/debug/examples/query_paths_probe${suffix}`, '--write']);
run(process.execPath, ['scripts/rust-golden/sfc.mjs', `target/debug/examples/sfc_probe${suffix}`, '--write', path.join(oracleRoot, 'ui/src'), ...process.argv.slice(2)]);
run(process.execPath, ['scripts/rust-golden/templates.mjs', `target/debug/examples/sfc_probe${suffix}`, '--write', ...process.argv.slice(2)]);
run(process.execPath, ['scripts/rust-golden/context.mjs', `target/debug/examples/context_probe${suffix}`, '--write']);
run(process.execPath, ['scripts/rust-golden/sync.mjs', `target/debug/examples/sync_probe${suffix}`]);
run(process.execPath, ['scripts/rust-golden/watcher.mjs', `target/debug/examples/watcher_probe${suffix}`, '--write']);
run('cargo', ['test', '--workspace', '--quiet']);
recordValidated(commands);
console.log('\nImplemented graph/search, component/template extraction, context retrieval and git sync utilities validated against the latest TypeScript sources; checkpoint recorded.');
