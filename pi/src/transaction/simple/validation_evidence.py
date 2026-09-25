"""Compact check logs, retaining retrievable diagnostic evidence.

This is output compression, NOT test-result caching or proof of correctness.
"""
import hashlib
import json
from pathlib import Path
import re


def compact_failure(value, limit=3000):
    """Bounded excerpts, not a claim that all failures have been summarized."""
    if len(value) <= limit:
        return value
    seen, highlights = set(), []
    remaining = 1300
    for line in value.splitlines():
        if not re.search(r'FAILED|ERROR|Error:|Exception:|AssertionError|^E\s|^_+ .+ _+$|\b(?:failed|failures)\b|\.(?:py|rs|ts|js):\d+', line):
            continue
        line = line.strip()
        if line in seen:
            continue
        seen.add(line)
        excerpt = line[:250]
        if len(excerpt) + 1 > remaining:
            break
        highlights.append(excerpt)
        remaining -= len(excerpt) + 1
    return (value[:300] + '\n[distinct diagnostic excerpts]\n' + '\n'.join(highlights)
            + '\n[failure log compacted; excerpts are incomplete; retrieve full evidence]\n'
            + value[-1000:])


def present(result, directory, provenance):
    if result.get('stage') != 'test':
        return result
    record = {'provenance': provenance, 'result': result}
    payload = json.dumps(record, sort_keys=True, ensure_ascii=False)
    key = hashlib.sha256(payload.encode()).hexdigest()
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    target = directory / (key + '.json')
    try:
        with target.open('x') as stream:
            stream.write(payload)
    except FileExistsError:
        if target.read_text() != payload:
            raise ValueError('Evidence identity collision or corrupted evidence')
    output = dict(result)
    output['evidence'] = {'id': key, 'read_command': 'evidence:' + key + ':0',
                          'characters': len(payload), 'reused_validation': False}
    if result.get('pass') is False:
        compacted = []
        for field in ('stdout', 'stderr'):
            value = result.get(field)
            if isinstance(value, str) and len(value) > 3000:
                output[field] = compact_failure(value)
                compacted.append(field)
        if compacted:
            output['diagnostics'] = {'compacted_fields': compacted, 'complete': False,
                                     'full_output': output['evidence']['read_command']}
        return output
    if result.get('pass') is not True or result.get('exit_code') != 0:
        return output
    # Keep useful summary/warning lines and the tail; full output stays available.
    for field in ('stdout', 'stderr'):
        value = result.get(field)
        if not isinstance(value, str) or len(value) <= 2400:
            continue
        highlights = [line for line in value.splitlines()
                      if re.search(r'warning|error|FAILED|PASSED|test result:|specs?,|Executed ', line, re.I)]
        summary = '\n'.join(highlights)
        if len(summary) > 1000:
            summary = summary[:1000] + '\n[additional diagnostics in evidence]'
        output[field] = summary + '\n[successful-check log compacted; full output in evidence]\n' + value[-1000:]
    return output


def read(command, directory):
    match = re.fullmatch(r'evidence:([0-9a-f]{64}):(\d{1,10})', command)
    if not match:
        raise ValueError('Expected evidence:<sha256>:<character-offset>')
    key, start = match.group(1), int(match.group(2))
    payload = (Path(directory) / (key + '.json')).read_text()
    if hashlib.sha256(payload.encode()).hexdigest() != key:
        raise ValueError('Corrupted evidence')
    end = min(start + 8000, len(payload))
    if start > len(payload):
        raise ValueError('Offset exceeds evidence length')
    return {'stage': 'evidence', 'pass': None, 'id': key, 'content': payload[start:end],
            'offset': start, 'next_command': f'evidence:{key}:{end}' if end < len(payload) else None}
