import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {ReviewDiff} from './review-diff.mjs';
async function fixture(t) {
 const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-diff-'));
 t.after(()=>fs.rm(root,{recursive:true,force:true}));
 const git=args=>execFileSync('git',args,{cwd:root});
 git(['init','-q']);await fs.writeFile(path.join(root,'a.txt'),'before\n');
 await fs.writeFile(path.join(root,'b.txt'),'untouched\n');
 git(['add','.']);git(['-c','user.name=Test','-c','user.email=test@example.test','-c','core.hooksPath=/dev/null','commit','-qm','base']);
 return {root,api:new ReviewDiff(),git};
}
test('review includes tracked and untracked changes only in literal scope',async t=>{
 const {root,api,git}=await fixture(t);
 await fs.writeFile(path.join(root,'a.txt'),'after\n');
 await fs.writeFile(path.join(root,'b.txt'),'not requested\n');
 await fs.writeFile(path.join(root,'new.txt'),'new content\n');
 git(['config','diff.external','/does/not/exist']);
 const result=await api.capture(root,['a.txt','new.txt']);
 assert.match(result.diff,/-before\n\+after/);assert.match(result.diff,/\+new content/);
 assert.ok(!result.diff.includes('not requested'));assert.equal(result.complete,true);
 assert.deepEqual(result.untracked,['new.txt']);assert.equal(result.validation,'not_run');
 assert.equal(await fs.readFile(path.join(root,'a.txt'),'utf8'),'after\n');
});
test('deletions and frozen pagination retain all text; changed scope gets a new id',async t=>{
 const {root,api}=await fixture(t);
 await fs.unlink(path.join(root,'a.txt'));
 await fs.writeFile(path.join(root,'new.txt'),'😀line\n'.repeat(10000));
 const first=await api.capture(root,['a.txt','new.txt']);
 assert.equal(first.complete,false);assert.match(first.diff,/-before/);
 let text=first.diff,part=first;
 while(part.next_offset!==null){part=api.read(first.diff_id,part.next_offset);text+=part.diff;}
 assert.equal(text.length,first.total_characters);assert.ok(!text.includes('\uFFFD'));
 await fs.writeFile(path.join(root,'new.txt'),'changed\n');
 const next=await api.capture(root,['a.txt','new.txt']);assert.notEqual(next.diff_id,first.diff_id);
 assert.equal(api.read(first.diff_id).diff,first.diff);
});
test('requested medium-sized review needs no continuation call',async t=>{
 const {root,api}=await fixture(t);
 await fs.writeFile(path.join(root,'new.txt'),'review line\n'.repeat(2000));
 const first=await api.capture(root,['new.txt']);
 assert.ok(first.total_characters>12000);
 assert.equal(first.complete,true);
 assert.equal(first.next_offset,null);
 assert.equal(first.diff.length,first.total_characters);
});
test('unsafe paths rejected; explicit gitignored additions included',async t=>{
 const {root,api}=await fixture(t);
 await fs.symlink(path.join(root,'a.txt'),path.join(root,'link.txt'));
 for(const file of ['../escape','/absolute'])await assert.rejects(api.capture(root,[file]),/INVALID_DIFF_PATH/);
 await assert.rejects(api.capture(root,['link.txt']),/SYMLINK/);
 await fs.writeFile(path.join(root,'.gitignore'),'ignored.txt\n');
 await fs.writeFile(path.join(root,'ignored.txt'),'requested\n');
 assert.match((await api.capture(root,['ignored.txt'])).diff,/\+requested/);
});
