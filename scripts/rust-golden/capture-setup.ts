/**
 * Vitest setup file that records the project every test builds in a temp directory, so the
 * suite's own fixtures (hundreds of small projects across every supported language) can be
 * replayed as corpora for the differential resolver check (`resolve.mjs`).
 *
 * Each directory `mkdtemp` hands out is copied — minus `.codegraph`, `.git` and
 * `node_modules` — to `$CG_CAPTURE_DIR/<test file>/<n>` just before the test removes it, or at
 * the end of the file when it never does. Run through `capture-fixtures.mjs`.
 */
import { afterAll } from 'vitest';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as nodeModule from 'module';

const captureDir = process.env.CG_CAPTURE_DIR;
const testFile = path.basename(expect.getState().testPath ?? 'unknown').replace(/\.test\.ts$/, '');
const SKIP = new Set(['.codegraph', '.git', 'node_modules', '.svelte-kit']);
const MAX_FILE_BYTES = 512 * 1024;
const MAX_TOTAL_BYTES = 4 * 1024 * 1024;

const roots: string[] = [];
const captured = new Set<string>();
let seq = 0;

function within(child: string, parent: string): boolean {
  const rel = path.relative(parent, child);
  return rel === '' || (!rel.startsWith('..') && !path.isAbsolute(rel));
}

function snapshot(root: string): void {
  if (!captureDir || captured.has(root)) return;
  captured.add(root);
  let total = 0;
  const dest = path.join(captureDir, testFile, `${process.pid}-${seq++}`);
  try {
    fs.cpSync(root, dest, {
      recursive: true,
      dereference: false,
      filter: (src) => {
        if (SKIP.has(path.basename(src))) return false;
        try {
          const st = fs.lstatSync(src);
          if (st.isSymbolicLink()) return false;
          if (st.isFile()) {
            if (st.size > MAX_FILE_BYTES) return false;
            total += st.size;
            if (total > MAX_TOTAL_BYTES) return false;
          }
          return true;
        } catch {
          return false;
        }
      },
    });
    // An empty capture is not a corpus.
    if (fs.readdirSync(dest).length === 0) fs.rmSync(dest, { recursive: true, force: true });
  } catch {
    /* a racing delete: the directory is gone */
  }
}

function snapshotAround(target: unknown): void {
  if (typeof target !== 'string') return;
  const abs = path.resolve(target);
  for (const root of roots) if (within(root, abs) || within(abs, root)) snapshot(root);
}

if (captureDir) {
  const realFs = (nodeModule.createRequire(import.meta.url))('fs') as typeof fs;
  const tmp = fs.realpathSync(os.tmpdir());
  const isProject = (dir: string): boolean => !path.basename(dir).startsWith('codegraph-test-home-');

  const mkdtempSync = realFs.mkdtempSync.bind(realFs);
  (realFs as any).mkdtempSync = (prefix: string, ...rest: any[]) => {
    const dir = (mkdtempSync as any)(prefix, ...rest);
    if (typeof dir === 'string' && isProject(dir)) {
      try { if (within(fs.realpathSync(dir), tmp)) roots.push(path.resolve(dir)); } catch { /* ignore */ }
    }
    return dir;
  };
  for (const name of ['rmSync', 'rmdirSync'] as const) {
    const real = (realFs as any)[name].bind(realFs);
    (realFs as any)[name] = (target: any, ...rest: any[]) => {
      snapshotAround(target);
      return real(target, ...rest);
    };
  }
  const realPromises = realFs.promises as any;
  for (const name of ['rm', 'rmdir'] as const) {
    const real = realPromises[name].bind(realPromises);
    realPromises[name] = async (target: any, ...rest: any[]) => {
      snapshotAround(target);
      return real(target, ...rest);
    };
  }
  const mkdtemp = realPromises.mkdtemp.bind(realPromises);
  realPromises.mkdtemp = async (prefix: string, ...rest: any[]) => {
    const dir = await mkdtemp(prefix, ...rest);
    if (typeof dir === 'string' && isProject(dir)) roots.push(path.resolve(dir));
    return dir;
  };
  (nodeModule as any).syncBuiltinESMExports();

  afterAll(() => {
    for (const root of roots) if (fs.existsSync(root)) snapshot(root);
  });
}
