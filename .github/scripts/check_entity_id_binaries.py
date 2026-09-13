"""Reject the shared-Cargo-target false comparison before collecting results."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def check_binaries(before, after):
    fingerprints = [hashlib.sha256(path.read_bytes()).hexdigest() for path in (before, after)]
    assert fingerprints[0] != fingerprints[1], 'baseline and candidate binaries are identical'
    env = dict(os.environ, SEM_CLOUD='0', SEM_TELEMETRY='0', SEM_PROFILE_MEM='1',
               GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_SYSTEM='/dev/null')
    with tempfile.TemporaryDirectory(prefix='sem320-provenance-') as temporary:
        repo = Path(temporary)
        (repo / 'sample.py').write_text('def target():\n    return 42\n\ndef caller():\n    return target()\n')
        outputs = []
        for binary, expect_interning in [(before, False), (after, True)]:
            result = subprocess.run([
                str(binary.resolve()), 'graph', str(repo), '--json', '--no-cache', '--file-exts', '.py',
            ], env=env, check=True, capture_output=True, text=True, timeout=60)
            has_interning = 'graph.interned_ids' in result.stderr
            assert has_interning == expect_interning, (
                f'{binary}: expected interning={expect_interning}, got {has_interning}; '
                'stale or incorrectly selected sem-core artifact')
            assert 'SEM_PROFILE_MEM[graph-return] process_rss_bytes=' in result.stderr
            graph = json.loads(result.stdout)
            assert len(graph['entities']) >= 2 and graph['edges'], 'empty provenance fixture'
            graph['entities'].sort(key=lambda item: item['id'])
            graph['edges'].sort(key=lambda item: (item['fromEntity'], item['toEntity'], item['refType']))
            outputs.append(graph)
        assert outputs[0] == outputs[1], 'provenance fixture changed graph semantics'
    return {'baseline_interning': False, 'candidate_interning': True,
            'baseline_sha256': fingerprints[0], 'candidate_sha256': fingerprints[1],
            'fixture_parity': True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(check_binaries(args.before, args.after), indent=2))
