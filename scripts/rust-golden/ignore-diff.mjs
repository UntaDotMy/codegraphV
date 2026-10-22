// Differential harness for the Rust port of node-ignore (crates/codegraph-extract/src/scan/ignore.rs).
//
//   node ignore-diff.mjs gen <out.json> [root...]   build a case corpus
//   node ignore-diff.mjs run <cases.json>            print what the real `ignore` package says
//
// A case is { id, kind: 'text' | 'array', patterns: string | string[], paths: string[] }.
// The Rust side (`examples/ignore_dump`) prints the same lines; `diff` the two.
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const ROOT = path.resolve(import.meta.dirname, '..', '..');
const ignore = require(path.join(ROOT, 'node_modules', 'ignore'));

const TORTURE = [
  '*', '**', '***', '/**', '**/', '**/foo', 'foo/**', 'a/**/b', 'a/**/**/b', '**/a/**/b/**', '!foo', '\\!foo', '\\#foo', '#foo',
  'foo\\ ', 'foo\\\\ ', 'foo  ', ' foo', '\tfoo', 'foo\t', '[abc]', '[a-c]', '[c-a]', '[!a]', '[^a]', '[a', 'a]', '[]', '[\\]]', '[a\\]',
  '\\[a]', '\\\\[a]', '[[:alpha:]]', '[a-]', '[-a]', '[z-a]x', '[é-a]x', '*.[ch]', 'foo?', '?foo', 'f??', '\\?', '\\*', '\\\\*', 'a\\*b',
  'a*b*c', '*a*', 'a/*', '/a/*', 'a/*/b', '/', '//', 'a//b', '\\', '\\\\', '\\\\\\', 'foo\\bar', '\\w+', '\\d', '\\x41', '\\u0041',
  'a{b}', '{a,b}', 'a|b', 'a+b', 'a(b)', 'a^b', 'a$b', 'a.b', 'é*', '日本/', '*😀', '?😀', '\\0', '\\1', '\\cA', 'a b', 'a\\tb', ' foo',
  '﻿foo', '!*.log', '!/a', '!a/', '!', '!!a', 'foo/', '/foo/', 'foo/bar/', '/foo/bar', 'foo/bar', '*.log', '*.LOG', 'Foo', 'x*', '*x',
  '/*', '*/', '*/*', '**/*', 'a**b', 'a***b', 'a/**b', '**a', '\\a', '\\-', '[\\d-z]', '[a-\\d]', '[\\w]', '[\\b]', '[\\^a]', 'a[b', 'a\\[b',
  'dist/', 'node_modules/', 'build', 'build/', '.git', '.*', '!.gitignore', 'src/**/build/', '**/res/layout*/', 'cmake-build-*/', '*.egg-info/',
  '[0-9]', '[a-zA-Z]', '[!-~]', '*[', '*]', '?[', 'ab?', '?', '??', '\\ ', '\\  ', 'a\\ b', ' foo', 'foo ', 'a\rb',
];
const TORTURE_PATHS = [
  'foo', 'foo/', 'a/foo', 'a/foo/', 'foo/bar', 'foo/bar/', 'bar/foo', 'a/b/c', 'a/b/', 'a/b', 'x.log', 'x/y.log', 'X.LOG', '.foo', 'foo.bar', 'a b',
  'éa', 'éé', '日本/x', '日本/', '😀', 'a😀', 'abc', 'a/bc', '[a]', 'a]', 'a[', 'a[b', '-', 'a-', 'b', 'c', 'a', 'A', 'FOO', 'Foo/', 'foo\\bar',
  'a\\x', 'ab', 'abb', 'a*b', 'a?b', 'a/b/c/d/e', 'a/x/b', 'a/x/y/b', 'a/b/b', 'src/build/x.js', 'src/main/res/layout-v21/a.xml', 'dist/a.js',
  'node_modules/p/i.js', 'cmake-build-debug/x', 'p.egg-info/PKG', '.gitignore', 'x.c', 'x.h', 'x.o', '0', '9', '~', '!', ' foo', 'foo ', 'foo\t', 'a\tb',
  'A', 'Aa', 'foobar', 'barfoo', 'afoo', 'foob', 'a/', 'a//b', 'a.b', 'a_b', 'z', 'zx', 'ax', 'bx', 'cx', 'wx', '1', 'A1', '1.log', '4', 'a1b', 'a b',
];

function walk(dir, rel, out, limit) {
  let entries;
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const e of entries) {
    if (out.length >= limit) return;
    if (e.name === '.git') continue;
    const r = rel ? `${rel}/${e.name}` : e.name;
    if (e.isDirectory()) { out.push(r + '/'); walk(path.join(dir, e.name), r, out, limit); } else out.push(r);
  }
}

