import test from 'node:test';
import assert from 'node:assert/strict';
import {createJevRanker} from './jev-ranker.mjs';
const fixture=()=>({coverage:'partial',matches:{results:[{next_offset:8,hits:Array.from({length:8},(_,i)=>({file:`f${i}`,line:1,text:`source ${i}`}))}]}});
const response=()=>({ok:true,json:async()=>({model:'mock',usage:{input_tokens:10,output_tokens:8},answers:Object.fromEntries(Array.from({length:8},(_,i)=>[`c${i}`,{score:i/4}]))})});
test('stable ranking retains every hit and coverage; exact repeat is cached',async()=>{
  let calls=0;const rank=createJevRanker({key:'test',task:'task',fetchImpl:async()=>{calls++;return response();}});
  const input=fixture(),out=await rank(input);
  assert.equal(out.matches.results[0].hits[0].file,'f7');
  assert.deepEqual([...out.matches.results[0].hits].sort((a,b)=>a.file.localeCompare(b.file)),input.matches.results[0].hits);
  assert.equal(out.matches.results[0].next_offset,8);assert.equal(out.coverage,'partial');
  assert.equal((await rank(input)).rerank.cached,true);assert.equal(calls,1);
  assert.equal(input.matches.results[0].hits[0].file,'f0');
});
test('failure keeps original ordering and caps attempts',async()=>{
  let calls=0;const rank=createJevRanker({key:'test',task:'task',maxCalls:1,fetchImpl:async()=>{calls++;throw Error('secret must not leak');}});
  const input=fixture(),out=await rank(input);assert.deepEqual(out.matches,input.matches);
  assert.ok(!JSON.stringify(out).includes('secret'));await rank(input);assert.equal(calls,1);
});
test('missing credentials and narrow queries do not call API',async()=>{
  const fail=async()=>{throw Error('should not call');};
  assert.equal(await createJevRanker({fetchImpl:fail})(fixture()).then(x=>x.rerank.reason),'missing_credential');
  const input=fixture();input.matches.results[0].hits.pop();
  const output=await createJevRanker({key:'test',task:'task',fetchImpl:fail})(input);
  assert.deepEqual(output.matches,input.matches);
  assert.equal(output.rerank.reason,'candidate_count');
});
test('invalid answers fall back, low separation preserves order',async()=>{
  for (const score of [NaN,3,1]) {
    const rank=createJevRanker({key:'test',task:'task',fetchImpl:async()=>({ok:true,json:async()=>({answers:Object.fromEntries(Array.from({length:8},(_,i)=>[`c${i}`,{score}]))})})});
    const input=fixture(),out=await rank(input);assert.deepEqual(out.matches,input.matches);
    assert.equal(out.rerank.status,score===1?'unchanged_low_separation':'fallback');
  }
});
