import test from 'node:test';
import assert from 'node:assert/strict';
import {ValidationCache,inputFingerprint} from './validation-cache.mjs';
test('no implicit reuse; success keyed on both command and complete input key',async()=>{
  const cache=new ValidationCache();let calls=0,key='a';
  const execute=async()=>({pass:true,stage:'test',duration_ms:123,n:++calls});
  await cache.run('test',null,execute);await cache.run('test',null,execute);assert.equal(calls,2);
  const fingerprint=async()=>key;
  await cache.run('test',fingerprint,execute);
  const hit=await cache.run('test',fingerprint,execute);
  assert.equal(calls,3);assert.equal(hit.validation_cache.reused,true);assert.equal(hit.duration_ms,0);
  key='b';await cache.run('test',fingerprint,execute);assert.equal(calls,4);
  await cache.run('other-test',fingerprint,execute);assert.equal(calls,5);
});
test('failures and unknown fingerprints execute again; input changes invalidate pass',async()=>{
  const cache=new ValidationCache();let calls=0;
  const fail=async()=>({pass:false,stage:'test',n:++calls});
  await cache.run('test',async()=>'a',fail);await cache.run('test',async()=>'a',fail);assert.equal(calls,2);
  await cache.run('test',async()=>{throw Error('unknown');},fail);assert.equal(calls,3);
  let key='a';
  const stale=await cache.run('test',async()=>key,async()=>{key='b';return {pass:true,stage:'test'};});
  assert.equal(stale.pass,null);assert.equal(stale.observed_pass,true);assert.equal(stale.stage,'stale_validation');
  assert.equal(cache.entries.size,0);
});
test('cache is bounded and returned values do not corrupt stored evidence',async()=>{
  const cache=new ValidationCache({limit:1});
  const result=await cache.run('test',async()=>'a',async()=>({pass:true,stage:'test',nested:{ok:true}}));
  result.nested.ok=false;
  const hit=await cache.run('test',async()=>'a',()=>{throw Error('unexpected execution');});
  assert.equal(hit.nested.ok,true);
  await cache.run('test',async()=>'b',async()=>({pass:true,stage:'test'}));
  assert.equal(cache.entries.size,1);
  assert.equal(inputFingerprint('test',{}),null);
});
test('fingerprint hook is explicit argv and fingerprints the environment',async()=>{
 const env={...process.env,SEM_VALIDATION_INPUT_KEY_ARGV:JSON.stringify([process.execPath,'-e','process.stdout.write(process.argv[1])'])};
 const command='python -m pytest tests/test_a.py';
 const key=await inputFingerprint(command,env)();
 assert.ok(key.includes(command));
 assert.notEqual(key,await inputFingerprint(command,{...env,SEM_VALIDATION_IMAGE:'changed-image'})());
 await assert.rejects(inputFingerprint(command,{...env,SEM_VALIDATION_INPUT_KEY_ARGV:'not json'})());
});
test('a failed second fingerprint never reuses a cached pass',async()=>{
 const cache=new ValidationCache();let calls=0;
 const execute=async()=>({pass:true,stage:'test',n:++calls});
 await cache.run('test',async()=>'a',execute);
 let samples=0;
 const result=await cache.run('test',async()=>{if(++samples>1)throw Error('unknown');return 'a';},execute);
 assert.equal(calls,2);assert.equal(result.validation_cache,undefined);
});
