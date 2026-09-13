"""Isolated Linux release-CLI comparison; results are CI artifacts, not commits."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import random
import re
import signal
import statistics
import subprocess
import tempfile
import time

from check_entity_id_binaries import check_binaries


def command(*args):
    return subprocess.check_output(list(map(str, args)), text=True).strip()


def interval(before, after):
    logs = [math.log(a / b) for b, a in zip(before, after)]
    rng = random.Random(320)
    means = sorted(math.exp(statistics.mean(rng.choices(logs, k=len(logs))))
                   for _ in range(10000))
    return {
        'before_median': statistics.median(before),
        'after_median': statistics.median(after),
        'paired_geomean_after_before': math.exp(statistics.mean(logs)),
        'bootstrap_95pct': [means[250], means[9749]],
    }


def invoke(binary, repo, ext, instrument, log_path):
    env = dict(os.environ, SEM_CLOUD='0', SEM_TELEMETRY='0', SEM_TIMINGS='json',
               SEM_PROFILE_MEM='1' if instrument else '0',
               GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_SYSTEM='/dev/null')
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
        started = time.monotonic()
        proc = subprocess.Popen([
            '/usr/bin/time', '-f', 'SEM320_PEAK_RSS_KIB %M\nSEM320_WALL_S %e',
            str(binary), 'graph', str(repo), '--json', '--no-cache', '--file-exts', ext,
        ], env=env, stdout=output, stderr=errors, start_new_session=True)
        failure = None
        while proc.poll() is None:
            if time.monotonic() - started > 300:
                failure = '300 second limit'
            rows = command('ps', '-eo', 'pgid=,rss=').splitlines()
            rss_kib = sum(int(fields[1]) for row in rows
                          if len(fields := row.split()) == 2 and int(fields[0]) == proc.pid)
            if rss_kib > 10 * 1024 * 1024:
                failure = '10 GiB process-group RSS limit'
            if failure:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
                break
            time.sleep(0.1)
        errors.seek(0)
        diagnostics = errors.read().decode(errors='replace')
        log_path.write_text(diagnostics)
        if failure or proc.returncode:
            raise RuntimeError(f'{binary}: {failure or proc.returncode}\n{diagnostics[-4000:]}')
        output.seek(0)
        graph = json.load(output)
        graph['entities'].sort(key=lambda item: item['id'])
        graph['edges'].sort(key=lambda item: (item['fromEntity'], item['toEntity'], item['refType']))
        digest = hashlib.sha256(json.dumps(graph, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        result = {
            'entities': len(graph['entities']), 'edges': len(graph['edges']),
            'sorted_json_sha256': digest,
            'peak_rss_bytes': int(re.search(r'SEM320_PEAK_RSS_KIB (\d+)', diagnostics)[1]) * 1024,
            'process_wall_s': float(re.search(r'SEM320_WALL_S ([\d.]+)', diagnostics)[1]),
        }
        for line in diagnostics.splitlines():
            if not line.startswith('{'):
                continue
            try:
                timing = json.loads(line)
            except ValueError:
                continue
            if timing.get('command') == 'graph':
                result['cli_total_ms'] = timing['totalMs']
                result['build_ms'] = next(p['durationMs'] for p in timing['phases']
                                          if p['name'] == 'full_graph_build')
        assert 'build_ms' in result, 'missing build timing'
        if instrument:
            result['rss_after_return_bytes'] = int(re.search(
                r'SEM_PROFILE_MEM\[graph-return\] process_rss_bytes=(\d+)', diagnostics)[1])
        return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    parser.add_argument('--corpora', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--pairs', type=int, default=9)
    parser.add_argument('--memory-pairs', type=int, default=5)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    provenance = check_binaries(args.before, args.after)
    report = {
        'runtime_core_provenance': provenance,
        'baseline_commit': '4cfe1374f79631b01aca5746441d826738a15448',
        'candidate_commit': command('git', 'rev-parse', 'HEAD'),
        'rustc': command('rustc', '-Vv'), 'platform': command('uname', '-a'),
        'cpu_count': os.cpu_count(), 'memory_before': Path('/proc/meminfo').read_text(),
        'baseline_sha256': hashlib.sha256(args.before.read_bytes()).hexdigest(),
        'candidate_sha256': hashlib.sha256(args.after.read_bytes()).hexdigest(),
        'profile': 'release; default mimalloc; no allocator override',
        'method': 'Fresh processes; sem caches bypassed; OS caches not flushed; alternating paired order; RSS instrumentation separate from timing; all JSON fields compared on every run.',
        'cases': {},
    }
    destination = args.output / 'results.json'

    def save():
        destination.write_text(json.dumps(report, indent=2) + '\n')

    try:
        for name, ext in [('kubernetes', '.go'), ('django', '.py'), ('tokio', '.rs')]:
            repo = (args.corpora / name).resolve()
            row = {
                'revision': command('git', '-C', repo, 'rev-parse', 'HEAD'),
                'tracked_source_files': len(command('git', '-C', repo, 'ls-files', '-z', '*' + ext).split('\0')) - 1,
                'runs': {}, 'summary': {},
            }
            report['cases'][name] = row
            expected_hash = None
            for mode, repeats in [('timing', args.pairs), ('memory', args.memory_pairs)]:
                samples = {'before': [], 'after': []}
                row['runs'][mode] = samples
                for pair in range(repeats):
                    for label in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
                        binary = args.before if label == 'before' else args.after
                        log_path = args.output / f'{name}-{mode}-{pair + 1}-{label}.log'
                        result = invoke(binary, repo, ext, mode == 'memory', log_path)
                        samples[label].append(result)
                        save()
                        if expected_hash is None:
                            expected_hash = result['sorted_json_sha256']
                        assert result['sorted_json_sha256'] == expected_hash, (name, 'JSON parity failed')
                        print(json.dumps(dict(case=name, mode=mode, pair=pair + 1, version=label, **result)), flush=True)
                keys = ['build_ms', 'cli_total_ms', 'process_wall_s', 'peak_rss_bytes'] if mode == 'timing' else ['rss_after_return_bytes', 'peak_rss_bytes']
                row['summary'][mode] = {
                    key: interval([sample[key] for sample in samples['before']],
                                  [sample[key] for sample in samples['after']])
                    for key in keys
                }
                save()
            row['parity'] = True
        report['complete'] = True
        save()
    except BaseException as error:
        report['error'] = repr(error)
        save()
        raise
    print(json.dumps({name: row['summary'] for name, row in report['cases'].items()}, indent=2))


if __name__ == '__main__':
    main()
