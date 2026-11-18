#!/usr/bin/env node
/** Compare git-hook bytes, results and worktree diagnostics on real temp repos.
 * cargo build -p codegraph-sync --example sync_probe
 * node scripts/rust-golden/sync.mjs target/debug/examples/sync_probe.exe
 */
import { createRequire } from 'node:module';
import { execFileSync,spawnSync } from 'node:child_process';
import { isDeepStrictEqual } from 'node:util';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { oracleRoot } from './check-upstream.mjs';
const require=createRequire(import.meta.url);
const hooks=require(path.join(oracleRoot,'dist/sync/git-hooks.js'));
const wt=require(path.join(oracleRoot,'dist/sync/worktree.js'));
const bin=path.resolve(process.argv[2]??'target/debug/examples/sync_probe.exe');
// Build the fixture under the long spelling of the temp root. Windows reports `TEMP` as an 8.3
// alias (`ADMINI~1`), and git answers with whatever spelling the directory is reached by, so a
// root under the alias mixed spellings between the two sides. Native's own worktree tests build
// their fixture the same way (`real_path(temp_dir)` in `tests/worktree_spelling.rs`); the 8.3
// case itself is covered there, not here.
const tempRoot=fs.realpathSync.native(os.tmpdir());
const work=fs.mkdtempSync(path.join(tempRoot,'cg-native-git-'));
const root=path.join(work,"repo's space");fs.mkdirSync(root);
const git=(dir,args)=>execFileSync('git',args,{cwd:dir,encoding:'utf8',windowsHide:true,timeout:5000,stdio:['ignore','pipe','pipe']}).trim();
const init=dir=>{fs.mkdirSync(dir,{recursive:true});git(dir,['init','-q','--template=']);git(dir,['-c','user.name=Fixture','-c','user.email=fixture@example.invalid','-c','core.hooksPath=','-c','commit.gpgSign=false','commit','-q','--allow-empty','-m','fixture']);};
const names=['post-commit','post-merge','post-checkout'];
function runTs(q) {
  switch(q.op) {
    case'install':return hooks.installGitSyncHook(q.root,q.hooks);
    case'remove':return hooks.removeGitSyncHook(q.root,q.hooks);
    case'is':return hooks.isSyncHookInstalled(q.root,q.hooks);
    case'repo':return hooks.isGitRepo(q.root);
    case'worktree':return wt.gitWorktreeRoot(q.root);
    case'common':return wt.gitCommonDir(q.root);
    case'mismatch':return wt.detectWorktreeIndexMismatch(q.root,q.index);
    case'nested':return wt.nestedRepositoryBelow(q.root,q.index);
    case'warning':return wt.worktreeMismatchWarning({worktreeRoot:q.root,indexRoot:q.index});
    case'notice':return wt.worktreeMismatchNotice({worktreeRoot:q.root,indexRoot:q.index});
  }
  throw Error(q.op);
}
function rust(queries) {
  const p=spawnSync(bin,[],{input:queries.map(q=>JSON.stringify(q)).join('\n')+'\n',encoding:'utf8',maxBuffer:16<<20,windowsHide:true});
  if(p.status!==0)throw Error(`${p.status}: ${p.stderr}`);
  return p.stdout.trim().split('\n').map(s=>JSON.parse(s));
}
const snapshot=dir=>Object.fromEntries(names.map(h=>{const p=path.join(dir,h);return[h,fs.existsSync(p)?fs.readFileSync(p,'utf8'):null];}));
function reset(dir,body) {fs.mkdirSync(dir,{recursive:true});for(const h of names){const p=path.join(dir,h);if(body===null)fs.rmSync(p,{force:true});else fs.writeFileSync(p,body);}}
// The native product deliberately uses its own name, and its one-line notice (always read by an
// agent) says the user can index, in the phrasing its longer warning uses. Only those spellings
// are normalized; every count, path, verdict and ordering is compared exactly. Both advice forms
// collapse to one token first, so the old-name pass cannot split them.
// Every spelling of the advice sentence, including the reference's own (which still names the old
// command as `codegraph init -i`), so all collapse to one token before names are rewritten.
const ADVICE=[
  'Run "codegraphV init" in this worktree for a worktree-local index.',
  'The user can run "codegraphV init" in this worktree for a worktree-local index.',
  'Run "codegraphV init" here for a worktree-local index.',
  'Run "codegraph init -i" in this worktree for a worktree-local index.',
  'The user can run "codegraph init -i" in this worktree for a worktree-local index.',
  'Run "codegraph init -i" here for a worktree-local index.',
];
const INIT_TOKEN='@@INIT@@';
const brand=value=>{
  if(typeof value==='string'){
    let text=value;
    for(const phrase of ADVICE) text=text.split(phrase).join(INIT_TOKEN);
    text=text.replace(/\bcodegraph init -i\b/g,'codegraphV init').replace(/\bCodeGraph(?!V)\b/g,'CodeGraphV');
    return text.split(INIT_TOKEN).join(ADVICE[0]);
  }
  if(Array.isArray(value))return value.map(brand);
  if(value&&typeof value==='object')return Object.fromEntries(Object.entries(value).map(([k,v])=>[k,brand(v)]));
  return value;
};

