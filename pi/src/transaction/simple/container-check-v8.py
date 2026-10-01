"""Identical offline, public-code validation for both benchmark arms.

No hidden tests, host mounts, or network. Called directly by Sem or as one
validation-only MCP tool alongside native code-reading/editing tools.
"""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
from patch_delta import incremental_patch
from validation_resources import bounded_command
from validation_evidence import present, read
from junit_reports import enrich_failed_check
import hashlib
import re

def cpu_affinity_flags():
    value=os.environ.get('SEM_VALIDATION_CPUSET','')
    if not value:
        return []
    if not re.fullmatch(r'[0-9]+(?:-[0-9]+)?(?:,[0-9]+(?:-[0-9]+)?)*',value):
        raise ValueError('Invalid SEM_VALIDATION_CPUSET')
    return ['--cpuset-cpus',value]

def offline_hostname_flags(name):
    # Network-disabled containers need a resolvable local hostname for JVM/HDFS
    # and other local integration tests. Never enable external networking.
    hostname = 'sem-check-' + hashlib.sha256(name.encode()).hexdigest()[:16]
    return ['--hostname', hostname, '--add-host', hostname + ':127.0.0.1',
            '-e', 'HOSTNAME=' + hostname]

def run(args, **kwargs):
    return subprocess.run(args,capture_output=True,text=True,check=True,**kwargs)

SUPPORTED_COMMANDS={'cargo','bazel','bazelisk','go','npm','pnpm','yarn','make','pytest','gradle','./gradlew','mvn','./mvnw','cmake','ctest'}

def parse_check_command(command):
    """Preserve argv and interpreter semantics; permit only the pytest module.

    In particular, do not rewrite python -m pytest to the pytest entrypoint:
    their import paths can differ. Execution remains shell-free and offline.
    """
    argv=shlex.split(command or '')
    pytest_module=(len(argv)>=3 and argv[0] in {'python','python3'}
                   and argv[1:3]==['-m','pytest'])
    if (not argv or (argv[0] not in SUPPORTED_COMMANDS and not pytest_module)
            or any(x in {'&&','||',';','|','>','<'} for x in argv)):
        raise ValueError('Use one supported project check command without shell operators')
    if argv[0]=='make' and any('=' in arg and any(c in arg.split('=',1)[1] for c in '|;&<>`$\n\r') for arg in argv[1:]):
        raise ValueError('Make variable overrides contain shell metacharacters that recipes may reinterpret. Use the direct test command instead, e.g. go test -run "TestA|TestB" ./package; use -exec "env NAME=value" for Go test-process environment.')
    return argv

