import {createHash} from 'node:crypto';
import {programSource} from './program-source.mjs';
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
const lineAt=(bytes,offset)=>1+bytes.subarray(0,offset).toString('utf8').split('\n').length-1;

// Reuse only complete source actually included in the response (including
// lossless same-response source references). Candidate-only lookups add nothing.
export function rememberExactSource(exact, result, snapshots, entities, files) {
  const revision=result.revision;
  const delivered=result.sources??(result.entity&&result.complete?[result]:[]);
  const deliveredFiles=result.files?.filter(f=>f.complete&&typeof f.content==='string')??[];
  if(!delivered.length&&!deliveredFiles.length)return;
  const snapshot=exact.get(revision);
  const relevant=new Set([...delivered.map(d=>d.entity.file),...deliveredFiles.map(f=>f.file)]);
  for(const file of relevant) {
    const bytes=snapshot.sources.get(file), fingerprint=hash(bytes);
    if(snapshots.get(file)!==fingerprint) {
      for(const [key,value] of entities)if(value.file===file)entities.delete(key);
      files.delete(file);
    }
    snapshots.set(file,fingerprint);
  }
  for(const source of delivered) {
    if(!source.complete)continue;
    const e=snapshot.byId.get(source.entity.id);
    if(!e)throw new Error('Unknown delivered entity');
    const content=exact.read(revision,e.id).content;
    if(hash(Buffer.from(content))!==source.sha256)throw new Error('Delivered source mismatch');
    const bytes=snapshot.sources.get(e.file);
    const parent=snapshot.entities.filter(p=>p.file===e.file&&p.id!==e.id&&p.start<=e.start&&p.end>=e.end&&(p.start<e.start||p.end>e.end))
      .sort((a,b)=>(a.end-a.start)-(b.end-b.start))[0];
    const entity={name:e.name,type:e.type,start_line:lineAt(bytes,e.start),end_line:lineAt(bytes,Math.max(e.start,e.end-1)),
      ...(e.parent_name?{parent_name:e.parent_name}:parent?{parent_name:parent.name}:{})};
    const key=JSON.stringify([e.file,entity.name,entity.parent_name,entity.start_line]);
    entities.set(key,{file:e.file,entity,...programSource(bytes,e),truncated:false,source_id:e.id,qualified_name:e.qualified_name});
  }
  for(const file of deliveredFiles) {
    if(hash(Buffer.from(file.content))!==hash(snapshot.sources.get(file.file)))throw new Error('Delivered file mismatch');
    files.set(file.file,{...file,truncated:false});
  }
}
