import {createHash} from 'node:crypto';
import fs from 'node:fs/promises';
import path from 'node:path';
export function selectEntity(entities, requested) {
  const sameName=entities.filter(e=>e.name?.toLowerCase()===requested.name.toLowerCase());
  const candidates=sameName.filter(e=>(!requested.entity_type||e.type===requested.entity_type)
    &&(!requested.parent_name||e.parent_name?.toLowerCase()===requested.parent_name.toLowerCase()));
  const ordinal=requested.ordinal;
  if((ordinal!==undefined&&(!Number.isInteger(ordinal)||ordinal<0||ordinal>=candidates.length))
      ||(ordinal===undefined&&candidates.length!==1)) {
    throw new Error('Ambiguous or incompatible entity selector; use an exact type/parent/ordinal: '+JSON.stringify(sameName.map(e=>({name:e.name,entity_type:e.type,parent_name:e.parent_name,start_line:e.start_line}))));
  }
  return candidates[ordinal??0];
}
export async function invalidateChanged(cwd, snapshots, entities, files) {
  const invalidated=[];
  for(const [file,hash] of snapshots) {
    const source=await fs.readFile(path.resolve(cwd,file)).catch(()=>null);
    if(source&&createHash('sha256').update(source).digest('hex')===hash)continue;
    snapshots.delete(file); files.delete(file);
    for(const [key,d]of entities)if(d.file===file)entities.delete(key);
    invalidated.push(file);
  }
  return invalidated;
}
export function receiptFor(definition) {
  return createHash('sha256').update(JSON.stringify(definition)).digest('hex').slice(0,24);
}
export function acknowledgeDefinitions(definitions, known=[]) {
  const acknowledged=new Set(known);
  return definitions.map(d=>{
    const receipt=receiptFor(d);
    if(!d.truncated&&typeof d.content==='string'&&acknowledged.has(receipt)) {
      const {content,...locator}=d;
      return {...locator,receipt,reused:true,reason:'Caller explicitly acknowledged this unchanged definition'};
    }
    return {...d,receipt};
  });
}
