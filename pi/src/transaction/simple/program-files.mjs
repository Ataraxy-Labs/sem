import fs from 'node:fs/promises';
import path from 'node:path';
import { createHash } from 'node:crypto';
export function boundedLines(source, limit) {
  let content='',bytes=0;
  for(const line of source.match(/[^\n]*\n|[^\n]+$/g)??[]) {
    const size=Buffer.byteLength(line);
    if(bytes+size>limit) break;
    content+=line;bytes+=size;
  }
  return {content,bytes,truncated:content!==source};
}
export async function readBoundedFiles(cwd, files, limit=48000) {
  if(!Array.isArray(files)||files.length>8) throw new Error('At most eight files per request');
  const root=await fs.realpath(cwd), results=[];
  let remaining=limit;
  for(const file of files) {
    let absolute;
    try { absolute=await fs.realpath(path.resolve(root,file)); }
    catch(error) {
      if(error.code!=='ENOENT')throw error;
      results.push({file,representation:'bounded_file_fallback',error:'not_found',content:null,truncated:false});
      continue;
    }
    const relative=path.relative(root,absolute);
    if(relative==='..'||relative.startsWith('..'+path.sep)||path.isAbsolute(relative)||relative.split(path.sep).includes('.git')) throw new Error('File outside repository source scope');
    const stat=await fs.stat(absolute);
    if(!stat.isFile()||stat.size>1024*1024) throw new Error('Fallback accepts regular source files up to 1MB; use entity queries for larger files');
    const buffer=await fs.readFile(absolute);
    if(buffer.includes(0)) throw new Error('Binary files are not supported');
    const source=buffer.toString('utf8'), packed=boundedLines(source,remaining);
    remaining-=packed.bytes;
    results.push({file,representation:'bounded_file_fallback',...packed,total_bytes:buffer.length,
      sha256:createHash('sha256').update(buffer).digest('hex')});
  }
  return results;
}
