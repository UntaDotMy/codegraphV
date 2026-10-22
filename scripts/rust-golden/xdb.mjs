#!/usr/bin/env node
/**
 * Cross-engine database compatibility check for the Rust store (crates/codegraph-db).
 *
 *   node scripts/rust-golden/xdb.mjs <path-to-xdb-example-binary> [workdir]
 *
 * 1. TS writes a fixture graph (real DatabaseConnection + QueryBuilder from dist/),
 *    Rust reads it back  -> the two engines must agree on the content.
 * 2. Rust writes the same fixture, TS reads it back (via the real QueryBuilder).
 * 3. Rust's own re-read of its file must also equal the fixture.
 *
 * Needs a built TS oracle (`npx tsc && npm run copy-assets`).
 */
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const { DatabaseConnection } = require(path.join(root, 'dist/db/index.js'));
const { QueryBuilder } = require(path.join(root, 'dist/db/queries.js'));

const xdb = process.argv[2];
if (!xdb) {
  console.error('usage: xdb.mjs <xdb-binary> [workdir]');
  process.exit(2);
}
const work = process.argv[3] ?? fs.mkdtempSync(path.join(os.tmpdir(), 'cg-xdb-'));
fs.mkdirSync(work, { recursive: true });

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------
const mk = (id, kind, name, file, line, extra = {}) => ({
  id,
  kind,
  name,
  qualifiedName: `${file}::${name}`,
  filePath: file,
  language: file.endsWith('.py') ? 'python' : 'typescript',
  startLine: line,
  endLine: line + 3,
  startColumn: 2,
  endColumn: 9,
  isExported: false,
  isAsync: false,
  isStatic: false,
  isAbstract: false,
  updatedAt: 1700000000000,
  ...extra,
});

const nodes = [
  mk('function:aaa', 'function', 'greet', 'src/a.ts', 1, {
    docstring: 'Says "hi" — héllo 世界 🚀\nline two',
    signature: 'greet(name: string): string',
    visibility: 'public',
    isExported: true,
    isAsync: true,
    decorators: ['@one', '@two("x")'],
    typeParameters: ['T', 'U extends T'],
    returnType: 'string',
  }),
  mk('class:bbb', 'class', 'Greeter', 'src/a.ts', 10, { isAbstract: true, isExported: true }),
  mk('method:ccc', 'method', 'run', 'src/b.ts', 4, { isStatic: true, visibility: 'private' }),
  mk('function:ddd', 'function', 'helper_fn', 'src/b.py', 7),
  mk('file:eee', 'file', 'a.ts', 'src/a.ts', 1),
  mk('import:fff', 'import', './b', 'src/a.ts', 1),
];
const edges = [
  { source: 'function:aaa', target: 'method:ccc', kind: 'calls', line: 2, column: 4 },
  { source: 'class:bbb', target: 'function:aaa', kind: 'contains' },
  {
    source: 'method:ccc',
    target: 'function:ddd',
    kind: 'calls',
    line: 5,
    column: 1,
    provenance: 'heuristic',
    metadata: { synthesizedBy: 'react-render', registeredAt: 'src/b.ts:4', n: 3, nested: { a: [1, 2] } },
  },
  { source: 'file:eee', target: 'class:bbb', kind: 'contains' },
];
const files = [
  {
    path: 'src/a.ts',
    contentHash: 'h1',
    language: 'typescript',
    size: 120,
    modifiedAt: 1700000000001,
    indexedAt: 1700000000002,
    nodeCount: 4,
    generated: false,
  },
  {
    path: 'src/b.ts',
    contentHash: 'h2',
    language: 'typescript',
    size: 99,
    modifiedAt: 1700000000003,
    indexedAt: 1700000000004,
    nodeCount: 1,
    errors: [{ message: 'oops', filePath: 'src/b.ts', line: 3, severity: 'warning' }],
    generated: true,
  },
  {
    path: 'src/b.py',
    contentHash: 'h3',
    language: 'python',
    size: 5,
    modifiedAt: 1700000000005,
    indexedAt: 1700000000006,
    nodeCount: 1,
    generated: false,
  },
];
const refs = [
  {
    fromNodeId: 'function:aaa',
    referenceName: 'util.thing',
    referenceKind: 'calls',
    line: 3,
    column: 7,
    filePath: 'src/a.ts',
    language: 'typescript',
    candidates: ['src/u.ts::thing', 'src/v.ts::thing'],
  },
  {
    fromNodeId: 'import:fff',
    referenceName: './b',
    referenceKind: 'imports',
    line: 1,
    column: 0,
    filePath: 'src/a.ts',
    language: 'typescript',
  },
  {
    fromNodeId: 'method:ccc',
    referenceName: 'cb',
    referenceKind: 'function_ref',
    line: 6,
    column: 2,
    filePath: 'src/b.ts',
    language: 'typescript',
  },
];

const fixture = { nodes, edges, files, refs };
const fixturePath = path.join(work, 'fixture.json');
fs.writeFileSync(fixturePath, JSON.stringify(fixture));

// ---------------------------------------------------------------------------
// Canonical form: sorted keys, stripped of fields neither engine can agree on
// ---------------------------------------------------------------------------
const canon = (v) =>
  Array.isArray(v)
    ? v.map(canon)
    : v && typeof v === 'object'
      ? Object.fromEntries(
          Object.keys(v)
            .sort()
            .filter((k) => v[k] !== undefined && v[k] !== null) // TS reads NULL columns back as null, Rust as absent
            .map((k) => [k, canon(v[k])])
        )
      : v;
