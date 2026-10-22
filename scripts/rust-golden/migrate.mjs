#!/usr/bin/env node
/**
 * Differential test of the Rust schema migrations (crates/codegraph-db/src/migrations.rs)
 * against the TypeScript ones (src/db/migrations.ts).
 *
 *   node scripts/rust-golden/migrate.mjs <path-to-xdb-example-binary> [workdir]
 *
 * Each scenario builds a legacy database, copies it, upgrades one copy through
 * the TS engine (DatabaseConnection.open) and the other through the Rust store
 * (`xdb migrate`), then diffs the schema objects and every table's rows.
 *
 * Legacy shapes:
 *   v1   — the very first schema.sql in git history (cc6e7a5c), seeded with rows
 *          a migration rewrites (duplicate edges, …)
 *   v9   — a current database degraded to the pre-#1988 shape, with Go
 *          containment edges the v10 migration tags
 *   v11  — a current database degraded below the failed-import/module-tail
 *          migrations, with failed refs parked under their legacy tails
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
const { DatabaseSync } = require('node:sqlite');

const xdb = process.argv[2];
if (!xdb) {
  console.error('usage: migrate.mjs <xdb-binary> [workdir]');
  process.exit(2);
}
const work = process.argv[3] ?? fs.mkdtempSync(path.join(os.tmpdir(), 'cg-migrate-'));
fs.mkdirSync(work, { recursive: true });

const nodeRow = (id, kind, name, file, language = 'typescript', line = 1) =>
  [id, kind, name, `${file}::${name}`, file, language, line, line + 1, 0, 1, 0];
const NODE_SQL = `INSERT INTO nodes (id, kind, name, qualified_name, file_path, language, start_line, end_line,
  start_column, end_column, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)`;

function seedNodes(db, rows) {
  const st = db.prepare(NODE_SQL);
  for (const r of rows) st.run(...r);
}

// ---------------------------------------------------------------------------
// Scenario builders (each returns the path of a fresh legacy database)
// ---------------------------------------------------------------------------
function buildV1(file) {
  const sql = execFileSync('git', ['show', 'cc6e7a5c:src/db/schema.sql'], { cwd: root, encoding: 'utf8' });
  const db = new DatabaseSync(file);
  db.exec(sql);
  seedNodes(db, [
    nodeRow('function:a', 'function', 'alpha', 'src/a.ts'),
    nodeRow('function:b', 'function', 'beta', 'src/b.ts'),
    nodeRow('class:c', 'class', 'Gamma', 'src/b.ts', 'typescript', 5),
  ]);
  const edge = db.prepare('INSERT INTO edges (source, target, kind, metadata, line, col) VALUES (?,?,?,?,?,?)');
  // Duplicates the v6 dedup collapses to the lowest id: a coordinate pair, and a NULL-coordinate pair.
  edge.run('function:a', 'function:b', 'calls', null, 3, 4);
  edge.run('function:a', 'function:b', 'calls', '{"x":1}', 3, 4);
  edge.run('function:a', 'function:b', 'calls', null, 3, 4);
  edge.run('function:a', 'class:c', 'references', null, null, null);
  edge.run('function:a', 'class:c', 'references', null, null, null);
  edge.run('function:b', 'class:c', 'calls', null, 9, 1);
  const ref = db.prepare(
    'INSERT INTO unresolved_refs (from_node_id, reference_name, reference_kind, line, col, candidates) VALUES (?,?,?,?,?,?)'
  );
  ref.run('function:a', 'util.greet', 'calls', 2, 1, '["x"]');
  ref.run('function:b', './foo', 'imports', 1, 0, null);
  db.prepare("INSERT INTO files (path, content_hash, language, size, modified_at, indexed_at, node_count) VALUES (?,?,?,?,?,?,?)").run(
    'src/a.ts', 'h', 'typescript', 1, 2, 3, 1
  );
  db.prepare('INSERT INTO vectors (node_id, embedding, model, created_at) VALUES (?,?,?,?)').run('function:a', Buffer.from([1, 2, 3]), 'm', 1);
  db.close();
}

function currentThenDegrade(file, degrade) {
  const conn = DatabaseConnection.initialize(file);
  conn.close();
  const db = new DatabaseSync(file);
  degrade(db);
  db.close();
}

function buildV9(file) {
  currentThenDegrade(file, (db) => {
    seedNodes(db, [
      nodeRow('struct:s', 'struct', 'Server', 'a.go', 'go'),
      nodeRow('method:m', 'method', 'Serve', 'b.go', 'go'),
      nodeRow('method:n', 'method', 'Stop', 'a.go', 'go', 7),
      nodeRow('function:f', 'function', 'helper', 'c.ts'),
    ]);
    const edge = db.prepare('INSERT INTO edges (source, target, kind, metadata, line, provenance) VALUES (?,?,?,?,?,?)');
    edge.run('struct:s', 'method:m', 'contains', null, null, null); // cross-file Go containment: tagged
    edge.run('struct:s', 'method:n', 'contains', 'broken containment {{{', null, null); // same file: untouched
    edge.run('struct:s', 'method:m', 'calls', 'broken json {{{', 1, null);
    edge.run('function:f', 'method:m', 'contains', null, null, 'heuristic'); // non-null provenance: untouched
    db.exec(`DROP INDEX idx_edges_synthesis_site;
      DROP TABLE synthesis_inputs;
      DROP INDEX idx_nodes_kind;
      CREATE INDEX idx_nodes_kind ON nodes(kind);
      DELETE FROM project_metadata;
      DELETE FROM schema_versions WHERE version >= 10;
      INSERT OR IGNORE INTO schema_versions (version, applied_at, description) VALUES (9, 0, 'legacy fixture');`);
  });
}

function buildV11(file) {
  currentThenDegrade(file, (db) => {
    seedNodes(db, [nodeRow('function:a', 'function', 'alpha', 'src/a.ts'), nodeRow('function:b', 'function', 'beta', 'src/b.ts')]);
    const ref = db.prepare(
      `INSERT INTO unresolved_refs (from_node_id, reference_name, reference_kind, line, col, file_path, language, status, name_tail)
       VALUES (?,?,?,?,?,?,?,?,?)`
    );
    const f = (name, kind, tail, status = 'failed') => ref.run('function:a', name, kind, 1, 0, 'src/a.ts', 'typescript', status, tail);
    f('package:app/b.dart', 'imports', 'dart'); // v12: path import tail
    f('./foo', 'imports', 'foo');
    f('../', 'imports', '');
    f('a/.eslintrc.js', 'imports', 'js');
    f('inc/db.php', 'imports', 'php', 'pending'); // pending rows are not rewritten
    f('snippets/price.liquid', 'references', 'liquid'); // v13: file-name tail
    f('snippets/price.liquid', 'references', 'price.liquid'); // already right
    f('lazy-import:./pages/Team', 'references', '/pages/Team'); // v14: module tail
    f('import:./home/home.component#HomeComponent', 'calls', 'component#HomeComponent');
    f('layout:lazy-import:@/views/error/404.vue', 'references', 'vue');
    f('import:#/views/x#Y', 'calls', 'x');
    f('mod::fn/2', 'calls', 'fn');
    f('store.get', 'calls', 'get');
    db.exec(`DROP INDEX idx_unresolved_failed_import_tail;
      DROP INDEX idx_unresolved_failed_import_name;
      DROP INDEX idx_unresolved_failed_module_name;
      DROP INDEX idx_unresolved_failed_module_member;
      DELETE FROM schema_versions WHERE version >= 12;
      INSERT OR IGNORE INTO schema_versions (version, applied_at, description) VALUES (11, 0, 'legacy fixture');`);
  });
}

// ---------------------------------------------------------------------------
// Dump + diff
// ---------------------------------------------------------------------------
function dumpAll(file) {
  const db = new DatabaseSync(file, { readOnly: true });
  const out = {};
  out.objects = db
    .prepare(
      `SELECT type, name, tbl_name, sql FROM sqlite_master
       WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE 'nodes_fts_%' ORDER BY type, name`
    )
    .all()
    .map((r) => ({ ...r, sql: r.sql && r.sql.replace(/\s+/g, ' ').trim() }));
  const tables = db
    .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'nodes_fts%' ORDER BY name")
    .all()
    .map((r) => r.name);
  for (const t of tables) {
    let cols = db.prepare(`PRAGMA table_info(${t})`).all().map((c) => c.name);
    if (t === 'schema_versions') cols = cols.filter((c) => c !== 'applied_at');
    // Positionally-stable: select by name so a column the migration appended last reads the same.
    const rows = db.prepare(`SELECT ${cols.map((c) => `"${c}"`).join(', ')} FROM ${t}`).all();
    out[t] = rows.map((r) => JSON.stringify(r)).sort();
  }
  out.indexSets = db
    .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%' ORDER BY name")
    .all()
    .map((r) => r.name);
  db.close();
  return out;
}

function diffDumps(a, b) {
  const problems = [];
  for (const key of new Set([...Object.keys(a), ...Object.keys(b)])) {
    const sa = JSON.stringify(a[key], null, 1);
    const sb = JSON.stringify(b[key], null, 1);
    if (sa === sb) continue;
    const la = (sa ?? '').split('\n');
    const lb = (sb ?? '').split('\n');
    const lines = [];
    for (let i = 0; i < Math.max(la.length, lb.length) && lines.length < 8; i++) {
      if (la[i] !== lb[i]) lines.push(`    ts  : ${la[i]}\n    rust: ${lb[i]}`);
    }
    problems.push(`  [${key}]\n${lines.join('\n')}`);
  }
  return problems;
}

let ok = true;
for (const [name, build] of [['v1 (first schema in history)', buildV1], ['v9 (pre-synthesis, Go containment)', buildV9], ['v11 (legacy failed-ref tails)', buildV11]]) {
  const dir = path.join(work, name.split(' ')[0]);
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  const base = path.join(dir, 'base.db');
  build(base);
  for (const f of ['base.db']) {
    // checkpoint to a single file so the copies are self-contained
    const db = new DatabaseSync(path.join(dir, f));
    db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
    db.close();
  }
  const tsCopy = path.join(dir, 'ts.db');
  const rsCopy = path.join(dir, 'rs.db');
  fs.copyFileSync(base, tsCopy);
  fs.copyFileSync(base, rsCopy);

  const before = dumpAll(base);
  const conn = DatabaseConnection.open(tsCopy);
  conn.close();
  execFileSync(xdb, ['migrate', rsCopy], { encoding: 'utf8' });

  const ts = dumpAll(tsCopy);
  const rs = dumpAll(rsCopy);
  const problems = diffDumps(ts, rs);
  const finalVersion = Math.max(...ts.schema_versions.map((r) => JSON.parse(r).version));
  if (problems.length === 0) {
    console.log(`ok   ${name}: base v${Math.max(...before.schema_versions.map((r) => JSON.parse(r).version))} -> v${finalVersion}, ${ts.indexSets.length} indexes, ${ts.edges.length} edges, ${ts.unresolved_refs.length} refs identical`);
  } else {
    ok = false;
    console.log(`FAIL ${name}\n${problems.join('\n')}`);
  }
}
console.log(ok ? '\nALL MIGRATION CHECKS PASSED' : '\nMIGRATION CHECKS FAILED');
process.exit(ok ? 0 : 1);
