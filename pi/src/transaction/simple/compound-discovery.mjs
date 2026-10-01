// Combine only independently specified work. No inferred neighbors or reads.
export function addCompoundDiscovery(plan,exact) {
 const original=plan.run;
 plan.schema.properties.reads={type:'object',properties:{
  files:exact.schema.properties.files,
  selectors:exact.schema.properties.selectors,
 },required:['files','selectors'],additionalProperties:false,
 description:'Optional explicit sem_exact query to run alongside this search. Mix file, entity and byte-range selectors; only requested context is returned. Do not repeat files here and in top-level files.'};
 plan.description+=' Batch independent search and exact reads using reads:{files,selectors}; each branch retains its errors and completeness information. No automatic related source is fetched.';
 plan.run=async(params,cwd)=>{
  if(params.reads===undefined)return original(params,cwd);
  const {reads,...search}=params;
  if(!reads||!Array.isArray(reads.files)||!reads.files.length||reads.files.length>64||
     reads.files.some(f=>typeof f!=='string')||!Array.isArray(reads.selectors)||
     !reads.selectors.length||reads.selectors.length>64||
     Object.keys(reads).some(k=>!['files','selectors'].includes(k)))
    throw Error('INVALID_EXPLICIT_READ_BATCH');
  const results=await Promise.allSettled([
   Promise.resolve().then(()=>original(search,cwd)),
   Promise.resolve().then(()=>exact.run({op:'query',files:reads.files,selectors:reads.selectors},cwd))
  ]);
  const branch=r=>r.status==='fulfilled'?{status:'ok',result:r.value}:
    {status:'error',error:String(r.reason?.message??r.reason)};
  return {search:branch(results[0]),reads:branch(results[1]),
   consistency:'independent read snapshots; inspect each branch and its coverage; not repository-wide isolation'};
 };
}