const key = (o) => JSON.stringify(canon(o));
const norm = (g) => {
  const refsN = g.refs.map((r) => {
    const { rowId, row_id, ...rest } = r;
    return rest;
  });
  return canon({
    nodes: [...g.nodes].sort((a, b) => a.id.localeCompare(b.id)),
    edges: [...g.edges].sort((a, b) => key(a).localeCompare(key(b))),
    files: [...g.files].sort((a, b) => a.path.localeCompare(b.path)),
    refs: refsN.sort((a, b) => key(a).localeCompare(key(b))),
  });
};

function readWithTs(dbPath) {
  const conn = DatabaseConnection.open(dbPath, { readOnly: true });
  const qb = new QueryBuilder(conn.getDb());
  const ns = qb.getAllNodes();
  const g = {
    nodes: ns,
    edges: ns.flatMap((n) => qb.getOutgoingEdges(n.id)),
    files: qb.getAllFiles(),
    refs: qb.getUnresolvedReferences(),
  };
  const version = conn.getSchemaVersion?.()?.version;
  conn.close();
  return { g, version };
}

function readWithRust(dbPath) {
  const out = execFileSync(xdb, ['read', dbPath], { encoding: 'utf8', maxBuffer: 1 << 28 });
  return JSON.parse(out);
}

// The fixture as each engine should return it: absent optionals become the
// stored defaults (flags false, generated false).
const expected = norm(fixture);

function diff(label, a, b) {
  const sa = JSON.stringify(a, null, 1);
  const sb = JSON.stringify(b, null, 1);
  if (sa === sb) {
    console.log(`ok   ${label}`);
    return true;
  }
  console.log(`FAIL ${label}`);
  const la = sa.split('\n');
  const lb = sb.split('\n');
  let shown = 0;
  for (let i = 0; i < Math.max(la.length, lb.length) && shown < 12; i++) {
    if (la[i] !== lb[i]) {
      console.log(`  line ${i}:\n    a: ${la[i]}\n    b: ${lb[i]}`);
      shown++;
    }
  }
  return false;
}

let ok = true;

// 1. TS writes -> TS reads (establishes what "expected" looks like through the oracle)
const tsDb = path.join(work, 'ts', '.codegraph', 'codegraph.db');
fs.mkdirSync(path.dirname(tsDb), { recursive: true });
for (const f of [tsDb, tsDb + '-wal', tsDb + '-shm']) fs.rmSync(f, { force: true });
{
  const conn = DatabaseConnection.initialize(tsDb);
  const qb = new QueryBuilder(conn.getDb());
  for (const file of files) {
    const ids = new Set(nodes.filter((n) => n.filePath === file.path).map((n) => n.id));
    const fnodes = nodes.filter((n) => ids.has(n.id));
    const fedges = edges.filter((e) => ids.has(e.source) && ids.has(e.target));
    const frefs = refs.filter((r) => ids.has(r.fromNodeId));
    qb.storeFileBundle({ nodes: fnodes, edges: fedges, refs: frefs, file });
  }
  qb.insertEdges(
    edges.filter((e) => {
      const s = nodes.find((n) => n.id === e.source).filePath;
      const t = nodes.find((n) => n.id === e.target).filePath;
      return s !== t;
    })
  );
  conn.close();
}
const tsRead = readWithTs(tsDb);
ok = diff('TS write -> TS read equals fixture', norm(tsRead.g), expected) && ok;

// 2. TS-written file read by Rust
const rustReadsTs = readWithRust(tsDb);
ok = diff('TS write -> Rust read equals fixture', norm(rustReadsTs), expected) && ok;
console.log(`     schema version (rust read of ts db): ${rustReadsTs.version}`);

// 3. Rust writes -> Rust reads, and TS reads
const rsDb = path.join(work, 'rs', '.codegraph', 'codegraph.db');
fs.mkdirSync(path.dirname(rsDb), { recursive: true });
for (const f of [rsDb, rsDb + '-wal', rsDb + '-shm']) fs.rmSync(f, { force: true });
execFileSync(xdb, ['write', rsDb, fixturePath], { encoding: 'utf8' });
ok = diff('Rust write -> Rust read equals fixture', norm(readWithRust(rsDb)), expected) && ok;
const tsReadsRs = readWithTs(rsDb);
ok = diff('Rust write -> TS read equals fixture', norm(tsReadsRs.g), expected) && ok;

// 4. Natural-key dump of both files must be identical (scripts/dump-graph.mjs).
const dump = (p) =>
  execFileSync(process.execPath, [path.join(root, 'scripts', 'dump-graph.mjs'), p], {
    encoding: 'utf8',
    maxBuffer: 1 << 28,
  });
ok = diff('dump-graph of TS file equals dump-graph of Rust file', dump(tsDb).split('\n'), dump(rsDb).split('\n')) && ok;

// 5. Same set of indexes / triggers / tables in both files.
const { DatabaseSync } = require('node:sqlite');
const schemaOf = (p) => {
  const db = new DatabaseSync(p, { readOnly: true });
  const rows = db
    .prepare("SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE 'nodes_fts_%' ORDER BY type, name")
    .all()
    .map((r) => `${r.type}:${r.name}`);
  db.close();
  return rows;
};
ok = diff('schema object set TS file equals Rust file', schemaOf(tsDb), schemaOf(rsDb)) && ok;

// 6. Pragmas that persist in the file.
const pragmaOf = (p) => {
  const db = new DatabaseSync(p, { readOnly: true });
  const jm = db.prepare('PRAGMA journal_mode').get().journal_mode;
  db.close();
  return jm;
};
ok = diff('journal_mode', pragmaOf(tsDb), pragmaOf(rsDb)) && ok;

console.log(ok ? '\nALL CROSS-ENGINE CHECKS PASSED' : '\nCROSS-ENGINE CHECKS FAILED');
process.exit(ok ? 0 : 1);
