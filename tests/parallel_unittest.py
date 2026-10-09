#!/usr/bin/env python3
"""Run unittest modules one test per process, several at a time.

    tests/parallel_unittest.py [-j N] [-W MODE] [--first PATTERN]... FILE.py [FILE.py ...]

The slow Python suites (publisher and publication callers) are hundreds of
independent tests, each with its own temporary directory, run serially by
`unittest discover`. This enumerates every test in the given files, runs each
as `python -m unittest module.Class.test` with the file's directory on
PYTHONPATH (exactly how `discover -s DIR -p FILE` imports it), and prints
nothing for a pass. `--first PATTERN` starts the tests whose id contains PATTERN
before the rest, so a few known-long tests do not begin last. A failure prints that test's own output. The exit status
is nonzero if any test fails, errors, or runs zero tests, so a renamed or
unloadable test cannot pass silently.
"""
import argparse
import concurrent.futures
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest


def enumerate_tests(path):
    """Test ids `Class.method` of one file, loaded the way discover loads it."""
    sys.path.insert(0, str(path.parent))
    try:
        module = __import__(path.stem)
    finally:
        sys.path.pop(0)
    ids = []

    def walk(suite):
        for item in suite:
            if isinstance(item, unittest.TestSuite):
                walk(item)
            elif isinstance(item, unittest.loader._FailedTest):
                raise SystemExit(f'{path}: cannot load tests: {item._exception}')
            else:
                ids.append(item.id())

    walk(unittest.defaultTestLoader.loadTestsFromModule(module))
    if not ids:
        raise SystemExit(f'{path}: no tests found')
    return ids


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('-j', '--jobs', type=int, default=min(4, os.cpu_count() or 1))
    parser.add_argument('-W', '--warnings', default=None, help='python -W mode for every test (e.g. error)')
    parser.add_argument('--first', action='append', default=[], metavar='PATTERN',
                        help='schedule tests whose id contains PATTERN first (repeatable)')
    parser.add_argument('--slowest', type=int, default=3, help='list the N slowest tests (default 3)')
    parser.add_argument('files', nargs='+', type=Path)
    args = parser.parse_args()

    jobs = []
    for path in args.files:
        path = path.resolve()
        for test_id in enumerate_tests(path):
            jobs.append((path, test_id))

    def rank(job):
        return next((index for index, pattern in enumerate(args.first) if pattern in job[1]), len(args.first))

    jobs.sort(key=rank)  # stable: otherwise file and definition order

    def run(job):
        path, test_id = job
        command = [sys.executable] + (['-W', args.warnings] if args.warnings else []) + ['-m', 'unittest', test_id]
        env = {**os.environ, 'PYTHONPATH': os.pathsep.join(filter(None, [str(path.parent), os.environ.get('PYTHONPATH')]))}
        started = time.monotonic()
        done = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        ran_one = '\nRan 1 test ' in '\n' + done.stdout
        return job, done.returncode == 0 and ran_one, done.stdout, time.monotonic() - started

    started = time.monotonic()
    failed = []
    serial = 0.0
    timings = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for job, ok, output, seconds in pool.map(run, jobs):
            serial += seconds
            timings.append((seconds, job[1]))
            if not ok:
                failed.append(job[1])
                print(f'FAILED {job[0].name}: {job[1]}\n{output}', flush=True)
    wall = time.monotonic() - started
    for seconds, test_id in sorted(timings, reverse=True)[:args.slowest]:
        print(f'  {seconds:6.1f}s  {test_id}')
    print(f'Ran {len(jobs)} tests in {wall:.1f}s with {args.jobs} jobs (serial sum {serial:.1f}s); '
          f'{"OK" if not failed else f"{len(failed)} FAILED"}')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
