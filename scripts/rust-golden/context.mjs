#!/usr/bin/env node
/** Same-DB context parity, followed by independent, portable Rust fixture tests.
 * cargo build -p codegraph-context --example context_probe
 * node scripts/rust-golden/context.mjs target/debug/examples/context_probe.exe --write
 */
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import { isDeepStrictEqual } from 'node:util';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { root, oracleRoot } from './check-upstream.mjs';
const require = createRequire(import.meta.url);
const { DatabaseConnection } = require(path.join(oracleRoot, 'dist/db/index.js'));
const { QueryBuilder } = require(path.join(oracleRoot, 'dist/db/queries.js'));
const { GraphTraverser } = require(path.join(oracleRoot, 'dist/graph/traversal.js'));
const { ContextBuilder } = require(path.join(oracleRoot, 'dist/context/index.js'));
const { CodeGraph } = require(path.join(oracleRoot, 'dist/index.js'));
const { formatSubgraphTree, formatBytes, formatContextAsJson } = require(path.join(oracleRoot, 'dist/context/formatter.js'));
const binary = path.resolve(process.argv[2] ?? 'target/debug/examples/context_probe.exe');
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'cg-native-context-'));
const node = (id, name, kind='function', extra={}) => ({ id,name,kind,qualifiedName:`src/service.ts::${name}`,filePath:'src/service.ts',language:'typescript',startLine:1,endLine:4,startColumn:0,endColumn:0,updatedAt:1700000000000,...extra });
const edge = (source,target,kind='calls',extra={}) => ({source,target,kind,line:2,column:0,...extra});
const file = (p, language='typescript', generated=false) => ({path:p,language,generated,contentHash:'hash',size:200,modifiedAt:1700000000000,indexedAt:1700000000000,nodeCount:1});
const sources = {'src/service.ts':'export function processCheckout() {\r\n  processPayment();\r\n  // café 🚀\r\n}\r\n', 'config/application.properties':'db.password=NEVER_DISCLOSE\n', '处理.ts':'export function 处理() {}\n'};
const scenarios = JSON.parse(fs.readFileSync(path.join(root,'crates/codegraph-graph/tests/fixtures/golden.json'),'utf8')).map(s => ({...s, sources:{}, cases:[]}));
scenarios.push({name:'context-channels',sources,nodes:[
  node('checkout','processCheckout'),node('payment','processPayment','method',{filePath:'src/payment.ts'}),node('confirm','confirmPayment','function',{filePath:'src/confirm.ts'}),
  node('service','PaymentService','class'),node('base','BasePaymentService','class',{filePath:'src/base.ts'}),node('sibling','CashPaymentService','class',{filePath:'src/cash.ts'}),
  node('controller','CheckoutController','class',{filePath:'src/checkout.ts'}),node('infix','getProfileInfoV2','method',{filePath:'src/profile.ts'}),
  node('compound','SearchShardRequest','class',{filePath:'src/search/shard.ts'}),node('common','FLAT','constant',{filePath:'src/unrelated.ts'}),
  node('seed','pinFeedIfNearBottom'), node('test','processCheckout','function',{filePath:'test/service.test.ts'}),
  node('generated','processCheckout','function',{filePath:'gen/wire.ts'}), node('han','处理.ts','file',{filePath:'处理.ts'}),
  node('config','db.password','constant',{language:'properties',filePath:'config/application.properties',signature:'db.password',endLine:1}),
  node('copyfile','CVACT01Y.cpy','file',{language:'cobol',filePath:'copy/CVACT01Y.cpy'}),
  node('copydecl','CUSTOMER-REC','variable',{language:'cobol',filePath:'copy/CVACT01Y.cpy'}),
  node('include','CVACT01Y','import',{language:'cobol',filePath:'src/COACTUPC.cbl'}),
  node('imp','PaymentService','import',{filePath:'src/import.ts'}), node('exp','PaymentService','export',{filePath:'src/export.ts'}),
],edges:[edge('checkout','payment'),edge('payment','confirm','calls',{provenance:'heuristic',metadata:{synthesizedBy:'callback',via:'registerPayment',registeredAt:'src/wire.ts:8'}}),
  edge('service','payment','contains'),edge('service','base','extends'),edge('sibling','base','extends'),edge('controller','checkout','contains'),edge('copyfile','copydecl','contains'),
  edge('include','copyfile','imports'),edge('imp','service','imports'),edge('exp','service','exports')], files:[file('src/service.ts'),file('gen/wire.ts','typescript',true),file('copy/CVACT01Y.cpy','cobol'),file('src/COACTUPC.cbl','cobol')]});
