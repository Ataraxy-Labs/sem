import test from 'node:test';
import assert from 'node:assert/strict';
import {matchInventory, measuredLookup} from './discovery-inventory.mjs';
test('returns all 20 locators, including a late affected visitor', () => {
  const hits=Array.from({length:20},(_,line)=>({file:'a.ts',line,text:line===19?'NonBindableVisitor':'Visitor'}));
  const r=matchInventory({total:20},hits);
  assert.equal(r.returned,20); assert.equal(r.complete,true);
  assert.equal(r.hits[19].text,'NonBindableVisitor');
});
test('pagination never pretends omitted matches are complete', () => {
  const hits=Array.from({length:70},(_,line)=>({file:'a.ts',line}));
  const r=matchInventory({total:70},hits);
  assert.equal(r.next_offset,60);assert.equal(r.complete,false);
  assert.equal(matchInventory({total:70},hits,60).returned,10);
});
test('upstream cap requires narrowing rather than impossible pagination', () => {
  const r=matchInventory({total:100},Array.from({length:60},()=>({file:'a'})));
  assert.equal(r.upstream_truncated,true);assert.equal(r.complete,false);
  assert.equal(r.next_offset,null);assert.match(r.next_action,/Narrow/);
});
test('deduplicates lookups within a plan, including misses', async () => {
  let calls=0;const timings=[];const lookup=measuredLookup(async()=>{calls++;return {hits:[]}},timings);
  await Promise.all([lookup('missing'),lookup('missing')]);await lookup('missing');
  assert.equal(calls,1);assert.equal(timings.length,1);
  await measuredLookup(async()=>{calls++;return {hits:[]}},[])('missing');
  assert.equal(calls,2);
});
test('errors remain errors and are not disguised as empty hits', async () => {
  const timings=[];const lookup=measuredLookup(async()=>{throw new Error('broken')},timings);
  await assert.rejects(lookup('x'),/broken/);await assert.rejects(lookup('x'),/broken/);
  assert.equal(timings.length,1);assert.match(timings[0].error,/broken/);
});
