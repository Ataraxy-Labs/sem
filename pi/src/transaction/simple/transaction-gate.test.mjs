import { test } from 'node:test';
import assert from 'node:assert/strict';
import { editFailed } from './transaction-gate.mjs';
test('rollback or partial failure cannot enter successful validation path', () => {
  assert.equal(editFailed({details:{rolledBack:true,succeeded:14}}),true);
  assert.equal(editFailed({details:{results:[{isError:true}]}}),true);
  assert.equal(editFailed({isError:true}),true);
  assert.equal(editFailed({details:{succeeded:34,rolledBack:false}}),false);
});
