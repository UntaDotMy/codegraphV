#!/usr/bin/env node
/** Standalone templates: refresh portable reference vectors, then benchmark
 * release builds on the same inputs. Extra directory arguments are read-only. */
import { createRequire } from 'node:module';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { root, oracleRoot } from './check-upstream.mjs';
import { normalize, compare } from './extraction-compare.mjs';
const require = createRequire(import.meta.url);
const { initGrammars, loadGrammarsForLanguages, detectLanguage } = require(path.join(oracleRoot, 'dist/extraction/grammars.js'));
const { extractFromSource } = require(path.join(oracleRoot, 'dist/extraction/tree-sitter.js'));
const ts = require(path.join(oracleRoot, 'node_modules/typescript'));
process.env.CODEGRAPH_KERNEL = '0';
await initGrammars(); await loadGrammarsForLanguages(['csharp']);
const binary = path.resolve(process.argv[2] ?? 'target/debug/examples/sfc_probe.exe');
const cases = [];
function add(file, source, language) {
  for (const variant of [source, source.replaceAll('\n', '\r\n'), '😀\n' + source]) cases.push({ file, source: variant, language });
}
for (const source of [
  '', "😀 {% render 'card' %} {% render 'card' %} {% section 'card' %}",
  "{% liquid\n assign value = product.title\n render 'price', v: value\n section 'header'\n%}",
  "{% comment %}{% render 'ignored' %}{% endcomment %}{% raw %}{% section 'ignored' %}{% endraw %}{% include 'real' %}",
  "{% liquid\n comment\n render 'ignored'\n endcomment\n raw\n assign wrong = 1\n endraw\n render 'real' # text\n%}",
  "{% schema %}{\"name\":{\"fr\":\"Prix\",\"en\":\"Price\"},\"settings\":[{\"default\":\"DO_NOT_COPY\"}]}{% endschema %}",
  "{% schema %}invalid{% endschema %}{% schema %}{\"name\":[\"List\"]}{% endschema %}",
  "{%- assign foo = 1 -%}\n{%- liquid\n\tinclude 'x'\n assign foo = 2\n-%}",
  "{% assign text = \"{% render 'not-a-tag' %}\" %}{% render 'after' %}",
]) add('theme/sections/example.liquid', source, 'liquid');
for (const source of ['null', '{"sections":{"a":{"type":"hero"},"b":{"type":"hero"},"c":{"type":"footer"}}}', '{"sections":[{"type":"hero"},{"type":0},{"type":""}]}', '{"sections":false}', '{"sections":', '{"sections":{"2":{"type":"two"},"1":{"type":"one"}}}',
  // Editor-saved JSONC: comments, trailing commas, and the strictness around them (#2812).
  '/* IMPORTANT: auto-generated */\n{\n  "sections": {\n    "a": { "type": "hero", },\n    "b": {"type":"😀x"},\n  },\n  "order": ["a", "b",],\n}',
  '// note\n{"sections":{"a":{"type":"x"}}} // after', '{"sections":{"a":{"type":"x"}} "b":{}}', '/* unclosed {"sections":{"a":{"type":"x"}}}',
  '{"sections":{"a":{"type":"typ\u0065"},"b":{"ty\u0070e":"esc"},"c":{"type":"a\\"b"}}}', '{"sections":{,}}', '{"sections":{"a":{"type":"x"},,"b":{"type":"y"}}}',
  '{"sections":{"a":{"type":"one","type":"two"}},"sections":{"z":{"type":"three"}}}', '{"n":-0.5e+3,"t":true,"sections":{"a":{"type":"x"}}}',
  '{"n":01,"sections":{"a":{"type":"x"}}}', '{"n":-,"sections":{}}', '{"sections":{"a":{"type":"x"}}}\n😀', '{"sections":{"a":{"type":"x"}}} {}',
  '{"sections":{"a":{"type":"line\nbreak"}}}', '{"sections":{"a":"hero","b":{"type":["x"]},"c":{"type":"ok"}}}', '[{"sections":{}}]', '{"sections":{"a":{"type":"x"}},"z":tru}',
]) add('theme/templates/index.json', source, 'liquid');
for (const source of [
  "{% sections 'header-group' %}{%- sections \"footer-group\" -%}{% section 'header-group' %}",
  "{% liquid\n sections 'overlay-group'\n render 'x'\n%}",
  "{% comment %}{% schema %}{\"name\":\"no\"}{% endschema %}{% endcomment %}{% schema %}{\"name\":\"yes\"}{% endschema %}",
  "{% raw %}{% schema %}{\"name\":\"no\"}{% endschema %}",
  "{% capture title %}x{% endcapture %}{% capture 'q1' %}{% endcapture %}{% capture \"q2\"-%}{%- endcapture %}{% capture bad-name %}{% capture a.b %}",
  "{% assign v = 1 %}{% capture v %}{% endcapture %}{% capture v %}{% endcapture %}",
  "{% liquid\r\n capture inner\r\n echo 1\r\n endcapture\r\n capture 'quoted'\r\n%}",
]) add('theme/sections/more.liquid', source, 'liquid');
for (const source of [
  '', 'object Form1: TForm1\n  OnCreate = FormCreate\n  object Button1: TButton\n    OnClick = ButtonClick\n  end\nend',
  'inherited Frame1: TFrame\n Items = <\n item\n object Ghost: TButton\n end>\n inline Button1: TButton\n OnClick = Click\n end\nend',
  "object Form1: TForm\n Lines.Strings = (\n 'object Ghost: TButton'\n 'end'\n )\n OnClick = RealClick\nend",
  'end\nOnClick = RootHandler\nobject Partial: TForm\n object Child: TPanel',
]) add('forms/Example.dfm', source, 'pascal');
for (const source of [
  '', '<mapper namespace="com.app.Mapper"><sql id="cols">id,name</sql><select id="find" parameterType="int" resultType="User">SELECT <include refid="cols"/> FROM user</select></mapper>',
  "<sqlMap><statement id='Account.find'>select 1</statement><procedure id='Account.proc'><include refid='Other.cols'/></procedure></sqlMap>",
  '<mapper namespace="X"><!-- 😀\n<select id="ghost">no</select> --><select id="live" databaseId="db1">SELECT 1</select><select id="live" databaseId="db2">SELECT 2</select></mapper>',
  '<mapper namespace="X"><select id="cdata"><![CDATA[SELECT "<!-- sql data -->"]]></select></mapper>',
  '<mapper namespace="X"><!-- unterminated <select id="no">bad</select>',
  '<sqlMapConfig><select id="notMapper">no</select></sqlMapConfig>',
  '<mapper><select id="noNamespace">no</select></mapper>',
  '<mapper namespace="X"><statement id="ibatisOnly">no</statement><select id="y">SELECT 😀, 世界</select>',
]) add('mappers/Example.xml', source, 'xml');
for (const source of [
  '', '@model App.Models.ViewModel\n@inherits Base<Thing, Other.Model>\n@inject IService<User> service\n<div>@typeof(App.Layout)</div>',
  '<Grid TItem="App.User" TKey="Key"><Card /><InputText /><Router /></Grid>',
  '@code {\n private IService service;\n protected void Load() { service.Run(); var model = new Model(); }\n}',
  '@functions {\n string Text = "}"; /* { */ void Load() { Run(); }\n}\n@{ var dto = new Dto(); }',
  '@code {\n // }\n void Load() { var s = "\\\"{"; Work(); }\n}\n@code { void Next() { Another(); } }',
  '@code { unclosed\n@functions { void Load() { Work(); } }',
]) for (const file of ['Views/Example.cshtml', 'Components/Example.razor']) add(file, source, 'razor');
for (const language of ['yaml', 'twig', 'properties']) for (const source of ['', 'x: y\n', '😀 {{ render() }}\n']) add(`config/example.${language}`, source, language);

