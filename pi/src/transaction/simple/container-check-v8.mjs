import {spawn} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {focusedCheck as localCheck} from './plan-policy.mjs';
export async function focusedCheck(api,cmd) {
  if(!process.env.SEM_VALIDATION_IMAGE)return localCheck(api,cmd);
  if(!cmd)return {pass:null,stage:'unavailable',error:'Provide a focused public test/build command'};
  return new Promise(resolve=>{
    const child=spawn(process.env.SEM_PYTHON ?? 'python3',[fileURLToPath(new URL('./container-check-v8.py',import.meta.url))],{stdio:['pipe','pipe','pipe']});
    let output='',errors='';
    child.stdout.on('data',data=>output+=data);
    child.stderr.on('data',data=>errors+=data);
    child.on('error',error=>resolve({pass:null,stage:'unavailable',error:String(error)}));
    child.on('close',()=>{try{resolve(JSON.parse(output));}catch{resolve({pass:null,stage:'unavailable',error:errors.slice(-2000)||'Invalid checker response'});}});
    child.stdin.end(JSON.stringify({cmd}));
  });
}
