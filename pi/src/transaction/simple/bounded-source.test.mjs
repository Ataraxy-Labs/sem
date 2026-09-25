import test from 'node:test';
import assert from 'node:assert/strict';
import {boundedSource} from './bounded-source.mjs';

test('only complete source can be marked complete', () => {
  for (const source of ['', 'abc', 'a😀z', 'const parent = {\n  nested() {}\n};']) {
    for (let budget = 1; budget <= source.length + 1; budget++) {
      const result = boundedSource(source, budget);
      assert.equal(result.truncated, result.content !== source);
      assert.ok(result.content.length <= budget);
      assert.ok(source.startsWith(result.content));
    }
  }
});
test('does not split surrogate pairs', () => {
  assert.deepEqual(boundedSource('a😀z', 2), {content:'a', truncated:true});
  assert.deepEqual(boundedSource('a😀z', 3), {content:'a😀', truncated:true});
});
test('invalid budgets fail explicitly', () => {
  for (const budget of [0,-1,1.5,NaN,Infinity]) assert.throws(() => boundedSource('abc',budget), RangeError);
});
