#!/usr/bin/env node
/** Compare native graph queries with the TypeScript oracle on the SAME DB.
 * cargo build -p codegraph-graph --example graph_probe
 * node scripts/rust-golden/graph.mjs target/debug/examples/graph_probe.exe [--write]
 * --write refreshes the portable, oracle-independent cargo-test fixtures.
 */
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import { isDeepStrictEqual } from 'node:util';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { oracleRoot } from './check-upstream.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const require = createRequire(import.meta.url);
const { DatabaseConnection } = require(path.join(oracleRoot, 'dist/db/index.js'));
const { QueryBuilder } = require(path.join(oracleRoot, 'dist/db/queries.js'));
const { GraphTraverser } = require(path.join(oracleRoot, 'dist/graph/traversal.js'));
const { GraphQueryManager } = require(path.join(oracleRoot, 'dist/graph/queries.js'));
const { buildTypeHierarchy, countImplementers } = require(path.join(oracleRoot, 'dist/graph/type-hierarchy.js'));
const { describeSynthesizedHop } = require(path.join(oracleRoot, 'dist/graph/synthesized-hop.js'));
const lookupFunctions = require(path.join(oracleRoot, 'dist/graph/symbol-lookup.js'));
const flowFunctions = require(path.join(oracleRoot, 'dist/graph/named-symbol-flow.js'));
const binary = path.resolve(process.argv[2] ?? 'target/debug/examples/graph_probe.exe');
const write = process.argv.includes('--write');
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-rust-graph-'));
let total = 0;

