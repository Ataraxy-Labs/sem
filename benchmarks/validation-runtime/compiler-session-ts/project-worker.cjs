// Experimental persistent, no-emit TypeScript project checker.
// Runs inside the offline validation container, serialized by bench-exec.
const fs=require('node:fs');
const path=require('node:path');
const crypto=require('node:crypto');
const net=require('node:net');
const {spawn,spawnSync}=require('node:child_process');
const {createRequire}=require('node:module');
const {performance}=require('node:perf_hooks');
const hash=text=>crypto.createHash('sha256').update(text).digest('hex');
let socket=process.env.SEM_TSC_SOCKET||`/tmp/sem-tsc-${process.getuid?.()||0}.sock`;
const states=new Map();
const {recorder,unchanged,resolutionsUnchanged}=require('./compiler-observations.cjs');
function check(project,fresh=false) {
 const start=performance.now(),root=path.dirname(project);
 const ts=createRequire(path.join(root,'package.json'))('typescript');
 const observed=recorder(ts.sys),sys=observed.sys;
 const raw=ts.readConfigFile(project,sys.readFile);
 const config=ts.parseJsonConfigFileContent(raw.config||{},sys,root,undefined,project);
 if(config.projectReferences?.length)throw new Error('Project references unsupported; use the normal project build');
 if(config.options.plugins?.length)throw new Error('Compiler plugins unsupported; use the normal project build');
 const options={...config.options,noEmit:true};
 const key=hash(JSON.stringify([project,ts.version,options]));
 let state=!fresh&&states.get(project);
 const reset=!state||state.key!==key;
 if(reset){states.delete(project);state={key,cache:new Map(),builder:undefined};}
 const rootKey=hash(JSON.stringify(config.fileNames));
 if(!fresh&&!reset&&state.rootKey===rootKey&&state.result&&unchanged(ts.sys,state.observations)){
  return {...state.result,compiler_ms:performance.now()-start,parsed:0,reused:0,reset:false,receipt_reused:true,changed_files:[]};
 }
 const host=ts.createIncrementalCompilerHost(options);let parsed=0,reused=0;
 for(const name of ['readFile','fileExists','directoryExists','getDirectories','readDirectory','realpath'])if(sys[name])host[name]=sys[name];
 const reads=new Map();
 host.getSourceFile=(file,languageVersion)=>{
  const text=sys.readFile(file);if(text===undefined)return undefined;
  const version=hash(text),cacheKey=file+'\0'+JSON.stringify(languageVersion);
  reads.set(file,version);
  const old=state.cache.get(cacheKey);
  if(!fresh&&old?.version===version){reused++;return old.source;}
  const source=ts.createSourceFile(file,text,languageVersion,true);source.version=version;parsed++;
  if(!fresh)state.cache.set(cacheKey,{source,version});
  return source;
 };
 const invalidateResolutions=reset||!resolutionsUnchanged(ts.sys,state.observations);
 host.hasInvalidatedResolutions=()=>invalidateResolutions;
 const builder=ts.createSemanticDiagnosticsBuilderProgram(config.fileNames,options,host,fresh?undefined:state.builder);
 const diagnostics=[...(raw.error?[raw.error]:[]),...config.errors,...ts.getPreEmitDiagnostics(builder)].map(d=>({
  code:d.code,category:ts.DiagnosticCategory[d.category],file:d.file?path.relative(root,d.file.fileName):null,
  start:d.start??null,length:d.length??null,message:ts.flattenDiagnosticMessageText(d.messageText,'\n')
 })).sort((a,b)=>JSON.stringify(a).localeCompare(JSON.stringify(b)));
 // The enclosing validation lock excludes cooperating source syncs. This
 // endpoint check additionally refuses a result if source drift is observed.
 const drift=[];for(const [file,digest] of reads){const text=ts.sys.readFile(file);if(text===undefined||hash(text)!==digest)drift.push(file);}
 if(drift.length) {states.delete(project);throw new Error('Source changed during compilation: '+drift.join(', '));}
 // Root additions/deletions are handled by the TypeScript builder itself.
 // Drop AST cache entries no longer requested by the current program.
 for(const cacheKey of state.cache.keys())if(!reads.has(cacheKey.split("\0")[0]))state.cache.delete(cacheKey);
 state.builder=builder;
 if(!fresh){if(states.size>=2&&!states.has(project))states.delete(states.keys().next().value);states.set(project,state);}
 const changedFiles=state.reads?[...new Set([...reads.keys(),...state.reads.keys()])].filter(file=>reads.get(file)!==state.reads.get(file)).map(file=>path.relative(root,file)):[];
 const result={receipt_reused:false,changed_files:changedFiles,status:diagnostics.some(d=>d.category==='Error')?'failed':'passed',project,compiler:ts.version,
  resolution_invalidated:invalidateResolutions,compiler_ms:performance.now()-start,worker_pid:process.pid,reset,fresh,parsed,reused,
  source_snapshot:hash(JSON.stringify([...reads].sort())),diagnostic_count:diagnostics.length,diagnostics,
  scope:'No-emit TypeScript diagnostics; behavioral tests and emit/build effects are not checked.'};
 // Conservatively retain observations from earlier builder epochs as well:
 // TypeScript may reuse a resolution without asking the host again.
 const combined=new Map(state.observations||[]);
 for(const [k,v] of observed.observations)combined.set(k,v);
 state.observations=combined;state.rootKey=rootKey;state.reads=reads;state.result=result;
 return result;
}
function publish(result) {
 const dir='/tmp/sem-tsc-evidence';fs.mkdirSync(dir,{recursive:true});
 const file=path.join(dir,crypto.randomUUID()+'.json');fs.writeFileSync(file,JSON.stringify(result));
 return {...result,diagnostics:result.diagnostics.slice(0,30),full_evidence:file};
}
if(process.argv[2]==='--server') {
 let queue=Promise.resolve();
 const server=net.createServer(connection=>{
  let input='';connection.setEncoding('utf8');
  connection.on('data',chunk=>{input+=chunk;if(!input.includes('\n'))return;
   const request=input.slice(0,input.indexOf('\n'));input='';
   queue=queue.then(()=>{try{const r=JSON.parse(request);connection.end(JSON.stringify({result:publish(check(r.project))})+'\n');}catch(e){connection.end(JSON.stringify({error:String(e.stack)})+'\n');}});
  });
 });
 server.listen(socket,()=>fs.chmodSync(socket,0o600));
 server.on('error',e=>{console.error(e);process.exit(2);});
} else {
 (async()=>{
  const args=process.argv.slice(2);let project='tsconfig.json',fresh=false,captureBaseline=false;
  for(let i=0;i<args.length;i++){
   if(args[i]==='-p'||args[i]==='--project'){if(!args[i+1])throw new Error('Missing project path');project=args[++i];}
   else if(args[i]==='--fresh')fresh=true;
   else if(args[i]==='--capture-baseline')captureBaseline=true;
   else throw new Error('Unsupported option '+args[i]);
  }
  project=path.resolve(project);if(fs.statSync(project).isDirectory())project=path.join(project,'tsconfig.json');
  const start=performance.now();
  // Separate project heaps: a base-revision check must not retain another
  // project's entire type checker in the same V8 heap.
  socket=process.env.SEM_TSC_SOCKET||`/tmp/sem-tsc-${process.getuid?.()||0}-${hash(project).slice(0,16)}.sock`;
  if(fresh){const result=publish(check(project,true));console.log(JSON.stringify({...result,wall_ms:performance.now()-start}));process.exitCode=result.status==='passed'?0:1;return;}
  const request=()=>new Promise((resolve,reject)=>{
   const client=net.createConnection(socket);let text='';
   client.setTimeout(600000,()=>client.destroy(new Error('compiler worker timeout')));
   client.on('connect',()=>client.write(JSON.stringify({project})+'\n'));
   client.on('data',b=>text+=b);client.on('error',reject);
   client.on('end',()=>{try{if(!text.trim()){const e=new Error('compiler worker exited without a result; inspect /tmp/sem-tsc-worker.log');e.code='WORKER_EXIT';throw e;}resolve(JSON.parse(text));}catch(e){reject(e);}});
  });
  let response;
  try {response=await request();}
  catch(e){
   if(['WORKER_EXIT','ECONNRESET','EPIPE'].includes(e.code)){
    // A no-emit check has no source mutations, so one fresh retry is safe.
    const fallback=spawnSync(process.execPath,['--max-old-space-size='+ (process.env.SEM_TSC_HEAP_MB||'8192'),__filename,'--fresh','-p',project],{encoding:'utf8',timeout:600000,maxBuffer:16*1024*1024});
    if(![0,1].includes(fallback.status))throw new Error('Fresh compiler recovery failed: '+fallback.stderr);
    const result=JSON.parse(fallback.stdout);
    console.log(JSON.stringify({...result,recovered_from_worker_exit:true,wall_ms:performance.now()-start}));process.exitCode=fallback.status;return;
   }
   if(!['ENOENT','ECONNREFUSED'].includes(e.code))throw e;
   try{fs.unlinkSync(socket);}catch{}
   const log=fs.openSync('/tmp/sem-tsc-worker.log','a');
   const heap=process.env.SEM_TSC_HEAP_MB||'8192';
   if(!/^[1-9][0-9]*$/.test(heap))throw new Error('SEM_TSC_HEAP_MB must be a positive integer');
   const child=spawn(process.execPath,['--max-old-space-size='+heap,__filename,'--server'],{detached:true,env:{...process.env,SEM_TSC_SOCKET:socket},stdio:['ignore',log,log]});child.unref();fs.closeSync(log);
   let ready=false;for(let i=0;i<100;i++){await new Promise(r=>setTimeout(r,50));if(fs.existsSync(socket)){ready=true;break;}}
   if(!ready)throw new Error('compiler worker failed to start; see /tmp/sem-tsc-worker.log');
   response=await request();
  }
  if(response.error)throw new Error(response.error);
  const baselineDir='/tmp/sem-tsc-baselines',baselineFile=path.join(baselineDir,hash(project)+'.json');
  const complete=JSON.parse(fs.readFileSync(response.result.full_evidence,'utf8'));
  if(captureBaseline){fs.mkdirSync(baselineDir,{recursive:true});fs.writeFileSync(baselineFile,JSON.stringify(complete));}
  if(fs.existsSync(baselineFile)){
   const baseline=JSON.parse(fs.readFileSync(baselineFile,'utf8'));
   const known=new Set(baseline.diagnostics.map(d=>JSON.stringify(d)));
   const matching=complete.diagnostics.filter(d=>known.has(JSON.stringify(d))).length;
   response.result.baseline={captured:captureBaseline,source_snapshot:baseline.source_snapshot,compiler:baseline.compiler,
    matching_diagnostic_count:matching,new_diagnostic_count:complete.diagnostics.length-matching,
    full_evidence:baselineFile,note:'Historical diagnostic comparison only; current failing status is preserved.'};
  }
  console.log(JSON.stringify({...response.result,wall_ms:performance.now()-start}));process.exitCode=response.result.status==='passed'?0:1;
 })().catch(e=>{console.error(JSON.stringify({status:'infrastructure_error',error:String(e.message)}));process.exitCode=2;});
}
