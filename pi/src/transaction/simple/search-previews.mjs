// Bound source text, not match counts. Keep every locator and expose omissions.
export function searchPreviews(groups, {perHit=400, total=12000}={}) {
  let remaining=total;
  return groups.map(group=>({...group,hits:group.hits.map(hit=>{
    const text=hit.text??'', bytes=Buffer.byteLength(text);
    const limit=Math.min(perHit,remaining);
    let preview='';
    for(const char of text) {
      if(Buffer.byteLength(preview)+Buffer.byteLength(char)>limit) break;
      preview+=char;
    }
    remaining-=Buffer.byteLength(preview);
    return {...hit,text:preview,...(Buffer.byteLength(preview)<bytes?{
      text_truncated:true,original_text_bytes:bytes,
      retrieval:{file:hit.file,line:hit.line},
    }:{})};
  })}));
}
