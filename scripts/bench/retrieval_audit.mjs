#!/usr/bin/env node
/** Compare two release retrieval_bench executables on this repository, interleaving arms.
 * node scripts/bench/retrieval_audit.mjs <before.exe> <after.exe> <indexed-repo> [report.json]
 * Build both executables before running this: do not run Cargo beside probes on Windows.
 */
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [before, after, project, output] = process.argv.slice(2);
if (!before || !after || !project) {
  throw Error('Usage: retrieval_audit.mjs <before> <after> <indexed-repo> [report.json]');
}
const queries = [
  'sync index_all',
  'crates/codegraph-core/src/codegraph.rs:667-698',
  'crates/codegraph-core/src/codegraph.rs:667-667',
];
const binaries = { before: path.resolve(before), after: path.resolve(after) };
const runs = { before: [], after: [] };
for (let round = 0; round < 3; round++) {
  for (const arm of round % 2 ? ['after', 'before'] : ['before', 'after']) {
    const text = execFileSync(binaries[arm], [path.resolve(project), ...queries], {
      encoding: 'utf8', windowsHide: true, timeout: 120000, maxBuffer: 4 * 1024 * 1024,
    });
    runs[arm].push(JSON.parse(text));
  }
}
const median = values => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)];
const rows = runs.before[0].rows.map((first, index) => {
  const left = runs.before.map(run => run.rows[index]);
  const right = runs.after.map(run => run.rows[index]);
  for (const row of [...left, ...right]) {
    if (row.operation !== first.operation || row.query !== first.query || row.files !== first.files) {
      throw Error('Benchmark case order or inputs changed');
    }
    if (first.operation === 'queryPaths' && row.resultSha256 !== first.resultSha256) {
      throw Error(`Path recognition changed for ${first.query}; do not compare speed`);
    }
    if (first.operation === 'explore' && row.sourceLines !== first.sourceLines) {
      throw Error(`Source line count changed for ${first.query}; inspect before comparing speed`);
    }
  }
  const beforeMs = median(left.flatMap(row => row.samplesMs));
  const afterMs = median(right.flatMap(row => row.samplesMs));
  return {
    operation: first.operation, files: first.files, query: first.query,
    beforeMedianMs: beforeMs, afterMedianMs: afterMs, speedup: beforeMs / afterMs,
    beforeChars: first.responseChars, afterChars: right[0].responseChars,
    sourceLines: first.sourceLines,
    answerHashesEqual: [...left, ...right].every(row => row.resultSha256 === first.resultSha256),
  };
});
const report = {
  collectedAt: new Date().toISOString(),
  baselineRevision: execFileSync('git', ['-C', path.resolve(project), 'rev-parse', 'HEAD'], {encoding:'utf8'}).trim(),
  platform: `${process.platform}-${process.arch}`, cpu: os.cpus()[0]?.model,
  binaries: Object.fromEntries(Object.entries(binaries).map(([arm, file]) =>
    [arm, { file: path.basename(file), sha256: createHash('sha256').update(fs.readFileSync(file)).digest('hex') }])),
  rounds: 3, samplesPerRound: 9, repetitionsPerSample: 5, rows, runs,
  scope: 'Warm in-process retrieval. Synthetic path indexes at three sizes plus explore on this checkout. Exact-range metadata intentionally changes; source line counts and separate verbatim-source tests are checked. Excludes startup, transport, model tokens/cost and agent task success.',
};
if (output) fs.writeFileSync(path.resolve(output), JSON.stringify(report, null, 2) + '\n');
for (const row of rows) {
  const chars = row.beforeChars === undefined ? '' : `; ${row.beforeChars} -> ${row.afterChars} chars`;
  console.log(`${row.operation} ${row.files ?? ''} ${row.query}: ${row.beforeMedianMs.toFixed(3)} -> ${row.afterMedianMs.toFixed(3)} ms (${row.speedup.toFixed(2)}x)${chars}`);
}
