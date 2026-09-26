import assert from 'node:assert/strict';
import { qualifyEntities, indexEntities, lookupEntities } from './entity-lookup.mjs';
import { ExactCode } from './exact-code.mjs';

// Synthetic systems microbenchmark; not a coding-task or token-efficiency result.
let count = Number(process.argv[2] ?? 10000);
let source = process.argv[2] === '--repo' ? null : Array.from({length:count},(_,i) => ({id:String(i),name:`fn${i}`,
  file:`src/f${Math.floor(i/250)}.ts`,type:'function',start:(i%250)*20,end:(i%250)*20+15}));
if (!source) {
  const exact = new ExactCode({semBin:process.env.SEM_TEST_BIN ?? 'sem'});
  const captured = await exact.capture(process.argv[3],process.argv.slice(4));
  source = exact.get(captured.revision).entities;
  count = source.length;
}
if (!count) throw Error('No entities to benchmark');
function baseline(rows) {
  for (const e of rows) {
    const parents=rows.filter(p=>p.file===e.file&&p.start<=e.start&&p.end>=e.end&&
      (p.start<e.start||p.end>e.end)).sort((a,b)=>a.start-b.start||b.end-a.end);
    e.qualified_name=[...parents.map(p=>p.name),e.name].join('.');
  }
}
const measure = fn => { const start=performance.now(); const value=fn(); return {ms:performance.now()-start,value}; };
for(let trial=0;trial<3;trial++) {
  const a=structuredClone(source), b=structuredClone(source);
  let before,after;
  const old=()=>{before=measure(()=>baseline(a));};
  const fresh=()=>{after=measure(()=>{qualifyEntities(b);return indexEntities(b);});};
  if(trial%2){fresh();old();}else{old();fresh();}
  assert.deepEqual(a,b);
  const names=Array.from({length:2000},(_,i)=>source[(i*7919)%count].name);
  const slow=measure(()=>names.map(name=>a.filter(e=>e.name===name||e.qualified_name===name)));
  const fast=measure(()=>names.map(name=>lookupEntities(after.value,{name})));
  assert.deepEqual(slow.value,fast.value);
  console.log(JSON.stringify({trial,corpus:process.argv[2]==='--repo'?'repository':'synthetic',entities:count,queries:names.length,
    baseline_build_ms:before.ms,indexed_build_ms:after.ms,
    baseline_queries_ms:slow.ms,indexed_queries_ms:fast.ms,identical:true}));
}
