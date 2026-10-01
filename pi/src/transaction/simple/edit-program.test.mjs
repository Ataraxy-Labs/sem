import {test} from 'node:test';
import assert from 'node:assert/strict';
import {generateEdits} from './edit-program.mjs';
import {selectEntity} from './repair-contract.mjs';
test('replaceEntity preserves overloaded method identity and rejects stale locations',async()=>{
  const outline=[10,20].map(start_line=>({name:'create',type:'method',parent_name:'Region',start_line}));
  const rows=outline.map(entity=>({file:'Region.java',entity,content:'void create() { old(); }'}));
  const result=await generateEdits('return {edits:[replaceEntity(entities[1],"old","new")]};',rows);
  assert.equal(result.edits[0].entity.start_line,20);
  assert.equal(selectEntity(outline,result.edits[0].entity),outline[1]);
  assert.throws(()=>selectEntity(outline,{...result.edits[0].entity,start_line:21}),/Ambiguous or incompatible/);
});
test('create-only programs normalize without accepting malformed edits',async()=>{
  assert.deepEqual(await generateEdits('return {creates:[{file:"new.go",content:"package test"}]};'),{creates:[{file:'new.go',content:'package test'}],edits:[]});
  for(const code of ['return {creates:[]};','return {creates:[{}]};','return {creates:[{file:"a",content:"b"}],edits:null};','return {creates:[{file:"a",content:"b"}],typo:[]};'])
    await assert.rejects(generateEdits(code),/at most 64 edits/);
});
test('replaceEntity carries parser selectors and validates the local anchor',async()=>{
  const row={file:'a.py',entity:{name:'run',type:'method',parent_name:'Agent'},content:'def run(): return old'};
  assert.deepEqual(await generateEdits('return {edits:[replaceEntity(entities[0],"old","new")]};',[row]),
    {edits:[{file:'a.py',entity:{name:'run',entity_type:'method',parent_name:'Agent'},old:'old',new:'new'}]});
  assert.equal(row.entity.entity_type,undefined);
  await assert.rejects(generateEdits('return {edits:[replaceEntity(entities[0],"missing","new")]};',[row]),/exactly once/);
  await assert.rejects(generateEdits('return {edits:[replaceEntity(entities[0],"old","new")]};',
    [{...row,content:'old old'}]),/exactly once/);
  await assert.rejects(generateEdits('return {edits:[replaceEntity({...entities[0]},"old","new")]};',[row]),/provided inputs/);
});
test('validation-only program preserves command and makes no implicit edits',async()=>{
  const command='python -m pytest tests/test_example.py -q';
  assert.deepEqual(await generateEdits('return {validation_cmd:'+JSON.stringify(command)+'};'),
    {validation_cmd:command,edits:[]});
  for(const code of ['return {validation_cmd:""};','return {validation_cmd:"  "};',
    'return {validation_cmd:"pytest",edits:null};','return {validation_cmd:"pytest",typo:[]};'])
    await assert.rejects(generateEdits(code),/at most 64 edits/);
});
test('read and edit type spellings select the same cached entity without changing cache',async()=>{
  for(const field of ['type','entity_type']) {
    const row=Object.freeze({file:'a.java',entity:Object.freeze({name:'PhysicalType',[field]:'enum'}),content:'enum PhysicalType {}'});
    const result=await generateEdits('const a=entities.find(e=>e.entity.entity_type==="enum"); const b=entities.find(e=>e.entity.type==="enum"); if(a!==b)throw new Error("mismatch"); return {edits:[{file:a.file,old:a.content,new:"enum PhysicalType { VALUE }"}]};',[row]);
    assert.equal(result.edits.length,1);
    assert.equal(Object.keys(row.entity).length,2);
  }
});
test('conflicting type metadata fails before generating edits',()=>{
  assert.throws(()=>generateEdits('return {edits:[]};',[{entity:{type:'class',entity_type:'enum'}}]),/Conflicting cached entity/);
});
test('generates repetitive edit data without copying source into the request',async()=>{
  const result=await generateEdits('return {edits:entities.map(d=>({file:d.file,old:d.content,new:d.content.replace("old","new")}))};',[{file:'a',content:'old'}]);
  assert.deepEqual(result,{edits:[{file:'a',old:'old',new:'new'}]});
});
test('invalid results and unavailable host globals fail before mutation',async()=>{
  await assert.rejects(generateEdits('return process.env;'),/process is not defined/);
  await assert.rejects(generateEdits('return {};'),/at most 64 edits/);
  await assert.rejects(generateEdits('return {edits:[], value: Function("return process")()};'),/Code generation from strings disallowed/);
});
test('loops and serializer loops are time-bounded',async()=>{
  await assert.rejects(generateEdits('while(true){}'),/timed out/);
  await assert.rejects(generateEdits('return {toJSON(){while(true){}}};'),/timed out/);
});
test('oversized batches and asynchronous results are rejected',async()=>{
  await assert.rejects(generateEdits('return {edits:Array(65).fill({})};'),/at most 64 edits/);
  await assert.rejects(generateEdits('return Promise.resolve({edits:[]});'),/at most 64 edits/);
  await assert.rejects(generateEdits('return {edits:[],payload:"x".repeat(500001)};'),/bounded JSON/);
  assert.throws(()=>generateEdits(' '.repeat(30001)),/30KB/);
});
