#!/usr/bin/env node
/** Conservative source/native drift detection. This never fetches or changes
 * branches, and only verify.mjs records a new validation checkpoint. */
import * as fs from 'node:fs';
import * as path from 'node:path';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

export const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
export const oracleRoot = path.resolve(process.env.CODEGRAPH_TS_ORACLE_ROOT ?? root);
const checkpointPath = path.join(root, 'scripts/rust-golden/upstream-checkpoint.json');
const groups = [
  { name: 'graph-search', status: 'partial', verifiedByRunner: true,
    sources: ['src/types.ts', 'src/utils.ts', 'package.json', 'src/db/schema.sql', 'src/db/queries.ts', 'src/extraction/generated-detection.ts', 'src/graph/traversal.ts', 'src/graph/queries.ts', 'src/graph/type-hierarchy.ts', 'src/graph/synthesized-hop.ts', 'src/graph/symbol-lookup.ts', 'src/graph/go-package.ts', 'src/graph/named-symbol-flow.ts', 'src/search'],
    native: ['crates/codegraph-graph', 'crates/codegraph-search', 'crates/codegraph-db', 'crates/codegraph-types', 'crates/codegraph-jsre', 'Cargo.toml', 'Cargo.lock', 'scripts/rust-golden/graph.mjs', 'scripts/rust-golden/query-paths.mjs', 'scripts/rust-golden/check-upstream.mjs', 'scripts/rust-golden/verify.mjs', 'scripts/rust-golden/perf.mjs'],
    note: 'This runner verifies implemented graph/search behavior. Other QueryBuilder methods still await their consumers.' },
  { name: 'remaining-graph', status: 'partial', sources: ['src/graph/branch-guards.ts', 'src/graph/dead-code.ts', 'src/graph/dynamic-boundary-report.ts', 'src/graph/cobol-copybooks.ts', 'src/mcp/dynamic-boundaries.ts', 'src/resolution/strip-comments.ts'], native: ['crates/codegraph-graph/src/cobol_copybooks.rs', 'crates/codegraph-graph/src/dead_code.rs', 'crates/codegraph-graph/src/dynamic_boundaries.rs', 'crates/codegraph-graph/src/dynamic_boundary_report.rs', 'crates/codegraph-graph/tests/cobol_copybooks.rs', 'crates/codegraph-graph/tests/dead_code.rs', 'crates/codegraph-graph/tests/dynamic_boundaries.rs', 'crates/codegraph-graph/tests/dynamic_boundary_report.rs', 'crates/codegraph-text', 'crates/codegraph-db/src/dead_code_queries.rs'], note: 'Copybook naming, the dynamic-dispatch scanner, the boundary report and dead-code are ported with their assertions. branch-guards remains.' },
  { name: 'component-extraction', status: 'partial', verifiedByRunner: true,
    sources: ['src/extraction/sfc-script.ts', 'src/extraction/svelte-extractor.ts', 'src/extraction/astro-extractor.ts', 'src/extraction/vue-extractor.ts', 'src/extraction/vue-options-api.ts', 'src/extraction/vue-template-calls.ts', 'src/extraction/liquid-extractor.ts', 'src/extraction/razor-extractor.ts', 'src/extraction/mybatis-extractor.ts', 'src/extraction/dfm-extractor.ts', 'src/extraction/tree-sitter.ts', 'src/extraction/tree-sitter-helpers.ts', 'src/extraction/grammars.ts'],
    native: ['crates/codegraph-extract', 'crates/codegraph-text', 'crates/codegraph-types', 'crates/codegraph-jsre', 'scripts/rust-golden/sfc.mjs', 'scripts/rust-golden/templates.mjs', 'scripts/rust-golden/extraction-compare.mjs', 'Cargo.toml', 'Cargo.lock'],
    note: 'Svelte/Vue/Astro wrappers and their delegated JS/TS extraction, plus the grammar-free Liquid/Razor/MyBatis/DFM extractors, are compared. This does not attest to the other extraction languages, indexing orchestration or full framework flow resolution.' },
  { name: 'extraction', status: 'partial', sources: ['src/extraction', 'codegraph-kernel/src', 'codegraph-kernel/grammars', 'codegraph-kernel/build.rs', 'codegraph-kernel/Cargo.toml'], native: ['crates/codegraph-extract'], note: 'Native walkers/discovery exist. This runner does not refresh extraction differential attestations.' },
  { name: 'resolution', status: 'partial', sources: ['src/resolution'], native: ['crates/codegraph-resolve'], note: 'Resolver foundations only; the full resolver is not yet compiled/ported.' },
  { name: 'runtime-and-viewer', status: 'pending', sources: ['AGENTS.md', 'src/index.ts', 'src/directory.ts', 'src/project-config.ts', 'src/mcp', 'src/bin', 'src/context', 'src/sync', 'src/installer', 'src/upgrade', 'src/ui-server', 'ui/src'], native: [], note: 'CLI/MCP/context/watching/installer/viewer replacement remains pending.' },
  { name: 'reference-tests', status: 'specification', sources: ['__tests__'], native: [], note: 'New or changed regressions are the specification. Port assertions with their matching feature; this runner does not run every TypeScript suite.' },
];
const extensions = new Set(['.ts', '.js', '.mjs', '.rs', '.sql', '.toml', '.lock', '.json', '.svelte', '.c', '.h', '.cc', '.cpp', '.wasm']);
const textExtensions = new Set([...extensions].filter(e => e !== '.wasm').concat(['.md', '.go', '.py', '.yaml', '.yml', '.xml', '.vue', '.css', '.html']));
function filesUnder(relative, base = root) {
  const absolute = path.join(base, relative);
  if (!fs.existsSync(absolute)) return [];
  if (fs.statSync(absolute).isFile()) return [relative];
  return fs.readdirSync(absolute, { withFileTypes: true }).filter(e => !['node_modules', 'target', 'dist', '.git'].includes(e.name)).flatMap(e => {
    const rel = `${relative}/${e.name}`;
    return e.isDirectory() ? filesUnder(rel, base) : extensions.has(path.extname(e.name)) || rel.startsWith('__tests__/') ? [rel] : [];
  });
}
function hash(relative, base) {
  let content = fs.readFileSync(path.join(base, relative));
  if (textExtensions.has(path.extname(relative))) content = content.toString('utf8').replace(/\r\n/g, '\n');
  return createHash('sha256').update(content).digest('hex');
}
export function snapshot() {
  const revision = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim();
  const oracleRevision = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: oracleRoot, encoding: 'utf8' }).trim();
  return { formatVersion: 1, revision, oracleRevision, groups: groups.map(group => ({ ...group,
    fingerprints: Object.fromEntries([
      ...group.sources.flatMap(p => filesUnder(p, oracleRoot).map(file => [file, hash(file, oracleRoot)])),
      ...group.native.flatMap(p => filesUnder(p, root).map(file => [file, hash(file, root)])),
    ].sort(([a], [b]) => a.localeCompare(b))),
  })) };
}
export function recordValidated(commands) {
  const current = snapshot();
  current.validation = { groups: ['graph-search', 'component-extraction'], commands };
  fs.writeFileSync(checkpointPath, JSON.stringify(current, null, 2) + '\n');
}
export function audit() {
  if (!fs.existsSync(checkpointPath)) {
    console.log('No checkpoint yet. Run node scripts/rust-golden/verify.mjs to validate and record one.');
    return 1;
  }
  const previous = JSON.parse(fs.readFileSync(checkpointPath, 'utf8'));
  const current = snapshot();
  console.log(`Last checkpoint: ${previous.revision}\nCurrent branch:  ${current.revision}`);
  if (current.oracleRevision !== current.revision) console.log(`TS reference:    ${current.oracleRevision}`);
  let changed = false;
  for (const group of current.groups) {
    const before = previous.groups.find(g => g.name === group.name)?.fingerprints ?? {};
    const names = new Set([...Object.keys(before), ...Object.keys(group.fingerprints)]);
    const drift = [...names].filter(name => before[name] !== group.fingerprints[name]);
    if (drift.length) {
      changed = true;
      console.log(`\n${group.name} (${group.status}): review ${drift.length} changed file(s)`);
      for (const file of drift) console.log(`  ${file}`);
      console.log(`  ${group.note}`);
    }
  }
  if (changed) console.log('\nReview changes, port applicable fixes, then run node scripts/rust-golden/verify.mjs.');
  else console.log('No source or native drift since the checkpoint. Pending areas remain pending.');
  return changed ? 1 : 0;
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) process.exitCode = audit();
