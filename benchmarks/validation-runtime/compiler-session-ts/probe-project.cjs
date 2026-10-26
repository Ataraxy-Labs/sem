const fs=require('node:fs'),path=require('node:path'),cp=require('node:child_process'),assert=require('node:assert/strict');
const ts=require('/testbed/node_modules/typescript');
const dir='/testbed/.compiler-probe';fs.mkdirSync(dir,{recursive:true});
const worker='/opt/sem-compiler/project-worker.cjs';const records=[];
function ordinary(configPath){const root=path.dirname(configPath),raw=ts.readConfigFile(configPath,ts.sys.readFile),p=ts.parseJsonConfigFileContent(raw.config,ts.sys,root,undefined,configPath);const program=ts.createProgram(p.fileNames,{...p.options,noEmit:true});return [...(raw.error?[raw.error]:[]),...p.errors,...ts.getPreEmitDiagnostics(program)].map(d=>({code:d.code,category:ts.DiagnosticCategory[d.category],file:d.file?path.relative(root,d.file.fileName):null,start:d.start??null,length:d.length??null,message:ts.flattenDiagnosticMessageText(d.messageText,'\n')})).sort((a,b)=>JSON.stringify(a).localeCompare(JSON.stringify(b)));}
function check(name,project){const p=cp.spawnSync(process.execPath,[worker,'-p',project],{encoding:'utf8',timeout:600000});assert([0,1].includes(p.status),p.stderr);const compact=JSON.parse(p.stdout),r=JSON.parse(fs.readFileSync(compact.full_evidence));assert.deepEqual(r.diagnostics,ordinary(project),name+' parity');records.push({...compact,name,diagnostics:undefined});return r;}
fs.writeFileSync(dir+'/base.json',JSON.stringify({compilerOptions:{strict:true,target:'ES2020',module:'commonjs',types:[]}}));
fs.writeFileSync(dir+'/tsconfig.json',JSON.stringify({extends:'./base.json',include:['*.ts']}));
fs.writeFileSync(dir+'/a.ts','export function value():number {return 1;}');
fs.writeFileSync(dir+'/b.ts','import {value} from "./a"; export const b:number=value();');
const project=dir+'/tsconfig.json';
assert.equal(check('cold',project).status,'passed');
const baselineRun=cp.spawnSync(process.execPath,[worker,'--capture-baseline','-p',project],{encoding:'utf8'});
assert.equal(baselineRun.status,0,baselineRun.stderr);assert.equal(JSON.parse(baselineRun.stdout).baseline.captured,true);
assert.equal(check('warm',project).receipt_reused,true);
fs.writeFileSync(dir+'/a.ts','export function value():string {return "bad";}');assert.equal(check('public-break',project).status,'failed');
const compared=cp.spawnSync(process.execPath,[worker,'-p',project],{encoding:'utf8'});
assert.equal(compared.status,1);assert(JSON.parse(compared.stdout).baseline.new_diagnostic_count>0);

fs.writeFileSync(dir+'/a.ts','export function value():number {return 1;}');assert.equal(check('repair',project).status,'passed');
fs.writeFileSync(dir+'/new.ts','export const invalid:number="wrong";');const added=check('glob-add',project);assert.equal(added.status,'failed');assert.equal(added.reset,false);assert(added.parsed<10,'adding a root must not reparse the project');
fs.unlinkSync(dir+'/new.ts');const deleted=check('glob-delete',project);assert.equal(deleted.status,'passed');assert.equal(deleted.reset,false);
fs.writeFileSync(dir+'/base.json',JSON.stringify({compilerOptions:{strict:true,noUnusedLocals:true,target:'ES2020',module:'commonjs',types:[]}}));assert.equal(check('config-change',project).reset,true);
// A missing import becoming available must invalidate a receipt even when
// the tsconfig root-file list is unchanged.
fs.mkdirSync(dir+'/.dep',{recursive:true});
fs.writeFileSync(dir+'/b.ts','import {value} from "./.dep/api"; export const b:number=value();');
assert.equal(check('missing-import',project).status,'failed');
assert.equal(check('missing-import-repeat',project).receipt_reused,true);
fs.writeFileSync(dir+'/.dep/api.ts','export function value():number {return 1;}');
const resolved=check('new-resolution',project);assert.equal(resolved.status,'passed');assert.equal(resolved.receipt_reused,false);
const stat=fs.statSync(dir+'/.dep/api.ts');
fs.writeFileSync(dir+'/.dep/api.ts','export function value():string {return "bad";}');fs.utimesSync(dir+'/.dep/api.ts',stat.atime,stat.mtime);
assert.equal(check('dependency-content-change',project).status,'failed');
fs.unlinkSync(dir+'/.dep/api.ts');assert.equal(check('dependency-deletion',project).status,'failed');
// A changed import must resolve again even when old filesystem answers hold.
fs.writeFileSync(dir+'/.dep/other.ts','export const value:string="bad";');
fs.writeFileSync(dir+'/b.ts','import {value} from "./a"; export const b:number=value();');
check('restore-import',project);
fs.writeFileSync(dir+'/b.ts','import {value} from "./.dep/other"; export const b:number=value;');
assert.equal(check('changed-import',project).status,'failed');
fs.mkdirSync(dir+'/node_modules/probe-pkg',{recursive:true});
fs.writeFileSync(dir+'/node_modules/probe-pkg/package.json',JSON.stringify({types:'good.d.ts'}));
fs.writeFileSync(dir+'/node_modules/probe-pkg/good.d.ts','export const value:number;');
fs.writeFileSync(dir+'/node_modules/probe-pkg/bad.d.ts','export const value:string;');
fs.writeFileSync(dir+'/b.ts','import {value} from "probe-pkg"; export const b:number=value;');
assert.equal(check('package-resolution',project).status,'passed');
fs.writeFileSync(dir+'/node_modules/probe-pkg/package.json',JSON.stringify({types:'bad.d.ts'}));
assert.equal(check('package-types-change',project).status,'failed');
fs.rmSync(dir,{recursive:true,force:true});
if(process.argv.includes('--fixture-only')){console.log(JSON.stringify({status:'passed',records}));process.exit(0);}
const main=check('ant-root-cold','/testbed/tsconfig.json');check('ant-root-warm','/testbed/tsconfig.json');
const alt='/testbed/tsconfig.compiler-probe.json';fs.copyFileSync('/testbed/tsconfig.json',alt);
const other=check('isolated-project',alt);assert.notEqual(other.worker_pid,main.worker_pid,'projects must use independent compiler heaps');
const again=check('return-to-main','/testbed/tsconfig.json');assert.equal(again.worker_pid,main.worker_pid);
fs.unlinkSync(alt);
if(fs.existsSync('/opt/sem-compiler/replay.patch')){
 // Replay a completed agent patch, not held-out tests, in this disposable image.
 cp.execFileSync('git',['apply','--check','/opt/sem-compiler/replay.patch'],{cwd:'/testbed'});
 cp.execFileSync('git',['apply','/opt/sem-compiler/replay.patch'],{cwd:'/testbed'});
 check('actual-task-patch','/testbed/tsconfig.json');
 const warm=check('actual-task-patch-unchanged','/testbed/tsconfig.json');
 assert(warm.receipt_reused);
}
console.log(JSON.stringify({status:'passed' ,typescript:ts.version,node:process.version,records,scripts:require('/testbed/package.json').scripts}));
