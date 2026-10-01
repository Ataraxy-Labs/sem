"""Read-only, deterministic diagnostic navigation. Never decides test correctness."""
import argparse
import hashlib
import json
from pathlib import Path
import re

ANSI = re.compile(r'\x1b\[[0-?]*[ -/]*[@-~]')
DIAGNOSTIC = re.compile(
    r'^\[ERROR\]|^FAILED\b|^FAIL\b|^--- FAIL:|^error(?:\[|:)|^E\s|'
    r'^.+ > .+ FAILED$|^●\s+|'
    r'\b(?:[\w.]*Exception|AssertionError|[\w.]*Error):|'
    r'\b(?:fatal error|error TS\d+|error CS\d+)\b')
TEST = re.compile(r'Tests run:|\b\d+ passed\b|test result:|^FAIL\s|^ok\s|'
                  r'^\d+ tests? completed(?:,|$)|BUILD (?:SUCCESS|SUCCESSFUL|FAILURE|FAILED)\b')
LOCATION = re.compile(r'([^\s\[\](),]+\.(?:java|py|rs|go|tsx?|jsx?|cpp|cc|c|h|rb)):(?:\[(\d+),(\d+)\]|(\d+)(?::(\d+))?)')

def continuation(lines, number):
    """Carry nearby compiler details without pretending to parse the whole log."""
    result = []
    # Jest prints a named failure followed by blank-separated assertion details.
    # Preserve those details, not the preceding list of all passing test names.
    jest = ANSI.sub('', lines[number-1]).strip().startswith('● ')
    if jest:
        for raw in lines[number:number+24]:
            line = ANSI.sub('', raw).rstrip()
            if DIAGNOSTIC.search(line.strip()) or TEST.search(line.strip()):
                break
            result.append(line.strip())
        return result
    # Go testing assertions often put expected/actual values after a bare
    # location and several blank lines. Preserve that bounded block, but never
    # consume a subsequent source location as detail of an unrelated log line.
    assertion = bool(re.fullmatch(r'\s*[^\s]+\.go:\d+:\s*', ANSI.sub('', lines[number-1])))
    for raw in lines[number:number+(12 if assertion else 4)]:
        line = ANSI.sub('', raw).rstrip()
        # Maven repeats compiler details with an [ERROR] prefix at build end.
        if re.match(r'^\[ERROR\]\s+(?:symbol|location|required|found|reason):', line):
            line = re.sub(r'^\[ERROR\]\s*', '  ', line)
        if DIAGNOSTIC.search(line.strip()) or TEST.search(line.strip()) or LOCATION.match(line.strip()):
            break
        if not line.strip():
            if assertion:
                result.append('')
                continue
            break
        if not (line[:1].isspace() or line.startswith((' -->', '-->', 'Caused by:'))):
            break
        result.append(line.strip())
    return result

def load(directory, key):
    if not re.fullmatch(r'[0-9a-f]{64}', key):
        raise ValueError('Expected evidence SHA-256')
    payload = (Path(directory) / (key + '.json')).read_text()
    if hashlib.sha256(payload.encode()).hexdigest() != key:
        raise ValueError('Corrupted evidence')
    return json.loads(payload)

def index(record):
    diagnostics, tests = {}, {}
    for stream in ('stdout', 'stderr'):
        lines = record.get('result', {}).get(stream, '').splitlines()
        covered_until = 0
        for number, raw in enumerate(lines, 1):
            line = ANSI.sub('', raw).strip()
            if TEST.search(line):
                tests.setdefault(line, {'text': line[:800], 'stream': stream, 'line': number,
                                        'text_truncated': len(line)>800})
            if number <= covered_until:
                continue
            # Go/compiler source diagnostics often have no "error:" prefix.
            source_diagnostic = LOCATION.match(line)
            if not DIAGNOSTIC.search(line) and not source_diagnostic:
                continue
            # Deduplicate exact diagnostics only; do not merge distinct files/errors.
            normalized = re.sub(r'^\[ERROR\]\s*', '', line)
            if not normalized:
                continue
            details = continuation(lines, number)
            covered_until = number + len(details)
            identity = (normalized, tuple(details))
            if identity in diagnostics:
                diagnostics[identity]['occurrences'] += 1
                continue
            location = LOCATION.search(line) or LOCATION.search('\n'.join(details))
            diagnostics[identity] = {
                'text': line[:1000], 'text_truncated': len(line)>1000,
                'context_lines': [detail[:500] for detail in details],
                'context_truncated': any(len(detail)>500 for detail in details)
                    or (line.startswith('● ') and len(details) == 24),
                'stream': stream, 'line': number, 'occurrences': 1,
                'location': None if not location else {
                    'file': location[1], 'line': int(location[2] or location[4]),
                    'column': int(location[3] or location[5] or 0)},
            }
    return list(diagnostics.values()), list(tests.values())

def summary(directory, key, offset=0, limit=20):
    if not 0 <= offset or not 1 <= limit <= 100:
        raise ValueError('Invalid diagnostic page bounds')
    record = load(directory, key)
    diagnostics, tests = index(record)
    if offset > len(diagnostics):
        raise ValueError('Offset exceeds diagnostic count')
    selected = diagnostics[offset:offset+limit]
    for entry in selected:
        entry['read_command'] = f"evidence-lines:{key}:{entry['stream']}:{max(1,entry['line']-2)}:12"
    end=offset+len(selected)
    result=record.get('result',{})
    return {
        'stage': 'evidence_summary', 'pass': None, 'id': key,
        'recorded_check': {k:result.get(k) for k in ('pass','exit_code','checked_patch_sha256')},
        'provenance': record.get('provenance',{}),
        'diagnostics': selected, 'total_unique_diagnostic_lines': len(diagnostics),
        'remaining_diagnostics': len(diagnostics)-end,
        'next_command': f'evidence-summary:{key}:{end}:{limit}' if end<len(diagnostics) else None,
        'test_summary_lines': tests[:20], 'omitted_test_summary_lines': max(0,len(tests)-20),
        'coverage': 'Regex-selected log lines only; not complete semantic diagnostics or proof of correctness. Repeated test totals are not summed.',
        'full_evidence_command': f'evidence:{key}:0',
    }

def read_lines(directory, key, stream, start, count):
    if stream not in ('stdout','stderr') or start<1 or not 1<=count<=100:
        raise ValueError('Invalid log line request')
    record=load(directory,key)
    lines=record.get('result',{}).get(stream,'').splitlines()
    if start>len(lines)+1:
        raise ValueError('Line exceeds log length')
    selected=lines[start-1:start-1+count]
    return {'stage':'evidence_lines','pass':None,'id':key,'stream':stream,'start_line':start,
            'lines':selected,'total_lines':len(lines),
            'next_command':f'evidence-lines:{key}:{stream}:{start+len(selected)}:{count}' if start-1+len(selected)<len(lines) else None}

def read(command,directory):
    match=re.fullmatch(r'evidence-summary:([0-9a-f]{64})(?::(\d+):(\d+))?',command)
    if match:
        return summary(directory,match[1],int(match[2] or 0),int(match[3] or 20))
    match=re.fullmatch(r'evidence-lines:([0-9a-f]{64}):(stdout|stderr):(\d+):(\d+)',command)
    if match:
        return read_lines(directory,match[1],match[2],int(match[3]),int(match[4]))
    raise ValueError('Expected evidence-summary:<id>[:offset:limit] or evidence-lines:<id>:<stream>:<line>:<count>')

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory',type=Path)
    parser.add_argument('command')
    args=parser.parse_args()
    print(json.dumps(read(args.command,args.directory),indent=2))
