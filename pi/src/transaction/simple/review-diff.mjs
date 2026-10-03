import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {createHash} from 'node:crypto';
const exec=promisify(execFile);
const cap=8*1024*1024;
export class ReviewDiff {
  constructor(){this.snapshots=new Map();}
  async capture(cwd,files,{since}={}) {
    if(!Array.isArray(files)||!files.length||files.length>64)throw Error('INVALID_DIFF_SCOPE');
    const root=await fs.realpath(cwd),scope=[...new Set(files)].sort(),present=new Set();
    const previous=since?this.snapshots.get(since):null;
    if(since&&!previous)throw Error('EXPIRED_OR_UNKNOWN_DIFF');
    if(previous&&(previous.root!==root||JSON.stringify(previous.files)!==JSON.stringify(scope)))throw Error('DIFF_BASE_SCOPE_MISMATCH');
    for(const file of scope) {
      if(typeof file!=='string'||path.isAbsolute(file)||file.split('/').some(p=>!p||p==='.'||p==='..'))throw Error('INVALID_DIFF_PATH');
      const parts=file.split('/');
      for(let i=1;i<=parts.length;i++) {
        let stat;
        try{stat=await fs.lstat(path.join(root,...parts.slice(0,i)));}
        catch(error){if(error.code==='ENOENT')break;throw error;}
        if(stat.isSymbolicLink())throw Error('DIFF_SYMLINK_UNSUPPORTED');
        if(i===parts.length&&(!stat.isFile()||stat.size>cap))throw Error('DIFF_REQUIRES_BOUNDED_FILES');
        if(i===parts.length)present.add(file);
      }
    }
    const git=async args=>(await exec('git',['--literal-pathspecs',...args],{cwd:root,maxBuffer:cap,timeout:10000})).stdout;
    const head=(await git(['rev-parse','HEAD'])).trim();
    const sources=new Map();let bytes=0;
    for(const file of scope) {
      const content=present.has(file)?await fs.readFile(path.join(root,file)):null;
      bytes+=content?.length??0;
      if(bytes>cap)throw Error('DIFF_TOO_LARGE: narrow file scope');
      sources.set(file,content);
    }
    let text=previous?'':await git(['diff','--no-ext-diff','--no-textconv','--no-color','--no-renames',head,'--',...scope]);
    const tracked=new Set((await git(['ls-files','--cached','-z','--',...scope])).split('\0').filter(Boolean));
    const untracked=scope.filter(file=>present.has(file)&&!tracked.has(file));
    for(const file of previous?[]:untracked) {
      try {text+=await git(['diff','--no-index','--no-ext-diff','--no-textconv','--no-color','--','/dev/null',file]);}
      catch(error){if(error.code===1&&typeof error.stdout==='string')text+=error.stdout;else throw error;}
      if(Buffer.byteLength(text)>cap)throw Error('DIFF_TOO_LARGE: narrow file scope');
    }
    if(previous) {
      const tmp=await fs.mkdtemp(path.join(os.tmpdir(),'sem-review-delta-'));
      try {
        for(const file of scope) {
          const before=previous.sources.get(file),after=sources.get(file);
          if(before===null&&after===null||before&&after&&before.equals(after))continue;
          const a=before===null?'/dev/null':path.join(tmp,'before');
          const b=after===null?'/dev/null':path.join(tmp,'after');
          if(before!==null)await fs.writeFile(a,before);
          if(after!==null)await fs.writeFile(b,after);
          let diff;
          try {diff=await git(['diff','--no-index','--no-ext-diff','--no-textconv','--no-color','--',a,b]);}
          catch(error){if(error.code===1&&typeof error.stdout==='string')diff=error.stdout;else throw error;}
          // Replace only Git header lines, never +/- source lines.
          const old=JSON.stringify('a/'+file),next=JSON.stringify('b/'+file);
          text+=diff.split('\n').map(line=>line.startsWith('diff --git ')?`diff --git ${old} ${next}`
            :line.startsWith('--- ')?`--- ${before===null?'/dev/null':old}`
            :line.startsWith('+++ ')?`+++ ${after===null?'/dev/null':next}`
            :line.startsWith('Binary files ')?`Binary files ${old} and ${next} differ`:line).join('\n');
          if(Buffer.byteLength(text)>cap)throw Error('DIFF_TOO_LARGE: narrow file scope');
        }
      } finally {await fs.rm(tmp,{recursive:true,force:true});}
    }
    const manifest=[...sources].map(([file,content])=>[file,content===null?null:createHash('sha256').update(content).digest('hex')]);
    const id=createHash('sha256').update(JSON.stringify([root,head,scope,text,since??null,manifest])).digest('hex');
    this.snapshots.set(id,{text,head,root,sources,files:scope,untracked,since});
    while(this.snapshots.size>4)this.snapshots.delete(this.snapshots.keys().next().value);
    return this.read(id,0);
  }
  read(id,offset=0) {
    const record=this.snapshots.get(id);
    if(!record)throw Error('EXPIRED_OR_UNKNOWN_DIFF');
    if(!Number.isSafeInteger(offset)||offset<0||offset>record.text.length)throw Error('INVALID_DIFF_OFFSET');
    // A requested review commonly spans several entities. Avoid forcing a
    // model round trip per small page; retain pagination for genuinely large diffs.
    let end=Math.min(offset+48000,record.text.length);
    if(end<record.text.length&&/[\uD800-\uDBFF]/.test(record.text[end-1]))end--;
    return {diff_id:id,base:record.head,files:record.files,untracked:record.untracked,
      diff:record.text.slice(offset,end),offset,next_offset:end<record.text.length?end:null,
      complete:end===record.text.length&&offset===0,total_characters:record.text.length,
      ...(record.since?{since_diff:record.since}:{}),
      coverage:record.since?'file_content_since_captured_review_in_same_explicit_scope; no_mode_changes; binary_changes_have_summary_only'
        :'working_tree_vs_HEAD_in_explicit_files_including_untracked; includes_preexisting_changes; binary_changes_have_summary_only',
      consistency:'captured_diff_not_atomic_repository_snapshot; recapture_after_edits',validation:'not_run'};
  }
}
