#!/usr/bin/env node
/**
 * The grammars the Rust engine loads from the TypeScript engine's own .wasm files (and objc's
 * native 2.1.0 build) parse each sample under crates/codegraph-extract/tests/fixtures/grammars to
 * the same S-expression as web-tree-sitter.
 *
 *   cargo build -p codegraph-extract --example tree_probe
 *   node --liftoff-only scripts/rust-golden/wasm-grammars.mjs target/debug/examples/tree_probe.exe
 *
 * (--liftoff-only: V8 runs out of memory optimizing the COBOL and VB.NET wasm otherwise.)
 * web-tree-sitter parses UTF-16, so the Rust side's UTF-16 tree is the one compared.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';

process.env.CODEGRAPH_KERNEL = '0';
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const dist = (p) => pathToFileURL(path.join(ROOT, 'dist', p)).href;
const probe = path.resolve(process.argv[2] ?? 'target/debug/examples/tree_probe.exe');
const { initGrammars, loadGrammarsForLanguages, getParser } = await import(dist('extraction/grammars.js'));
const { EXTRACTORS } = await import(dist('extraction/languages/index.js'));
const fixtures = path.join(ROOT, 'crates/codegraph-extract/tests/fixtures/grammars');
const cases = [
  ['objc', 'sample.m'], ['arkts', 'sample.ets'], ['pascal', 'sample.pas'], ['vbnet', 'sample.vb'],
  ['cobol', 'sample.cbl'], ['erlang', 'sample.erl'], ['terraform', 'sample.tf'], ['cfml', 'sample.cfm'],
  ['cfscript', 'sample.cfc'], ['cfquery', 'sample.sql'],
];
await initGrammars();
await loadGrammarsForLanguages(cases.map(([lang]) => lang));
let failed = 0;
for (const [lang, name] of cases) {
  const file = path.join(fixtures, name);
  let source = fs.readFileSync(file, 'utf8');
  const pre = EXTRACTORS[lang]?.preParse;
  if (pre) source = pre(source, file);
  const wasm = getParser(lang).parse(source).rootNode.toString();
  const out = execFileSync(probe, [lang, file], { encoding: 'utf8', maxBuffer: 64 << 20 });
  const native = out.split('\n').find((l) => l.startsWith('UTF16:'))?.slice('UTF16:'.length);
  const same = native === wasm;
  if (!same) failed++;
  console.log(`${same ? 'same' : 'DIFF'} ${lang} (${wasm.length} chars)${same ? '' : `\n  wasm:   ${wasm.slice(0, 400)}\n  native: ${native?.slice(0, 400)}`}`);
}
console.log(`${cases.length - failed} / ${cases.length} grammars parse their sample like web-tree-sitter`);
process.exit(failed ? 1 : 0);
