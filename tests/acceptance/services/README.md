# Services E2E harness

Real-`sbx` end-to-end scenarios for the services fork (project Docker endpoint, snapshots, forks). The harness
core lives here; scenarios are added by the tasks that gate them. Every scenario ID of the test plan is registered
in `catalogue.py`, unimplemented ones as `pending`.

```bash
python3 tests/acceptance/services/services_uat.py --list          # ID, status, tier, owner, title
python3 tests/acceptance/services/services_uat.py --list-breaks   # the break registry, one JSON cell per line
make services-e2e TIER=smoke DEV_PREFIX=~/.marsh-dev-X
make services-e2e TIER=smoke ONLY=HARNESS-01 BREAK=leak-on-purpose DEV_PREFIX=~/.marsh-dev-X
make services-e2e TIER=break ONLY=HARNESS-01,HARNESS-02 DEV_PREFIX=~/.marsh-dev-X   # needs ~/.marsh-dev-X-breaks
make services-e2e-gc DRY_RUN=1                                    # leftovers of finished runs
```

| Module | Does |
|---|---|
| `services_uat.py` | CLI, the suite on `harness.py`'s `ScenarioSuite`, cleanup and the baseline/leak oracle (H-3), the break guards, the break protocol |
| `scenarios_harness.py` | HARNESS-01/02/03 (HARNESS-03 through H1b's `fixtures/harness03.py` `REGISTRATION`, when present) |
| `scopes.py` | `launch_daemon` (`marshd --home H --test-breaks-from-env`), `reap`, side scopes, the ownership-map UUID proof |
| `plan-r3.3-breaks.json` | an independent copy of the plan's §A.6 defect table (cells, prerequisites, variants) the catalogue is tested against |
| `catalogue.py` | every scenario ID, tier, owner, estimate; every break cell (mode, variant, the one check it fails at, its holds and faults); the hold-point registry |
| `breaks.py` | mode classes, the guard rules, verdicts, `--list-breaks` |
| `nfx_lock.py` | the harness's adapter to the ONE shared VM lock helper `harness/vmlock.py` (path from `NFX_VMLOCK`; `VMLOCK_TEST_PATH` points tests at a scratch lock). No lock logic lives here: no flock, no stale-lock detection, stealing or recovery, and the lock file is never created, deleted, renamed, truncated or written. Who holds it is in `<lock>.info` (display only). `services_uat.py` runs its body as the child of the helper's own CLI (`vmlock.py --run RUN -- ...`, `nfx_lock.supervise`): the helper starts it in its own session, holds the lock until every process of that session has exited and forwards INT/TERM/HUP. The worker proves it is under a live holder from process-table and kernel facts (`hold_proof`: its parent is the helper for this run, the lock is flock-held, it leads its session, `lock.info` names the parent), never from an environment variable, and `--gc` refuses to run without that proof. Nothing the harness spawns leaves the session (`process_group=0`, never `start_new_session`) |
| `nfx_host.py` | baselines and restoration of the sandbox inventory and `sbx policy ls` (rules are rolled back by the IDs this run recorded); the global `sandbox.disk.dockerVolume` is only observed unchanged (revision 3.3: S1 sizes each VM per sandbox) and never restored: a change fails the run |
| `nfx_gc.py` | run records (`harness/runs/<run>.json`: intent first, then stable IDs) and `make services-e2e-gc`; whether a run is finished is never guessed from a pid: the collector holds the VM lock, so every record it finds is a finished run's |
| `observer.py` | `vx` (read-only, `engine="boot"` or `"prod"`, target identity checked before every probe), `vaudit`, `vboot_audit`, `vfreeze`, `vsnap`, `vevents`, the escape canary; `vA`/`vB` are stubs for H1b |
| `holds.py` | the harness side of hold points (`hold-<point>`: `.reached`/`.release` files) |
| `drivers.py` | the `--driver internal\|product` switch (stubs; `internal` = an unprivileged client inside the engine) |
| `evidence.py`, `evidence.schema.json` | per-scenario evidence (H-10), redaction, timing series (H-9) |

Rules the harness enforces:

- **One run at a time per Mac, and the lock is the kernel's.** `--lock-wait SECONDS` waits (with a timeout) for the `flock`; the
  default exits 4 naming the holder from `lock.info` ("display only": a `lock.info` that names a dead pid does not make a
  held lock free, and a live holder is never displaced). A run holds it in a parent process that forwards INT/TERM/HUP to the working child and releases only after reaping it (HARNESS-01 c1 proves that on scratch locks, with a negative control: a holder that dies on SIGTERM frees the lock under live work).
- **Cleanup is scoped to this run and proves ownership by name AND stable ID.** The scope lives in `nfx-<run>/` under
  `/private/tmp` (or `MARSH_UAT_ROOT`); the test daemon runs with `MARSH_VM_PREFIX=marsh-xnfx01-`. A run removes only
  sandboxes whose ID it recorded when it created them (intent persisted before the effect) or that the ownership map
  lists with a UUID that still matches the live one; a name without that proof, or with a different live UUID (an
  impostor), is left in place and fails the run. Policy rules go by the IDs the run attributed to its own operation
  (decision, scope and resources), never by a diff. Resources with the harness prefixes that belong to another run are
  listed in evidence as `foreign` and left alone. `make services-e2e-gc` removes one only on affirmative evidence: a
  run record that lists the name and the ID the resource still has, found while the collector holds the VM lock (so no
  other run is alive); unknown owners and replacements are left alone, and a record is kept until everything it lists
  is gone. A sandbox is created through `create_sandbox`: a name that already exists is refused, the ID is recorded only
  after a successful create, and a failed or ambiguous create adopts nothing.
- **The baseline oracle is not vacuous.** A baseline sandbox or policy rule that disappeared or changed, a new product
  VM in nobody's ownership map, and anything of this run left behind all fail the run: the harness cannot tell
  another tool's churn from a side effect of the product. `--tolerate-baseline-changes` accepts sandbox and
  foreign-scoped policy churn as recorded notes (a removal this harness issued, and global rules, are never excused),
  and the result is then **non-qualifying** (`oracle: inconclusive-tolerated`, HARNESS-01 c2 skipped): the matrix does
  not accept it. A new product VM under a name another run's record lists is foreign only if its live ID equals the one
  recorded.
  HARNESS-01 c2 runs the oracle against adversarial inventories to prove it catches each defect.
- **Break runs.** A break mode reaches a daemon only if the harness launches `marshd --home <home>
  --test-breaks-from-env` itself with `MARSH_TEST_BREAK` in its environment (H2); a CLI-started daemon, or a
  daemon launched without the flag, never honors the variable (A10), and a release `marshd` refuses the flag. Every
  break, fault and hold run, and a breaks-build pass 2 even with no modes, launches the daemon this way, and the
  daemon that answers `marsh status --json` must be that process. `test_breaks` must be present and exactly the
  requested modes (or absent altogether when none were requested: a present null or empty list is not absent).
  `--break MODES` must be a complete cell: the defect and exactly its holds, faults and paired controls (naming
  only the defect adds them).
- **Three passes per cell** (`TIER=break`): (1) normal build; (2) breaks build with only the holds and faults the cell
  needs; (3) the same plus the defect. Pass 3 is verified only if the scenario fails at the one check the cell names
  (`failed_check`) and its process exits 0 (a child that prints `verified` and then fails cleanup or validation is an
  invalid negative control). A setup failure, unexpected error, timeout or a failure at another check is an **invalid
  negative control**; a pass is **not detected**; a pending scenario is not a pass (`--require-executed`). All fail
  the matrix.
- **The matrix coordinator holds no lock and forwards cancellation.** Each pass child is its own lock holder; the coordinator
  forwards INT/TERM/HUP to the running child, starts no further pass and waits for the child to finish its own cleanup. A child's
  timeout starts at its handshake (the worker creates `NFX_STARTED_FILE` only after proving it holds the lock), never while it
  waits for other agents' runs.
- **Evidence** is `evidence/<run>/<ID>.json`, validated against `evidence.schema.json` when written and again for every
  file at the end of the run. Public calls are `commands`; `vx` calls are `observations`. Every string is redacted with
  the literals and patterns in `fixtures/redaction.json` (H1b) and a few built-in synthetic-secret patterns.
- **Timing.** `Series(label, N)` collects N+3 and drops the first 3 as warm-ups; `Series(label, N, cold=True)` drops none.
  Failed and gapped attempts stay in the series.
- Pending scenarios do not fail a run unless `--strict-pending`.
