import fs from 'node:fs/promises';
import {createHash,randomBytes} from 'node:crypto';
import readline from 'node:readline';
import {pathToFileURL} from 'node:url';
import {ExactCode} from './exact-code.mjs';
import {applyExact} from './exact-weave.mjs';

// Addresses locate a captured entity, not a timeless semantic identity.
// Classification may order cards, but must not change their identity/evidence.
export class EntityInterface {
  constructor(root, {semBin='sem', exact=new ExactCode({semBin}),shortAddresses=false}={}) {
    this.root=root; this.exact=exact; this.semBin=semBin;
    this.shortAddresses=shortAddresses;
    this.addressSession=randomBytes(6).toString('hex');
    this.addressSequence=0n;this.handles=new Map();this.reverseHandles=new Map();
    this.snapshotIndices=new WeakMap();
  }
  async initialize() {
    this.root=await fs.realpath(this.root);
    this.repository=createHash('sha256').update(this.root).digest('hex').slice(0,24);
    return this;
  }
  pruneHandles() {
    for(const [handle,binding] of this.handles) if(!this.exact.snapshots.has(binding.revision)) {
      this.handles.delete(handle);this.reverseHandles.delete(`${binding.revision}:${binding.index}`);
    }
  }
  address(revision,index) {
    if(!this.shortAddresses) return `sem-entity:v1:${this.repository}:${revision}:e${index}`;
    const key=`${revision}:${index}`;
    if(this.reverseHandles.has(key)) return this.reverseHandles.get(key);
    const handle=`e:${this.addressSession}:${(this.addressSequence++).toString(36)}`;
    this.handles.set(handle,{revision,index});this.reverseHandles.set(key,handle);
    return handle;
  }
  resolve(address) {
    if(typeof address!=='string') throw Error('INVALID_ENTITY_ADDRESS');
    const short=/^e:([a-f0-9]{12}):([0-9a-z]+)$/.exec(address);
    if(short) {
      if(short[1]!==this.addressSession) throw Error('WRONG_ADDRESS_SESSION');
      const binding=this.handles.get(address);
      if(!binding||!this.exact.snapshots.has(binding.revision)) throw Error('EXPIRED_OR_UNKNOWN_ENTITY_ADDRESS');
      const snapshot=this.exact.get(binding.revision),entity=snapshot.entities[binding.index];
      if(!entity) throw Error('UNKNOWN_ENTITY_ADDRESS');
      return {revision:binding.revision,entity,snapshot};
    }
    const match=/^sem-entity:v1:([a-f0-9]{24}):([a-f0-9]{64}):e(0|[1-9][0-9]*)$/.exec(address);
    if(!match) throw Error('INVALID_ENTITY_ADDRESS');
    if(match[1]!==this.repository) throw Error('WRONG_REPOSITORY');
    const revision=match[2], snapshot=this.exact.get(revision);
    const entity=snapshot.entities[Number(match[3])];
    if(!entity) throw Error('UNKNOWN_ENTITY_ADDRESS');
    return {revision,entity,snapshot};
  }
  async orient(files, options={}) {
    const snapshot=await this.exact.capture(this.root,files,{allowMissing:true});
    this.pruneHandles();
    return {...this.list(snapshot.revision,options),missing_files:snapshot.missing_files};
  }
  async query({files,snapshot,selectors}) {
    if(Boolean(files)===Boolean(snapshot)) throw Error('PROVIDE_SNAPSHOT_OR_FILES');
    const captured=files?await this.exact.capture(this.root,files,{allowMissing:true,partialReads:true}):null;
    this.pruneHandles();
    const revision=captured?.revision??snapshot;
    const state=this.exact.get(revision);
    let indices=this.snapshotIndices.get(state);
    if(!indices) {
      indices=new Map(state.entities.map((entity,index)=>[entity.id,index]));
      this.snapshotIndices.set(state,indices);
    }
    // Allocate handles only for returned entities, not every entity in scope.
    // The weak index follows snapshot lifetime without retaining evicted source.
    const addressFor=id=>indices.has(id)?this.address(revision,indices.get(id)):undefined;
    const located=entity=>({...entity,address:addressFor(entity.id)});
    const result=this.exact.query(revision,selectors);
    return {...result,snapshot:revision,missing_files:captured?.missing_files??[],
      ...(state.fileErrors?.length?{file_errors:state.fileErrors,complete:false}:{}),
      sources:result.sources.map(source=>({...source,address:addressFor(source.entity.id)})),
      results:result.results.map(item=>({...item,
        ...(item.source_id?{address:addressFor(item.source_id)}:{}),
        ...(item.matches?{matches:item.matches.map(located)}:{}),
        ...(item.candidates?{candidates:item.candidates.map(located)}:{}),
        ...(item.deferred?{deferred:item.deferred.map(located)}:{})})),
      identity:'snapshot_scoped; apply checks current file hashes'};
  }
  list(revision,{name,file,offset=0,limit=30}={}) {
    if(!Number.isSafeInteger(offset)||offset<0||!Number.isSafeInteger(limit)||limit<1||limit>100)
      throw Error('INVALID_PAGE');
    const snapshot=this.exact.get(revision);
    const candidates=snapshot.entities.map((entity,index)=>({entity,index})).filter(({entity})=>
      (!file||file===entity.file)&&(!name||[entity.name,entity.qualified_name,entity.declared_qualified_name].includes(name)));
    if(offset>candidates.length) throw Error('PAGE_OUT_OF_RANGE');
    const selected=candidates.slice(offset,offset+limit);
    return {schema:'sem.entity-card/1',repository:this.repository,snapshot:revision,
      identity:'snapshot_scoped; reacquire after edits; never silently retarget',
      coverage:'parser_reported entities in explicit files; runtime targets not resolved',
      total:candidates.length,next_offset:offset+selected.length<candidates.length?offset+selected.length:null,
      cards:selected.map(({entity:e,index})=>({address:this.address(revision,index),
        name:e.declared_qualified_name||e.qualified_name||e.name,kind:e.type,file:e.file,
        bytes:e.end-e.start,source:'not_loaded'}))};
  }
  neighbors(address) {
    const {revision,entity:e,snapshot}=this.resolve(address);
    // Only the nearest strict lexical containers, including ambiguous parents.
    const parents=snapshot.entities.map((p,index)=>({p,index})).filter(({p})=>
      p.file===e.file&&p.start<=e.start&&p.end>=e.end&&(p.start<e.start||p.end>e.end));
    const nearest=parents.filter(({p})=>!parents.some(({p:q})=>
      q!==p&&q.start>=p.start&&q.end<=p.end&&(q.start>p.start||q.end<p.end)));
    return {address,edges:nearest.map(({index})=>({relation:'contained_by',
      target:this.address(revision,index),evidence:'parser_byte_ranges',resolution:'lexical'})),
      scope:'captured_file_only',runtime_calls:'not_analyzed'};
  }
  batch(addresses) {
    if(!Array.isArray(addresses)||!addresses.length||addresses.length>64) throw Error('INVALID_BATCH');
    const resolved=addresses.map(address=>this.resolve(address));
    if(resolved.some(r=>r.revision!==resolved[0].revision)) throw Error('MIXED_SNAPSHOTS');
    return {revision:resolved[0].revision,entities:resolved.map(r=>r.entity)};
  }
  read(addresses) {
    const {revision,entities}=this.batch(addresses);
    return {addresses,...this.exact.query(revision,entities.map(e=>({id:e.id}))),
      freshness:'captured_source; apply checks current file hashes'};
  }
  edits(changes) {
    if(!Array.isArray(changes)) throw Error('INVALID_BATCH');
    const {revision,entities}=this.batch(changes.map(c=>c.address));
    const edits=changes.map((c,i)=>{
      if(Object.keys(c).some(k=>!['address','old','new','content'].includes(k))) throw Error('UNKNOWN_EDIT_FIELD');
      if(typeof c.content==='string') {
        if(c.old!==undefined||c.new!==undefined) throw Error('MIXED_EDIT_FORMS');
        return {id:entities[i].id,content:c.content};
      }
      if(typeof c.old!=='string'||!c.old||typeof c.new!=='string') throw Error('INVALID_EDIT');
      return {id:entities[i].id,old:c.old,new:c.new};
    });
    return {revision,edits};
  }
  async apply(changes) {
    const {revision,edits}=this.edits(changes);
    return applyExact(this.exact,this.root,revision,edits,{semBin:this.semBin});
  }
  async dispatch(request) {
    switch(request.op) {
      case 'query': return this.query(request);
      case 'orient': return this.orient(request.files,request.select);
      case 'list': return this.list(request.snapshot,request.select);
      case 'neighbors': return this.neighbors(request.address);
      case 'read': return this.read(request.addresses);
      case 'apply': return this.apply(request.changes);
      default: throw Error('Use orient, list, neighbors, read, or apply');
    }
  }
}

if(process.argv[1]&&import.meta.url===pathToFileURL(process.argv[1]).href) {
  const api=await new EntityInterface(process.argv[2]||process.cwd(),{semBin:process.env.SEM_TEST_BIN||'sem'}).initialize();
  // Sequential JSONL requests: no hidden background hydration or mutation.
  for await(const line of readline.createInterface({input:process.stdin})) {
    try {console.log(JSON.stringify({ok:true,result:await api.dispatch(JSON.parse(line))}));}
    catch(error) {console.log(JSON.stringify({ok:false,error:error.message}));}
  }
}
