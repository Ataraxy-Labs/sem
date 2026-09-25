import test from 'node:test';
import assert from 'node:assert/strict';
import {pythonModuleHint} from './import-hints.mjs';

test('other languages never become Python module authority',()=>{
  for(const file of ['src/lib.rs','src/index.ts','src/index.tsx','foo.py/source.rs','src/Foo.java',null]) {
    assert.equal(pythonModuleHint(file),null);
  }
});
test('Python hints use module or package basename, not extension',()=>{
  assert.equal(pythonModuleHint('src/pkg/callback.py'),'callback');
  assert.equal(pythonModuleHint('src/pkg/__init__.py'),'pkg');
  assert.equal(pythonModuleHint('C:\\src\\pkg\\callback.py'),'callback');
  assert.equal(pythonModuleHint('__init__.py'),null);
  assert.equal(pythonModuleHint('pkg/invalid-name.py'),null);
});
