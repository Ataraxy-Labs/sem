// Record deterministic filesystem answers used by the compiler. Reuse is valid
// only under the enclosing workspace lock and an unchanged loaded toolchain.
const {createHash}=require('node:crypto');
const digest=x=>createHash('sha256').update(x).digest('hex');
const operations=['readFile','fileExists','directoryExists','getDirectories','readDirectory','realpath'];
function normalize(name,value){
 if(value===undefined)return ['undefined'];
 if(name==='readFile')return ['content',digest(value)];
 return ['value',value];
}
function recorder(sys){
 const observations=new Map();
 const wrapped={...sys};
 for(const name of operations)if(typeof sys[name]==='function')wrapped[name]=(...args)=>{
  const result=sys[name](...args);
  observations.set(JSON.stringify([name,args]),{name,args,answer:normalize(name,result)});
  return result;
 };
 return {sys:wrapped,observations};
}
function unchanged(sys,observations){
 for(const {name,args,answer:expected} of observations.values()){
  try{if(JSON.stringify(normalize(name,sys[name](...args)))!==JSON.stringify(expected))return false;}
  catch{return false;}
 }
 return true;
}
// Source content edits are handled by TypeScript's versioned SourceFiles.
// Module resolution also depends on missing paths, directories and package/config
// JSON. Preserve those observations; never infer validity from mtime alone.
function resolutionsUnchanged(sys,observations){
 if(!observations)return false;
 for(const {name,args,answer:expected} of observations.values()){
  try {
   if(name==='readFile' && /\.[cm]?[jt]sx?$/.test(args[0])){
    if((sys.readFile(...args)===undefined)!==(expected[0]==='undefined'))return false;
   } else if(JSON.stringify(normalize(name,sys[name](...args)))!==JSON.stringify(expected))return false;
  } catch {return false;}
 }
 return true;
}
module.exports={recorder,unchanged,resolutionsUnchanged};
