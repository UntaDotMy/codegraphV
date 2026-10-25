// Differential test for the Rust scanner (crates/codegraph-extract/src/scan).
//
//   node scan.mjs <scan_dump.exe> <root> [sinceCommit]   one root: prints a diff summary, exit 1 on mismatch
//   node scan.mjs --ts <root> [sinceCommit]              print the TypeScript result as JSON
//
// Compares the file list (order included), skip tallies, git change sets, embedded repo
// discovery, the unindexed-ignored hint list, HEAD and the fast-path verdict.
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

process.env.CODEGRAPH_KERNEL = '0';
const ROOT = path.resolve(import.meta.dirname, '..', '..');
const dist = (p) => pathToFileURL(path.join(ROOT, 'dist', p)).href;
const ex = await import(dist('extraction/index.js'));
const { loadExtensionOverrides } = await import(dist('project-config.js'));

function tsResult(root, since) {
  const stats = { unsupportedByExtension: new Map() };
  let scanned;
  try { scanned = ex.scanDirectory(root); } catch (e) { scanned = `ERROR: ${e.message}`; }
  // skip tallies come from the async variant
  return (async () => {
    try { await ex.scanDirectoryAsync(root, undefined, stats); } catch { /* the sync call already reported it */ }
    const changes = ex.getGitChangedFiles(root, since ?? undefined);
    let embedded = null;
    try { embedded = ex.discoverEmbeddedRepoRoots(root); } catch { /* */ }
    let unindexed = null;
    try { unindexed = ex.findUnindexedIgnoredRepos(root); } catch { /* */ }
    const filesList = Array.isArray(scanned) ? scanned : [];
    const overrides = loadExtensionOverrides(root);
    return {
      files: scanned,
      skipped: [...stats.unsupportedByExtension.entries()],
      changes: changes ? { modified: changes.modified, added: changes.added, deleted: changes.deleted } : null,
      embedded,
      unindexedIgnored: unindexed,
      head: ex.getGitHeadSha(root),
      trust: ex.canTrustGitFastPath(root, since ?? undefined),
      preload: ex.preloadLanguagesForFiles(filesList, overrides, root),
    };
  })();
}

const args = process.argv.slice(2);
if (args[0] === '--ts') {
  console.log(JSON.stringify(await tsResult(path.resolve(args[1]), args[2])));
  process.exit(0);
}
const [exe, rootArg, since] = args;
const root = path.resolve(rootArg);
const ts = await tsResult(root, since);
const rustJson = execFileSync(exe, since ? [root, since] : [root], { maxBuffer: 1 << 30, encoding: 'utf8' });
const rust = JSON.parse(rustJson);

let bad = 0;
function same(label, a, b, { sorted = false } = {}) {
  const norm = (v) => JSON.stringify(sorted && Array.isArray(v) ? [...v].sort() : v);
  if (norm(a) === norm(b)) return;
  bad++;
  console.log(`MISMATCH ${label}`);
  if (Array.isArray(a) && Array.isArray(b)) {
    const sa = new Set(a), sb = new Set(b);
    const onlyTs = a.filter((x) => !sb.has(x)).slice(0, 12);
    const onlyRs = b.filter((x) => !sa.has(x)).slice(0, 12);
    console.log(`  counts ts=${a.length} rust=${b.length}`);
    if (onlyTs.length) console.log('  only in TS  :', onlyTs);
    if (onlyRs.length) console.log('  only in Rust:', onlyRs);
    if (!onlyTs.length && !onlyRs.length) console.log('  same members, different order');
  } else {
    console.log('  ts  :', JSON.stringify(a)?.slice(0, 400));
    console.log('  rust:', JSON.stringify(b)?.slice(0, 400));
  }
}
same('files', ts.files, rust.files);
same('skipped', ts.skipped, rust.skipped);
same('changes.modified', ts.changes?.modified, rust.changes?.modified, { sorted: true });
same('changes.added', ts.changes?.added, rust.changes?.added, { sorted: true });
same('changes.deleted', ts.changes?.deleted, rust.changes?.deleted, { sorted: true });
same('changes.null', ts.changes === null, rust.changes === null);
same('embedded', ts.embedded, rust.embedded);
same('unindexedIgnored', ts.unindexedIgnored, rust.unindexedIgnored);
same('head', ts.head, rust.head);
same('trust', ts.trust, rust.trust);
same('preload', ts.preload, rust.preload);
const n = Array.isArray(ts.files) ? ts.files.length : -1;
console.log(`${bad ? 'FAIL' : 'ok  '} ${root}  (${n} files, ${ts.changes ? ts.changes.modified.length + ts.changes.added.length + ts.changes.deleted.length : 'no'} changes, ${ts.embedded?.length ?? '?'} embedded)`);
process.exit(bad ? 1 : 0);
