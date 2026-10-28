# Results and shell history

`marsh results` and `marsh jobs` read durable receipts for registered-command
invocations, with different list views. Brush's `history` builtin continues to
show ordinary shell command history.

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
cleanup result, timings, and bounded arguments for display. It stores the first
16 arguments, each truncated to 200 characters with lossy UTF-8 decoding
(`DaemonStore::set_job_args`). A prompt passed as an argument can be retained.
The journal captures no stdin, stdout, or stderr. Treat paths and arguments as
potentially sensitive metadata. JSON receipts expose
`execution` as a typed worker outcome, independently of the public `exit`
code/cause. Legacy receipts without that field report
`unknown`; do not reconstruct it by parsing diagnostic prose.

The versioned checksum journal lives under the host-only control directory at
`$HOME/Library/Application Support/marsh/control/<selected-home-hash>/state/results.journal`.
When `MARSH_CONTROL_HOME` is set, the journal uses
`$MARSH_CONTROL_HOME/<selected-home-hash>/state/results.journal`.
`selected-home-hash` means SHA256 of canonical **`MARSH_HOME`**, not its `home/`
child. Both status forms report the absolute `control_home` path. The control
and state directories are mode `0700`, and the journal is mode `0600`;
the daemon never imports a journal from guest-writable `$MARSH_HOME/state`.

Terminal transitions are synchronized before completion is reported. During
replay, the daemon truncates an incomplete final frame, but any checksum or
structural failure in a complete frame prevents startup. An owner-only lock
in the state directory excludes another journal writer for the selected home,
even with a different `TMPDIR`. Stop daemons from builds predating that lock
before upgrading, since they do not hold it.

Retention keeps the newest 1,000 terminal receipts and all active receipts.
After a daemon exit, replay marks queued or running jobs `unknown` with cause
`daemon_restarted`; it never resumes the old containers. Quarantine blocks
worker reuse within a daemon generation, but the adapter's in-memory
quarantine set does not survive restart.
Process admission separately retains uncertain jobs' held slots from receipts
until a successful `marsh workers reset KIT` is recorded. Use that reset to
retire the affected owned VM; do not delete the journal to recover.

## Change events

`marsh events --json` (host only) streams NDJSON until it is killed: first
`{"type":"hello","seq":N,"features":[...]}`, where N is the last change
already made, then one line per change, each with the next `seq`:

- `{"seq","type":"job","job":<the jobs --json summary>}` whenever a receipt
  is journaled;
- `{"seq","type":"split",...}` and `{"seq","type":"checkpoint",...}`
  (`workspaces.md`, s14);
- `{"type":"heartbeat"}` every 15 s without a change.

The subscription is not an ordinary request: it never holds off `marsh stop`,
`marsh reset`, or the daemon's exit, and it ends with the daemon. Publishing
never blocks: each subscriber has a queue of 1024 lines, and one that falls
that far behind gets `{"type":"resync"}` and its stream ends. A client then
lists jobs, splits, and checkpoints again and subscribes anew. At most 8
subscriptions are open at once (each holds one of the daemon's 128
connections, so the rest stay free); a ninth fails with "marsh events: at
most 8 subscribers at once", and a closed one frees its place.

## Scope configuration and job limits

Use `marsh status` to find the host-control directory and inspect the daemon's
per-job defaults. `marsh status --json` exposes `control_home` and `job_defaults`;
`job_defaults.resources` contains the effective values and
`job_defaults.environment_overrides` lists the fixed variable names supplied
at daemon startup. Fields not listed use built-in defaults. An inspection-only
daemon can report no job defaults; absence is not an unlimited policy.

| Resource | Built-in per-job value | Daemon-launch variable |
| --- | --- | --- |
| CPU | 2,000 millicpus (2 CPUs) | `MARSH_JOB_CPU_MILLIS` |
| Memory | 4 GiB (4,294,967,296 bytes) | `MARSH_JOB_MEMORY_BYTES` |
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

ACP agents are not configured here: a command is an agent when its Kit
declares `com.docker.sandbox/acp@1` ([registry guide](command-registry.md#acp-adapters)).
