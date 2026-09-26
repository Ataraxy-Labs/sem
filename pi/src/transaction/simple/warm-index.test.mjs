import test from 'node:test';
import assert from 'node:assert/strict';
import {hasMutations,refreshIndex} from './warm-index.mjs';
test('refresh only mutation attempts, never validation-only calls',()=>{
  assert.equal(hasMutations({validation_cmd:'test'}),false);
  for(const key of ['edits','creates','imports','remove_imports'])assert.equal(hasMutations({[key]:[{}]}),true);
});
test('missing index executable is explicitly unavailable',async()=>{
  const result=await refreshIndex(process.cwd(),'/nonexistent-sem-warm-test');
  assert.equal(result.ok,false);assert.match(result.error,/ENOENT/);
});
