import {test} from 'node:test';
import assert from 'node:assert/strict';
import {matchInventory} from './discovery-inventory.mjs';

test('pagination completeness does not claim repository coverage', () => {
  const result=matchInventory({pattern:'x',total:0,coverage:'parser_supported_files_only'},[]);
  assert.equal(result.complete,true);
  assert.equal(result.completeness_scope,'returned_search_results_not_repository_coverage');
  assert.equal(result.coverage,'parser_supported_files_only');
  assert.equal(matchInventory({total:0},[]).coverage,'unspecified_upstream_search_scope');
});
