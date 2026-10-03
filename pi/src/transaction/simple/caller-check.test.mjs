import assert from 'node:assert/strict';
import test from 'node:test';
import { signatureEdits, uncoveredDependents, dependentsBeforeEdit, callerReview } from './caller-check.mjs';

const edits = [
  { file: 'lib.go', entity: { name: 'Open' }, op: 'replace', allow_signature_change: true },
  { file: 'repo.go', entity: { name: 'Load' }, op: 'replace' },
  { file: 'old.go', entity: { name: 'Legacy' }, op: 'delete' },
];

test('only deletions and intentional signature changes are checked', () => {
  assert.deepEqual(signatureEdits(edits).map(edit => edit.entity.name), ['Open', 'Legacy']);
});

test('dependents edited in the same batch are covered', () => {
  const dependents = [
    { file: 'repo.go', name: 'Load', type: 'function' },
    { file: 'cmd.go', name: 'main', type: 'function' },
    { file: 'cmd.go', name: 'main', type: 'function' },
  ];
  assert.deepEqual(uncoveredDependents(dependents, edits), [{ file: 'cmd.go', name: 'main', type: 'function' }]);
});

test('review reports unedited dependents and unavailable lookups', async () => {
  const fake = async (_bin, _cwd, _file, name) => name === 'Open'
    ? { ok: true, dependents: [{ file: 'cmd.go', name: 'main', type: 'function' }, { file: 'repo.go', name: 'Load', type: 'function' }] }
    : { ok: false, reason: 'not indexed' };
  const before = await dependentsBeforeEdit(edits, '/repo', 'sem', fake);
  const review = callerReview(before, edits);
  assert.equal(review.signature_edits, 2);
  assert.equal(review.checked, 1);
  assert.equal(review.unavailable, 1);
  assert.deepEqual(review.unedited_dependents, [{ file: 'cmd.go', name: 'main', type: 'function' }]);
  assert.equal(callerReview({ targets: 0 }, edits), null);
});
