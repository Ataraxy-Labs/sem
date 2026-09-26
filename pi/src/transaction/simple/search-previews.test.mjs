import {test} from 'node:test';
import assert from 'node:assert/strict';
import {searchPreviews} from './search-previews.mjs';
test('bounds Unicode previews without dropping locators or mutating search evidence',()=>{
  const groups=[{complete:true,hits:[{file:'a.js',line:2,text:'é'.repeat(50000)},{file:'b.py',line:3,text:'hello'}]}];
  const result=searchPreviews(groups,{perHit:7,total:9});
  assert.equal(result[0].hits[0].text,'ééé');
  assert.equal(result[0].hits[1].text,'hel');
  assert.equal(result[0].hits[0].original_text_bytes,100000);
  assert.deepEqual(result[0].hits[1].retrieval,{file:'b.py',line:3});
  assert.equal(groups[0].hits[0].text.length,50000);
  assert.equal(result[0].complete,true);
});