// Read literal regression inputs as data. Also fold string concatenations so
// the existing MyBatis robustness suite contributes complete mapper documents.
function literal(node) {
  if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) return node.text;
  if (ts.isBinaryExpression(node) && node.operatorToken.kind === ts.SyntaxKind.PlusToken) {
    const a = literal(node.left), b = literal(node.right); if (a !== undefined && b !== undefined) return a + b;
  }
}
for (const name of ['extraction.test.ts', 'mybatis-extractor-robustness.test.ts', 'frameworks-integration.test.ts']) {
  const source = ts.createSourceFile(name, fs.readFileSync(path.join(oracleRoot, '__tests__', name), 'utf8'), ts.ScriptTarget.Latest, true);
  function visit(node) {
    const value = literal(node);
    if (value !== undefined && value.length < 100_000) {
      if (/\{%/.test(value)) add('regression/example.liquid', value, 'liquid');
      if (/<(?:mapper|sqlMap)\b/.test(value)) add('regression/example.xml', value, 'xml');
      if (/^\s*(?:object|inherited|inline)\s+\w+\s*:/m.test(value)) add('regression/example.dfm', value, 'pascal');
      if (/@(?:model|inherits|inject|code|functions|typeof)\b|@\{/.test(value)) for (const ext of ['razor', 'cshtml']) add(`regression/example.${ext}`, value, 'razor');
    }
    ts.forEachChild(node, visit);
  }
  visit(source);
}
function collect(directory, base = directory) {
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    if (['.git', 'node_modules', 'dist', 'target', 'bin', 'obj'].includes(entry.name)) continue;
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) { collect(file, base); continue; }
    const extension = path.extname(file).toLowerCase();
    let language = ({ '.liquid': 'liquid', '.xml': 'xml', '.dfm': 'pascal', '.fmx': 'pascal', '.razor': 'razor', '.cshtml': 'razor' })[extension];
    if ((!language && extension !== '.json') || fs.statSync(file).size > 1024 * 1024) continue;
    const source = fs.readFileSync(file, 'utf8');
    if (source.includes('\uFFFD') || source.includes('\0')) continue;
    if (!language && detectLanguage(path.relative(base, file), source, undefined, base) === 'liquid') language = 'liquid';
    if (!language) continue;
    cases.push({ file: path.relative(base, file).replaceAll('\\', '/'), source, language, corpus: true });
  }
}
for (const directory of process.argv.slice(3).filter(a => !a.startsWith('--'))) collect(path.resolve(directory));
const expected = cases.map(c => normalize(extractFromSource(c.file, c.source, c.language)));
compare(binary, cases, expected);
if (process.argv.includes('--write')) {
  fs.writeFileSync(path.join(root, 'crates/codegraph-extract/tests/golden/templates.json'), JSON.stringify(cases.flatMap((input, i) => input.corpus ? [] : [{ input, expected: expected[i] }])));
}
console.log(`${cases.length} template/config extractions match the current TypeScript reference`);
