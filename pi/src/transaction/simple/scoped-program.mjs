import fs from 'node:fs/promises';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {generateEdits} from './edit-program.mjs';
import {programSource} from './program-source.mjs';

const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
export async function prepareScopedProgram(exact,cwd,scope,code) {
  const root=await fs.realpath(cwd);
  const captured=await exact.capture(root,scope,{allowMissing:true});
  const snapshot=exact.get(captured.revision);
  const files=[...snapshot.sources].map(([file,bytes])=>({file,content:bytes.toString('utf8'),sha256:hash(bytes)}));
  const entities=snapshot.entities.map(e=>({file:e.file,
    entity:{name:e.name,type:e.type,...(e.parent_name?{parent_name:e.parent_name}:{})},
    ...programSource(snapshot.sources.get(e.file),e),source_id:e.id,
    qualified_name:e.qualified_name,
    }));
  if(Buffer.byteLength(JSON.stringify({files,entities}))>8*1024*1024)
    throw Error('PROGRAM_INPUT_TOO_LARGE: narrow explicit file scope');
  const generated=await generateEdits(code,entities,files);
  const allowed=new Set(scope);
  if(generated.format_files!==undefined&&(!Array.isArray(generated.format_files)||generated.format_files.some(file=>!allowed.has(file))))
    throw Error('PROGRAM_FORMAT_OUTSIDE_EXPLICIT_SCOPE');
  for(const key of ['edits','imports','remove_imports','creates']) {
    if(generated[key]===undefined)continue;
    if(!Array.isArray(generated[key]))throw Error('INVALID_PROGRAM_OPERATIONS');
    for(const operation of generated[key]) {
      const file=operation?.file??operation?.path??operation?.entity?.file;
      if(typeof file!=='string'||!allowed.has(file))throw Error('PROGRAM_WRITE_OUTSIDE_EXPLICIT_SCOPE');
    }
  }
  // Check every input, not merely the files that happened to receive edits.
  for(const file of captured.files) {
    const absolute=path.join(root,file.file);
    if(await fs.realpath(absolute)!==absolute||hash(await fs.readFile(absolute))!==file.sha256)
      throw Error('PROGRAM_SOURCE_CHANGED: '+file.file);
  }
  for(const file of captured.missing_files) {
    try {await fs.lstat(path.join(root,file));}
    catch(error) {if(error.code==='ENOENT')continue;throw error;}
    throw Error('PROGRAM_SOURCE_CHANGED: previously missing '+file);
  }
  return {generated,receipt:{input_snapshot:captured.revision,inputs:captured.files,
    missing_files:captured.missing_files,source_visibility:'loaded_inside_program_not_sent_to_model',
    consistency:'input_hashes_checked_before_apply; not_repository_wide_isolation'}};
}
