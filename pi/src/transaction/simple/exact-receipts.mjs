import {createHash} from 'node:crypto';

// Explicit caller acknowledgement only; never infer retention from delivery.
// Fingerprint logical location + bytes, not snapshot IDs, so an unrelated edit
// can change the revision without retransmitting an unchanged entity body.
export function presentExact(result, {known_receipts=[],refresh_source=false}={}) {
  if(!Array.isArray(known_receipts)||known_receipts.length>128||known_receipts.some(x=>typeof x!=='string'))
    throw Error('INVALID_SOURCE_RECEIPTS');
  const known=new Set(refresh_source?[]:known_receipts);
  const present=(source,kind)=>{
    if(typeof source.content!=='string'||source.complete===false||source.truncated)return source;
    const e=source.entity;
    const identity=e?[e.file,e.name,e.type,e.qualified_name,e.parent_name,e.start,e.end]
      :[source.file,source.start_byte,source.end_byte];
    const receipt=createHash('sha256').update(JSON.stringify([kind,identity,source.content])).digest('hex');
    const full={...source,receipt};
    if(!known.has(receipt))return full;
    const {content,...metadata}=full;
    const reference={...metadata,reused:true,content_source:{kind:'acknowledged_receipt',receipt}};
    return Buffer.byteLength(JSON.stringify(reference))<Buffer.byteLength(JSON.stringify(full))?reference:full;
  };
  if(result.entity&&typeof result.content==='string')return present(result,'entity');
  // A compact child may reference its parent's same-response content. Keep
  // that parent present instead of creating a dangling cross-response slice.
  const required=new Set((result.sources??[]).map(s=>s.content_source?.source_id).filter(Boolean));
  return {...result,
    ...(result.sources?{sources:result.sources.map(s=>required.has(s.entity?.id)?s:present(s,'entity'))}:{}),
    ...(result.files?{files:result.files.map(s=>present(s,'file'))}:{}),
    ...(result.ranges?{ranges:result.ranges.map(s=>present(s,'range'))}:{}),
  };
}
