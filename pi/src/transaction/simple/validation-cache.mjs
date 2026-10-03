import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {createHash} from 'node:crypto';
const exec=promisify(execFile);

// Reuse is off without an operator-owned, complete-input fingerprint provider.
// A source diff alone cannot attest toolchains, dependencies or runtime state.
export class ValidationCache {
  constructor({limit=32}={}) {
    if(!Number.isSafeInteger(limit)||limit<1)throw Error('INVALID_VALIDATION_CACHE_CAPACITY');
    this.limit=limit;this.entries=new Map();
  }
  async run(command, fingerprint, execute) {
    if(!fingerprint)return execute();
    const identify=async()=>{
      try {
        const value=await fingerprint();
        return typeof value==='string'&&value.length>0&&value.length<=65536
          ?createHash('sha256').update(JSON.stringify([command,value])).digest('hex'):null;
      } catch {return null;}
    };
    const before=await identify();
    if(!before)return execute();
    const cached=this.entries.get(before);
    if(cached) {
      // Recheck even on a hit; a failed fingerprint never authorizes omission.
      if(await identify()===before) {
        this.entries.delete(before);this.entries.set(before,cached);
        return {...structuredClone(cached),duration_ms:0,
          validation_cache:{reused:true,input_key:before,executed:false,
            original_duration_ms:cached.duration_ms??null,contract:'operator_attested_complete_inputs'}};
      }
      return execute();
    }
    const result=await execute();
    const after=await identify();
    if(after!==before)return {...result,observed_pass:result.pass,pass:null,
      stage:'stale_validation',validation_cache:{reused:false,reason:'input_fingerprint_changed_or_unavailable'}};
    // Never cache timeouts, infrastructure failures, evidence reads, or failures.
    if(result.pass===true&&result.stage==='test') {
      this.entries.set(before,structuredClone(result));
      while(this.entries.size>this.limit)this.entries.delete(this.entries.keys().next().value);
    }
    return {...result,validation_cache:{reused:false,input_key:before,executed:true,
      contract:'operator_attested_complete_inputs'}};
  }
}

// JSON argv, supplied by the operator, never a model-provided shell command.
// The provider receives the validation command on argv and must fingerprint all
// relevant files (including missing/untracked inputs), configuration, resolved
// dependencies, toolchain, environment and any mutable runtime inputs.
export function inputFingerprint(command,env=process.env) {
  if(!env.SEM_VALIDATION_INPUT_KEY_ARGV)return null;
  return async()=>{
    const argv=JSON.parse(env.SEM_VALIDATION_INPUT_KEY_ARGV);
    if(!Array.isArray(argv)||!argv.length||argv.some(x=>typeof x!=='string'))throw Error('INVALID_INPUT_KEY_ARGV');
    const {stdout}=await exec(argv[0],[...argv.slice(1),command],{
      cwd:env.SEM_VALIDATION_CWD||process.cwd(),env,timeout:10000,maxBuffer:65536});
    const key=stdout.trim();
    if(!key)throw Error('EMPTY_INPUT_KEY');
    // No environment values or credentials are returned to the model.
    return JSON.stringify([key,process.cwd(),Object.entries(env).sort(([a],[b])=>a.localeCompare(b))]);
  };
}
