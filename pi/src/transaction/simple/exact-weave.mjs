import fs from 'node:fs/promises';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {performWeaveEdit} from '../../tools/weave-edit.ts';
const hash=b=>createHash('sha256').update(b).digest('hex');

export async function applyExact(exact,cwd,revision,edits,{semBin='sem',validate}={}) {
  const snapshot=exact.get(revision);
  if(!Array.isArray(edits)||!edits.length||edits.length>64) throw new Error('INVALID_EDITS');
  const root=await fs.realpath(cwd),seen=new Set(),targets=new Map(),batch=[];
  let replacementBytes=0;
  for(const edit of edits) {
    const entity=snapshot.entities.find(e=>e.id===edit.id);
    if(!entity) throw new Error('INVALID_EDIT');
    let content=edit.content;
    if(content!==undefined&&(edit.old!==undefined||edit.new!==undefined)) throw new Error('INVALID_EDIT');
    if(content===undefined) {
      if(typeof edit.old!=='string'||!edit.old||typeof edit.new!=='string') throw new Error('INVALID_EDIT');
      const original=exact.read(revision,edit.id).content;
      const offset=original.indexOf(edit.old);
      if(offset<0||original.indexOf(edit.old,offset+1)>=0) throw new Error('ANCHOR_NOT_UNIQUE');
      content=original.slice(0,offset)+edit.new+original.slice(offset+edit.old.length);
    }
    if(typeof content!=='string') throw new Error('INVALID_EDIT');
    replacementBytes+=Buffer.byteLength(content);
    if(replacementBytes>exact.maxBytes) throw new Error('RESULT_TOO_LARGE');
    if(edit.allow_signature_change!==undefined&&typeof edit.allow_signature_change!=='boolean') throw new Error('INVALID_EDIT');
    if(seen.has(entity.file)) throw new Error('MULTIPLE_TARGETS_PER_FILE_NOT_SUPPORTED');
    seen.add(entity.file);
    const file=path.join(root,entity.file);
    if(await fs.realpath(file)!==file) throw new Error('SYMLINK_NOT_SUPPORTED');
    const expected=hash(snapshot.sources.get(entity.file));
    if(hash(await fs.readFile(file))!==expected) throw new Error('STALE_SNAPSHOT');
    targets.set(file,{sha256:expected,start:entity.start,end:entity.end});
    // Exact entities exclude indentation; Weave replaces whole source lines.
    // Preserve surrounding whitespace, and reject inline siblings rather than
    // silently dropping bytes outside the requested entity.
    const original=snapshot.sources.get(entity.file);
    const lineStart=original.lastIndexOf(10,entity.start-1)+1;
    const nextNewline=original.indexOf(10,entity.end);
    const lineEnd=nextNewline<0?original.length:nextNewline;
    const prefix=original.subarray(lineStart,entity.start).toString('utf8');
    const suffix=original.subarray(entity.end,lineEnd).toString('utf8');
    if(!/^[\t ]*$/.test(prefix)||!/^[\t \r]*$/.test(suffix)) throw new Error('INLINE_ENTITY_NOT_SUPPORTED');
    batch.push({file:entity.file,entity:{name:entity.name,entity_type:entity.type},op:'replace',content:prefix+content+suffix.replace(/\r$/,''),allow_signature_change:edit.allow_signature_change??false});
  }
  let outcome;
  try {
    outcome=await performWeaveEdit({edits:batch,atomic:true,claim:false},{cwd:root,semBin,checkDependents:false,snapshotTargets:targets});
  } catch(error) {
    return {revision,status:'apply_error',error:String(error),validation:'not_run',requires_recapture:true};
  }
  if(outcome.isError||outcome.details.failed>0||outcome.details.rolledBack) {
    return {revision,status:'apply_failed',details:outcome.details,validation:'not_run',requires_recapture:true};
  }
  const files=[];
  for(const file of seen)files.push({file,sha256:hash(await fs.readFile(path.join(root,file)))});
  const appliedRevision=hash(JSON.stringify(files));
  let verification={status:'not_run'};
  if(validate) {
    try { verification=await validate(); }
    catch(error) { verification={status:'error',error:String(error)}; }
    const unchanged=(await Promise.all(files.map(async f=>{
      try{return hash(await fs.readFile(path.join(root,f.file)))===f.sha256;}catch{return false;}
    }))).every(Boolean);
    verification={...verification,...(!unchanged?{observed_pass:verification.pass,pass:null,status:'stale_validation'}:{}),edited_files_unchanged:unchanged,scope:'checker_patch_receipt; file hashes cover edited files only'};
  }
  return {revision,status:'applied',applied_revision:appliedRevision,revision_scope:'edited_files_only',files,validation:verification,requires_recapture:true,
    concurrency:'Weave guarded writes and compensating rollback; not cross-process repository isolation'};
}
