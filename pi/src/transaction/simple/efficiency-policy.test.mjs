import assert from 'node:assert/strict';
import test from 'node:test';
import config from '../../../config/simple-transaction.mjs';
import { simpleEfficiencyPolicy } from './efficiency-policy.mjs';

test('simple config includes shared efficiency policy exactly once', () => {
  assert.ok(simpleEfficiencyPolicy.length > 0);
  assert.equal(config.systemPromptAddendum.split(simpleEfficiencyPolicy).length, 2);
  assert.deepEqual(config.sessionPolicy.activeBuiltins, []);
  assert.equal(config.servers[0].tools.sem_exact, true);
});

test('guidance preserves final-patch checks and honest coverage reporting', () => {
  assert.match(simpleEfficiencyPolicy, /required public gate and added regression tests against the final patch/);
  assert.match(simpleEfficiencyPolicy, /Do not omit required checks or conceal failures/);
  assert.match(simpleEfficiencyPolicy, /do not invent formatter capabilities or bypass tool restrictions/);
});
