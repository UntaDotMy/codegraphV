// Builds throwaway git repos that exercise every branch of the scanner, for
// scan.mjs to compare the TypeScript and Rust engines on.
//
//   node make-scan-fixtures.mjs <outDir>
//
// Prints one line per fixture: `<name>\t<root>\t<sinceCommit|->`.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const out = path.resolve(process.argv[2]);
fs.rmSync(out, { recursive: true, force: true });
fs.mkdirSync(out, { recursive: true });

const GIT = ['-c', 'user.name=t', '-c', 'user.email=t@t', '-c', 'commit.gpgsign=false', '-c', 'core.autocrlf=false', '-c', 'protocol.file.allow=always'];
const git = (cwd, ...args) => execFileSync('git', [...GIT, ...args], { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
const write = (root, rel, text = `// ${rel}\nexport const x = 1;\n`) => {
  const p = path.join(root, rel);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, text);
};
const init = (dir) => { fs.mkdirSync(dir, { recursive: true }); git(dir, 'init', '-q', '-b', 'main'); return dir; };
const commit = (dir, msg) => { git(dir, 'add', '-A'); git(dir, 'commit', '-q', '-m', msg, '--allow-empty'); return git(dir, 'rev-parse', 'HEAD'); };
const fixtures = [];
const snapshot = (root, name) => { const dest = path.join(out, name); fs.cpSync(root, dest, { recursive: true, verbatimSymlinks: true }); return dest; };
const add = (name, root, since = '-') => fixtures.push(`${name}\t${root}\t${since}`);

const sources = (root, prefix = '') => {
  write(root, `${prefix}src/a.ts`);
  write(root, `${prefix}src/b.js`);
  write(root, `${prefix}src/main.py`, 'def f():\n    pass\n');
  write(root, `${prefix}src/lib/util.go`, 'package lib\nfunc U() {}\n');
  write(root, `${prefix}README.md`, '# readme\n');
  write(root, `${prefix}data.json`, '{}\n');
  write(root, `${prefix}Makefile`, 'all:\n');
  write(root, `${prefix}src/日本語.ts`);
  write(root, `${prefix}src/café.py`, 'x = 1\n');
  write(root, `${prefix}src/with space.ts`);
  write(root, `${prefix}src/emoji😀.ts`);
};

// 1. basic: ignore rules, tracked-but-default-ignored, negation, nested .gitignore, android res, src/build
{
  const root = init(path.join(out, 'basic'));
  sources(root);
  write(root, '.gitignore', '*.log\n/secret/\n!vendor/\n# comment\r\nbuild-cache/\r\n\r\n');
  write(root, 'node_modules/x/i.js');
  write(root, 'vendor/v.go', 'package v\n');
  write(root, 'dist/out.js');
  write(root, 'secret/s.ts');
  write(root, 'debug.log', 'x');
  write(root, 'src/gen/.gitignore', '*.ts\n!keep.ts\n');
  write(root, 'src/gen/gen1.ts');
  write(root, 'src/gen/keep.ts');
  write(root, 'app/src/main/res/layout/main.xml', '<a/>');
  write(root, 'app/src/main/res/raw/schema.sql', 'select 1;');
  write(root, 'app/src/main/java/com/build/Foo.java', 'class Foo {}');
  write(root, 'src/build/legacy/page.tsx');
  write(root, 'src/build/legacy/page.html', '<p/>');
  write(root, 'src/build/legacy/out.js');
  write(root, 'src/build/types.d.ts', 'export {};');
  write(root, 'build-cache/c.ts');
  write(root, 'cmake-build-debug/x.c', 'int x;');
  write(root, 'pkg.egg-info/PKG.py', 'x=1');
  write(root, 'A.PY', 'x = 1\n');
  const first = commit(root, 'one');
  write(root, 'src/later.ts');
  fs.writeFileSync(path.join(root, 'src/a.ts'), 'export const changed = 2;\n');
  fs.rmSync(path.join(root, 'src/b.js'));
  write(root, 'untracked/new.ts');
  write(root, 'untracked/deep/er/more.ts');
  write(root, 'build-cache/untracked.ts');
  add('basic-dirty', snapshot(root, 'basic-dirty'));
  const second = commit(root, 'two');
  write(root, 'src/c.ts');
  fs.writeFileSync(path.join(root, 'src/main.py'), 'def g():\n    pass\n');
  add('basic-since-first', root, first);
  add('basic-since-head', root, second);
  add('basic-since-bogus', root, '0123456789012345678901234567890123456789');
  fs.mkdirSync(path.join(root, '.git/info'), { recursive: true });
  fs.writeFileSync(path.join(root, '.git/info/exclude'), 'src/c.ts\nuntracked/deep/\n');
  add('basic-info-exclude', root, second);
}

// 2. root .gitignore that is not UTF-8 / uncompilable lines / BOM
{
  const root = init(path.join(out, 'badignore'));
  sources(root);
  fs.writeFileSync(path.join(root, '.gitignore'), Buffer.from([0xff, 0xfe, 0x2a, 0x00, 0x74, 0x00, 0x73, 0x00, 0x0a, 0x00]));
  write(root, 'sub/.gitignore', '[é-a]x\n*.js\n');
  write(root, 'sub/x.js');
  write(root, 'sub/y.ts');
  commit(root, 'one');
  add('badignore', root);
  const root2 = init(path.join(out, 'bomignore'));
  sources(root2);
  fs.writeFileSync(path.join(root2, '.gitignore'), '﻿src/lib/\nsrc/*.py\n\\!literal\n[z-a]q\n');
  write(root2, '!literal/y.ts');
  commit(root2, 'one');
  add('bomignore', root2);
}

// 3. embedded repos: untracked clone, gitignored clone (+ includeIgnored), gitlink, worktree, submodule-ish
{
  const root = init(path.join(out, 'embedded'));
  sources(root);
  write(root, '.gitignore', '/ignored-child/\n/refs/\n');
  commit(root, 'parent');
  const child = init(path.join(root, 'child'));
  sources(child);
  write(child, '.gitignore', 'local/\n');
  write(child, 'local/l.ts');
  commit(child, 'child');
  write(child, 'child-untracked.ts');
  const ign = init(path.join(root, 'ignored-child'));
  sources(ign);
  commit(ign, 'ign');
  const refsA = init(path.join(root, 'refs', 'a'));
  sources(refsA);
  commit(refsA, 'refs a');
  const refsB = init(path.join(root, 'refs', 'b'));
  sources(refsB);
  commit(refsB, 'refs b');
  // nested embedded inside the untracked embedded
  const deep = init(path.join(child, 'deep', 'inner'));
  sources(deep);
  commit(deep, 'inner');
  // a gitlink: `git add` of an embedded repo records mode 160000
  const gl = init(path.join(root, 'gitlinked'));
  sources(gl);
  commit(gl, 'gl');
  try { git(root, 'add', 'gitlinked'); } catch { /* */ }
  // a worktree of `child`: must be skipped
  try { git(child, 'worktree', 'add', '-q', path.join(root, 'child-wt'), '-b', 'wt'); } catch { /* */ }
  // an embedded repo inside node_modules: never project code
  const nm = init(path.join(root, 'node_modules', 'gitdep'));
  sources(nm);
  commit(nm, 'nm');
  add('embedded-default', root);
  const v1 = snapshot(root, 'embedded-ii');
  write(v1, 'codegraph.json', JSON.stringify({ includeIgnored: ['ignored-child/', 'refs/a/'] }));
  add('embedded-includeIgnored', v1);
  const v2 = snapshot(root, 'embedded-ii-parent');
  write(v2, 'codegraph.json', JSON.stringify({ includeIgnored: ['refs/'] }));
  add('embedded-includeIgnored-parent', v2);
}

// 4. codegraph.json: extensions, exclude, include
{
  const root = init(path.join(out, 'config'));
  sources(root);
  write(root, '.gitignore', 'Tools/\n/local-src/\n');
  write(root, 'codegraph.json', JSON.stringify({
    extensions: { '.foo': 'python', '.dota_lua': 'lua', '.bad': 'nope', 'x.y': 'php' },
    exclude: ['static/', '**/generated/**'],
    include: ['Tools/**', 'local-src/'],
    deprioritize: ['scripts/'],
  }));
  write(root, 'static/theme.js');
  write(root, 'src/generated/g.ts');
  write(root, 'x.foo', 'x = 1\n');
  write(root, 'rules.dota_lua', 'local x = 1\n');
  write(root, 'Tools/gen.ts');
  write(root, 'Tools/node_modules/n.js');
  write(root, 'Tools/static/ex.ts');
  write(root, 'Tools/sub/deep.ts');
  write(root, 'local-src/l.ts');
  commit(root, 'one');
  write(root, 'x2.foo', 'y = 2\n');
  write(root, 'static/more.js');
  add('config', root);
  const nc = snapshot(root, 'config-removed');
  fs.rmSync(path.join(nc, 'codegraph.json'));
  add('config-removed', nc);
}

// 5. non-git copy (filesystem walk) with nested ignores, symlink-free
{
  const root = path.join(out, 'nongit');
  fs.mkdirSync(root, { recursive: true });
  sources(root);
  write(root, '.gitignore', '*.log\n/secret/\n!vendor/\ndist/\n');
  write(root, 'node_modules/x/i.js');
  write(root, 'vendor/v.go', 'package v\n');
  write(root, 'dist/out.js');
  write(root, 'secret/s.ts');
  write(root, 'debug.log', 'x');
  write(root, 'src/gen/.gitignore', '*.ts\n!keep.ts\n');
  write(root, 'src/gen/gen1.ts');
  write(root, 'src/gen/keep.ts');
  write(root, 'src/gen/deeper/.gitignore', '[é-a]x\nonly.ts\n');
  write(root, 'src/gen/deeper/only.ts');
  write(root, 'src/gen/deeper/keep.py', 'x=1');
  write(root, 'app/src/main/res/layout/main.xml', '<a/>');
  write(root, 'src/build/legacy/page.tsx');
  write(root, 'src/build/legacy/page.html', '<p/>');
  write(root, '.codegraph/codegraph.db', 'x');
  write(root, '.codegraph-wsl/x.ts');
  write(root, 'codegraph.json', JSON.stringify({ exclude: ['src/café.py'], include: ['dist/keep-me.ts'] }));
  write(root, 'dist/keep-me.ts');
  add('nongit', root);
}

// 6. a project inside a parent repo that ignores it, and a sub-root of a repo
{
  const parent = init(path.join(out, 'parent'));
  write(parent, '.gitignore', 'ignored-proj/\n');
  write(parent, 'top.ts');
  sources(parent, 'sub/');
  commit(parent, 'one');
  sources(parent, 'ignored-proj/');
  write(parent, 'ignored-proj/.gitignore', 'src/lib/\n');
  add('parent-ignores-root', path.join(parent, 'ignored-proj'));
  add('sub-root', path.join(parent, 'sub'));
  write(parent, 'sub/new.ts');
  add('sub-root-dirty', path.join(parent, 'sub'));
}

// 7. no commits yet; empty repo
{
  const root = init(path.join(out, 'nocommits'));
  sources(root);
  add('nocommits', root);
  const empty = init(path.join(out, 'empty'));
  add('empty', empty);
}

console.log(fixtures.join('\n'));
