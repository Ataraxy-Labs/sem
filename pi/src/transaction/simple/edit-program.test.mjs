import {test} from 'node:test';
import assert from 'node:assert/strict';
import {generateEdits} from './edit-program.mjs';
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
