// Session-local parse metadata, keyed by repository/file/content fingerprint.
// Never stores source or editable IDs; every snapshot derives its own IDs.
export class ParseCache {
  constructor({maxEntries=256,maxBytes=16*1024*1024}={}) {
    if(!Number.isSafeInteger(maxEntries)||maxEntries<1||!Number.isSafeInteger(maxBytes)||maxBytes<1) throw new Error('INVALID_PARSE_CACHE_CAPACITY');
    this.maxEntries=maxEntries;this.maxBytes=maxBytes;this.bytes=0;this.entries=new Map();
  }
  get(key) {
    const entry=this.entries.get(key);
    if(!entry)return undefined;
    this.entries.delete(key);this.entries.set(key,entry);
    return entry.rows;
  }
  set(key,rows) {
    const frozen=Object.freeze(rows.map(row=>Object.freeze({...row})));
    const size=Buffer.byteLength(JSON.stringify(frozen))+Buffer.byteLength(key);
    if(this.entries.has(key)) {this.bytes-=this.entries.get(key).size;this.entries.delete(key);}
    if(size>this.maxBytes)return;
    this.entries.set(key,{rows:frozen,size});this.bytes+=size;
    while(this.entries.size>this.maxEntries||this.bytes>this.maxBytes) {
      const oldest=this.entries.keys().next().value;
      this.bytes-=this.entries.get(oldest).size;this.entries.delete(oldest);
    }
  }
}
