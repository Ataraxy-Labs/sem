import test from 'node:test';
import assert from 'node:assert/strict';
import {RequestScheduler,requestKind} from './request-scheduler.mjs';
const deferred=()=>{let resolve;const promise=new Promise(r=>resolve=r);return {promise,resolve};};
const tick=()=>new Promise(r=>setImmediate(r));
test('reads overlap a check but mutations wait; queued order is preserved',async()=>{
  const s=new RequestScheduler(),gate=deferred(),events=[];
  const validation=s.run('validation',async()=>{events.push('check');await gate.promise;events.push('checked');});
  const read=s.run('read',async()=>{events.push('read');});
  const write=s.run('write',async()=>{events.push('write');});
  const after=s.run('read',async()=>{events.push('after');});
  await read; assert.deepEqual(events,['check','read']);
  gate.resolve(); await Promise.all([validation,write,after]);
  assert.deepEqual(events,['check','read','checked','write','after']);
});
test('checks never overlap and errors release the queue',async()=>{
  const s=new RequestScheduler(),gate=deferred(),events=[];
  const first=s.run('validation',async()=>{await gate.promise;throw new Error('failed');});
  const failure=assert.rejects(first,/failed/);
  const second=s.run('validation',async()=>events.push('second'));
  await tick();assert.deepEqual(events,[]);gate.resolve();
  await Promise.all([failure,second]);assert.deepEqual(events,['second']);
});
test('validation cannot start during an edit',async()=>{
  const s=new RequestScheduler(),gate=deferred(),events=[];
  const edit=s.run('write',()=>gate.promise);
  const check=s.run('validation',()=>events.push('check'));
  await tick();assert.deepEqual(events,[]);gate.resolve();
  await Promise.all([edit,check]);assert.deepEqual(events,['check']);
});
test('only known nonmutating operations are eligible',()=>{
  assert.equal(requestKind('sem_plan',{}),'read');
  assert.equal(requestKind('sem_exact',{op:'query'}),'read');
  assert.equal(requestKind('sem_exact',{op:'apply'}),'write');
  assert.equal(requestKind('weave_transaction',{validation_cmd:'mvn test'}),'validation');
  assert.equal(requestKind('weave_transaction',{validation_cmd:'mvn test',edits:[{}]}),'write');
  assert.equal(requestKind('weave_transaction',{validation_cmd:'mvn test',unknown:true}),'write');
});
