import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {normalizeEdits} from './normalize-edits-simple.mjs';

test('replace plus boundary insertions compose without another agent round trip', async () => {
  const cwd = await fs.mkdtemp(path.join(os.tmpdir(), 'sem-compose-'));
  const source = 'impl Foo {\n    fn value() -> i32 { 1 }\n}\n';
  const entity = {name:'Foo', entity_type:'impl'};
  const api = {outline:async()=>({entities:[{name:'Foo',type:'impl',start_line:1,end_line:3}]})};
  try {
    await fs.writeFile(path.join(cwd,'a.rs'),source);
    for (const changeFirst of [true,false]) {
      const change = {file:'a.rs',old:'{ 1 }',new:'{ 2 }'};
      const additions = [
        {file:'./a.rs',entity,op:'insert_before',content:'// before'},
        {file:'a.rs',entity,op:'insert_after',content:'// first'},
        {file:'a.rs',entity,op:'insert_after',content:'// second'},
      ];
      const result = await normalizeEdits(changeFirst ? [change,...additions] : [...additions,change],cwd,api);
      assert.equal(result.edits.length,1);
      assert.equal(result.edits[0].content,'// before\nimpl Foo {\n    fn value() -> i32 { 2 }\n}\n// first\n// second');
      assert.equal(await fs.readFile(path.join(cwd,'a.rs'),'utf8'),source);
    }
    await assert.rejects(normalizeEdits([
      {file:'a.rs',entity,content:'impl Foo {}'},
      {file:'a.rs',entity,content:'impl Foo { /* conflicting */ }'},
    ],cwd,api), /OVERLAPPING_ENTITY_EDITS/);
  } finally { await fs.rm(cwd,{recursive:true}); }
});
