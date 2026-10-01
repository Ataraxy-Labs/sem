import {test} from 'node:test';
import assert from 'node:assert/strict';
import {programSource} from './program-source.mjs';
test('parser span and full-line span have explicit lossless UTF8 boundaries',()=>{
 const bytes=Buffer.from('// 😀\n  public enum Kind { A }\n');
 const start=bytes.indexOf('public'),end=bytes.indexOf('}')+1;
 const row=programSource(bytes,{start,end});
 assert.equal(row.content,'public enum Kind { A }');
 assert.equal(row.line_content,'  public enum Kind { A }');
 for(const [field,range] of [['content','source_range'],['line_content','line_range']])
  assert.equal(bytes.subarray(row[range].start_byte,row[range].end_byte).toString(),row[field]);
});
test('same-line declarations are never silently included in parser content',()=>{
 const bytes=Buffer.from('class A {} class B {}');
 const row=programSource(bytes,{start:11,end:21});
 assert.equal(row.content,'class B {}');
 assert.equal(row.line_content,bytes.toString());
});