function gen(outFile, roots) {
  const cases = [];
  for (const p of TORTURE) cases.push({ id: `torture:${JSON.stringify(p)}`, kind: 'array', patterns: [p], paths: TORTURE_PATHS });
  // pairs: a rule then a negation, to exercise the ordering logic
  for (const [a, b] of [['*.log', '!keep.log'], ['dist/', '!dist/keep.txt'], ['/*', '!/src'], ['**', '!src/'], ['a/*', '!a/b'], ['build', '!build/keep']]) {
    cases.push({ id: `pair:${a}|${b}`, kind: 'array', patterns: [a, b], paths: [...TORTURE_PATHS, 'src/', 'src/x', 'keep.log', 'dist/keep.txt', 'dist/other', 'a/b', 'a/c', 'build/keep', 'build/x'] });
  }
  // seeded random patterns over a hostile alphabet, tested against random paths over a friendlier one
  let seed = 0x9e3779b9;
  const rnd = (n) => { seed = (Math.imul(seed ^ (seed >>> 15), 0x2c1b3c6d) + 0x297a2d39) | 0; return (seed >>> 0) % n; };
  const PAT_TOKENS = ['a', 'b', 'c', '/', '/', '*', '**', '?', '[', ']', '\\', '!', '#', ' ', '.', '-', '^', 'é', '😀', '{', '}', '(', ')', '|', '+', '$', 'x', '0', 'z', 'Z', '\t', '\\\\', '[a-c]', '[!a]', '/**/', '**/', '/**'];
  const PATH_TOKENS = ['a', 'b', 'c', 'x', 'é', '😀', '/', '/', '.', '-', ' ', '0', 'z', 'A', 'ab', 'abc', '*', '?', '[', ']'];
  const randomPaths = (count) => {
    const set = new Set();
    for (let i = 0; i < count; i++) {
      let p = '';
      for (let k = 1 + rnd(6); k > 0; k--) p += PATH_TOKENS[rnd(PATH_TOKENS.length)];
      if (p && !p.startsWith('/') && !/^\.{1,2}(\/|$)/.test(p)) set.add(p);
    }
    return [...set];
  };
  const fuzzPaths = randomPaths(80);
  for (let i = 0; i < 4000; i++) {
    let p = '';
    for (let k = 1 + rnd(7); k > 0; k--) p += PAT_TOKENS[rnd(PAT_TOKENS.length)];
    cases.push({ id: `fuzz:${JSON.stringify(p)}`, kind: 'array', patterns: [p], paths: fuzzPaths });
  }
  for (let i = 0; i < 400; i++) {
    const pats = [];
    for (let n = 2 + rnd(3); n > 0; n--) {
      let p = rnd(3) === 0 ? '!' : '';
      for (let k = 1 + rnd(5); k > 0; k--) p += PAT_TOKENS[rnd(PAT_TOKENS.length)];
      pats.push(p);
    }
    cases.push({ id: `fuzzset:${JSON.stringify(pats)}`, kind: 'array', patterns: pats, paths: fuzzPaths });
  }
  cases.push({ id: 'text:crlf', kind: 'text', patterns: '# c\r\nfoo\r\n\r\n!bar\r\n  \r\nbaz/\r\n', paths: TORTURE_PATHS });
  cases.push({ id: 'text:lone-cr', kind: 'text', patterns: 'foo\rbar\nbaz\n', paths: ['foo', 'bar', 'baz', 'foo\rbar'] });
  for (const root of roots) {
    const gis = [];
    (function find(dir, depth) {
      if (gis.length >= 400 || depth > 6) return;
      let entries;
      try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
      for (const e of entries) {
        if (e.name === '.git') continue;
        const full = path.join(dir, e.name);
        if (e.isDirectory()) find(full, depth + 1);
        else if (e.name === '.gitignore') gis.push(full);
      }
    })(root, 0);
    for (const gi of gis) {
      let text;
      try { text = fs.readFileSync(gi, 'utf8'); } catch { continue; }
      if (text.includes('\0')) continue;
      const paths = [];
      walk(path.dirname(gi), '', paths, 600);
      cases.push({ id: `file:${gi}`, kind: 'text', patterns: text, paths: [...paths, ...TORTURE_PATHS.slice(0, 30)] });
    }
  }
  fs.writeFileSync(outFile, JSON.stringify(cases));
  console.error(`${cases.length} cases`);
}

function classify(err) {
  const m = String(err && err.message);
  if (m.includes('must not be empty')) return 'E:empty';
  if (m.includes('path.relative')) return 'E:rel';
  return 'E:regex';
}

function run(casesFile) {
  const cases = JSON.parse(fs.readFileSync(casesFile, 'utf8'));
  const out = [];
  for (const c of cases) {
    const ig = ignore();
    ig.add(c.patterns);
    const ig2 = ignore();
    ig2.add(c.patterns);
    for (const p of c.paths) {
      let a, b;
      try { a = ig.ignores(p) ? 'I' : 'N'; } catch (e) { a = classify(e); }
      try { const r = ig2.test(p); b = `${r.ignored ? 'I' : 'N'}${r.unignored ? 'U' : '-'}`; } catch (e) { b = classify(e); }
      out.push(`${c.id}\t${JSON.stringify(p)}\t${a}\t${b}`);
    }
  }
  process.stdout.write(out.join('\n') + '\n');
}

const [cmd, file, ...rest] = process.argv.slice(2);
if (cmd === 'gen') gen(file, rest);
else if (cmd === 'run') run(file);
else { console.error('usage: gen <out.json> [root...] | run <cases.json>'); process.exit(2); }
