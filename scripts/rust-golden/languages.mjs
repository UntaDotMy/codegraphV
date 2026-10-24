// Reference vectors for the native language walkers: every file under
// crates/codegraph-extract/tests/fixtures/languages/<language>/ is extracted by the
// TypeScript engine (dist/, so run `npm run build` first) and written with its
// normalized result to crates/codegraph-extract/tests/golden/languages.json.
// tests/languages.rs then checks the Rust walkers against it without Node.
//
//   node scripts/rust-golden/languages.mjs
import { createRequire } from 'node:module';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { root, oracleRoot } from './check-upstream.mjs';
import { normalize } from './extraction-compare.mjs';

const require = createRequire(import.meta.url);
const { initGrammars, loadGrammarsForLanguages } = require(path.join(oracleRoot, 'dist/extraction/grammars.js'));
const { TreeSitterExtractor } = require(path.join(oracleRoot, 'dist/extraction/tree-sitter.js'));

const fixtures = path.join(root, 'crates/codegraph-extract/tests/fixtures/languages');
const cases = [];
for (const language of fs.readdirSync(fixtures).sort()) {
  const dir = path.join(fixtures, language);
  for (const name of fs.readdirSync(dir).sort()) {
    cases.push({ file: `${language}/${name}`, language, source: fs.readFileSync(path.join(dir, name), 'utf8') });
  }
}
await initGrammars();
await loadGrammarsForLanguages([...new Set(cases.map((c) => c.language))]);
const out = cases.map((input) => ({
  input,
  expected: normalize(new TreeSitterExtractor(input.file, input.source, input.language).extract()),
}));
const destination = path.join(root, 'crates/codegraph-extract/tests/golden/languages.json');
fs.writeFileSync(destination, JSON.stringify(out, null, 1) + '\n');
console.log(`${out.length} case(s) → ${path.relative(root, destination)}`);
