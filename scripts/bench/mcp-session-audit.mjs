#!/usr/bin/env node
// Compare released binaries over real MCP sessions. No model calls or billing estimates.
import { spawn, spawnSync } from 'node:child_process';
import { createInterface } from 'node:readline';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';

const [beforeArg, afterArg, outputArg] = process.argv.slice(2);
if (!outputArg) throw Error('Usage: node mcp-session-audit.mjs before.exe after.exe report.json');
const before = path.resolve(beforeArg), after = path.resolve(afterArg), output = path.resolve(outputArg);
// Outside the checkout: target/ would inherit gitignore rules and could
// accidentally benchmark an empty index.
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'codegraph-session-audit-'));
const hash = text => createHash('sha256').update(text).digest('hex');
const numberedSource = text => text.split(/\r?\n/).filter(line => /^\s*\d+\t/.test(line)).join('\n');
const queries = ['entry deep leaf', 'entry deep leaf', 'flow.ts:1-3', 'install.ps1:1-200'];

async function session(binary, root) {
  const child = spawn(binary, ['serve', '--mcp', '--no-watch', '--path', root], {
    windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'],
    env: { ...process.env, CODEGRAPHV_DAEMON: '0', CODEGRAPHV_NO_UPDATE_CHECK: '1' },
  });
  const pending = new Map();
  let next = 1, stderr = '';
  child.stderr.on('data', data => { stderr += data; });
  createInterface({ input: child.stdout }).on('line', line => {
    const response = JSON.parse(line);
    const callback = pending.get(response.id);
    if (callback) { pending.delete(response.id); callback(response); }
  });
  const call = (method, params) => new Promise((resolve, reject) => {
    const id = next++;
    const timeout = setTimeout(() => reject(Error(`${method} timed out: ${stderr}`)), 90000);
    pending.set(id, value => { clearTimeout(timeout); value.error ? reject(Error(JSON.stringify(value.error))) : resolve(value.result); });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  try {
    const initialized = await call('initialize', {});
    const tools = await call('tools/list', {});
    // Open/catch up outside the measured requests and reset only session source history.
    await call('tools/call', { name: 'codegraphV_explore', arguments: { query: 'flow.ts:1-1' } });
    await call('initialize', {});
    const rows = [];
    for (const query of queries) {
      const start = performance.now();
      const answer = await call('tools/call', { name: 'codegraphV_explore', arguments: { query } });
      const ms = performance.now() - start;
      if (answer.isError) throw Error(JSON.stringify(answer));
      const text = answer.content.map(c => c.text ?? '').join('\n');
      rows.push({ query, ms, chars: [...text].length, sourceHash: hash(numberedSource(text)), sourceLines: numberedSource(text).split('\n').filter(Boolean).length, text });
    }
    return { instructionsChars: [...initialized.instructions].length, toolSchemaChars: [...JSON.stringify(tools)].length, rows };
  } finally {
    const exited = once(child, 'exit');
    child.stdin.end();
    const timer = setTimeout(() => child.kill(), 10000);
    await exited;
    clearTimeout(timer);
  }
}

const results = [];
const sizes = process.env.CODEGRAPHV_AUDIT_SIZES?.split(',').map(Number) ?? [50, 500, 5000];
if (sizes.some(n => !Number.isSafeInteger(n) || n < 2 || n > 50000)) throw Error('Invalid fixture sizes');
for (const count of sizes) {
  const root = path.join(work, `files-${count}`);
  fs.mkdirSync(root);
  fs.writeFileSync(path.join(root, 'flow.ts'), [
    'export function entry(value: number = 1): number { return mid1(value) + mid2(value); }',
    'function mid1(value: number): number { return deep(value); }',
    'function mid2(value: number): number { return leaf(value); }',
    'export function deep(value: number): number { return leaf(value); }',
    'export function leaf(value: number): number { return value + 1; }',
    'export function install(): number { return 123456789; }', '',
  ].join('\n'));
  for (let i = 1; i < count; i++) fs.writeFileSync(path.join(root, `filler${i}.ts`), `export function filler${i}(value: number): number { return value + ${i}; }\n`);
  const indexed = spawnSync(before, ['init', '--yes', root], { windowsHide: true, encoding: 'utf8', timeout: 300000 });
  if (indexed.status !== 0) throw Error(indexed.stderr || indexed.error || indexed.stdout);
  const arms = { before: [], after: [] };
  for (let round = 0; round < 3; round++) {
    for (const arm of round % 2 ? ['after', 'before'] : ['before', 'after']) {
      arms[arm].push(await session(arm === 'before' ? before : after, root));
    }
  }
  // Source of the first flow and exact range must remain complete and identical.
  for (const i of [0, 2]) {
    if (!arms.before[0].rows[i].sourceLines || !arms.after[0].rows[i].sourceLines) throw Error('Missing source: invalid benchmark');
    if (arms.before[0].rows[i].sourceHash !== arms.after[0].rows[i].sourceHash) throw Error(`Source changed: ${count} / ${queries[i]}`);
  }
  results.push({ files: count, arms });
  console.log(`Compared ${count} files, three sessions per binary.`);
}
fs.mkdirSync(path.dirname(output), { recursive: true });
fs.writeFileSync(output, JSON.stringify({ date: new Date().toISOString(), platform: process.platform,
  beforeSha256: hash(fs.readFileSync(before)), afterSha256: hash(fs.readFileSync(after)), queries, results,
  scope: 'Warm MCP round trips and cumulative returned characters. Three sessions per arm per synthetic repository. Not provider token usage, model task success, indexing speed, or a real-repository agent A/B.',
}, null, 2) + '\n');
console.log(output);