function same(actual,expected,label) {
  const a=brand(actual),e=brand(expected);
  if(isDeepStrictEqual(a,e))return;
  if(Array.isArray(a)&&Array.isArray(e)){
    for(let i=0;i<Math.max(a.length,e.length);i++){
      if(!isDeepStrictEqual(a[i],e[i])){
        console.error(`${label}: FIRST DIFF at ${i}`);
        console.error(`  actual  : ${JSON.stringify(a[i])}`);
        console.error(`  expected: ${JSON.stringify(e[i])}`);
        break;
      }
    }
  }
  throw Error(`${label}\nactual: ${JSON.stringify(a)}\nexpected: ${JSON.stringify(e)}`);
}
let count=0;
try {
  init(root);const a=path.join(root,'a'),b=path.join(root,'b');fs.mkdirSync(a);fs.mkdirSync(b);
  const hookDir=path.join(root,'.git/hooks');const custom=path.join(root,'custom-hooks');
  for(const dir of [hookDir,custom]) {
    git(root,['config','core.hooksPath',dir]);
    const bodies=[null,'#!/bin/sh\necho user\nexit 0\n','echo no-shebang\n','#!/bin/sh\n','#!/bin/sh\r\n\r\necho user\r\n',
      '#!/usr/bin/env python3\nprint("user")\n','#!/usr/bin/env -S bash\necho user\n',
      '#!/bin/sh\n# >>> codegraph sync hook >>>\ncodegraph sync\n# <<< codegraph sync hook <<<\n',
      '#!/bin/sh\n# >>> codegraph sync hook >>>\n# codegraph-root: /someone-else\ncodegraph sync\n# <<< codegraph sync hook <<<\n'];
    for(const body of bodies) {
      const queries=[{op:'repo',root},{op:'is',root},{op:'is',root:a},{op:'install',root:a},{op:'install',root:b,hooks:['post-commit']},
        {op:'install',root:a},{op:'is',root:b},{op:'remove',root:a},{op:'is',root:a},{op:'is',root:b},
        {op:'install',root},{op:'remove',root:b},{op:'remove',root},{op:'is',root}];
      reset(dir,body);const expected=queries.map(runTs);const ts=snapshot(dir);
      reset(dir,body);same(rust(queries),expected,`hook results ${body}`);same(snapshot(dir),ts,`hook contents ${body}`);count+=queries.length+names.length;
    }
  }
  const plain=path.join(work,'plain');fs.mkdirSync(plain);const linked=path.join(root,'nested/linked');
  git(root,['-c','core.hooksPath=','worktree','add','-q','-b','fixture-linked',linked]);
  const embedded=path.join(root,'vendor/service');init(embedded);
  const qs=[];
  const paths=[root,a,linked,embedded,plain,embedded+'/new/file.ts'];
  paths.push(...paths.map(p=>fs.existsSync(p)?fs.realpathSync.native(p):p));
  for(const p of paths) {
    for(const op of ['repo','worktree','common'])qs.push({op,root:p});
    for(const index of [root,a,linked,embedded,plain])for(const op of ['mismatch','nested'])qs.push({op,root:p,index});
  }
  for(const op of ['install','remove','is'])qs.push({op,root:plain});
  qs.push({op:'install',root:root+'/bad\npath'});
  qs.push({op:'warning',root:linked,index:root},{op:'notice',root:linked,index:root});
  same(rust(qs),qs.map(runTs),'worktree diagnostics');count+=qs.length;
  console.log(`${count} git hook/worktree results and file snapshots match the current TypeScript oracle.`);
} finally {fs.rmSync(work,{recursive:true,force:true,maxRetries:5,retryDelay:100});}
