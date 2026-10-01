// Serial source operations, with read-only overlap during validation-only calls.
// Writes and further validations wait for the active validation to finish.
export class RequestScheduler {
  #queue=[];
  #foreground=false;
  #validation=false;
  run(kind, operation) {
    if (!['read','write','validation'].includes(kind)) return Promise.reject(new Error('Unknown request kind'));
    return new Promise((resolve,reject)=>{
      this.#queue.push({kind,operation,resolve,reject});
      this.#pump();
    });
  }
  #pump() {
    if (this.#foreground || !this.#queue.length) return;
    const next=this.#queue[0];
    if (this.#validation && next.kind!=='read') return;
    this.#queue.shift();
    const validation=next.kind==='validation';
    if (validation) this.#validation=true;
    else this.#foreground=true;
    Promise.resolve().then(next.operation).then(next.resolve,next.reject).finally(()=>{
      if (validation) this.#validation=false;
      else this.#foreground=false;
      this.#pump();
    });
    if (validation) this.#pump();
  }
}

export function requestKind(name, params={}) {
  if (name==='sem_plan') return 'read';
  if (name==='sem_exact' && ['capture','resolve','read','query','prepare','orient','list','neighbors','diff'].includes(params.op)) return 'read';
  if (name==='weave_transaction' && typeof params.validation_cmd==='string' &&
      Object.keys(params).every(k=>k==='validation_cmd' ||
        (['edits','creates','imports','remove_imports'].includes(k) && Array.isArray(params[k]) && params[k].length===0))) {
    return 'validation';
  }
  return 'write'; // Unknown operations fail closed to exclusive scheduling.
}
