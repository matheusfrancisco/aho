#!/usr/bin/env python3
"""Prove approved provider access works and resolved consumer aliases are rejected.

Requires the active Rust toolchain's Clippy component. This does not
compile the workspace, so the fixture cannot pass because unrelated packages fail.
Every negative case must fail specifically with clippy::disallowed_methods and no
other compiler error. Artifacts are permanent, under the mandatory scratch root.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRATCH = Path(os.environ.get('AHO_PROBE_SCRATCH', str(ROOT / 'target')))
CASES = ('fs_alias', 'fs_function_pointer', 'file_open', 'path_metadata', 'process', 'network', 'environment', 'environment_iteration')


def run(command: list[str], directory: Path, name: str, env: dict[str, str]) -> subprocess.CompletedProcess:
    result = subprocess.run(command, cwd=directory, env=env, text=True, capture_output=True, check=False)
    with (directory / f'{name}.json').open('x') as output:
        json.dump({'command': command, 'returncode': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}, output, indent=2)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work-dir', type=Path, default=SCRATCH / 'aho-authority-probes')
    args = parser.parse_args()
    work = args.work_dir.absolute()
    if work.parent != SCRATCH or work.name in ('', '.', '..') or work.is_symlink():
        parser.error(f'--work-dir must be one task folder directly under {SCRATCH}/')
    work.mkdir(parents=True, exist_ok=True)
    directory = work / f'run-{time.time_ns()}'
    directory.mkdir()
    driver = shutil.which('clippy-driver')
    env = dict(os.environ, CLIPPY_CONF_DIR=str(ROOT))
    # rustup shims are not installed by every distribution; discover the active component.
    if driver is None:
        found = run(['rustup', 'which', 'clippy-driver'], directory, 'toolchain', env)
        if found.returncode != 0:
            print('authority probe: Clippy component unavailable; install rustup component add clippy', file=sys.stderr)
            return 1
        driver = found.stdout.strip()
    command = [driver, '--edition=2024', '--crate-name=authority_probe', str(Path(__file__).with_name('probe.rs')),
               '-Dclippy::disallowed_methods', '--error-format=json', '-o', str(directory / 'allowed')]
    allowed = run(command, directory, 'allowed-compile', env)
    if allowed.returncode != 0:
        print(f'authority probe: declared provider failed to compile; see {directory}/allowed-compile.json', file=sys.stderr)
        return 1
    smoke = run([str(directory / 'allowed'), str(directory / 'provider-output')], directory, 'allowed-run', env)
    if smoke.returncode != 0 or 'declared provider filesystem roundtrip passed' not in smoke.stdout:
        print(f'authority probe: provider filesystem smoke failed; see {directory}/allowed-run.json', file=sys.stderr)
        return 1
    for case in CASES:
        negative = command[:-2] + ['--cfg', f'authority_probe="{case}"', '--emit=metadata', '-o', str(directory / f'{case}.rmeta')]
        result = run(negative, directory, case, env)
        diagnostics = []
        for line in result.stderr.splitlines():
            try:
                diagnostics.append(json.loads(line))
            except json.JSONDecodeError:
                print(f'authority probe: non-JSON compiler failure for {case}; see {directory}/{case}.json', file=sys.stderr)
                return 1
        errors = [d for d in diagnostics if d.get('level') == 'error' and d.get('code') is not None]
        if result.returncode == 0 or not errors or any(d['code'].get('code') != 'clippy::disallowed_methods' for d in errors):
            print(f'authority probe: {case} was not rejected solely for ambient authority; see {directory}/{case}.json', file=sys.stderr)
            return 1
        print(f'authority probe: rejected consumer {case}')
    with (directory / 'result.txt').open('x') as output:
        output.write('PASS: provider runtime filesystem roundtrip; each alias/constructor/environment consumer rejected by resolved Clippy lint.\n')
    print(f'authority probe: PASS; evidence {directory}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
