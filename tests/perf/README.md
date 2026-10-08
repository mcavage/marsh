# Local warm-path performance probe

`make perf` runs an opt-in benchmark against the assembled release binaries and
stock Docker Sandboxes on the local Apple Silicon host. It creates a private
temporary `MARSH_HOME`, registers the acceptance fixture as `fixture`, warms the
shell and worker VMs, discards five samples, and records 30 sequential warm
invocations by default.

The JSON report contains every end-to-end sample, its matching public job
receipt, and nearest-rank p50/p95 summaries for end-to-end latency, receipt
wall/orchestration time, time outside the receipt, and every receipt phase.
There is intentionally no pass/fail latency threshold: compare reports from
the same unloaded machine, SBX version, Kit, and cache state.

~~~console
make perf
make perf PERF_SAMPLES=50 PERF_WARMUPS=10
~~~

Results are printed and written to `target/perf/result.json`. The runner removes
only VM identities derived from or reported by its isolated scope, terminates
that scope's daemon, and removes its temporary home. `PERF_KIT`, `SBX`, and
`PERF_OUTPUT` can override the fixture, stock CLI, and report path.

## Dev-build warm probe

`tests/perf/warm.py` measures warm `marsh -c true` and
`marsh -c 'fixture identity'` against a `make dev` build, and counts the stock
`sbx` processes each warm call starts. It uses an isolated `MARSH_HOME`, removes
only VMs that appeared during the run, and stops its own daemon.

~~~console
python3 tests/perf/warm.py --marsh target/release/marsh \
  --guest-artifacts target/libexec/marsh --kit <fixture-ref>
~~~

`--split-repo PATH` (repeatable) clones each repository into the project (the
first one is the project itself) and times `split { a: true, b: true } | join`
against `fanout { a: true, b: true } | collect` there, plus a replay of the
split's Git plumbing inside the shell VM that reports snapshot, allocate,
capture, and release separately from branch time:

~~~console
python3 tests/perf/warm.py --marsh ~/.marsh-dev/bin/marsh \
  --guest-artifacts ~/.marsh-dev/libexec/marsh --kit <fixture-ref> \
  --split-repo "$PWD" --split-repo /path/to/small-repo
~~~

The runner removes only leftover VMs recorded in its own scope's ownership map.