const node = (id, kind = 'function', extra = {}) => ({
  id, kind, name: id, qualifiedName: id, filePath: `src/${id}.ts`, language: 'typescript',
  startLine: 1, endLine: 10, startColumn: 0, endColumn: 0,
  isExported: false, isAsync: false, isStatic: false, isAbstract: false,
  updatedAt: 1700000000000, ...extra,
});
const edge = (source, target, kind = 'calls', line = 1, extra = {}) => ({ source, target, kind, line, column: 0, ...extra });
const sub = g => ({ nodes: [...g.nodes.values()], edges: g.edges, roots: g.roots });
function normalize(value) {
  if (value instanceof Set) return [...value].map(normalize);
  if (value instanceof Map) return Object.fromEntries([...value].map(([k, v]) => [k, normalize(v)]));
  if (Array.isArray(value)) return value.map(normalize);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).filter(([k, v]) => v !== undefined && !(k === 'visibility' && v === null && 'qualifiedName' in value) && !(k === 'provenance' && v === null && 'source' in value)).map(([k, v]) => [k, normalize(v)]));
  return value;
}
// DB field round-tripping has its own cross-engine suite. Persist identifiers
// here to keep thousands of graph topology/order assertions reviewable.
function compact(value) {
  if (value && typeof value === 'object' && !Array.isArray(value) && 'qualifiedName' in value && 'id' in value) return value.id;
  if (Array.isArray(value)) return value.map(compact);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, compact(v)]));
  return value;
}
function run(q, queries) {
  queries.setProjectNameTokens(new Set(q.projectTokens ?? []));
  queries.setDeprioritizedPathMatcher(p => (q.deprioritized ?? []).includes(p));
  const t = new GraphTraverser(queries), m = new GraphQueryManager(queries);
  const id = q.id ?? 'missing', depth = q.depth ?? 2;
  const host = new Proxy(queries, { get(target, prop) {
    if (prop === 'getCallers') return t.getCallers.bind(t);
    if (prop === 'getCallees') return t.getCallees.bind(t);
    if (prop === 'getNode') return target.getNodeById.bind(target);
    if (prop === 'getProjectRoot') return () => work;
    if (prop === 'generatedFilePredicate') return target.generatedPredicateFor.bind(target);
    if (prop === 'searchNodes') return (text, opts) => {
      const results = target.searchNodes(text, opts);
      (q.searches ??= {})[`${text}|${opts?.limit ?? 50}`] = results.map(r => r.node.id);
      return results;
    };
    const value = target[prop];
    return typeof value === 'function' ? value.bind(target) : value;
  } });
  const go = file => q.goPackages?.[file] ?? null;
  switch (q.op) {
    case 'dominantFile': return queries.getDominantFile();
    case 'topRouteFile': return queries.getTopRouteFile();
    case 'search': return queries.searchNodes(q.query, q.options);
    case 'searchExact': return queries.findNodesByExactName(q.names, q.options);
    case 'searchSubstring': return queries.findNodesByNameSubstring(q.query, q.options);
    case 'bfs': return sub(t.traverseBFS(id, q.opts));
    case 'dfs': return sub(t.traverseDFS(id, q.opts));
    case 'callers': return t.getCallers(id, depth);
    case 'callees': return t.getCallees(id, depth);
    case 'callGraph': return sub(t.getCallGraph(id, depth));
    case 'impact': return sub(t.getImpactRadius(id, depth));
    case 'path': return t.findPath(id, q.to, q.kinds);
    case 'ancestors': return t.getAncestors(id);
    case 'children': return t.getChildren(id);
    case 'usages': return t.findUsages(id);
    case 'legacyHierarchy': return sub(t.getTypeHierarchy(id));
    case 'context': try { return m.getContext(id); } catch (e) { return { error: e.message }; }
    case 'metrics': return m.getNodeMetrics(id);
    case 'dependencies': return m.getFileDependencies(q.file);
    case 'dependents': return m.getFileDependents(q.file);
    case 'exports': return m.getExportedSymbols(q.file);
    case 'modules': return m.getModuleStructure();
    case 'cycles': return m.findCircularDependencies();
    case 'deadCode': return m.findDeadCode();
    case 'qualified': return m.findByQualifiedName(q.pattern);
    case 'filtered': return sub(m.getFilteredSubgraph(n => !q.file || n.filePath === q.file, q.includeEdges));
    case 'hierarchy': return queries.getNodeById(id) ? buildTypeHierarchy(queries, queries.getNodeById(id), { overrides: q.overrides }) : null;
    case 'implementers': return countImplementers(queries, id);
    case 'between': return queries.findEdgesBetweenNodes(q.ids, q.kinds);
    case 'hop': return describeSynthesizedHop(q.edge);
    case 'flowTokens': return { tokens: flowFunctions.flowTokens(q.query), normalized: flowFunctions.normalizeToken(q.query) };
    case 'symbol': return { matches: lookupFunctions.matchesSymbol(queries.getNodeById(id), q.symbol, go), lastPart: lookupFunctions.lastQualifierPart(q.symbol), qualified: lookupFunctions.isQualifiedSymbol(q.symbol), description: lookupFunctions.describeSymbolNode(queries.getNodeById(id)) };
    case 'groupDefinitions': return lookupFunctions.groupDefinitions(queries.getAllNodes(), q.fileFilter);
    case 'lookupSymbol': return lookupFunctions.lookupSymbolNodes(host, q.symbol, go);
    case 'qualifiedSymbols': return lookupFunctions.qualifiedSymbolMatches(host, q.symbol, go);
    case 'allSymbols': return flowFunctions.findAllSymbols(host, q.symbol);
    case 'flow': return flowFunctions.resolveNamedSymbolFlow(host, q.query, q.options);
    case 'tokens': return flowFunctions.resolveNamedTokens(host, q.query, q.options);
    default: throw Error(`Unknown operation ${q.op}`);
  }
}

function casesFor(nodes) {
  const cases = [{ op: 'modules' }, { op: 'cycles' }, { op: 'deadCode' }, { op: 'filtered' }, { op: 'filtered', includeEdges: false }];
  for (const pattern of ['*', '?', '*.*', 'A*', 'Base::*', 'not[glob]*']) cases.push({ op: 'qualified', pattern });
  for (const id of [...nodes.map(n => n.id), 'missing']) {
    for (const op of ['ancestors', 'children', 'usages', 'legacyHierarchy', 'context', 'metrics', 'hierarchy', 'implementers']) cases.push({ op, id });
    for (const depth of [0, 1, 2, 3, 5]) for (const op of ['callers', 'callees', 'callGraph', 'impact']) cases.push({ op, id, depth });
    for (const to of [nodes[0].id, nodes.at(-1).id, 'missing']) for (const kinds of [[], ['calls'], ['contains', 'instantiates']]) cases.push({ op: 'path', id, to, kinds });
    for (const direction of ['outgoing', 'incoming', 'both']) for (const op of ['bfs', 'dfs']) {
      for (const opts of [{}, { limit: 0 }, { limit: 1 }, { limit: 3 }, { maxDepth: 0 }, { maxDepth: 1 }, { maxDepth: 2, includeStart: false }, { includeStart: false }, { edgeKinds: ['calls'] }, { nodeKinds: ['method', 'class'] }]) cases.push({ op, id, opts: { direction, ...opts } });
    }
  }
  for (const file of new Set([...nodes.map(n => n.filePath), 'missing.ts'])) {
    for (const op of ['dependencies', 'dependents', 'exports']) cases.push({ op, file });
    cases.push({ op: 'filtered', file });
  }
  for (const kinds of [[], ['calls'], ['extends', 'implements']]) cases.push({ op: 'between', ids: nodes.map(n => n.id), kinds });
  cases.push({ op: 'between', ids: [] });
  return cases;
}

