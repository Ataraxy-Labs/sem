import {acknowledgeDefinitions, receiptFor} from './repair-contract.mjs';

// Opt-in transport contract: the caller asserts that source delivered in the
// named epoch is still in its context. Rotate after compaction/context loss.
// Without that assertion, full source is always sent. This is not a graph cache.
export class ContextReceipts {
  constructor(limit=128) {
    this.limit=limit; this.epoch=null; this.receipts=new Set();
    this.sequence=0; this.pending=[];
  }
  receipt() { return this.epoch ? {context_epoch:this.epoch,ack_sequence:this.sequence} : null; }
  presentBundle({definitions=[],files=[]}, options={}) {
    const tagged=[...definitions.map(d=>({...d,_source_kind:'definition'})),
      ...files.map(d=>({...d,_source_kind:'file'}))];
    const result=this.present(tagged,options).map(({_source_kind,...d})=>d);
    return {definitions:result.slice(0,definitions.length),files:result.slice(definitions.length),
      context_receipt:this.receipt()};
  }
  present(definitions, {context_epoch, ack_sequence, refresh_source=false, known_receipts=[]}={}) {
    if (context_epoch !== undefined && (typeof context_epoch!=='string'
        || context_epoch.length<1 || context_epoch.length>80)) {
      throw new Error('context_epoch must be a nonempty string of at most 80 characters');
    }
    if (ack_sequence!==undefined && (!Number.isSafeInteger(ack_sequence)||ack_sequence<0)) {
      throw new Error('ack_sequence must be a nonnegative safe integer');
    }
    if (refresh_source || context_epoch!==this.epoch) {
      this.receipts.clear();
      this.pending=[];
      this.epoch=context_epoch ?? null;
    } else if (context_epoch && ack_sequence===this.sequence) {
      // Only explicitly acknowledged delivery can authorize source omission.
      // A lost response cannot silently populate the client's context.
      for (const receipt of this.pending) {
        this.receipts.delete(receipt);
        this.receipts.add(receipt);
        while (this.receipts.size>this.limit) this.receipts.delete(this.receipts.values().next().value);
      }
    }
    const known=refresh_source ? [] : [...known_receipts, ...(context_epoch ? this.receipts : [])];
    const result=acknowledgeDefinitions(definitions,known);
    this.sequence++;
    this.pending=context_epoch ? definitions.filter(d=>!d.truncated&&typeof d.content==='string')
      .map(receiptFor).slice(-this.limit) : [];
    return result.map((d,i)=>{
      if (!d.reused) return d;
      const reference=context_epoch
        ? {...d,reason:'Caller asserts earlier source remains available in this context epoch',context_epoch}
        : d;
      const full={...definitions[i],receipt:receiptFor(definitions[i])};
      // Tiny bodies cost less than a reference's explanatory metadata.
      return Buffer.byteLength(JSON.stringify(reference)) < Buffer.byteLength(JSON.stringify(full))
        ? reference : full;
    });
  }
}
