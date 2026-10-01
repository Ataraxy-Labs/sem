// Deterministic lexical edit planning, not semantic reference resolution.
// This function is also serialized into the IO-free program worker.
export function planLineRewrites(row, needle, transform, expectedLines) {
  if(!row || typeof row.file!=='string' || typeof row.content!=='string')
    throw Error('rewriteLines requires a complete input file');
  if(typeof needle!=='string'||!needle||/[\r\n]/.test(needle)||typeof transform!=='function')
    throw Error('rewriteLines requires a nonempty single-line literal and a function');
  if(expectedLines!==undefined&&(!Number.isSafeInteger(expectedLines)||expectedLines<1))
    throw Error('Invalid expectedLines');
  const source=row.content;
  const lines=source.match(/[^\n]*\n|[^\n]+$/g)||[];
  const changed=new Map();
  let matches=0;
  for(let i=0;i<lines.length;i++) {
    if(!lines[i].includes(needle))continue;
    matches++;
    const ending=lines[i].endsWith('\r\n')?'\r\n':lines[i].endsWith('\n')?'\n':'';
    const body=ending?lines[i].slice(0,-ending.length):lines[i];
    // The callback receives text, not its record separator. Empty means delete
    // the complete line; otherwise preserve the original separator exactly.
    const value=transform(body);
    const replacement=value===''?'':typeof value==='string'?value+ending:value;
    if(typeof replacement!=='string')throw Error('rewriteLines callback must return a string');
    if(replacement!==lines[i])changed.set(i,replacement);
  }
  if(!matches)throw Error('rewriteLines found no matching lines in '+row.file);
  if(expectedLines!==undefined&&matches!==expectedLines)
    throw Error('rewriteLines expected '+expectedLines+' matching lines, got '+matches);
  const ranges=[];
  for(const i of changed.keys()) {
    let start=i,end=i+1;
    while(true) {
      const anchor=lines.slice(start,end).join('');
      const at=source.indexOf(anchor);
      if(at>=0&&source.indexOf(anchor,at+1)<0)break;
      if(start===0&&end===lines.length)throw Error('Cannot construct unique rewrite anchor');
      if(start>0)start--;
      if(end<lines.length)end++;
    }
    ranges.push({start,end});
  }
  ranges.sort((a,b)=>a.start-b.start||a.end-b.end);
  const merged=[];
  for(const range of ranges) {
    const previous=merged[merged.length-1];
    if(previous&&range.start<=previous.end)previous.end=Math.max(previous.end,range.end);
    else merged.push({...range});
  }
  if(merged.length>64)throw Error('Too many disjoint rewrite regions; narrow the scope');
  return merged.map(({start,end})=>({file:row.file,
    old:lines.slice(start,end).join(''),
    new:lines.slice(start,end).map((line,index)=>changed.get(start+index)??line).join('')}));
}
