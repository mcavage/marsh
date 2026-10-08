# Results and shell history

`marsh results` (also `marsh jobs`) shows durable structural receipts for registered-command
invocations. It is intentionally separate from Brush's `history` builtin:
`history` continues to show the user's ordinary shell command history.

```console
marsh results
marsh results show 17
marsh results show 2f41f39a
marsh results --json
marsh results show 17 --json
```

The list is newest first and displays the full job UUID, placement, lifecycle
status, cleanup state, `WALL` time and exit code. `WALL` is the receipt's
`timing.wall_ms`: the whole request from the invocation to verified cleanup,
including Kit VM preparation (`vm_prepare`) and image work. `marsh jobs`
shows `RUN` instead: from the job record's creation (the Kit VM is ready) to
its end. A cold first call therefore shows a much larger `WALL` than `RUN`;
`marsh results show ID --json` breaks `WALL` down by phase. Read cleanup separately from
status: a command can finish while deletion remains uncertain. `show` accepts
the displayed cursor, a full job UUID, or a unique UUID prefix. The existing
`marsh jobs --json` and `marsh jobs show JOB --json` interfaces remain available
for automation.

The same-user daemon stores only structural receipt fields: command registry
name, opaque execution identities, public mount targets, state, exit cause,
cleanup result, and timings. It never stores prompt bytes or captured stdout or
stderr in this journal. This is execution metadata, not transcript history or
an assertion that path and command metadata are secret-free. JSON receipts expose
`execution` as a typed worker outcome, independently of the public `exit`
code/cause. Legacy receipts without that field report
`unknown`; do not reconstruct it by parsing diagnostic prose.

The versioned checksum journal lives under the host-only control directory at
`$HOME/Library/Application Support/marsh/control/<selected-home-hash>/state/results.journal`.
When `MARSH_CONTROL_HOME` is set, the journal uses
`$MARSH_CONTROL_HOME/<selected-home-hash>/state/results.journal`.
`selected-home-hash` means SHA256 of canonical **`MARSH_HOME`**, not its `home/`
child. `marsh status` and `marsh status --json` report the absolute `control_home`
so callers do not need to reconstruct that path. The control and state
directories are mode `0700`, and the journal is mode `0600`. A journal in the guest-writable
`$MARSH_HOME/state` is never loaded or imported. Terminal transitions are synchronized before they are
reported complete. Replay truncates only an incomplete final frame; a checksum
or structural failure anywhere, including the last complete frame, fails
daemon startup. An owner-only lock in the same state directory prevents another
daemon from replaying or writing that selected home's journal, even if its
`TMPDIR` differs. Drain resident daemons from builds that predate this lock
before upgrading, since those processes do not hold it. Retention is bounded to the
newest 1,000 terminal receipts, while currently active receipts are retained.
Jobs left queued or running by a daemon exit are reported as `unknown` with a
`daemon_restarted` cause after replay. The daemon does not claim to resume the
old container. Quarantine blocks worker reuse in the current daemon generation.
The receipt remains durable evidence, but restarting the daemon does not restore
quarantine from historical receipts. Independently verify removal of the affected
VM before recreating work; do not delete the journal to recover.

## Scope configuration and job limits

Use `marsh status` to find the host-control directory and inspect the daemon's
per-job defaults. `marsh status --json` exposes `control_home` and `job_defaults`;
`job_defaults.resources` contains the effective values and
`job_defaults.environment_overrides` lists the fixed variable names supplied
at daemon startup. Fields not listed use built-in defaults. An inspection-only
daemon can report no job defaults; absence is not an unlimited policy.

| Resource | Built-in per-job value | Daemon-launch variable |
| --- | --- | --- |
| CPU | 4,000 millicpus (4 CPUs) | `MARSH_JOB_CPU_MILLIS` |
| Memory | 8 GiB (8,589,934,592 bytes) | `MARSH_JOB_MEMORY_BYTES` |
| PIDs | 4,096 | `MARSH_JOB_PIDS` |
| Writable layer | 10 GiB (10,737,418,240 bytes) | `MARSH_JOB_WRITABLE_BYTES` |
| Combined stdout/stderr | 256 MiB (268,435,456 bytes) | `MARSH_JOB_OUTPUT_BYTES` |
| Wall time | 24 hours (86,400 seconds) | `MARSH_JOB_WALL_SECONDS` |

The writable-layer limit is enforced by detect-and-kill, not by the storage
driver: the Kit VM's containerd overlayfs snapshotter ignores Docker's
`--storage-opt size` (still passed, and honored where supported). While a job
runs, its worker samples the allocated size of the container's overlay upper
directory every 20 ms (less often for very large writable trees, so the
measurement stays under ~10% of one CPU) and kills the container's cgroup once
it exceeds the limit; the receipt's cause is `limit:writable`. A fast writer
can exceed the limit by up to what it writes in one sampling window before the
kill lands (tens of MiB on a Kit VM disk), plus whatever it writes in the first
moments before the worker has found the started container. Files in mounted
directories (the project, the guest home) are not part of the writable layer.

The first launcher starts the same-user daemon with its environment. That
snapshot applies to jobs in the scope, not just that shell. Values must be
positive decimal integers within their supported range; zero does not mean
unlimited. Later shells cannot change a running daemon's defaults. A conflicting
explicit `MARSH_JOB_*` setting is rejected rather than silently ignored.
Unset conflicting variables to inspect status. To change the snapshot, finish
active work, close shells, stop that scope, then launch with the desired settings;
or use a separate `MARSH_HOME`. These are job resource limits, not hard provider
billing limits or per-shell policy flags.

Host-control `commands.json` merges over the packaged command registry by name.
Both declarations are validated, including shadowed references; the combined
registry is limited to 251 commands. Names are at most 128 ASCII bytes using
letters, digits, `_` and `-`, cannot start with `-`, and cannot use reserved
product names. Duplicate JSON keys are rejected. See the
[shared name rules](../../crates/marsh-contracts/src/command_registry_rules.json)
and [registry guide](command-registry.md).

Host-control `agents.json` instead **replaces** the complete packaged ACP agent
list. A missing file uses packaged defaults; `[]` intentionally disables ACP.
It does not merge individual agents or revive omitted defaults. Malformed or
unsafe declarations and invalid ACP-to-Kit bindings fail daemon startup, rather
than silently disabling ACP. Fix the reported host-control or packaged file;
do not delete state to bypass a configuration failure.
