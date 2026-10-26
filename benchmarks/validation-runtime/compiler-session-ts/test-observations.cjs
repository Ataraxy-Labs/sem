const assert=require('node:assert/strict');
const {recorder,unchanged}=require('./compiler-observations.cjs');
let text='abc',missing=true;
const sys={
 readDirectory:(p,e,x,i)=>{assert.equal(i,undefined);return ['a.ts'];},
 readFile:p=>text,
 fileExists:p=>!missing
};
const r=recorder(sys);
r.sys.readDirectory('.',undefined,undefined,undefined);
r.sys.readFile('a.ts');r.sys.fileExists('missing.ts');
assert(unchanged(sys,r.observations));
text='abd';assert(!unchanged(sys,r.observations));
text='abc';missing=false;assert(!unchanged(sys,r.observations));
console.log('Receipt arguments, content changes, and negative lookups passed');
const {resolutionsUnchanged}=require('./compiler-observations.cjs');
const files=new Map([['source.ts','export const n=1'],['package.json','{"types":"a.d.ts"}']]);
const moduleSys={readFile:p=>files.get(p),fileExists:p=>files.has(p)};
const recorded=recorder(moduleSys);
recorded.sys.readFile('source.ts');recorded.sys.readFile('package.json');recorded.sys.fileExists('missing.ts');
files.set('source.ts','export const n=2');assert(resolutionsUnchanged(moduleSys,recorded.observations));
files.set('package.json','{"types":"b.d.ts"}');assert(!resolutionsUnchanged(moduleSys,recorded.observations));
files.set('package.json','{"types":"a.d.ts"}');files.set('missing.ts','export {}');assert(!resolutionsUnchanged(moduleSys,recorded.observations));
files.delete('missing.ts');files.delete('source.ts');assert(!resolutionsUnchanged(moduleSys,recorded.observations));
console.log('Resolution inputs preserve content edits and invalidate package, missing-path and source deletion changes');