const queries=['', '   ', 'missing_symbol_129', 'processCheckout processPayment confirmPayment', 'PaymentService', 'payment checkout', 'flat object', 'search shard request', 'profileInfo', '处理', '处理.ts', 'CVACT01Y', 'COACTUPC CVACT01Y', 'db.password', 'auto scroll bottom', 'render state', 'A Base root', 'allocation caching', 'HTTP REST API', 'src/service.ts'];
let state=0x87654321;
const rand=n=>{state^=state<<13;state^=state>>>17;state^=state<<5;return(state>>>0)%n;};
for(let i=0;i<3;i++) {
  const names=['SearchShardRequest','SearchShardResponse','getProfileInfoV2','getProfileInfoLegacy','CheckoutService','CheckoutController','PaymentService','processPayment','processCheckout','renderFrame','setFrameState','FLAT','cache','CacheBuilder','HTTPServer','MAX_RETRIES','custom_handler','executeQuery','Handler','Handler'];
  const nodes=names.map((n,j)=>node(`r${j}`,n,['class','method','function','interface','constant','union'][rand(6)],{filePath:`${j%7===0?'test':j%11===0?'sample':'src'}/f${j%5}.ts`}));
  const edges=Array.from({length:60},()=>edge(`r${rand(nodes.length)}`,`r${rand(nodes.length)}`,['calls','references','contains','extends','implements'][rand(5)]));
  scenarios.push({name:`context-random-${i}`,nodes,edges,files:[],sources:{},cases:[]});
}
function clean(v) {
  if(Array.isArray(v)) return v.map(clean);
  if(v && typeof v==='object') return Object.fromEntries(Object.entries(v).filter(([k,v])=>v!==undefined).map(([k,v])=>[k,clean(v)]));
  return v;
}
const persisted=[];let total=0;let activeDb;
try {
  for(const s of scenarios) {
    const dir=path.join(work,s.name); fs.mkdirSync(dir,{recursive:true});
    for(const [p,text] of Object.entries(s.sources)) {fs.mkdirSync(path.dirname(path.join(dir,p)),{recursive:true});fs.writeFileSync(path.join(dir,p),text);}
    const dbPath=path.join(dir,'codegraph.db'); const db=activeDb=DatabaseConnection.initialize(dbPath);const q=new QueryBuilder(db.getDb());
    q.insertNodes(s.nodes);q.insertEdges(s.edges);for(const f of s.files)q.upsertFile(f);
    const tests=[];
    const named=[...new Set(s.nodes.map(n=>n.name))].slice(0,10);
    for(const query of [...queries,...named]) {
      for(const options of [{},{searchLimit:1,maxNodes:4},{searchLimit:5,maxNodes:30,traversalDepth:2},{nodeKinds:[]},{nodeKinds:['import','export','file']},{seedNames:['pinFeedIfNearBottom']},{edgeKinds:['calls'],nodeKinds:['function','method']},{maxNodes:0,searchLimit:0}]) tests.push({op:'find',query,options});
      for(const op of ['build','markdown','json','tree','paths']) tests.push({op,query,options:{searchLimit:4,maxNodes:25,traversalDepth:2,maxCodeBlockSize:70,...(op==='json'?{format:'json'}:{})}});
    }
    tests.push({op:'build',input:{title:'Checkout',description:'PaymentService'},options:{includeCode:false}});
    for(const id of [...s.nodes.slice(0,5).map(n=>n.id),'config','missing'])tests.push({op:'code',id});
    for(const bytes of [-0,0,1,1023,1024,1177.6,1280,1536,2355.2,1048576,1310720,104857600])tests.push({op:'bytes',bytes});
    for(const words of [[],['checkout'],['state','machine'],['services'],['profile','info'],['payment'],['search','shard'],['feed','bottom'],['MAX','RETRIES']]) {
      tests.push({op:'segments',words});
    }
    tests.push({op:'find',query:'payment checkout',deprioritized:['src/service.ts'],options:{searchLimit:5}});
    tests.push({op:'find',query:'payment checkout',projectTokens:['payment'],options:{searchLimit:5}});
    for(const t of tests) {
      q.setProjectNameTokens(new Set(t.projectTokens??[])); q.setDeprioritizedPathMatcher(p=>(t.deprioritized??[]).includes(p));
      const b=new ContextBuilder(dir,q,new GraphTraverser(q));const text=t.query??'';
      if(t.op==='find') {const g=await b.findRelevantContext(text,t.options); t.expected={nodes:[...g.nodes.keys()],edges:g.edges.map(e=>[e.source,e.target,e.kind,e.line??null,e.column??null]),roots:g.roots,confidence:g.confidence??null};}
      else if(t.op==='build') t.expected=JSON.parse(await b.buildContext(t.input??text,{...t.options,format:'json'}));
      else if(t.op==='markdown'||t.op==='json') t.expected=await b.buildContext(t.input??text,t.options);
      else if(t.op==='code') t.expected=await b.getCode(t.id);
      else if(t.op==='bytes') t.expected=formatBytes(t.bytes);
      else if(t.op==='segments') {const facade=Object.create(CodeGraph.prototype);facade.queries=q;t.expected=facade.getSegmentMatches(t.words,t.limit);}
      else {const g=await b.findRelevantContext(text,t.options);t.expected=t.op==='paths'?b.buildCallPathsSection(g):formatSubgraphTree(g,g.roots.map(id=>g.nodes.get(id)).filter(Boolean));}
    }
    db.close();activeDb=undefined;
    const output=spawnSync(binary,[dbPath,dir],{input:tests.map(({expected,...q})=>JSON.stringify(q)).join('\n')+'\n',encoding:'utf8',maxBuffer:128*1024*1024,windowsHide:true});
    if(output.status!==0)throw Error(`${s.name}: probe failed ${output.status}\n${output.stderr}`);
    const actual=output.stdout.trim().split('\n').map(l=>JSON.parse(l));
    if(actual.length!==tests.length)throw Error('wrong output count');
    for(let i=0;i<tests.length;i++) {
      if(!isDeepStrictEqual(clean(actual[i]),clean(tests[i].expected))) {
        fs.writeFileSync(path.join(root,'target/context-diff.json'),JSON.stringify({scenario:s.name,query:tests[i],actual:actual[i]},null,2));
        throw Error(`${s.name} case ${i}: ${JSON.stringify(tests[i]).slice(0,200)} differs; see target/context-diff.json`);
      }
    }
    console.log(`${s.name}: ${tests.length} results identical`);total+=tests.length;
    persisted.push({...s,cases:tests.map(({expected,...query})=>({query,expected}))});
  }
  if(process.argv.includes('--write')) {
    fs.mkdirSync(path.join(root,'crates/codegraph-context/tests/fixtures'),{recursive:true});
    fs.writeFileSync(path.join(root,'crates/codegraph-context/tests/fixtures/golden.json'),JSON.stringify(persisted)+'\n');
  }
  console.log(`${total} context results match the current TypeScript oracle.`);
} finally {activeDb?.close();fs.rmSync(work,{recursive:true,force:true,maxRetries:5,retryDelay:100});}
