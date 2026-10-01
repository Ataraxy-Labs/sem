import {test} from 'node:test';
import assert from 'node:assert/strict';
import {planLineRewrites} from './line-rewrites.mjs';
import {generateEdits} from './edit-program.mjs';

function apply(source, edits) {
  const spans=edits.map(e=>{
    const start=source.indexOf(e.old);
    assert.ok(start>=0);
    assert.equal(source.indexOf(e.old,start+1),-1);
    return {start,end:start+e.old.length,text:e.new};
  }).sort((a,b)=>a.start-b.start);
  for(let i=1;i<spans.length;i++)assert.ok(spans[i-1].end<=spans[i].start);
  for(const s of spans.toReversed())source=source.slice(0,s.start)+s.text+source.slice(s.end);
  return source;
}
test('repeated identical lines are anchored and merged without changing neighbors',()=>{
  const content='head\nold\nseparator\nold\nold\ntail\n';
  const edits=planLineRewrites({file:'a',content},'old',line=>line.replaceAll('old','new'),3);
  assert.equal(apply(content,edits),content.replaceAll('old','new'));
});
test('line deletion and repeated replacements compose against original source',()=>{
  const content='import p.Old.Type;\nOld.Type a;\nOld.Type b;\nkeep\n';
  const edits=planLineRewrites({file:'a',content},'Old.Type',line=>
    line.startsWith('import p.Old.Type;')?'':line.replaceAll('Old.Type','Type'));
  assert.equal(apply(content,edits),'Type a;\nType b;\nkeep\n');
});
test('Iceberg import comparison excludes LF and CRLF and preserves other lines',()=>{
  for(const eol of ['\n','\r\n']) {
    const imp='import p.Old.Type;';
    const content=imp+eol+'Old.Type a;'+eol;
    const edits=planLineRewrites({file:'a',content},'Old.Type',line=>
      line===imp?'':line.replaceAll('Old.Type','Type'),2);
    assert.equal(apply(content,edits),'Type a;'+eol);
  }
});
test('unicode, CRLF, no trailing newline, and newlines in replacements are preserved',()=>{
  for(const content of ['😀old\r\nold\r\nkeep','old','old\nkeep\nold']) {
    const edits=planLineRewrites({file:'a',content},'old',line=>line.replaceAll('old','new\nextra'));
    assert.equal(apply(content,edits),content.replaceAll('old','new\nextra'));
  }
});
test('no-op, missing matches, wrong counts and invalid callbacks are explicit',()=>{
  const row={file:'a',content:'old\n'};
  assert.deepEqual(planLineRewrites(row,'old',line=>line),[]);
  assert.throws(()=>planLineRewrites(row,'absent',line=>line),/no matching/);
  assert.throws(()=>planLineRewrites(row,'old',line=>line,2),/expected 2/);
  assert.throws(()=>planLineRewrites(row,'old',()=>undefined),/return a string/);
  assert.throws(()=>planLineRewrites(row,'',line=>line),/nonempty/);
});
test('worker helper only accepts supplied files and preserves host restrictions',async()=>{
  const row=Object.freeze({file:'a',content:'old\nold\n'});
  const result=await generateEdits('return {edits:rewriteLines(files[0],"old",line=>line.replaceAll("old","new"),2)};',[],[row]);
  assert.equal(apply(row.content,result.edits),'new\nnew\n');
  await assert.rejects(generateEdits('return {edits:rewriteLines({...files[0]},"old",line=>line)};',[],[row]),/provided input/);
  await assert.rejects(generateEdits('return {edits:rewriteLines(files[0],"old",()=>process.env)};',[],[row]),/process is not defined/);
  assert.equal(row.content,'old\nold\n');
});
test('deterministic repeated-line stress cases produce exactly the requested text',()=>{
  for(let n=1;n<=80;n++) {
    const content=Array.from({length:n},(_,i)=>i%3===0?'repeat Old.Type\n':'keep\n').join('');
    const edits=planLineRewrites({file:'a',content},'Old.Type',line=>line.replaceAll('Old.Type','Type'));
    assert.equal(apply(content,edits),content.replaceAll('Old.Type','Type'));
  }
});
