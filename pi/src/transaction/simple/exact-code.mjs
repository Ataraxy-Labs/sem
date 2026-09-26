import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import {createHash} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {qualifyEntities, indexEntities, lookupEntities} from './entity-lookup.mjs';
const exec = promisify(execFile);
const hash = x => createHash('sha256').update(x).digest('hex');
const fail = code => { throw new Error(code); };
const order = (a,b) => a < b ? -1 : a > b ? 1 : 0;

export function compactSources(sources) {
  const output=sources.map(s=>({...s}));
  const ordered=[...output].sort((a,b)=>(b.entity.end-b.entity.start)-(a.entity.end-a.entity.start)||order(a.entity.id,b.entity.id));
  const full=[];
  for(const source of ordered) {
    const e=source.entity;
    if(typeof source.content!=='string') continue;
    const parent=full.find(p=>p.entity.file===e.file&&p.entity.start<=e.start&&p.entity.end>=e.end);
    if(parent) {
      const ref={source_id:parent.entity.id,start_byte:e.start-parent.entity.start,end_byte:e.end-parent.entity.start};
      const bytes=Buffer.from(parent.content).subarray(ref.start_byte,ref.end_byte);
      if(hash(bytes)===source.sha256&&JSON.stringify(ref).length+32<Buffer.byteLength(source.content)) {
        delete source.content;
        source.content_source={kind:'same_response_utf8_slice',...ref};
        continue;
      }
    }
    full.push(source);
  }
  return output;
}

