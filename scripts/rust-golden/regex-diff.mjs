// Differential test for crates/codegraph-jsre: every regex literal in src/ is run over a
// corpus of source lines by V8 and by the Rust translation; results must be identical.
//
//   node regex-diff.mjs <re_probe.exe> [dir-with-sources ...]
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';

const ROOT = path.resolve(import.meta.dirname, '..', '..');
const ts = createRequire(path.join(ROOT, 'package.json'))('typescript');
const [exe, ...corpusDirs] = process.argv.slice(2);

function walk(dir, out = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    if (e.name === 'node_modules' || e.name === '.git' || e.name === 'target') continue;
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, out); else out.push(p);
  }
  return out;
}

// 1. regex literals
const seen = new Map();
for (const f of walk(path.join(ROOT, 'src')).filter((f) => f.endsWith('.ts'))) {
  const sf = ts.createSourceFile(f, fs.readFileSync(f, 'utf8'), ts.ScriptTarget.Latest, true);
  const visit = (n) => {
    if (n.kind === ts.SyntaxKind.RegularExpressionLiteral) {
      const t = n.getText();
      const i = t.lastIndexOf('/');
      const src = t.slice(1, i), flags = t.slice(i + 1).replace(/[gdv]/g, '');
      seen.set(`${src}/${flags}`, [src, flags, path.relative(ROOT, f)]);
    }
    ts.forEachChild(n, visit);
  };
  visit(sf);
}
const cases = [...seen.values()].filter(([src, flags]) => { try { new RegExp(src, flags); return true; } catch { return false; } });
console.log(`${cases.length} distinct regex literals`);

// 2. corpus
const lines = [];
const files = [];
for (const d of [path.join(ROOT, 'src'), path.join(ROOT, '__tests__'), ...corpusDirs]) {
  try { files.push(...walk(path.resolve(d))); } catch { /* */ }
}
let seed = 12345;
const rnd = () => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff;
const exts = new Set(['.ts', '.js', '.py', '.go', '.rs', '.java', '.kt', '.swift', '.c', '.cpp', '.h', '.hpp', '.cs', '.rb', '.php', '.dart', '.lua', '.scala', '.vue', '.svelte', '.json', '.md', '.yaml', '.yml', '.toml', '.xml']);
for (const f of files) {
  if (!exts.has(path.extname(f).toLowerCase())) continue;
  if (rnd() > 0.06) continue;
  let text; try { text = fs.readFileSync(f, 'utf8'); } catch { continue; }
  for (const l of text.split(/\r?\n/)) if (l.length > 0 && l.length < 300 && rnd() < 0.2) lines.push(l);
  if (lines.length > 12000) break;
}
// edge-case lines (no astral characters: V8 matches them as two UTF-16 units, the Rust side as one code point)
lines.push('', ' ', '\t', 'é', 'naïve café', '日本語', 'a b', 'a b', 'x\ry', 'é combining', 'foo.bar(baz)', 'this.x = y;', "import { a } from './b';", 'class A extends B implements C {', '$x->y()', 'a::b::c', 'a.b.c.d');
console.log(`${lines.length} lines`);

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'rediff-'));
fs.writeFileSync(path.join(tmp, 'cases.json'), JSON.stringify(cases.flatMap(([s, f]) => [s, f])));
fs.writeFileSync(path.join(tmp, 'lines.json'), JSON.stringify(lines));

// 3. V8
function fmt(li, m) { return `${li}:${m.index}:[${[...m].map((g) => g ?? '\u0000').join('|')}]\u0001`; }
const v8 = cases.map(([src, flags]) => {
  const sticky = flags.includes('y');
  const re = new RegExp(src, flags.replace('y', '') + 'g' + (sticky ? 'y' : ''));
  let res = '';
  for (let li = 0; li < lines.length; li++) {
    const line = lines[li];
    re.lastIndex = 0;
    let n = 0, m;
    if (sticky) { m = re.exec(line); if (m) res += fmt(li, m); continue; }
    while ((m = re.exec(line)) !== null) {
      res += fmt(li, m);
      if (m[0] === '') re.lastIndex += (flags.includes('u') && (line.codePointAt(re.lastIndex) ?? 0) > 0xffff) ? 2 : 1;
      if (++n >= 50) break;
    }
  }
  return res;
});

// 4. Rust
const out = execFileSync(exe, [path.join(tmp, 'cases.json'), path.join(tmp, 'lines.json')], { maxBuffer: 1 << 30, encoding: 'utf8' });
const rust = out.split('\n'); rust.pop();
let bad = 0, compileErr = 0;
for (let i = 0; i < cases.length; i++) {
  if (rust[i] === v8[i]) continue;
  if (rust[i].startsWith('COMPILE-ERROR')) compileErr++;
  bad++;
  if (bad <= 25) {
    const [src, flags, file] = cases[i];
    console.log(`MISMATCH /${src}/${flags}  (${file})`);
    console.log('  ' + (rust[i].startsWith('COMPILE-ERROR') ? rust[i] : (() => {
      const a = v8[i].split('\u0001'), b = rust[i].split('\u0001');
      const k = a.findIndex((x, j) => x !== b[j]);
      return `first diff at #${k}: v8=${JSON.stringify(a[k])} rust=${JSON.stringify(b[k])}`;
    })()));
  }
}
console.log(`${bad ? 'FAIL' : 'ok'}: ${cases.length - bad}/${cases.length} identical (${compileErr} compile errors)`);
process.exit(bad ? 1 : 0);
