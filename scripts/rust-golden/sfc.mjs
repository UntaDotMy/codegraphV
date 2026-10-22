#!/usr/bin/env node
/** Compare full SFC extraction against the current TypeScript reference.
 * cargo build -p codegraph-extract --example sfc_probe
 * node scripts/rust-golden/sfc.mjs target/debug/examples/sfc_probe.exe [--write] [corpus...]
 */
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as os from 'node:os';
import { createHash } from 'node:crypto';
import { performance } from 'node:perf_hooks';
import { root, oracleRoot, snapshot } from './check-upstream.mjs';
import { normalize, compare } from './extraction-compare.mjs';
const require = createRequire(import.meta.url);
const { initGrammars, loadGrammarsForLanguages } = require(path.join(oracleRoot, 'dist/extraction/grammars.js'));
const { SvelteExtractor } = require(path.join(oracleRoot, 'dist/extraction/svelte-extractor.js'));
const { AstroExtractor } = require(path.join(oracleRoot, 'dist/extraction/astro-extractor.js'));
const { VueExtractor } = require(path.join(oracleRoot, 'dist/extraction/vue-extractor.js'));
const extractors = { svelte: SvelteExtractor, astro: AstroExtractor, vue: VueExtractor };
await initGrammars();
await loadGrammarsForLanguages(['typescript', 'javascript']);
const binary = path.resolve(process.argv[2] ?? 'target/debug/examples/sfc_probe.exe');
const sources = [
  '', '<div>hello 世界 😀</div>\n<Child /><Fragment />',
  '<script>\nimport { load } from "./api";\nconst value = load();\nfunction click() { return save(value); }\n</script>\n<Button on:click={click}> {format(value)} </Button>',
  '<script lang="ts">\ninterface User { id: number }\nexport let user: User;\nclass Service { run() { return user.id; } }\nconst api = { load() { return getUser(); } };\n</script>\n<Child>{api.load()}</Child>',
  '<script module>\nconst x = moduleCall();\nexport function fetchAll() { return load(); }\n</script>\n<script>\nlet v = instanceCall();\n</script>\n{show(v)}',
  '<script context="module" lang="ts">\nexport const data = build();\n</script>\n<script\n lang="typescript"\n>\nconst data = local();\n</script>\n{show(data)}',
  '<script>\nlet x = $state(0);\nlet { value } = $props();\n$effect(() => changed(x));\n</script>\n{format(x)}\n{#if ok(x)}<Card />{/if}',
  '<script>const x = "<Phantom>";</script><Child />\n<style>.Foo { content: "{never()}"; }</style>\n<Real>{yes()}</Real>',
  '<script lang="ts">\nconst text = "😀";\nfunction refresh() { return action(text); }\n</script>\n😀<Child x={helper(text)} />',
  '<script>\nfunction same() { inner(); }\n</script>\n<script>\nfunction same() { second(); }\n</script>\n{same()}',
  '---\nimport Layout from "./Layout.astro";\nconst posts = loadPosts();\nfunction format(x: string) { return decorate(x); }\n---\n<Layout>{format(posts)}</Layout>',
  '\n\n---\nconst value = frontmatter();\n---\n<script>\nconst v: string = client();\n</script>\n<Card>{format(v)}</Card>',
  '---\n---\n<Fragment><Code /><Debug /><Custom /></Fragment>\n{items.map((item) => (\n<Card item={render(item)} />\n))}',
  '---\nunclosed()\n<Child>{shown()}</Child>',
  'prefix\n---\nnotFrontmatter()\n---\n{normal()}',
  '<Card value={first()}>{second()} {items.map((item) => (\n<Child item={item} />\n))}',
  '<script lang="ts">\n// 😀\nexport async function f(): Promise<string> { return await load(); }\n</script>\n{await value()}\n{function fn() { called(); }}',
];
const cases = [];
const vueSources = [
  '<script>\nexport default { data() { return load() }, methods: { save(x) { return api.save(x) }, cancel: () => reset() }, computed: { price: { get() { return format() }, set(x) { update(x) } } }, watch: { item: { handler() { reload() } }, x: "onX" }, mounted() { this.save() } };\n</script>\n<template><MyCard @click="save(item)">{{ price(item) }} {{ helper() }}</MyCard></template>',
  '<script setup lang="ts">\nimport { useData } from "./data";\nconst value = useData();\n</script>\n<template><my-card :value="format(value)" @click="store?.save?.(value)" /></template>',
  '<template><div v-for="(fmt, i) in getItems()" :value="fmt(i)">{{ render(i) }}<slot :x="fmt()" /></div>{{ fmt() }}<Comp #default="{ format }">{{format()}}{{other()}}</Comp>{{format()}}</template>',
  '<template><div v-pre :x="never()">{{never()}}<Comp @click="never()" /></div><div>{{ yes() }}</div><!-- {{ no() }} --></template>',
  '<template lang="pug">div {{ nope() }}</template>',
  '<template><div :x="items.map((render) => render()) + outside()" @click="function cb(save) { save(); external(); }">{{ ({ method() { inner() } }) }} {{ new Service(load()) }} {{ Math.max(helper()) }} {{ $router.push(path()) }}</div></template>',
  '<template>😀 {{ `x ${format()} ${`y ${nested()}`}` }} {{ /fake\\(x\\)/.test(actual()) }} {{ store!.load() }}<div :x="count > 0 ? yes() : no()" /></template>',
  '<script>export default defineComponent({ methods: { "quoted-key": function() { work() }, async go() { await run() }, *iter() { yield values() } } })</script>\n<template>{{go()}}<keep-alive><some-card /></keep-alive></template>',
  '<template><div v-for="load in list()"><div v-for="x of load()">{{ load() }}{{ x() }}{{ helper() }}</div></div><input v-for="f in inputList()" />{{f()}}</template>',
  '<template>{{list.map((fn)\u2003=> fn())}}{{plain()}} {{ "😀" + Unicode() }}</template>',
];
// Keep existing reference regression inputs useful after TypeScript is retired.
// Literal sources are read as data, never executed; assertions are native tests.
const ts = require(path.join(oracleRoot, 'node_modules/typescript'));
for (const name of ['vue-template-calls.test.ts', 'vue-options-api.test.ts', 'sfc-component-owns-script.test.ts', 'sfc-private-declarations.test.ts', 'astro-component-markup-only.test.ts']) {
  const file = path.join(oracleRoot, '__tests__', name);
  const ast = ts.createSourceFile(name, fs.readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true);
  function visit(node) {
    if ((ts.isNoSubstitutionTemplateLiteral(node) || ts.isStringLiteral(node)) && /<(?:template|script)\b/.test(node.text)) {
      if (name.startsWith('vue')) vueSources.push(node.text);
      else sources.push(node.text);
    }
    ts.forEachChild(node, visit);
  }
  visit(ast);
}
for (const language of Object.keys(extractors)) for (const source of language === 'vue' ? sources.concat(vueSources) : sources) for (const variant of [source, source.replaceAll('\n', '\r\n'), '<!-- 😀 -->\n' + source]) {
  cases.push({ file: `src/components/Example.${language}`, source: variant, language });
}
function collect(directory, base = directory) {
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    if (['.git', 'node_modules', 'dist', 'target', '.svelte-kit', '.astro'].includes(entry.name)) continue;
    const absolute = path.join(directory, entry.name);
    if (entry.isDirectory()) collect(absolute, base);
    else {
      const language = path.extname(entry.name).slice(1);
      if (!(language in extractors) || fs.statSync(absolute).size > 1024 * 1024) continue;
      const source = fs.readFileSync(absolute, 'utf8');
      if (source.includes('\uFFFD')) continue;
      cases.push({ file: path.relative(base, absolute).replaceAll('\\', '/'), source, language, corpus: true });
    }
  }
}
for (const arg of process.argv.slice(3).filter(p => !p.startsWith('--'))) collect(path.resolve(arg));
const expected = cases.map(c => normalize(new extractors[c.language](c.file, c.source).extract()));
compare(binary, cases, expected);
if (process.argv.includes('--write')) {
  const destination = path.join(root, 'crates/codegraph-extract/tests/golden/sfc.json');
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  fs.writeFileSync(destination, JSON.stringify(cases.flatMap((input, i) => input.corpus ? [] : [{ input, expected: expected[i] }])));
  const { TreeSitterExtractor } = require(path.join(oracleRoot, 'dist/extraction/tree-sitter.js'));
  const names = ['store', '$store', 'store$', 'store_2', 'store2', 'as', 'default', 'foo', 'alias', 'comment'];
  const exports = [
    ...names.map(n => `export default ${n}`), ...names.map(n => `export default ${n};`),
    ...names.map(n => `export { ${n} }`), ...names.map(n => `export { ${n} as alias }`),
    'export { store$, store2, $store, store_2 }', 'export { store as default }',
    'export { foostore, store2 }', 'export { x /* comment store */ }',
    'export default store; // comment', 'export default store.member',
    'export { store }\nexport default foo', 'export { store }\nexport { alias }',
    'export\uFEFFdefault store', 'export\u0085default store',
    'export {😀store😀}', 'export { 123store, store123 }',
    'const x = 1; export {store}', '// export default store',
  ];
  const vectors = [];
  for (const statement of exports) for (const newline of ['\n', '\r\n', '\r', '\u2028', '\u2029']) {
    const source = `// 😀 prefix${newline}\t${statement.replaceAll('\n', newline)}${newline}`;
    const oracle = new TreeSitterExtractor('file.js', source, 'javascript');
    vectors.push({ source, expected: Object.fromEntries(names.map(name => [name, oracle.isExportedLater(name)])) });
  }
  fs.writeFileSync(path.join(root, 'crates/codegraph-extract/tests/golden/later-exports.json'), JSON.stringify(vectors));
}
console.log(`${cases.length} component extractions match the current TypeScript reference`);
const report = {
  date: new Date().toISOString(), platform: `${process.platform}-${process.arch}`, cpu: os.cpus()[0]?.model,
  nativeRevision: spawnSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8', windowsHide: true }).stdout.trim(),
  oracleRevision: spawnSync('git', ['rev-parse', 'HEAD'], { cwd: oracleRoot, encoding: 'utf8', windowsHide: true }).stdout.trim(),
  sourceFingerprint: createHash('sha256').update(JSON.stringify(snapshot().groups.find(g => g.name === 'component-extraction').fingerprints)).digest('hex'),
  binarySha256: createHash('sha256').update(fs.readFileSync(binary)).digest('hex'),
  implementation: 'Working-tree sources, including changes not yet committed; source fingerprint and binary hash identify the measured build.',
  node: process.version, rustc: spawnSync('rustc', ['-V'], { encoding: 'utf8', windowsHide: true }).stdout.trim(),
  comparisons: cases.length, synthetic: cases.filter(c => !c.corpus).length,
  byLanguage: Object.fromEntries(Object.keys(extractors).map(l => [l, cases.filter(c => c.language === l).length])),
  corpora: process.argv.slice(3).filter(p => !p.startsWith('--')).map(directory => ({
    name: path.basename(directory), revision: spawnSync('git', ['rev-parse', 'HEAD'], { cwd: directory, encoding: 'utf8', windowsHide: true }).stdout?.trim() || null,
  })),
};
if (process.argv.includes('--bench')) {
  report.scope = 'Warm serial extraction only; no output serialization, process startup, IPC, resolution, database writes or end-to-end indexing. Seven samples, three warmups and three rounds/sample. Native release build required. Engines run sequentially; JS garbage collection is included when triggered.';
  report.benchmarks = {};
  const median = a => [...a].sort((a, b) => a - b)[Math.floor(a.length / 2)];
  for (const language of Object.keys(extractors)) {
    const pool = cases.filter(c => c.language === language && c.corpus);
    if (!pool.length) pool.push(...cases.filter(c => c.language === language));
    const selected = Array.from({ length: Math.min(100, pool.length) }, (_, i) => pool[Math.floor(i * pool.length / Math.min(100, pool.length))]);
    const rounds = 3, warmup = 3, samplesMs = [];
    let count = 0;
    const pass = () => { for (const c of selected) {
      const result = new extractors[c.language](c.file, c.source).extract();
      count += result.nodes.length + result.edges.length + result.unresolvedReferences.length + result.errors.length;
    } };
    for (let i = 0; i < warmup; i++) pass();
    count = 0;
    for (let sample = 0; sample < 7; sample++) {
      const start = performance.now();
      for (let i = 0; i < rounds; i++) pass();
      samplesMs.push((performance.now() - start) / rounds);
    }
    const native = spawnSync(binary, [], { input: JSON.stringify({ op: 'bench', cases: selected, rounds, warmup }) + '\n', encoding: 'utf8', windowsHide: true, maxBuffer: 16 * 1024 * 1024, timeout: 120000 });
    if (native.status !== 0) throw Error(native.stderr || native.error);
    const nativeTiming = JSON.parse(native.stdout);
    if (nativeTiming.count !== count) throw Error(`benchmark result count mismatch: ${language}`);
    report.benchmarks[language] = {
      files: selected.length, bytes: selected.reduce((n, c) => n + Buffer.byteLength(c.source), 0),
      inputsSha256: createHash('sha256').update(JSON.stringify(selected.map(({ file, source, language }) => ({ file, source, language })))).digest('hex'),
      selectedFiles: selected.map(c => c.file), typescriptSamplesMs: samplesMs, nativeSamplesMs: nativeTiming.samplesMs,
      typescriptMedianMs: median(samplesMs), nativeMedianMs: median(nativeTiming.samplesMs), speedup: median(samplesMs) / median(nativeTiming.samplesMs),
    };
    console.log(`${language}: ${selected.length} files, TS ${median(samplesMs).toFixed(2)} ms, native ${median(nativeTiming.samplesMs).toFixed(2)} ms (${report.benchmarks[language].speedup.toFixed(2)}x)`);
  }
}
const reportPath = process.argv.find(a => a.startsWith('--report='))?.slice('--report='.length);
if (reportPath) fs.writeFileSync(reportPath, JSON.stringify(report, null, 2) + '\n');
