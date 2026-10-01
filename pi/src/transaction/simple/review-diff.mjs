import fs from 'node:fs/promises';
import path from 'node:path';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {createHash} from 'node:crypto';
const exec=promisify(execFile);
const cap=8*1024*1024;
export class ReviewDiff {
  constructor(){this.snapshots=new Map();}
  async capture(cwd,files) {
    if(!Array.isArray(files)||!files.length||files.length>64)throw Error('INVALID_DIFF_SCOPE');
    const root=await fs.realpath(cwd),scope=[...new Set(files)].sort(),present=new Set();
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
    let text=await git(['diff','--no-ext-diff','--no-textconv','--no-color','--no-renames',head,'--',...scope]);
    const tracked=new Set((await git(['ls-files','--cached','-z','--',...scope])).split('\0').filter(Boolean));
    const untracked=scope.filter(file=>present.has(file)&&!tracked.has(file));
    for(const file of untracked) {
      try {text+=await git(['diff','--no-index','--no-ext-diff','--no-textconv','--no-color','--','/dev/null',file]);}
      catch(error){if(error.code===1&&typeof error.stdout==='string')text+=error.stdout;else throw error;}
      if(Buffer.byteLength(text)>cap)throw Error('DIFF_TOO_LARGE: narrow file scope');
    }
    const id=createHash('sha256').update(JSON.stringify([root,head,scope,text])).digest('hex');
    this.snapshots.set(id,{text,head,files:scope,untracked});
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
      coverage:'working_tree_vs_HEAD_in_explicit_files_including_untracked; includes_preexisting_changes; binary_changes_have_summary_only',
      consistency:'captured_diff_not_atomic_repository_snapshot; recapture_after_edits',validation:'not_run'};
  }
}
