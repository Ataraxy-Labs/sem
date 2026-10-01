import fs from 'node:fs/promises';
import {readFileSync,writeFileSync,realpathSync} from 'node:fs';
import path from 'node:path';
import {execFile} from 'node:child_process';
import {withFileMutationQueue} from '@earendil-works/pi-coding-agent';

export function gofmt(source) {
  return new Promise((resolve,reject)=>{
    const child=execFile('gofmt',[],{encoding:'buffer',timeout:5000,maxBuffer:8*1024*1024},
      (error,stdout,stderr)=>error?reject(Error('gofmt failed: '+stderr.toString().slice(0,1000))):resolve(stdout));
    child.stdin.on('error',()=>{}); // Process callback reports early exit/EPIPE.
    child.stdin.end(source);
  });
}

// Explicit paths only. Formatting is not semantic validation or repo isolation.
export async function formatFiles(cwd,files,{format=gofmt}={}) {
  if(!Array.isArray(files)||!files.length||files.length>64)throw Error('INVALID_FORMAT_SCOPE');
  const root=await fs.realpath(cwd),plans=[];let bytes=0;
  for(const file of new Set(files)) {
    if(typeof file!=='string'||path.isAbsolute(file)||file.split('/').some(p=>!p||p==='.'||p==='..')||!file.endsWith('.go'))
      throw Error('FORMAT_REQUIRES_EXPLICIT_GO_FILES');
    const absolute=path.join(root,file);
    if(await fs.realpath(absolute)!==absolute)throw Error('FORMAT_SYMLINK_UNSUPPORTED');
    const stat=await fs.stat(absolute);
    if(!stat.isFile()||stat.size>4*1024*1024-bytes)throw Error('FORMAT_SCOPE_TOO_LARGE');
    const before=await fs.readFile(absolute);bytes+=before.length;
    if(bytes>4*1024*1024)throw Error('FORMAT_SCOPE_TOO_LARGE');
    const after=await format(before);
    if(!Buffer.isBuffer(after))throw Error('INVALID_FORMATTER_OUTPUT');
    plans.push({file,absolute,before,after});
  }
  const written=[];
  try {
    for(const p of plans)await withFileMutationQueue(p.absolute,async()=>{
      // Sync check/write narrows, but cannot eliminate, cross-process races.
      if(realpathSync(p.absolute)!==p.absolute||!readFileSync(p.absolute).equals(p.before))throw Error('FORMAT_SOURCE_CHANGED: '+p.file);
      if(!p.before.equals(p.after)){writeFileSync(p.absolute,p.after);written.push(p);}
    });
  }catch(error) {
    const restored=[],unrestored=[];
    for(const p of written.reverse())await withFileMutationQueue(p.absolute,async()=>{
      try {
        if(realpathSync(p.absolute)!==p.absolute||!readFileSync(p.absolute).equals(p.after))throw Error('changed');
        writeFileSync(p.absolute,p.before);restored.push(p.file);
      }catch {unrestored.push(p.file);}
    });
    return {ok:false,error:error.message,restored,unrestored,validation:'not_run'};
  }
  return {ok:true,formatter:'gofmt',files:plans.map(p=>p.file),changed:written.map(p=>p.file),
    validation:'not_run',consistency:'per_file_guards; compensating_rollback; not_repository_isolation'};
}