const scenarios = [
  { name: 'regressions', nodes: [node('t'), node('a'), node('b'), node('c'), node('P', 'class'), node('M', 'method', { filePath: 'src/P.ts', qualifiedName: 'P::M' }), node('Q'), node('S', 'method', { filePath: 'src/P.ts' })],
    edges: [edge('a', 't'), edge('b', 'a'), edge('b', 't'), edge('c', 'b'), edge('P', 'M', 'contains'), edge('P', 'S', 'contains'), edge('Q', 'M'), edge('Q', 'P'), edge('Q', 'P', 'instantiates'), edge('a', 't', 'references', 2), edge('a', 't', 'calls', 7)] },
  { name: 'hierarchy', nodes: [node('I', 'interface'), node('Base', 'class'), node('Derived', 'class'), node('Leaf', 'struct'), node('baseMethod', 'method', { name: 'run', qualifiedName: 'Base::run' }), node('derivedMethod', 'method', { name: 'run', qualifiedName: 'Derived::run' }), node('file', 'file'), node('imp', 'import'), node('ret', 'type_alias')],
    edges: [edge('Derived', 'Base', 'extends'), edge('Derived', 'I', 'implements'), edge('Leaf', 'Derived', 'extends'), edge('Base', 'baseMethod', 'contains'), edge('Derived', 'derivedMethod', 'contains'), edge('file', 'Derived', 'contains'), edge('file', 'imp', 'imports'), edge('derivedMethod', 'ret', 'returns'), edge('derivedMethod', 'ret', 'type_of'), edge('baseMethod', 'derivedMethod', 'calls')] },
  { name: 'collation', nodes: [node('root', 'interface'), ...['zebra', 'éclair', 'Eclair', 'alpha', 'Alpha', 'a_b', 'a-b', 'a.b', 'Ωmega', '中', 'a10', 'a2', 'Éclair'].map(id => node(id, 'class'))], edges: [] },
];
scenarios[2].edges = scenarios[2].nodes.slice(1).map(n => edge(n.id, 'root', 'implements'));
let state = 0x12345678;
const rand = n => { state ^= state << 13; state ^= state >>> 17; state ^= state << 5; return (state >>> 0) % n; };
for (let i = 0; i < 4; i++) {
  const kinds = ['function', 'method', 'class', 'interface', 'variable', 'file', 'union', 'component'];
  const nodes = Array.from({ length: 9 }, (_, j) => node(`n${j}`, kinds[j % kinds.length], { filePath: `src/f${j % 3}.ts`, isExported: j % 3 === 0 }));
  const edges = Array.from({ length: 32 }, (_, j) => edge(`n${rand(9)}`, `n${rand(9)}`, ['calls', 'references', 'contains', 'instantiates', 'navigates', 'extends', 'implements'][rand(7)], j));
  scenarios.push({ name: `random-${i}`, nodes, edges });
}
const fan = { name: 'wide-hierarchy', nodes: [node('root', 'interface'), ...Array.from({ length: 430 }, (_, i) => node(`C${i}`, 'class'))], edges: [], cases: [{ op: 'hierarchy', id: 'root' }, { op: 'implementers', id: 'root' }] };
fan.edges = fan.nodes.slice(1).map(n => edge(n.id, 'root', 'implements', 1, { provenance: 'heuristic', metadata: { synthesizedBy: 'go-implements', registeredAt: `src/${n.id}.go:1` } }));
scenarios.push(fan);
const deep = { name: 'deep-hierarchy', nodes: Array.from({ length: 14 }, (_, i) => node(`C${i}`, 'class')), edges: [], cases: [{ op: 'hierarchy', id: 'C0' }, { op: 'hierarchy', id: 'C13' }] };
deep.edges = deep.nodes.slice(1).map((n, i) => edge(n.id, `C${i}`, 'extends'));
scenarios.push(deep);
const mechanisms = ['callback', 'http-client', 'queue-job', 'event-bus', 'event-emitter', 'react-render', 'jsx-render', 'vue-handler', 'interface-impl', 'closure-collection', 'fn-pointer-dispatch', 'goframe-route', 'go-func-field', 'redux-thunk', 'cpp-override', 'rn-event-channel'];
scenarios.push({ name: 'synthesized-hops', nodes: [node('a'), node('b')], edges: [], cases: [
  ...mechanisms.flatMap(synthesizedBy => [
    { op: 'hop', edge: edge('a', 'b', 'calls', 1, { provenance: 'heuristic', metadata: { synthesizedBy } }) },
    { op: 'hop', edge: edge('a', 'b', 'calls', 1, { provenance: 'heuristic', metadata: { synthesizedBy, registeredAt: 'src/wire.ts:7', via: 'subscribe', method: 'POST', href: '/users', queue: 'emails', event: 'chat', tier: 'client→server', channel: 'socket', field: 'handlers', promotedInto: 'Runner', route: '/users' } }) },
  ]), { op: 'hop', edge: null }, { op: 'hop', edge: edge('a', 'b') }, { op: 'hop', edge: edge('a', 'b', 'calls', 1, { provenance: 'heuristic', metadata: { confidence: 0.5 } }) },
] });

