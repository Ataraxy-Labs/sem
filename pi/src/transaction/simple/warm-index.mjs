import {spawn} from 'node:child_process';
export function hasMutations(params) {
  return ['edits','creates','imports','remove_imports'].some(key => params[key]?.length > 0);
}
export async function refreshIndex(cwd, semBin='sem') {
  const start=performance.now();
  return new Promise(resolve => {
    let stderr='';
    const child=spawn(semBin,['graph','--json'],{cwd,stdio:['ignore','ignore','pipe']});
    const timer=setTimeout(()=>child.kill('SIGTERM'),120000);
    child.stderr.on('data',data=>{stderr=(stderr+data).slice(-2000)});
    child.on('error',error=>{clearTimeout(timer);resolve({ok:false,ms:performance.now()-start,error:String(error)})});
    child.on('close',(code,signal)=>{clearTimeout(timer);resolve({ok:code===0,ms:performance.now()-start,code,signal,...(code===0?{}:{error:stderr})})});
  });
}
