#!/usr/bin/env node
/**
 * The TS side of `tree_probe`: a file's wasm S-expression after the language's `preParse`.
 *
 *   node scripts/rust-golden/tree-probe.mjs <lang> <file>
 *
 * Prints `wasm has_error=… sexp_len=…` then `WASM:<sexp>`, the same shape as the Rust example,
 * so `grep -c` / `diff` on the three S-expressions says which parser a divergence comes from.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

process.env.CODEGRAPH_KERNEL = '0';
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const dist = (p) => pathToFileURL(path.join(ROOT, 'dist', p)).href;

const [lang, file] = process.argv.slice(2);
if (!lang || !file) {
  console.error('usage: tree-probe.mjs <lang> <file>');
  process.exit(2);
}
const { initGrammars, loadGrammarsForLanguages, getParser } = await import(dist('extraction/grammars.js'));
const { EXTRACTORS } = await import(dist('extraction/languages/index.js'));
await initGrammars();
await loadGrammarsForLanguages([lang]);
let source = fs.readFileSync(file, 'utf8');
const pre = EXTRACTORS[lang]?.preParse;
if (pre) source = pre(source, file);
const tree = getParser(lang).parse(source);
console.log(`wasm has_error=${tree.rootNode.hasError} sexp_len=${tree.rootNode.toString().length}`);
console.log(`WASM:${tree.rootNode.toString()}`);
