// Local benchmark experiment, not a security boundary for hostile tenants.
// The worker only generates JSON; repository mutations remain in Weave.
import {Worker} from 'node:worker_threads';
import {planLineRewrites} from './line-rewrites.mjs';
const workerSource = `
const {parentPort,workerData}=require('node:worker_threads');
const vm=require('node:vm');
try {
 const context=vm.createContext(Object.create(null),{codeGeneration:{strings:false,wasm:false}});
 const helpers=String.raw\`
 function replaceEntity(row,old,value) {
   if(!entities.includes(row)||!row.entity||typeof row.file!=='string'||typeof row.content!=='string')
     throw new Error('replaceEntity requires an entity from the provided inputs');
   if(typeof old!=='string'||!old||typeof value!=='string')
     throw new Error('replaceEntity requires nonempty old and string new');
   if(row.content.split(old).length!==2)
     throw new Error('replaceEntity anchor must occur exactly once in '+row.file+':'+row.entity.name);
   const {name,entity_type,parent_name,start_line}=row.entity;
   if(typeof name!=='string'||!name||typeof entity_type!=='string'||!entity_type)
     throw new Error('replaceEntity requires parser-provided name and kind');
   return {file:row.file,entity:{name,entity_type,...(parent_name?{parent_name}:{}),...(Number.isInteger(start_line)?{start_line}:{})},old,new:value};
 }
 \`;
 const rewriteHelper='const planLineRewrites=('+workerData.linePlanner+'); function rewriteLines(row,needle,transform,expectedLines){if(!files.includes(row))throw Error("rewriteLines requires a provided input file");return planLineRewrites(row,needle,transform,expectedLines);}';
 const source='JSON.stringify((function(){"use strict";const entities='+JSON.stringify(workerData.entities)+';const files='+JSON.stringify(workerData.files)+';'+helpers+rewriteHelper+workerData.code+'\\n})())';
 const value=new vm.Script(source).runInContext(context,{timeout:500});
 if(typeof value!=='string'||value.length>500000)throw new Error('Program must return bounded JSON');
 parentPort.postMessage({value});
}catch(error){parentPort.postMessage({error:String(error.message??error)});}
`;
export function generateEdits(code, entities=[], files=[]) {
  if(typeof code!=='string'||code.length>30000)throw new Error('Program exceeds 30KB');
  // Read results historically use type; edit selectors use entity_type. Expose
  // both spellings with the same parser-provided value, never infer a kind.
  // Copy rather than mutate cache entries shared with later reads.
  entities=entities.map(row=>{
    if(!row.entity)return row;
    const {type,entity_type}=row.entity;
    if(type!==undefined&&entity_type!==undefined&&type!==entity_type)
      throw new Error('Conflicting cached entity type and entity_type');
    const kind=type??entity_type;
    return kind===undefined?row:{...row,entity:{...row.entity,type:kind,entity_type:kind}};
  });
  return new Promise((resolve,reject)=>{
    const worker=new Worker(workerSource,{eval:true,execArgv:[],env:{},workerData:{code,entities,files,linePlanner:planLineRewrites.toString()},resourceLimits:{maxOldGenerationSizeMb:32,stackSizeMb:2}});
    let settled=false;
    const finish=(error,value)=>{if(settled)return;settled=true;clearTimeout(timer);void worker.terminate();error?reject(error):resolve(value);};
    const timer=setTimeout(()=>finish(new Error('Edit generator timed out')),1500);
    worker.once('error',error=>finish(error));
    worker.once('exit',code=>{if(!settled)finish(new Error('Edit generator exited without a result: '+code));});
    worker.once('message',message=>{
      try {
        if(message.error)throw new Error(message.error);
        const value=JSON.parse(message.value);
        if(value && !Object.hasOwn(value,'edits') && Array.isArray(value.creates) && value.creates.length>0 && value.creates.length<=64 &&
           Object.keys(value).every(k=>['creates','validation_cmd','format_files'].includes(k)) &&
           value.creates.every(c=>c && typeof c.file==='string' && c.file.length>0 && typeof c.content==='string')) value.edits=[];
        // A program may compute only a check after asserting cached state.
        // Normalize this exact shape; never forgive malformed edit payloads.
        if(value&&Object.keys(value).length===1&&typeof value.validation_cmd==='string'&&value.validation_cmd.trim())
          value.edits=[];
        if(!value||!Array.isArray(value.edits)||value.edits.length>64)throw new Error('Return {edits:[...]} with at most 64 edits');
        finish(null,value);
      }catch(error){finish(error);}
    });
  });
}
