"""Scheduling contract: preserve environment, outputs and failures; truly overlap."""
import json
import os
from pathlib import Path
import runpy
import shlex
import subprocess
import tempfile

runner = runpy.run_path(str(Path(__file__).resolve().parents[1] / 'bin/bench-validate'))['RUNNER']
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    executable = root / 'validation-path-sentinel'
    executable.write_text('#!/bin/sh\necho found-toolchain\n')
    executable.chmod(0o755)
    environment = dict(os.environ, PATH=directory + ':' + os.environ['PATH'])
    a, b = [shlex.quote(str(root / name)) for name in ('a', 'b')]
    commands = [
        f'touch {a}; while [ ! -f {b} ]; do sleep .01; done; validation-path-sentinel; exit 7',
        f'touch {b}; while [ ! -f {a} ]; do sleep .01; done; echo second-check',
    ]
    result = subprocess.run(['python3', '-c', runner, json.dumps(commands)],
                            env=environment, capture_output=True, text=True, timeout=10)
    assert result.returncode == 1, result
    assert all(s in result.stdout for s in ('found-toolchain', 'second-check', '"exit_code": 7', '"exit_code": 0')), result
    result = subprocess.run(['python3', '-c', runner, json.dumps(['true', 'true'])],
                            env=environment, capture_output=True, text=True, timeout=10)
    assert result.returncode == 0, result
print('PASS: concurrent completion, toolchain PATH, full output and exit status propagation')
