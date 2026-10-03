// Local implementation microbenchmark, NOT an agent-session speed claim.
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import assert from 'node:assert/strict';
import {ExactCode} from './exact-code.mjs';
import {presentExact} from './exact-receipts.mjs';
const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-efficiency-bench-'));
const files=Array.from({length:16},(_,i)=>`module_${i}.py`);
const source=Array.from({length:8},(_,i)=>`def function_${i}(value):\n${'    # explanatory source used to measure response transport\n'.repeat(12)}    return value + ${i}\n`).join('\n');
const median=xs=>[...xs].sort((a,b)=>a-b)[Math.floor(xs.length/2)];
try {
  await Promise.all(files.map(file=>fs.writeFile(path.join(root,file),source)));
  const timings={};let reference;
  for(const concurrency of [1,4]) {
    const samples=[];
    for(let run=0;run<5;run++) {
      const exact=new ExactCode({semBin:process.env.SEM_TEST_BIN||'sem',parseConcurrency:concurrency});
      const start=performance.now(),snapshot=await exact.capture(root,files);
      samples.push(performance.now()-start);
      const entities=exact.get(snapshot.revision).entities;
      if(!reference)reference=entities;else assert.deepEqual(entities,reference);
    }
    timings[concurrency]={median_ms:median(samples),samples_ms:samples};
  }
  const exact=new ExactCode({semBin:process.env.SEM_TEST_BIN||'sem'});
  await exact.capture(root,files);
  await fs.writeFile(path.join(root,files[0]),source+'\ndef added():\n    return 1\n');
  const start=performance.now(),snapshot=await exact.capture(root,files);
  const incremental={ms:performance.now()-start,...exact.lastCaptureParsing};
  assert.equal(incremental.parser_calls,1);assert.equal(incremental.cache_hits,15);
  const result=exact.query(snapshot.revision,[{name:'function_0',view:'source'}]);
  const full=presentExact(result);
  const reused=presentExact(result,{known_receipts:full.sources.map(s=>s.receipt)});
  console.log(JSON.stringify({kind:'implementation_microbenchmark',files:files.length,
    cold_capture:timings,incremental,acknowledged_response_bytes:{full:Buffer.byteLength(JSON.stringify(full)),reused:Buffer.byteLength(JSON.stringify(reused))}},null,2));
} finally {await fs.rm(root,{recursive:true,force:true});}
