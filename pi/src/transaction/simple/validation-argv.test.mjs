import {test} from 'node:test';
import assert from 'node:assert/strict';
import {execFileSync} from 'node:child_process';
import {normalizeValidation} from './validation-argv.mjs';
test('argv survives checker parsing including regex, spaces and quotes',()=>{
  const argv=['go','test','-run','TestA|TestB','-exec','env NAME=value',"a'b",''];
  const result=normalizeValidation({validation_argv:argv,edits:[]});
  const roundtrip=JSON.parse(execFileSync('python',['-c','import json,shlex,sys; print(json.dumps(shlex.split(sys.argv[1])))',result.validation_cmd],{encoding:'utf8'}));
  assert.deepEqual(roundtrip,argv);assert.deepEqual(result.edits,[]);
  assert.equal(result.validation_argv,undefined);
});
test('reject conflicting commands and malformed argv',()=>{
  assert.throws(()=>normalizeValidation({validation_cmd:'go test',validation_argv:['go']}),/not both/);
  for(const argv of [[],['go',null],['go','bad\nvalue'],Array(129).fill('x')])assert.throws(()=>normalizeValidation({validation_argv:argv}));
  assert.deepEqual(normalizeValidation({validation_cmd:'go test'}),{validation_cmd:'go test'});
});