const snapshots = [];
const flow = { name: 'named-flow', nodes: [
  node('start', 'function', { name: 'entryStart', qualifiedName: 'entryStart' }),
  node('bridge', 'function', { name: 'middleBridge', qualifiedName: 'middleBridge' }),
  node('target', 'function', { name: 'finalTarget', qualifiedName: 'finalTarget' }),
  node('route', 'route', { name: 'GET /api/run', qualifiedName: '/api/run' }),
  node('runner', 'class', { name: 'Alpha', qualifiedName: 'Alpha' }),
  node('thunk1', 'constant', { name: 'loadUsers', qualifiedName: 'loadUsers' }),
  node('thunk2', 'variable', { name: 'saveUsers', qualifiedName: 'saveUsers' }),
  node('plain', 'constant', { name: 'plainValue', qualifiedName: 'plainValue' }),
  ...['Alpha', 'Beta', 'Gamma', 'Delta', 'Epsilon'].map((type, i) => node(`handler${i}`, 'method', { name: 'handle', qualifiedName: `${type}::handle` })),
  node('generated', 'function', { name: 'entryStart', qualifiedName: 'entryStart', filePath: 'src/generated.pb.go', language: 'go' }),
  node('fixture', 'function', { name: 'entryStart', qualifiedName: 'entryStart', filePath: '__tests__/fixture.ts' }),
  node('short', 'method', { name: 'ok', qualifiedName: 'ok' }),
  node('predicate', 'method', { name: 'valid?', qualifiedName: 'valid?' }),
  node('selector', 'method', { name: 'initWithFrame:', qualifiedName: 'Widget::initWithFrame:' }),
], edges: [
  edge('start', 'bridge', 'calls', 5), edge('bridge', 'target', 'calls', 8),
  edge('start', 'handler1', 'calls', 2), edge('start', 'handler0', 'calls', 3),
  edge('handler1', 'target', 'calls', 9), edge('handler0', 'target', 'calls', 10),
  edge('start', 'route', 'navigates', 6), edge('route', 'target', 'calls', 12),
  edge('target', 'short', 'calls'), edge('short', 'predicate', 'calls'), edge('predicate', 'selector', 'calls'),
  edge('start', 'thunk1', 'calls', 14, { provenance: 'heuristic', metadata: { synthesizedBy: 'redux-thunk' } }),
  edge('thunk1', 'thunk2', 'calls', 15, { provenance: 'heuristic', metadata: { synthesizedBy: 'redux-thunk' } }),
], cases: [] };
for (const query of ['entryStart finalTarget', 'entryStart middleBridge finalTarget', 'Alpha handle finalTarget', 'entryStart Alpha.handle finalTarget', 'entryStart Beta.handle finalTarget', 'loadUsers saveUsers plainValue', 'entryStart loadUsers saveUsers finalTarget', 'mistyped missing', 'handle finalTarget', 'entryStart.ts, middleBridge() [finalTarget]', 'how entryStart calls finalTarget', 'entryStart', '']) {
  for (const op of ['flow', 'tokens']) for (const options of [{}, { maxHops: 2 }, { maxHops: 0 }, { maxBridge: 0 }, { maxBridge: 2 }, { maxChains: 3 }]) flow.cases.push({ op, query, options });
}
for (const from of ['entryStart', 'finalTarget', 'ok', 'valid?', 'initWithFrame:', 'Alpha.handle', 'missing']) {
  for (const to of ['finalTarget', 'ok', 'valid?', 'initWithFrame:', 'entryStart', 'missing']) {
    for (const maxHops of [1, 2, 3, 12]) flow.cases.push({ op: 'flow', query: '', options: { mode: 'directed', from, to, maxHops, maxChains: 3 } });
  }
}
scenarios.push(flow);
const symbols = { name: 'symbol-lookup', nodes: [
  node('nested', 'method', { name: 'refresh', qualifiedName: 'Discovery::refresh', filePath: 'pkg/refresh/v2/loop.go', language: 'go' }),
  node('root', 'function', { name: 'New', qualifiedName: 'New', filePath: 'gin.go', language: 'go' }),
  node('dotted', 'function', { name: 'group', qualifiedName: 'AppWeb.Format::group', filePath: 'src/format.ex' }),
  node('rust', 'function', { name: 'run', qualifiedName: 'run', filePath: 'src/stage_apply/mod.rs', language: 'rust' }),
  node('erlang', 'function', { name: 'call', qualifiedName: 'server::call/3', filePath: 'src/server.erl', language: 'erlang' }),
  node('file', 'file', { name: 'product-card.liquid', qualifiedName: 'product-card.liquid', filePath: 'snippets/product-card.liquid', language: 'liquid' }),
  node('overload1', 'method', { name: 'refresh', qualifiedName: 'Discovery::refresh', filePath: 'pkg/refresh/v2/loop.go', language: 'go', startLine: 20 }),
  node('overload2', 'method', { name: 'refresh', qualifiedName: 'Discovery::refresh', filePath: 'generated.pb.go', language: 'go' }),
  node('nix1', 'property', { name: 'options.xdg.configFile', qualifiedName: 'options.xdg.configFile', language: 'nix' }),
  node('nix2', 'property', { name: 'xdg.configFile', qualifiedName: 'xdg.configFile', language: 'nix' }),
  node('nix3', 'property', { name: 'xdg.configFile."git/config".text', qualifiedName: 'xdg.configFile."git/config".text', language: 'nix' }),
], edges: [], cases: [], goPackages: { 'pkg/refresh/v2/loop.go': 'refresh', 'gin.go': 'gin', 'generated.pb.go': 'generated' } };
for (const id of symbols.nodes.map(n => n.id)) for (const symbol of ['refresh', 'Discovery.refresh', 'refresh.Discovery.refresh', 'pkg/refresh/Discovery.refresh', 'New', 'gin.New', 'wrong.New', 'AppWeb.Format.group', 'AppWeb::Format::group', 'Format.group', 'group', 'stage_apply::run', 'crate::stage_apply::run', 'self::run', 'call/3', 'server::call/3', 'call/2', 'product-card', 'options.xdg.configFile', 'xdg.configFile', '.', '::', '/3', '']) symbols.cases.push({ op: 'symbol', id, symbol, goPackages: symbols.goPackages });
for (const symbol of ['refresh', 'Discovery.refresh', 'refresh.Discovery.refresh', 'gin.New', 'AppWeb.Format.group', 'stage_apply::run', 'call/3', 'server::call/2', 'product-card', 'xdg.configFile', 'misspelled', 'refrsh']) {
  for (const op of ['lookupSymbol', 'qualifiedSymbols', 'allSymbols']) symbols.cases.push({ op, symbol, goPackages: symbols.goPackages });
}
for (const fileFilter of [undefined, 'pkg/refresh/v2/loop.go', './loop.go', 'loop.go', 'missing.rs', 'go']) symbols.cases.push({ op: 'groupDefinitions', fileFilter });
for (const query of ['Class.method()', 'crate::run finalTarget.ts', 'valid? ok initWithFrame:', 'one.TSX two.js', 'some.tsx, fnRef()', 'foo\uFEFFbar', 'one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen', '', 'Ωmega finalTarget', 'foo/3 bar/2']) symbols.cases.push({ op: 'flowTokens', query });
scenarios.push(symbols);
const search = { name: 'search-ranking', nodes: [
  node('get-user', 'function', { name: 'getUser', qualifiedName: 'Api::getUser', filePath: 'src/api/user.ts', docstring: 'Load an authenticated user from storage', signature: 'getUser(id: string): User' }),
  node('get-user-doc', 'class', { name: 'UserDocumentation', qualifiedName: 'UserDocumentation', filePath: 'docs/user.ts', docstring: 'getUser authenticate user login' }),
  node('signin', 'function', { name: 'signInWithGoogle', qualifiedName: 'AuthService::signInWithGoogle', filePath: 'src/auth/google.ts' }),
  node('short', 'method', { name: 'run', qualifiedName: 'Runner::run', filePath: 'src/runner.ts' }),
  node('upper', 'method', { name: 'Run', qualifiedName: 'Tests::Run', filePath: '__tests__/runner.test.ts' }),
  node('generated-search', 'method', { name: 'getUser', qualifiedName: 'ApiClient::getUser', filePath: 'vendor/sdk.pb.go', language: 'go' }),
  node('unicode', 'function', { name: 'Été', qualifiedName: 'Météo::Été', filePath: 'src/Été.ts', docstring: '天气 hello 世界' }),
  ...Array.from({ length: 125 }, (_, i) => node(`compound${i}`, i % 2 ? 'method' : 'class', { name: `getUserDescriptor${i}`, qualifiedName: `Api::getUserDescriptor${i}`, filePath: `src/user/descriptor${i % 5}.ts`, docstring: i % 3 ? '' : 'getUser' })),
  ...Array.from({ length: 20 }, (_, i) => node(`overload${i}`, 'function', { name: 'run', qualifiedName: `Runner${i}::run`, filePath: `src/runner${i}.ts` })),
], edges: [], cases: [] };
const searchOptions = [{}, { limit: 0 }, { limit: 1 }, { limit: 5 }, { limit: 10, offset: 2 }, { kinds: ['function'] }, { languages: ['go'] }, { kinds: ['method', 'class'], limit: 5 }];
for (const query of ['getUser', 'get', 'Descriptor', 'signIn', 'InWith', 'getUsr', 'signInWithGoogl', 'rnu', 'run', 'Run', 'Api::getUser', 'kind:method getUser', 'kind:function', 'lang:go getUser', 'kind:class lang:typescript', 'name:run path:src', 'path:vendor', 'name:get path:"src/api"', 'kind:method lang:go name:get', 'Été', '天气', 'hello', '"', '*', 'AND OR NOT NEAR', 'one:two', 'getUser run', 'missing', '', ' ', 'x', 'get_User', 'get%User']) {
  for (const options of searchOptions) search.cases.push({ op: 'search', query, options });
  for (const projectTokens of [[], ['user', 'api']]) search.cases.push({ op: 'search', query, projectTokens, deprioritized: ['src/api/user.ts', 'src/runner.ts'] });
}
for (const names of [[], ['getUser'], ['run'], ['getUser', 'run'], ['getUser', 'signInWithGoogle', 'run'], ['getUser', 'GETUSER'], ['absent'], ['getUser', 'getUser']]) for (const options of searchOptions) search.cases.push({ op: 'searchExact', names, options });
for (const query of ['get', 'User', 'InWith', 'run', 'e', '', '%', '_']) for (const options of [{}, { excludePrefix: true }, { limit: 2 }, { kinds: ['function'], excludePrefix: true }, { languages: ['go'] }]) search.cases.push({ op: 'searchSubstring', query, options });
scenarios.push(search);
for (const [name, tests, generatedCount] of [['nineteen-tests', 19, 0], ['twenty-tests', 20, 0], ['generated-batches', 20, 21]]) {
  const scenario = { name, nodes: [], edges: [], generatedPaths: [], cases: [{ op: 'dominantFile' }, { op: 'dominantFile' }, { op: 'topRouteFile' }] };
  const addFile = (file, count, routeCount, generated = false) => {
    const prefix = `f${scenario.nodes.length}`;
    scenario.nodes.push(node(prefix + 'a', 'function', { filePath: file }), node(prefix + 'b', 'function', { filePath: file }));
    for (let i = 0; i < count; i++) scenario.edges.push(edge(prefix + 'a', prefix + 'b', 'calls', i + 1));
    for (let i = 0; i < routeCount; i++) scenario.nodes.push(node(prefix + 'route' + i, 'route', { filePath: file }));
    if (generated) scenario.generatedPaths.push(file);
  };
  addFile('src/core.ts', 60, 8);
  addFile('src/second.ts', 50, 4);
  for (let i = 0; i < tests; i++) addFile(`tests/t${i}.test.ts`, 70, 10);
  for (let i = 0; i < generatedCount; i++) addFile(`src/auto${i}.ts`, 80, 20, true);
  scenarios.push(scenario);
}
try {
  for (const scenario of scenarios) {
    const dbPath = path.join(work, `${scenario.name}.db`);
    const db = DatabaseConnection.initialize(dbPath);
    try {
      const queries = new QueryBuilder(db.getDb());
      queries.insertNodes(scenario.nodes);
      queries.insertEdges(scenario.edges);
      const files = [...new Set(scenario.nodes.map(n => n.filePath))].map(p => ({ path: p, contentHash: 'fixture', language: 'typescript', size: 100, modifiedAt: 1, indexedAt: 2, nodeCount: scenario.nodes.filter(n => n.filePath === p).length, generated: scenario.generatedPaths?.includes(p) ?? false }));
      for (const file of files) queries.upsertFile(file);
      for (const [file, pkg] of Object.entries(scenario.goPackages ?? {})) {
        const abs = path.join(work, file);
        fs.mkdirSync(path.dirname(abs), { recursive: true });
        fs.writeFileSync(abs, `package ${pkg}\n`);
      }
      const cases = scenario.cases ?? casesFor(scenario.nodes);
      const expected = cases.map(q => normalize(run(q, queries)));
      const native = spawnSync(binary, [dbPath], { input: cases.map(q => JSON.stringify(q)).join('\n') + '\n', encoding: 'utf8', maxBuffer: 128 * 1024 * 1024, timeout: 120000 });
      if (native.status !== 0) throw Error(native.stderr || `native probe failed: ${native.error}`);
      const actual = native.stdout.trim().split(/\r?\n/).map(line => JSON.parse(line));
      if (actual.length !== cases.length) throw Error(`expected ${cases.length} results, received ${actual.length}`);
      for (let i = 0; i < cases.length; i++) {
        if (!isDeepStrictEqual(actual[i], expected[i])) {
          console.error(`${scenario.name}: ${JSON.stringify(cases[i])}`);
          fs.writeFileSync(path.join(work, 'expected.json'), JSON.stringify(expected[i], null, 2));
          fs.writeFileSync(path.join(work, 'actual.json'), JSON.stringify(actual[i], null, 2));
          throw Error(`graph mismatch; inspect ${work}`);
        }
      }
      total += cases.length;
      snapshots.push({ name: scenario.name, nodes: scenario.nodes, edges: scenario.edges, files, cases: cases.map((query, i) => ({ query, expected: compact(expected[i]) })) });
      console.log(`${scenario.name}: ${cases.length} queries match`);
    } finally { db.close(); }
  }
  if (write) {
    const destination = path.join(root, 'crates/codegraph-graph/tests/fixtures/golden.json');
    fs.mkdirSync(path.dirname(destination), { recursive: true });
    fs.writeFileSync(destination, JSON.stringify(snapshots));
  }
  console.log(`${total} graph queries match the TypeScript oracle`);
} finally {
  // Keep diagnostics only on failure; every target is under this mkdtemp root.
  if (total > 0 && snapshots.length === scenarios.length) fs.rmSync(work, { recursive: true, force: true });
}