def _check(command):
    try:
        argv=parse_check_command(command)
    except ValueError as error:
        return {'pass':None,'stage':'unavailable','error':'Use one supported project check command without shell operators',
                'detail':str(error),
                'supported_commands':sorted(SUPPORTED_COMMANDS)+['python -m pytest','python3 -m pytest']}
    try:
        argv=bounded_command(argv)
    except ValueError as error:
        return {'pass':None,'stage':'unavailable','error':str(error)}
    name=os.environ['SEM_VALIDATION_CONTAINER']
    image=os.environ['SEM_VALIDATION_IMAGE']
    cwd=Path(os.environ['SEM_VALIDATION_CWD'])
    options={'timeout':60}
    timings={}
    phase_start=time.monotonic()
    exists=subprocess.run(['docker','inspect',name],capture_output=True).returncode==0
    try:
        if not exists:
            run(['docker','run','-d','--name',name,'--network','none',
                 *offline_hostname_flags(name),*cpu_affinity_flags(),image,'tail','-f','/dev/null'],**options)
            head=run(['docker','exec',name,'git','-C','/testbed','rev-parse','HEAD'],**options).stdout.strip()
            if head!=os.environ['SEM_VALIDATION_BASE']:
                return {'pass':None,'stage':'unavailable','error':'Validation image is not at the task base revision'}
        else:
            state=json.loads(run(['docker','inspect','--format','{{json .State}}',name],**options).stdout)
            if state.get('Running') is False:
                run(['docker','start',name],**options)
        timings['container_ready_ms']=round((time.monotonic()-phase_start)*1000)
        phase_start=time.monotonic()
        run(['git','add','-N','--','.'],cwd=cwd,**options)
        patch=run(['git','diff','--no-ext-diff','--binary','HEAD'],cwd=cwd,**options).stdout
        previous=subprocess.run(['docker','exec',name,'cat','/tmp/sem-applied.diff'],capture_output=True,text=True)
        unchanged=previous.returncode==0 and previous.stdout==patch
        delta=''
        if not unchanged:
            delta=incremental_patch(cwd,previous.stdout if previous.returncode==0 else '',patch)
            with tempfile.TemporaryDirectory(prefix='sem-public-check-') as temporary:
                patchfile=Path(temporary)/'candidate.diff'
                patchfile.write_text(patch)
                run(['docker','cp',str(patchfile),name+':/tmp/sem-candidate.diff'],**options)
                deltafile=Path(temporary)/'delta.diff'
                deltafile.write_text(delta)
                if delta:
                    run(['docker','cp',str(deltafile),name+':/tmp/sem-delta.diff'],**options)
            if delta:
                run(['docker','exec','-w','/testbed',name,'git','apply','--check','/tmp/sem-delta.diff'],**options)
                run(['docker','exec','-w','/testbed',name,'git','apply','/tmp/sem-delta.diff'],**options)
            run(['docker','exec',name,'mv','/tmp/sem-candidate.diff','/tmp/sem-applied.diff'],**options)
        timings['patch_sync_ms']=round((time.monotonic()-phase_start)*1000)
        environment=['PATH=/testbed/node_modules/.bin:/opt/maven/bin:/usr/local/go/bin:/usr/local/cargo/bin:/root/.cargo/bin:/root/.local/bin:/usr/local/bin:/usr/bin:/bin',
            'CARGO_NET_OFFLINE=true','GOPROXY=off','GOSUMDB=off','npm_config_offline=true',
            'YARN_ENABLE_NETWORK=0','COREPACK_ENABLE_DOWNLOAD_PROMPT=0']
        phase_start=time.monotonic()
        command_started_at=time.time()
        result=subprocess.run(['docker','exec','-u','root','-w','/testbed',name,'env',*environment,*argv],
            capture_output=True,text=True,timeout=600)
        timings['command_ms']=round((time.monotonic()-phase_start)*1000)
        phase_start=time.monotonic()
        state=json.loads(run(['docker','inspect','--format','{{json .State}}',name],**options).stdout)
        timings['state_inspection_ms']=round((time.monotonic()-phase_start)*1000)
        if state.get('OOMKilled'):
            # Restart only this disposable validation container. Writable layers
            # and build caches survive; dead server processes do not.
            run(['docker','restart',name],**options)
            return {'pass':None,'stage':'infrastructure','reason':'container_oom',
                    'recovery':'checker_restarted; retry the requested test',
                    'timings_ms':timings,
                    'executed_argv':argv,'exit_code':result.returncode,
                    'stderr':result.stderr[-12000:]}
        checked={'pass':result.returncode==0,'stage':'test','runner':'same_offline_container',
            'checked_patch_sha256':hashlib.sha256(patch.encode()).hexdigest(),
            'timings_ms':timings,
            'executed_argv':argv,
            'patch_unchanged':unchanged,'applied_delta_bytes':len(delta.encode()),
            'exit_code':result.returncode,'stdout':result.stdout,'stderr':result.stderr}
        return enrich_failed_check(checked,name,command_started_at)
    except subprocess.TimeoutExpired as error:
        # Kill the timed-out test tree, then restore only this disposable checker.
        # Preserve partial diagnostics and caches; a timeout is never a pass.
        def decoded(value):
            return value.decode('utf-8',errors='replace') if isinstance(value,bytes) else (value or '')
        recovery='checker_restarted; retry a narrower test target'
        try:
            run(['docker','restart','-t','1',name],timeout=30)
        except (subprocess.SubprocessError,OSError) as restart_error:
            recovery='checker_restart_failed: '+str(restart_error)[:300]
        return {'pass':None,'stage':'timeout','error':'Public validation exceeded its bounded timeout',
                'stdout':decoded(error.stdout),'stderr':decoded(error.stderr),
                'recovery':recovery,'executed_argv':argv,'timings_ms':timings,
                'checked_patch_sha256':hashlib.sha256(patch.encode()).hexdigest() if 'patch' in locals() else None}
    except (subprocess.CalledProcessError,OSError) as error:
        return {'pass':None,'stage':'unavailable','error':str(getattr(error,'stderr',None) or error)[-12000:]}

def check(command):
    start=time.monotonic()
    cwd=Path(os.environ['SEM_VALIDATION_CWD'])
    evidence_dir=cwd/'.validation'/'evidence'
    if isinstance(command,str) and command.startswith(('evidence:', 'evidence-summary:', 'evidence-lines:')):
        try: return read(command,evidence_dir)
        except (ValueError,OSError) as error:
            return {'pass':None,'stage':'unavailable','error':str(error)}
    result=_check(command)
    result['duration_ms']=round((time.monotonic()-start)*1000)
    try:
        # Capture provenance, not authorization to reuse a result.
        return present(result,evidence_dir,{
            'base':os.environ['SEM_VALIDATION_BASE'],
            'image':os.environ['SEM_VALIDATION_IMAGE'],
            'patch_sha256':result.get('checked_patch_sha256'),
            'checker_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'command':command})
    except (OSError,ValueError,subprocess.SubprocessError):
        # Losing the evidence store must not hide diagnostics or change verdicts.
        return result

def serve():
    for line in sys.stdin:
        message=json.loads(line)
        if 'id' not in message: continue
        reply={'jsonrpc':'2.0','id':message['id']}
        method=message.get('method')
        if method=='initialize':
            reply['result']={'protocolVersion':message.get('params',{}).get('protocolVersion','2025-06-18'),
                'capabilities':{'tools':{}},'serverInfo':{'name':'public-validation','version':'1'}}
        elif method=='tools/list':
            reply['result']={'tools':[{'name':'repo_check','description':'Run a focused public test/build command in the offline task Linux image. No hidden tests. Failed checks include deduplicated diagnostics and source locations. Use returned evidence-summary and evidence-lines commands for precise log drilldown; original evidence pages remain available. Summaries are not completeness proofs.',
                'inputSchema':{'type':'object','properties':{'cmd':{'type':'string'}},'required':['cmd'],'additionalProperties':False}}]}
        elif method=='tools/call' and message['params']['name']=='repo_check':
            result=check(message['params']['arguments']['cmd'])
            reply['result']={'content':[{'type':'text','text':json.dumps(result)}]}
        elif method=='ping': reply['result']={}
        else: reply['error']={'code':-32601,'message':'Unknown method'}
        print(json.dumps(reply),flush=True)
if __name__=='__main__':
    if '--serve' in sys.argv: serve()
    else: print(json.dumps(check(json.load(sys.stdin).get('cmd'))))
