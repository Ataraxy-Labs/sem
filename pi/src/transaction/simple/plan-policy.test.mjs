import { test } from 'node:test';
import assert from 'node:assert/strict';
import { boundedInteger, inScope, candidatePage, MAX_PLAN_CALLS, focusedCheck, compactEditReceipt, packDefinitions, compactDefinition } from './plan-policy.mjs';
test('pages expose all ambiguous definitions without gaps', () => {
  const hits = Array.from({length: 23}, (_,i) => i);
  const seen = []; let offset = 0;
  do { const p = candidatePage(hits, offset, 4); seen.push(...p.hits); offset = p.next_offset; }
  while (offset !== null);
  assert.deepEqual(seen, hits);
});
test('scope distinguishes a file and directory from prefix siblings', () => {
  assert.ok(inScope('internal/backend/core.go','./internal/backend/'));
  assert.ok(inScope('internal/backend/core.go','internal/backend/core.go'));
  assert.ok(!inScope('internal/backends/core.go','internal/backend'));
});
test('bounds reject invalid input, clamp oversize and permit recovery calls', () => {
  assert.equal(boundedInteger(160,10,1,16),16);
  for (const n of [-1, 1.5, NaN, '3']) assert.throws(() => boundedInteger(n,10,1,16));
  assert.ok(MAX_PLAN_CALLS > 1 && MAX_PLAN_CALLS <= 8);
});
test('rejected focused validation never falls back to whole-suite execution', async () => {
  const calls=[];
  const result=await focusedCheck({check:async args=>{calls.push(args);throw new Error('runner rejected');}},'ENV=off go test ./part');
  assert.deepEqual(calls,[{cmd:'ENV=off go test ./part'}]);
  assert.equal(result.pass,null);
  assert.equal(result.stage,'unavailable');
});
test('accepted focused checks keep their original result', async () => {
  const result={pass:true};
  assert.equal(await focusedCheck({check:async args=>{assert.equal(args.cmd,'go test ./part');return result;}},'go test ./part'),result);
});
test('compact receipts retain verification and preserve full failure evidence', () => {
  const bad={isError:true,details:{failed:1,reason:'identity conflict'}};
  assert.equal(compactEditReceipt(bad),bad.details);
  const good=compactEditReceipt({details:{succeeded:1,results:[{file:'a',details:{verification:{ok:true},dependents:{before:['lots']}}}]}});
  assert.deepEqual(good.results[0].verification,{ok:true});
  assert.equal(good.dependents.checked,false);
  assert.ok(!JSON.stringify(good).includes('lots'));
});
test('many small definitions fit without an artificial sixteen-entity cutoff', () => {
  const defs=Array.from({length:64},(_,i)=>({file:`${i}.go`,entity:{name:'f'},content:'func f() {}'}));
  const result=packDefinitions(defs);
  assert.deepEqual(result.definitions,defs);
  assert.equal(result.deferred,0);
});
test('oversized definitions are explicitly deferred, never silently truncated', () => {
  const large={file:'large',entity:{name:'x'},content:'é'.repeat(1000)};
  const small={file:'small',entity:{name:'y'},content:'y'};
  const result=packDefinitions([large,small],100);
  assert.equal(result.definitions[0].deferred,true);
  assert.equal(result.definitions[0].content,undefined);
  assert.deepEqual(result.definitions[1],small);
  assert.ok(result.full_definition_bytes<=100);
});
test('compact context preserves content, ranges, errors and truncation without mutating input', () => {
  const d={file:'a.go',entity:{file:'a.go',name:'f',start_line:1,end_line:3,parent_name:null},content:'func f() {}',truncated:true,budget:2000,related:[],error:'partial'};
  const r=compactDefinition(d);
  assert.equal(r.content,d.content);
  assert.equal(r.entity.end_line,3);
  assert.equal(r.truncated,true);
  assert.equal(r.error,'partial');
  assert.equal(r.entity.file,undefined);
  assert.equal(d.entity.file,'a.go');
  assert.deepEqual(compactDefinition({...d,related:[{name:'g'}]}).related,[{name:'g'}]);
});
