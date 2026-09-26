import test from 'node:test';
import assert from 'node:assert/strict';
import { qualifyEntities, indexEntities, lookupEntities } from './entity-lookup.mjs';

function reference(entities) {
  for (const e of entities) {
    const parents = entities.filter(p => p.file === e.file && p.start <= e.start && p.end >= e.end &&
      (p.start < e.start || p.end > e.end)).sort((a,b) => a.start-b.start || b.end-a.end);
    e.qualified_name = [...parents.map(p => p.name), e.name].join('.');
  }
}
test('containment sweep exactly matches all-pairs including crossing/equal/zero ranges', () => {
  let seed = 712;
  const random = n => { seed = (Math.imul(seed,1664525)+1013904223) >>> 0; return seed % n; };
  for (let trial = 0; trial < 200; trial++) {
    const rows = Array.from({length:100}, (_,i) => {
      const start = random(30);
      return {id:String(i), name:`e${i}`, file:`f${random(4)}`, start, end:start+random(30)};
    });
    const expected = structuredClone(rows);
    reference(expected); qualifyEntities(rows);
    assert.deepEqual(rows,expected);
  }
});
test('indexes preserve order, ambiguity, qualifiers and selector filters', () => {
  const rows = [
    {id:'a',name:'X',qualified_name:'X',file:'a',type:'class'},
    {id:'b',name:'f',qualified_name:'X.f',file:'a',type:'function'},
    {id:'c',name:'f',qualified_name:'X.f',file:'b',type:'function'},
    {id:'d',name:'X.f',qualified_name:'Z.X.f',file:'c',type:'variable'},
  ];
  const snapshot = indexEntities(rows);
  for (const selector of [{name:'X'},{name:'f'},{name:'X.f'},{name:'X.f',file:'a'},
    {name:'X.f',type:'variable'},{id:'c'},{id:'c',file:'a'},{id:'missing'},{name:'missing'}]) {
    const expected = rows.filter(e => (selector.id ? e.id===selector.id : e.name===selector.name||e.qualified_name===selector.name) &&
      (!selector.file||e.file===selector.file)&&(!selector.type||e.type===selector.type));
    assert.deepEqual(lookupEntities(snapshot,selector),expected);
  }
});
