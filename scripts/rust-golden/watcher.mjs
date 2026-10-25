#!/usr/bin/env node
/** Clock-driven scheduler parity against the current TS FileWatcher. No OS mocks
 * substitute for the separate real-filesystem native tests.
 * cargo build -p codegraph-sync --example watcher_probe
 * node scripts/rust-golden/watcher.mjs target/debug/examples/watcher_probe.exe --write
 */
import {createRequire} from 'node:module';
import {spawnSync} from 'node:child_process';
import {isDeepStrictEqual} from 'node:util';
import {createHash} from 'node:crypto';
import * as fs from 'node:fs';import * as path from 'node:path';import * as os from 'node:os';
import {root,oracleRoot} from './check-upstream.mjs';
const require=createRequire(import.meta.url);
const {FileWatcher,LockUnavailableError}=require(path.join(oracleRoot,'dist/sync/watcher.js'));
const bin=path.resolve(process.argv[2]??'target/debug/examples/watcher_probe.exe');
const temp=fs.mkdtempSync(path.join(os.tmpdir(),'cg-watcher-model-'));
const saved={setTimeout:global.setTimeout,clearTimeout:global.clearTimeout,now:Date.now,noWatch:process.env.CODEGRAPH_NO_WATCH,force:process.env.CODEGRAPH_FORCE_WATCH};
let now=1000,watcher,resolveSync,rejectSync,inFlight,currentPaths;
const scenarios=[];let clock=1000;
function scenario(name,debounce,body){const actions=[{op:'new',debounce,now:clock},{op:'start',now:clock}];body(actions);scenarios.push({name,actions});clock+=100_000;}
const add=(a,op,extra={})=>a.push({op,now:clock+=20,...extra});
scenario('pending-and-mid-sync',2000,a=>{add(a,'event',{path:'a.ts'});add(a,'event',{path:'a.ts'});add(a,'event',{path:'b.ts'});add(a,'event',{path:'c.ts'});add(a,'begin');add(a,'event',{path:'a.ts'});add(a,'full');add(a,'finish');add(a,'begin');add(a,'full');add(a,'finish');add(a,'begin');add(a,'finish');add(a,'stop');});
scenario('scoped-ceiling',2000,a=>{for(let i=0;i<501;i++)add(a,'event',{path:`f${i}.ts`});add(a,'begin');add(a,'finish');});
for(const kind of ['lock','error'])scenario(`${kind}-retry-recovery`,25,a=>{add(a,'event',{path:'a.ts'});for(let i=0;i<6;i++){add(a,'begin');add(a,'finish',{result:kind});}add(a,'rearm');if(kind==='lock'){add(a,'begin');add(a,'finish');}add(a,'stop');});
scenario('full-failure-without-files',2000,a=>{add(a,'full');add(a,'begin');add(a,'finish',{result:'error'});add(a,'begin');add(a,'finish');});
let seed=0x76543210;const rand=n=>{seed^=seed<<13;seed^=seed>>>17;seed^=seed<<5;return(seed>>>0)%n;};
for(let i=0;i<24;i++)scenario(`mixed-${i}`,[25,100,300,2000][i%4],a=>{
    let running=true;
    for(let j=0;j<40;j++){
        if(!running){add(a,'start');running=true;}
        for(let k=0;k<1+rand(5);k++)add(a,'event',{path:`file${rand(8)}.ts`});
        if(rand(4)===0)add(a,'full');add(a,'begin');
        if(rand(3)===0)add(a,'event',{path:`file${rand(8)}.ts`});
        if(rand(5)===0)add(a,'full');
        const result=['ok','ok','lock','error'][rand(4)];add(a,'finish',{result});
        // Periodic clean completions keep this random arm below either terminal budget.
        if(j%3===0){add(a,'begin');add(a,'finish');}
        if(rand(13)===0){add(a,'stop');running=false;}
    }
});
const normalize=s=>JSON.parse(JSON.stringify(s));const all=[];
function compact(value){if(value.pendingFiles.length<=16)return value;const pending=value.pendingFiles;return {...value,pendingFiles:[...pending.slice(0,8),...pending.slice(-8)],pendingCount:pending.length,pendingSha256:createHash('sha256').update(JSON.stringify(pending)).digest('hex')};}
try{
    delete process.env.CODEGRAPH_NO_WATCH;process.env.CODEGRAPH_FORCE_WATCH='1';
    global.setTimeout=(_fn,delay)=>({at:now+delay});global.clearTimeout=()=>{};Date.now=()=>now;
    for(const scenario of scenarios){scenario.cases=[];
        for(const q of scenario.actions){now=q.now;
            switch(q.op){
                case'new':watcher?.stop();watcher=new FileWatcher(temp,paths=>{currentPaths=paths??null;return new Promise((resolve,reject)=>{resolveSync=resolve;rejectSync=reject;});},{inertForTests:true,debounceMs:q.debounce});inFlight=undefined;currentPaths=null;break;
                case'start':watcher.start();break;case'stop':watcher.stop();break;
                case'event':watcher.ingestEventForTests(q.path);break;
                case'full':watcher.needsFullScan=true;watcher.scheduleSync();break;
                case'begin':watcher.debounceTimer=null;if(!watcher.syncing&&!watcher.stopped){inFlight=watcher.flush();}break;
                case'finish':if(inFlight){if(q.result==='lock')rejectSync(new LockUnavailableError('busy'));else if(q.result==='error')rejectSync(new Error('sync failed'));else resolveSync({filesChanged:0,durationMs:0});await inFlight;inFlight=undefined;}break;
                case'rearm':watcher.rearmAfterLockContention();break;
            }
            const expected=normalize({active:watcher.isActive(),ready:watcher.ready,syncing:watcher.syncing,recovering:watcher.isRecoveringFromLock(),degradedReason:watcher.getDegradedReason(),pendingFiles:watcher.getPendingFiles(),needsFullScan:watcher.needsFullScan,deadline:watcher.debounceTimer?.at??null,
                paths:watcher.syncing?((!watcher.needsFullScan&&watcher.pendingFiles.size>0&&watcher.pendingFiles.size<=500)?[...watcher.pendingFiles.keys()]:null):null});
            // Capture the actual argument supplied to syncFn, not the pending set after mid-sync events.
            expected.paths=watcher.syncing?currentPaths:null;
            scenario.cases.push({query:q,expected});all.push({q,expected,name:scenario.name});
        }
    }
    const probe=spawnSync(bin,[],{input:all.map(x=>JSON.stringify(x.q)).join('\n')+'\n',encoding:'utf8',maxBuffer:64<<20,windowsHide:true});
    if(probe.status!==0)throw Error(`${probe.status}: ${probe.stderr}`);
    const actual=probe.stdout.trim().split('\n').map(s=>JSON.parse(s));if(actual.length!==all.length)throw Error('wrong output count');
    for(let i=0;i<actual.length;i++)if(!isDeepStrictEqual(actual[i],all[i].expected)){
        fs.writeFileSync(path.join(root,'target/watcher-diff.json'),JSON.stringify({i,...all[i],actual:actual[i]},null,2));throw Error(`watcher parity mismatch at ${i} (${all[i].name}); see target/watcher-diff.json`);
    }
    if(process.argv.includes('--write')){const dir=path.join(root,'crates/codegraph-sync/tests/fixtures');fs.mkdirSync(dir,{recursive:true});fs.writeFileSync(path.join(dir,'watcher.json'),JSON.stringify(scenarios.map(({name,cases})=>({name,cases:cases.map(({query,expected})=>({query,expected:compact(expected)}))})))+'\n');}
    console.log(`${actual.length} scheduler states match the current TypeScript watcher.`);
}finally{watcher?.stop();global.setTimeout=saved.setTimeout;global.clearTimeout=saved.clearTimeout;Date.now=saved.now;
    for(const [key,value]of [['CODEGRAPH_NO_WATCH',saved.noWatch],['CODEGRAPH_FORCE_WATCH',saved.force]]){if(value===undefined)delete process.env[key];else process.env[key]=value;}
    fs.rmSync(temp,{recursive:true,force:true,maxRetries:5,retryDelay:100});}