// Session-local immutable, explicitly scoped snapshots. Not a whole-repo revision.
export class ExactCode {
  constructor({semBin='sem', maxBytes=4*1024*1024, maxSnapshots=8}={}) {
    Object.assign(this,{semBin,maxBytes,maxSnapshots}); this.snapshots=new Map();
  }
  async capture(cwd, files, {allowMissing=false}={}) {
    if(!Array.isArray(files)||!files.length||files.length>64) fail('INVALID_FILE_SCOPE');
    const root=await fs.realpath(cwd), sources=new Map();
    let total=0;
    const missing=[];
    for(const file of [...new Set(files)].sort(order)) {
      if(typeof file!=='string'||!file||path.isAbsolute(file)||file.split('/').some(x=>!x||x==='.'||x==='..')) fail('INVALID_PATH');
      const absolute=path.join(root,file);
      let stat;
      try {
        if(await fs.realpath(absolute)!==absolute) fail('SYMLINK_NOT_SUPPORTED');
        stat=await fs.stat(absolute);
      } catch(error) {
        if(allowMissing&&error.code==='ENOENT') {missing.push(file);continue;}
        throw error;
      }
      if(!stat.isFile()||stat.size>this.maxBytes-total) fail('SCOPE_TOO_LARGE');
      const bytes=await fs.readFile(absolute); total+=bytes.length;
      if(total>this.maxBytes) fail('SCOPE_TOO_LARGE');
      new TextDecoder('utf-8',{fatal:true}).decode(bytes);
      sources.set(file,bytes);
    }
    const manifest=[...sources].map(([file,b])=>({file,sha256:hash(b)}));
    const revision=hash(JSON.stringify([manifest,missing]));
    if(!this.snapshots.has(revision)) {
      if(this.snapshots.size>=this.maxSnapshots) fail('SNAPSHOT_CAPACITY_REACHED');
      const tmp=await fs.mkdtemp(path.join(os.tmpdir(),'sem-exact-'));
      const entities=[];
      try {
        for(const [file,bytes] of sources) {
          const target=path.join(tmp,file); await fs.mkdir(path.dirname(target),{recursive:true});
          await fs.writeFile(target,bytes);
          const {stdout}=await exec(this.semBin,['entities',target,'--json','--no-default-excludes'],{maxBuffer:8*1024*1024,timeout:30000});
          const rows=JSON.parse(stdout);
          if(!Array.isArray(rows)) fail('INVALID_PARSER_RESPONSE');
          for(const row of rows) {
            const {name,type,start_byte:start,end_byte:end}=row;
            if(typeof name!=='string'||typeof type!=='string'||!Number.isInteger(start)||!Number.isInteger(end)||start<0||end<start||end>bytes.length) fail('INVALID_ENTITY_RANGE');
            const item={file,name,type,start,end};
            entities.push({...item,id:hash(JSON.stringify([revision,item]))});
          }
        }
      } finally { await fs.rm(tmp,{recursive:true,force:true}); }
      entities.sort((a,b)=>order(a.file,b.file)||a.start-b.start||a.end-b.end||order(a.id,b.id));
      // Lexical containment only, not receiver/type or runtime resolution.
      qualifyEntities(entities);
      this.snapshots.set(revision,{sources,entities,manifest,...indexEntities(entities)});
    }
    return {revision,files:manifest,missing_files:missing,scope:'explicit_files',coverage:'parser_reported_only',consistency:'captured_file_bytes_not_atomic_repository_snapshot'};
  }
  get(revision) {return this.snapshots.get(revision)??fail('UNKNOWN_SNAPSHOT');}
  resolve(revision,name) {
    if(typeof name!=='string'||!name) fail('INVALID_NAME');
    const matches=lookupEntities(this.get(revision),{name});
    return {revision,status:matches.length===0?'not_found':matches.length===1?'unique':'ambiguous',matches,coverage:'parser_reported_only'};
  }
  read(revision,id) {
    const s=this.get(revision), entity=s.byId.get(id);
    if(!entity) fail('UNKNOWN_ENTITY');
    const bytes=s.sources.get(entity.file).subarray(entity.start,entity.end);
    return {revision,entity,sha256:hash(bytes),content:new TextDecoder('utf-8',{fatal:true}).decode(bytes),complete:true};
  }
  // Resolve and hydrate explicitly requested symbols in one deterministic call.
  // Ambiguous selectors return locators, never an arbitrary chosen body.
  query(revision,selectors) {
    if(!Array.isArray(selectors)||!selectors.length||selectors.length>64) fail('INVALID_SELECTORS');
    const s=this.get(revision), sources=new Map(), files=new Map();
    let fileBudget=48000;
    const results=selectors.map(selector=>{
      if(selector && typeof selector==='object' && Object.keys(selector).length===1 && typeof selector.file==='string' && selector.file) {
        const bytes=s.sources.get(selector.file);
        if(!bytes) return {selector,status:'not_found'};
        if(!files.has(selector.file)) {
          if(bytes.length>fileBudget) return {selector,status:'deferred',bytes:bytes.length,reason:'FILE_READ_BUDGET',next_action:'Select named entities or use bounded file/range reads; no source was silently truncated.'};
          files.set(selector.file,{file:selector.file,sha256:hash(bytes),content:bytes.toString('utf8'),complete:true});
          fileBudget-=bytes.length;
        }
        return {selector,status:'unique',file:selector.file};
      }
      if(!selector||typeof selector!=='object'||Array.isArray(selector)||
         Object.keys(selector).some(k=>!['id','name','file','type'].includes(k))||
         (typeof selector.id==='string')===(typeof selector.name==='string')||
         Object.values(selector).some(v=>typeof v!=='string'||!v)) fail('INVALID_SELECTOR');
      const matches=lookupEntities(s,selector);
      const status=matches.length===0?'not_found':matches.length===1?'unique':'ambiguous';
      if(status==='unique') {
        const e=matches[0];
        if(!sources.has(e.id)) {
          const {revision:unused,...source}=this.read(revision,e.id);
          sources.set(e.id,source);
        }
        return {selector,status,source_id:e.id};
      }
      return {selector,status,matches};
    });
    // Reuse containing source only within this response. No assumption that a
    // previous tool response is still in the model's context.
    return {revision,results,sources:compactSources([...sources.values()]),...(files.size?{files:[...files.values()]}:{}),coverage:'parser_reported_only'};
  }
  prepare(revision,edits) {
    if(!Array.isArray(edits)||!edits.length||edits.length>64) fail('INVALID_EDITS');
    const s=this.get(revision), byFile=new Map();
    for(const {id,content} of edits) {
      const e=s.byId.get(id);
      if(!e||typeof content!=='string') fail('INVALID_EDIT');
      const group=byFile.get(e.file)||[]; group.push({...e,content});byFile.set(e.file,group);
    }
    const files=[];
    for(const [file,group] of [...byFile].sort(([a],[b])=>order(a,b))) {
      group.sort((a,b)=>a.start-b.start||a.end-b.end);
      for(let i=1;i<group.length;i++) if(group[i].start<group[i-1].end||group[i].start===group[i-1].start) fail('OVERLAPPING_EDITS');
      const original=s.sources.get(file);let output=original;
      for(const e of group.toReversed()) output=Buffer.concat([output.subarray(0,e.start),Buffer.from(e.content),output.subarray(e.end)]);
      files.push({file,expected_sha256:hash(original),result_sha256:hash(output),content:output.toString('utf8')});
    }
    if(files.reduce((n,f)=>n+Buffer.byteLength(f.content),0)>this.maxBytes) fail('RESULT_TOO_LARGE');
    return {revision,status:'prepared_not_applied',files,validation:'not_run'};
  }
}
