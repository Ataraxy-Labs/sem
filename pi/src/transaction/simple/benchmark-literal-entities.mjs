// Read-only replay against base-commit source, not a solver or recall benchmark.
// Usage: SEM_TEST_BIN=... node benchmark-literal-entities.mjs ROOT TERM FILE...
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {ExactCode} from './exact-code.mjs';
const [root,term,...files]=process.argv.slice(2);
if(!root||!term||!files.length) throw new Error('Expected ROOT TERM FILE...');
const tmp=await fs.mkdtemp(path.join(os.tmpdir(),'sem-literal-replay-'));
try {
  let fileBytes=0;
  for(const file of files) {
    if(path.isAbsolute(file)||file.split('/').includes('..')) throw new Error('Invalid file');
    const bytes=execFileSync('git',['-C',root,'show',`HEAD:${file}`]);
    fileBytes+=bytes.length;
    await fs.mkdir(path.dirname(path.join(tmp,file)),{recursive:true});
    await fs.writeFile(path.join(tmp,file),bytes);
  }
  const api=new ExactCode({semBin:process.env.SEM_TEST_BIN||'sem'});
  const start=performance.now();
  const snapshot=await api.capture(tmp,files);
  const captured=performance.now();
  const candidates=api.query(snapshot.revision,[{contains:term}]);
  const result=api.query(snapshot.revision,[{contains:term,view:'source'}]);
  const queried=performance.now();
  const sourceBytes=result.sources.reduce((n,s)=>n+Buffer.byteLength(s.content||''),0);
  // Stronger native baseline than whole-file reads: one batched rg call with
  // twenty lines of context each side. Its coverage is lexical, not entity-complete.
  const nativeStart=performance.now();
  let native;
  try { native=execFileSync('rg',['--json','-F','-C','20','--',term,...files],{cwd:tmp}); }
  catch(error) { if(error.status!==1) throw error; native=error.stdout; }
  const nativeMs=performance.now()-nativeStart;
  let nativeSourceBytes=0;
  for(const line of native.toString().split('\n')) {
    if(!line) continue;
    const event=JSON.parse(line);
    if(event.type==='match'||event.type==='context') nativeSourceBytes+=Buffer.byteLength(event.data.lines.text??'');
  }
  console.log(JSON.stringify({files:files.length,term,file_bytes:fileBytes,entity_source_bytes:sourceBytes,
    candidate_response_bytes:Buffer.byteLength(JSON.stringify(candidates)),
    native_rg_context_source_bytes:nativeSourceBytes,native_rg_context_ms:nativeMs,
    response_bytes:Buffer.byteLength(JSON.stringify(result)),source_reduction:result.results[0].match_count?1-sourceBytes/fileBytes:null,
    capture_ms:captured-start,query_ms:queried-captured,
    matches:result.results[0].match_count,entities:result.sources.length,
    uncovered:result.results[0].uncovered.length,deferred:result.results[0].deferred.length,
    limitation:'Selected base-source retrieval replay, not session speed, task sufficiency or model token savings.'},null,2));
} finally {await fs.rm(tmp,{recursive:true,force:true});}
