import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import {createHash} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {qualifyEntities, indexEntities, lookupEntities} from './entity-lookup.mjs';
import {ParseCache} from './parse-cache.mjs';
import {requireExactPath} from './exact-path.mjs';
import {parallelMap} from './parallel-map.mjs';
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
  constructor({semBin='sem', maxBytes=4*1024*1024, maxSnapshots=8,parseCacheOptions,parseConcurrency=4}={}) {
    if(!Number.isSafeInteger(maxSnapshots)||maxSnapshots<1) fail('INVALID_SNAPSHOT_CAPACITY');
    if(!Number.isSafeInteger(parseConcurrency)||parseConcurrency<1||parseConcurrency>16) fail('INVALID_PARSE_CONCURRENCY');
    Object.assign(this,{semBin,maxBytes,maxSnapshots,parseConcurrency}); this.snapshots=new Map();
    this.parseCache=new ParseCache(parseCacheOptions);
  }
  async parseEntityRows(target) {
    const {stdout}=await exec(this.semBin,['entities',target,'--json','--no-default-excludes'],{maxBuffer:8*1024*1024,timeout:30000});
    const rows=JSON.parse(stdout);
    if(!Array.isArray(rows))return rows;
    return rows.map(row=>{
      // Normalize parser-owned local parent IDs before caching. Temporary
      // extraction paths must never become persistent identities or aliases.
      const prefix=target+'::';
      if(typeof row.parent_id!=='string'||!row.parent_id.startsWith(prefix))return row;
      const local=row.parent_id.slice(prefix.length), split=local.indexOf('::');
      if(split<=0||split===local.length-2)return row;
      return {...row,parent_type:local.slice(0,split),parent_name:local.slice(split+2)};
    });
  }
  async capture(cwd, files, {allowMissing=false,partialReads=false}={}) {
    if(!Array.isArray(files)||!files.length||files.length>64) fail('INVALID_FILE_SCOPE');
    const root=await fs.realpath(cwd), sources=new Map();
    let total=0;
    const missing=[];
    const fileErrors=[];
    for(const file of [...new Set(files)].sort(order)) {
      if(typeof file!=='string'||!file||path.isAbsolute(file)||file.split('/').some(x=>!x||x==='.'||x==='..')) fail('INVALID_PATH');
      const absolute=path.join(root,file);
      let stat;
      try {
        await requireExactPath(root,file);
        stat=await fs.stat(absolute);
      } catch(error) {
        if(allowMissing&&error.code==='ENOENT') {missing.push(file);continue;}
        const code=error.message?.split(':')[0];
        // Read-only callers can keep independent results. Never follow an alias,
        // weaken edit validation, or swallow parser/resource/internal failures.
        if(partialReads&&['SYMLINK_NOT_SUPPORTED','PATH_CANONICAL_MISMATCH','PATH_OUTSIDE_REPOSITORY'].includes(code)) {
          fileErrors.push({file,code,message:error.message});continue;
        }
        throw error;
      }
      if(!stat.isFile()||stat.size>this.maxBytes-total) fail('SCOPE_TOO_LARGE');
      const bytes=await fs.readFile(absolute); total+=bytes.length;
      if(total>this.maxBytes) fail('SCOPE_TOO_LARGE');
      new TextDecoder('utf-8',{fatal:true}).decode(bytes);
      sources.set(file,bytes);
    }
    const manifest=[...sources].map(([file,b])=>({file,sha256:hash(b)}));
    const revision=hash(JSON.stringify(fileErrors.length?[manifest,missing,fileErrors]:[manifest,missing]));
    const parsing={cache_hits:0,parser_calls:0,snapshot_reused:this.snapshots.has(revision)};
    if(!this.snapshots.has(revision)) {
      let tmpPromise;
      const entities=[];
      try {
        const groups=await parallelMap([...sources],this.parseConcurrency,async([file,bytes])=>{
          const key=JSON.stringify([this.semBin,root,file,hash(bytes)]);
          let rows=this.parseCache.get(key);
          if(rows===undefined) {
            tmpPromise??=fs.mkdtemp(path.join(os.tmpdir(),'sem-exact-'));
            const target=path.join(await tmpPromise,file);await fs.mkdir(path.dirname(target),{recursive:true});
            await fs.writeFile(target,bytes);
            rows=await this.parseEntityRows(target);parsing.parser_calls++;
          } else parsing.cache_hits++;
          if(!Array.isArray(rows)) fail('INVALID_PARSER_RESPONSE');
          const validated=[];
          const fileEntities=[];
          for(const row of rows) {
            const {name,type,start_byte:start,end_byte:end}=row;
            if(typeof name!=='string'||typeof type!=='string'||!Number.isInteger(start)||!Number.isInteger(end)||start<0||end<start||end>bytes.length) fail('INVALID_ENTITY_RANGE');
            const parent=typeof row.parent_name==='string'&&row.parent_name&&typeof row.parent_type==='string'
              ?{parent_name:row.parent_name,parent_type:row.parent_type,declared_qualified_name:row.parent_name+'.'+name}:{};
            const item={file,name,type,start,end,...parent};
            fileEntities.push({...item,id:hash(JSON.stringify([revision,item]))});
            validated.push({name,type,start_byte:start,end_byte:end,...parent});
          }
          this.parseCache.set(key,validated);
          return fileEntities;
        });
        entities.push(...groups.flat());
      } finally { if(tmpPromise)await fs.rm(await tmpPromise,{recursive:true,force:true}); }
      entities.sort((a,b)=>order(a.file,b.file)||a.start-b.start||a.end-b.end||order(a.id,b.id));
      // Lexical containment only, not receiver/type or runtime resolution.
      qualifyEntities(entities);
      this.snapshots.set(revision,{sources,entities,manifest,fileErrors,...indexEntities(entities)});
      // Evict only after a successful capture. Failed parsing must not destroy
      // usable snapshots. IDs remain revision-bound, never redirected.
      while(this.snapshots.size>this.maxSnapshots) this.snapshots.delete(this.snapshots.keys().next().value);
    }
    this.get(revision);
    this.lastCaptureParsing=parsing;
    return {revision,files:manifest,missing_files:missing,...(fileErrors.length?{file_errors:fileErrors,complete:false}:{}),scope:'explicit_files',coverage:'parser_reported_only',consistency:'captured_file_bytes_not_atomic_repository_snapshot'};
  }
  get(revision) {
    const snapshot=this.snapshots.get(revision);
    if(!snapshot) fail('UNKNOWN_SNAPSHOT: missing or expired; recapture files and use returned revision and entity IDs');
    this.snapshots.delete(revision);
    this.snapshots.set(revision,snapshot);
    return snapshot;
  }
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
    const s=this.get(revision), sources=new Map(), files=new Map(), ranges=new Map();
    let fileBudget=48000;
    const results=selectors.map(selector=>{
      try {
      const unavailable=s.fileErrors?.find(error=>error.file===selector?.file);
      if(unavailable) return {selector,status:'error',error:unavailable,complete:false};
      if(selector && (Object.hasOwn(selector,'start_byte')||Object.hasOwn(selector,'end_byte'))) {
        const {file,start_byte:start,end_byte:end}=selector;
        if(Object.keys(selector).some(k=>!['file','start_byte','end_byte'].includes(k))||
          typeof file!=='string'||!file||!Number.isSafeInteger(start)||!Number.isSafeInteger(end)||start<0||end<=start) fail('INVALID_SELECTOR');
        const bytes=s.sources.get(file);
        if(!bytes) return {selector,status:'not_found'};
        if(end>bytes.length) return {selector,status:'error',error:{code:'INVALID_SOURCE_RANGE',byte_length:bytes.length},complete:false};
        const key=JSON.stringify([file,start,end]);
        if(!ranges.has(key)) {
          if(end-start>fileBudget) return {selector,status:'deferred',reason:'SOURCE_BUDGET'};
          // Reject mid-codepoint ranges; do not silently corrupt source.
          let content;
          try {content=new TextDecoder('utf-8',{fatal:true}).decode(bytes.subarray(start,end));}
          catch {return {selector,status:'error',error:{code:'INVALID_UTF8_RANGE',byte_length:bytes.length},complete:false};}
          ranges.set(key,{file,start_byte:start,end_byte:end,content,complete:true,scope:'requested_range_only'});
          fileBudget-=end-start;
        }
        return {selector,status:'unique',range_key:key};
      }
      // Literal discovery over captured bytes, immediately hydrated to the
      // smallest enclosing entities. This is lexical containment, not a call graph.
      if(selector && Object.hasOwn(selector,'contains')) {
        if(Object.keys(selector).some(k=>!['contains','file','view','offset'].includes(k)) ||
           typeof selector.contains!=='string'||!selector.contains ||
           (selector.view!==undefined&&!['candidates','source'].includes(selector.view)) ||
           (selector.offset!==undefined&&(!Number.isSafeInteger(selector.offset)||selector.offset<0)) ||
           (selector.file!==undefined&&(typeof selector.file!=='string'||!selector.file))) fail('INVALID_SELECTOR');
        const needle=Buffer.from(selector.contains), ids=new Set(), uncovered=[];
        const evidence=new Map();
        let matchCount=0;
        for(const [file,bytes] of s.sources) {
          if(selector.file && selector.file!==file) continue;
          const entities=s.entities.filter(e=>e.file===file);
          for(let start=bytes.indexOf(needle);start!==-1;start=bytes.indexOf(needle,start+1)) {
            matchCount++;
            const enclosing=entities.filter(e=>e.start<=start&&e.end>=start+needle.length);
            const smallest=Math.min(...enclosing.map(e=>e.end-e.start));
            const preview=bytes.subarray(Math.max(0,bytes.lastIndexOf(10,start)+1),
              bytes.indexOf(10,start)===-1?bytes.length:bytes.indexOf(10,start)).toString('utf8').slice(0,180);
            if(!enclosing.length) uncovered.push({file,start_byte:start,end_byte:start+needle.length,preview});
            for(const e of enclosing) if(e.end-e.start===smallest) {
              ids.add(e.id);
              const previous=evidence.get(e.id);
              evidence.set(e.id,{match_count:(previous?.match_count??0)+1,preview:previous?.preview??preview});
            }
          }
        }
        // Explicit response shape, not a heuristic about how much context an
        // unknown task needs. Page entities and uncovered matches together.
        const inventory=[...[...ids].map(id=>({kind:'entity',id})),...uncovered.map(hit=>({kind:'uncovered',...hit}))];
        const offset=selector.offset??0, page=inventory.slice(offset,offset+100);
        const pageIds=page.filter(x=>x.kind==='entity').map(x=>x.id);
        const pageUncovered=page.filter(x=>x.kind==='uncovered').map(({kind,...hit})=>hit);
        const nextOffset=offset+page.length<inventory.length?offset+page.length:null;
        const deferred=[];
        for(const id of selector.view==='source'?pageIds:[]) {
          if(sources.has(id)) continue;
          const source=this.read(revision,id);
          if(Buffer.byteLength(source.content)>fileBudget) {deferred.push(source.entity);continue;}
          fileBudget-=Buffer.byteLength(source.content);
          const {revision:unused,...body}=source;
          sources.set(id,body);
        }
        return {selector,status:matchCount===0?'not_found':uncovered.length||deferred.length||nextOffset!==null?'partial':'matched',
          view:selector.view??'candidates',match_count:matchCount,total_candidates:ids.size,
          ...(selector.view!=='source'?{candidates:pageIds.map(id=>({...s.byId.get(id),bytes:s.byId.get(id).end-s.byId.get(id).start,...evidence.get(id)}))}:{}),
          source_ids:pageIds.filter(id=>sources.has(id)),uncovered:pageUncovered,uncovered_count:uncovered.length,deferred,
          next_offset:nextOffset,selection:'smallest_lexical_container_not_semantic_relevance',
          ...(uncovered.length?{next_action:'Read uncovered byte ranges explicitly; parser coverage is incomplete.'}:{}),
          ...(deferred.length?{deferred_reason:'SOURCE_BUDGET',resume:'Read deferred entities by ID from this revision.'}:{})};
      }
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
         Object.keys(selector).some(k=>!['id','name','file','type','view'].includes(k))||
         (selector.view!==undefined&&!['candidates','source'].includes(selector.view))||
         (typeof selector.id==='string')===(typeof selector.name==='string')||
         Object.values(selector).some(v=>typeof v!=='string'||!v)) fail('INVALID_SELECTOR');
      const matches=lookupEntities(s,selector);
      const status=matches.length===0?'not_found':matches.length===1?'unique':'ambiguous';
      if(selector.view==='candidates') return {selector,status,matches};
      if(selector.view==='source' && status==='ambiguous') {
        const source_ids=[],deferred=[];
        for(const e of matches) {
          if(!sources.has(e.id)) {
            const {revision:unused,...source}=this.read(revision,e.id);
            const bytes=Buffer.byteLength(source.content);
            if(bytes>fileBudget) {deferred.push(e);continue;}
            sources.set(e.id,source);
            fileBudget-=bytes;
          }
          source_ids.push(e.id);
        }
        // Reading every explicitly requested match does not resolve ambiguity.
        // Mutations still require a specific snapshot ID and hash checks.
        return {selector,status,matches,source_ids,deferred,
          complete:deferred.length===0,
          ...(deferred.length?{deferred_reason:'SOURCE_BUDGET',resume:'Read deferred entities by ID from this revision.'}:{})};
      }
      if(status==='unique') {
        const e=matches[0];
        if(!sources.has(e.id)) {
          const {revision:unused,...source}=this.read(revision,e.id);
          sources.set(e.id,source);
        }
        return {selector,status,source_id:e.id};
      }
      return {selector,status,matches};
      } catch(error) {
        if(error.message!=='INVALID_SELECTOR') throw error;
        return {selector,status:'error',error:{code:'INVALID_SELECTOR',
          hint:'Use name/id with optional file/type, a file-only read, contains with optional file/view/offset, or {file,start_byte,end_byte}. Range reads require file and do not accept view.'},complete:false};
      }
    });
    // Reuse containing source only within this response. No assumption that a
    // previous tool response is still in the model's context.
    return {revision,results,...(s.fileErrors?.length?{file_errors:s.fileErrors,complete:false}:{}),sources:compactSources([...sources.values()]),...(files.size?{files:[...files.values()]}:{}),...(ranges.size?{ranges:[...ranges].map(([key,range])=>({key,...range}))}:{}),coverage:'parser_reported_only'};
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
