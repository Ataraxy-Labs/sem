"""Read bounded existing test reports through Docker's copy API; no extraction."""
import io
import os
import select
import subprocess
import tarfile
import time
import xml.etree.ElementTree as ET
from junit_evidence import parse_report, MAX_REPORT_BYTES

MAX_ARCHIVE_BYTES=8*1024*1024
REPORT_DIRS={'gradle':'/testbed/build/test-results/test',
             'maven':'/testbed/target/surefire-reports'}

def enrich_failed_check(result, container, started_at, collector=None):
    """Attach diagnostic evidence, never reinterpret the test process verdict.

    Only failed supported checks trigger I/O. This avoids another test execution
    merely to obtain assertion details absent from normal console output.
    """
    argv=result.get('executed_argv') or []
    kind={'gradle':'gradle','./gradlew':'gradle',
          'mvn':'maven','./mvnw':'maven'}.get(argv[0] if argv else '')
    if result.get('pass') is not False or result.get('stage')!='test' or not kind:
        return result
    start=time.monotonic()
    try:
        evidence=(collector or collect)(container,kind,started_at)
    except Exception as error:
        # Auxiliary diagnostics must not mask the original failed check.
        evidence={'status':'unavailable','reason':'report_collection_error',
                  'detail':type(error).__name__}
    return {**result,'test_report_evidence':evidence,
            'timings_ms':{**result.get('timings_ms',{}),
                          'report_collection_ms':round((time.monotonic()-start)*1000)}}

def parse_archive(payload, not_before, limit=5):
    if len(payload)>MAX_ARCHIVE_BYTES or not 1<=limit<=20:
        raise ValueError('Invalid archive or failure bound')
    failures=[]
    reports=0
    skipped=0
    total_failures=0
    with tarfile.open(fileobj=io.BytesIO(payload),mode='r:') as archive:
        for member in archive:
            if not member.isfile() or not member.name.endswith('.xml'):
                continue
            if member.size>MAX_REPORT_BYTES or member.mtime<not_before-1:
                skipped+=1
                continue
            stream=archive.extractfile(member)
            try:
                parsed=parse_report(stream.read(MAX_REPORT_BYTES+1),member.name)
            except (ValueError,ET.ParseError):
                skipped+=1
                continue
            finally:
                stream.close()
            reports+=1
            total_failures+=parsed['total_failure_elements']
            for failure in parsed['failures']:
                if len(failures)<limit:
                    failures.append({'report':member.name,**failure})
    return {'failures':failures,'reports_read':reports,'reports_skipped':skipped,
            'omitted_failures':total_failures-sum(item['occurrences'] for item in failures),
            'coverage':'Bounded report excerpts from one directory; timestamps filtered, not proven command attribution. Never a validation verdict.'}

def collect(container, kind, not_before, timeout=5):
    if kind not in REPORT_DIRS or not container or container.startswith('-'):
        raise ValueError('Unsupported report collection target')
    # Only this copy client can be terminated. The test container is never stopped.
    try:
        process=subprocess.Popen(['docker','cp',container+':'+REPORT_DIRS[kind]+'/.','-'],
                                 stdout=subprocess.PIPE,stderr=subprocess.DEVNULL)
    except OSError:
        return {'status':'unavailable','reason':'report_copy_start_failed'}
    deadline=time.monotonic()+timeout
    chunks=[]
    size=0
    try:
        while True:
            remaining=deadline-time.monotonic()
            if remaining<=0 or not select.select([process.stdout],[],[],remaining)[0]:
                return {'status':'unavailable','reason':'report_copy_timeout'}
            chunk=os.read(process.stdout.fileno(),65536)
            if not chunk:
                break
            size+=len(chunk)
            if size>MAX_ARCHIVE_BYTES:
                return {'status':'unavailable','reason':'report_archive_too_large'}
            chunks.append(chunk)
        if process.wait(timeout=max(.01,deadline-time.monotonic())):
            return {'status':'unavailable','reason':'report_copy_failed'}
        try:
            return {'status':'available',**parse_archive(b''.join(chunks),not_before)}
        except (ValueError,tarfile.TarError,EOFError) as error:
            return {'status':'unavailable','reason':'invalid_report_archive','detail':str(error)[:200]}
    except (OSError,subprocess.TimeoutExpired):
        return {'status':'unavailable','reason':'report_copy_failed'}
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        process.stdout.close()
