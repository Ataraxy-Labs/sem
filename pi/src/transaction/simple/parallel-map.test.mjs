import test from 'node:test';
import assert from 'node:assert/strict';
import {parallelMap} from './parallel-map.mjs';
test('bounded concurrency preserves deterministic order',async()=>{
  let active=0,peak=0;
  const values=await parallelMap([30,10,20,1],2,async(value)=>{
    peak=Math.max(peak,++active);
    await new Promise(r=>setTimeout(r,value));active--;return value;
  });
  assert.equal(peak,2);assert.equal(active,0);assert.deepEqual(values,[30,10,20,1]);
});
test('failed workers join in-flight operations before rejecting',async()=>{
  let finished=false;
  await assert.rejects(parallelMap([0,1,2],2,async(value)=>{
    if(value===0){await new Promise(r=>setTimeout(r,5));throw Error('parse failed');}
    await new Promise(r=>setTimeout(r,20));finished=true;
  }),/parse failed/);
  assert.equal(finished,true);
  await assert.rejects(parallelMap([],0,()=>{}),/CONCURRENCY/);
});
