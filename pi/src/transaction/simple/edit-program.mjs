// Local benchmark experiment, not a security boundary for hostile tenants.
// The worker only generates JSON; repository mutations remain in Weave.
import {Worker} from 'node:worker_threads';
const workerSource = `
const {parentPort,workerData}=require('node:worker_threads');
const vm=require('node:vm');
try {
 const context=vm.createContext(Object.create(null),{codeGeneration:{strings:false,wasm:false}});
 const source='JSON.stringify((function(){"use strict";const entities='+JSON.stringify(workerData.entities)+';const files='+JSON.stringify(workerData.files)+';'+workerData.code+'\\n})())';
 const value=new vm.Script(source).runInContext(context,{timeout:500});
 if(typeof value!=='string'||value.length>500000)throw new Error('Program must return bounded JSON');
 parentPort.postMessage({value});
}catch(error){parentPort.postMessage({error:String(error.message??error)});}
`;
export function generateEdits(code, entities=[], files=[]) {
  if(typeof code!=='string'||code.length>30000)throw new Error('Program exceeds 30KB');
  return new Promise((resolve,reject)=>{
    const worker=new Worker(workerSource,{eval:true,execArgv:[],env:{},workerData:{code,entities,files},resourceLimits:{maxOldGenerationSizeMb:32,stackSizeMb:2}});
    let settled=false;
    const finish=(error,value)=>{if(settled)return;settled=true;clearTimeout(timer);void worker.terminate();error?reject(error):resolve(value);};
    const timer=setTimeout(()=>finish(new Error('Edit generator timed out')),1500);
    worker.once('error',error=>finish(error));
    worker.once('exit',code=>{if(!settled)finish(new Error('Edit generator exited without a result: '+code));});
    worker.once('message',message=>{
      try {
        if(message.error)throw new Error(message.error);
        const value=JSON.parse(message.value);
        if(!value||!Array.isArray(value.edits)||value.edits.length>64)throw new Error('Return {edits:[...]} with at most 64 edits');
        finish(null,value);
      }catch(error){finish(error);}
    });
  });
}
