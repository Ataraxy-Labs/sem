import {test} from 'node:test';
import assert from 'node:assert/strict';
import {requireExactPath} from './exact-path.mjs';
test('canonical spelling mismatch gives the actionable path without accepting it',async()=>{
 const io={realpath:async()=>'/repo/tests/Modal.test.tsx',lstat:async()=>({isSymbolicLink:()=>false})};
 await assert.rejects(requireExactPath('/repo','tests/modal.test.tsx',io),error=>{
  assert.match(error.message,/PATH_CANONICAL_MISMATCH/);
  assert.match(error.message,/"canonical":"tests\/Modal.test.tsx"/);return true;
 });
});
test('real parent symlinks remain rejected and correct paths use the fast path',async()=>{
 await assert.rejects(requireExactPath('/repo','tests/a.py',{
  realpath:async()=>'/elsewhere/a.py',lstat:async()=>({isSymbolicLink:()=>true})}),/SYMLINK_NOT_SUPPORTED/);
 assert.equal(await requireExactPath('/repo','a.py',{realpath:async p=>p,lstat:async()=>{throw Error('unused');}}),'/repo/a.py');
});
