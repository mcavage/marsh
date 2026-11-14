# Test breaks

Status: implemented for the Mac-side daemon. No guest break mode exists yet.

E2E tests need to show that each checked behavior actually fails when broken.
A test build can therefore break one named behavior on purpose. The mode is
selected by the `MARSH_TEST_BREAK` environment variable of the **daemon**, and
only a daemon the test harness launches with the explicit flag
`--test-breaks-from-env` honors it:

```console
MARSH_TEST_BREAK=demo <prefix>/bin/marshd --home <home> --test-breaks-from-env   # harness launch
marsh status --json                                                               # "test_breaks":["demo"]
```

`MARSH_TEST_BREAK` is a comma-separated list of mode names. The daemon reads it
once, at startup. **Without `--test-breaks-from-env` a feature-built daemon
ignores the variable entirely.** The `marsh` CLI never passes the flag when it
auto-starts a daemon, whatever features the CLI or the daemon was built with,
so no CLI environment can select a mode. A release `marshd` does not know the
flag and refuses it as a usage error.

## Mechanism

- `marsh_contracts::break_mode(name: &str) -> bool` is the only entry point. It
  consults a set that is empty until
  `marsh_contracts::test_breaks::init_from_daemon_launch(&mut args)` runs.
  Nothing else reads the environment.
- `marshd` calls `init_from_daemon_launch` first thing in `main`, with its
  arguments. The function removes `--test-breaks-from-env` from them. Only if the
  flag was there, and only on macOS (the host), does it read `MARSH_TEST_BREAK`.
  Without the flag, or on any other target, the set stays empty and the
  environment is never read.
- The `test-breaks` cargo feature on `marsh-contracts` enables all of the above.
  `marsh-sbx`, `marsh-daemon`, `marsh-backend`, `marsh-worker`, `marsh-runtime`, `marsh-mcp` and `marsh` each
  declare `test-breaks`, forwarding it to `marsh-contracts/test-breaks`. Every
  feature is off by default. Without it, `break_mode` is `const` `false`,
  `init_from_daemon_launch` does nothing, and the variable name does not appear
  in the binary.
- The daemon reports the modes its set selected in the `test_breaks` field of
  `marsh status --json` (`StatusDocument`). The field is omitted when empty. The
  known modes are listed in `KNOWN_TEST_BREAKS` (`crates/marsh-daemon/src/lib.rs`).

## The boundary

| Process | Reads `MARSH_TEST_BREAK`? | Break modes |
|---|---|---|
| `marshd` on macOS, launched by the harness with `--test-breaks-from-env` | yes, once at startup | the listed modes |
| `marshd` on macOS, launched without the flag (a feature build, direct launch) | never | none |
| `marshd` on macOS, started by the `marsh` CLI on demand (any CLI build, any `marshd` build) | never: the CLI does not pass the flag | none |
| `marsh` CLI, `marsh-mcp`, `marsh-relay` on macOS | never | none |
| Guest binaries on Linux (VM or container), including the guest `marshd` even with the flag | never (the set is empty on Linux) | none; a future guest mode arrives as a daemon-attested field |
| Jobs, shells, project and job environment | never | none |

A CLI's environment cannot select a mode, whether or not a daemon is running,
and whatever the CLI's build. A daemon that is already running ignores later CLI
environment. The boundary is enforced at the daemon's launch argument, so it
does not depend on the CLI being built without the feature (a default CLI next
to a feature `marshd` is tested).

## Harness launch path

The harness (H1a onward) starts the daemon itself, with the mode in its
environment and the flag on its command line:

```console
MARSH_TEST_BREAK=demo MARSH_SBX=$(command -v sbx) \
  MARSH_CONTROL_HOME=<scope>/control \
  <prefix>/bin/marshd --home <scope>/home --test-breaks-from-env
```

The exact flag is `--test-breaks-from-env`. Wait until the endpoint socket
exists. It is `/tmp/marsh-UID/SCOPE/s`, where `SCOPE` is the first 16 hex digits
of the SHA-256 of the canonical home path. Then every `marsh` command for that
home talks to this daemon. End with `marsh stop`, and on any failure path stop
and wait for the daemon before deleting the scope (`tests/breaks/daemon_boundary.sh`
does, including on INT, TERM and HUP). A harness that auto-starts the daemon
through the CLI, or sets only the variable, gets no modes.

## Releases

Release packaging refuses feature-enabled binaries in layers. The first two
fail early; the last two are independent checks at packaging time.

1. **Compile time.** `marsh-contracts/build.rs` sets the cfg
   `marsh_release_packaging` when `MARSH_RELEASE_PACKAGING=1`. Combined with
   `test-breaks`, the cfg triggers a `compile_error!`
   (`crates/marsh-contracts/src/test_breaks.rs`). The release path sets the
   variable: CI on the host build (`make build-host`), CI and `make build-linux`
   on the guest build (`RELEASE_PACKAGING=1`, see
   `packaging/linux-arm64/Dockerfile`).
2. **Make.** `make dist` fails first when `HOST_FEATURES`,
   `ACCEPTANCE_HOST_FEATURES` or `DEV_FEATURES` contains `test-breaks`
   (`check-release-features`).
3. **Build receipt** (does not depend on binary contents).
   `make build-host`, `make dev-host` and the guest Dockerfile run the cargo
   build through `scripts/provenance.py build`. The tool adds
   `--message-format=json-render-diagnostics` and records what cargo reports:
   every unit it compiled with the features it enabled for that unit, and each
   executable with its sha256. Nothing is supplied by the caller. The receipt is
   `target/release/provenance.json` (host) and `provenance.json` beside the guest
   binaries. Build groups are content-addressed (the id is a hash of the units
   and features) and never rewritten, and the tool refuses to rebind an artifact
   name already recorded, so a later build cannot relabel an earlier artifact.
   `scripts/dist.sh` runs `provenance.py verify`, which fails closed: it refuses
   an artifact whose receipt is missing, has an unknown version or is not valid,
   whose group is missing, has no feature data or does not match its id, whose
   package is not in its group, whose hash differs, or whose build enabled
   `test-breaks`. So an executable that forwards the feature but has no live
   reference to `marsh-contracts` (and so no marker) is still refused, and so is
   an artifact swapped in after its build.
4. **The binaries themselves.** Every shipped executable's crate has a
   `build.rs` calling `marsh_provenance::build::emit()` and calls
   `marsh_provenance::embed!()` in `main` (`crates/marsh-provenance`). The
   build script sees the crate's own `CARGO_FEATURE_TEST_BREAKS`, or
   `DEP_MARSH_CONTRACTS_TEST_BREAKS` (exported by `marsh-contracts` through its
   `links` key) when any contracts consumer enabled the feature, and sets
   `MARSH_PROVENANCE_MARKER=marsh-feature:test-breaks`; `embed!()` keeps it in
   the executable, with or without any code that uses the feature. `marsh-contracts` also
   keeps the same bytes. `scripts/check-no-test-breaks.sh`, run by `dist.sh`,
   refuses a file with the marker or the name `MARSH_TEST_BREAK`.
   `scripts/check-provenance-wiring.py` (`make lint`) fails if a shipped
   executable lacks the build script or the `embed!()` call. `make dist-local`
   (a dev build) is not checked.

`dist.sh` runs both packaging checks every time and names every file either one
refuses.

`scripts/test-release-refusal.sh` is the regression. Each case starts from
all-normal artifacts and swaps exactly one thing: each of the nine shipped
files in turn (refusal must name that file at both layers); the feature build's
own receipt (refusal on resolved features, not on a hash); a wired executable
with a forged clean receipt (only the marker can refuse it); the reviewer's
feature-forwarding executable with no live contracts reference and no wiring
(only the receipt can refuse it, through `make dist`); group reuse (a normal
build recorded after the feature probe cannot clean it); malformed and
inconsistent receipts (missing or relabelled group, no feature data, retargeted
artifact, missing hash, unknown version, truncated, not an object); the
`--all-features` CLI together with a test-breaks guest worker (both named
independently); and a missing receipt. A control with all-normal artifacts must
package. The script creates and removes only its own temporary directory.

Builds that set no `DEV_FEATURES`, `HOST_FEATURES`, or
`ACCEPTANCE_HOST_FEATURES` use default features. `check-no-test-breaks.sh`
must pass on them: `scripts/check-no-test-breaks.sh target/release/marsh
target/release/marshd`.

`make acceptance` is not gated. Its candidate may carry a test feature, so
release refusal does not apply there. A candidate built with a break feature
is not a release pass. Guest breaks would need a separately named breaks
candidate, which does not exist yet.

## Test builds

```console
make dev DEV_FEATURES=test-breaks DEV_PREFIX=/path/outside/checkout
```

`DEV_FEATURES` is passed to both the host build (`dev-host`) and the Linux
guest build (`packaging/linux-arm64/Dockerfile`, `CARGO_FEATURES`). Without it,
`make dev` is unchanged. A dev install built this way is for tests only.

## Tests

- `tests/breaks/daemon_boundary.sh FEATURE_PREFIX DEFAULT_MARSH` (macOS): P1
  positive (harness launch with the flag reports `demo`), N0 (same variable,
  no flag: nothing), N1 (feature CLI with the variable and no daemon: the
  auto-started daemon reports nothing), N2 (CLI with the variable and a daemon
  launched without it: nothing), N3 (mixed binaries: a default CLI next to a
  feature `marshd`, auto-start: nothing). It stops and waits for every daemon it
  started on every exit path before deleting scopes, and fails if it cannot. It
  signals only exact pids it started and recorded (start time and the exact
  command line, compared as a whole against the commands of its own scopes,
  re-checked before each signal) and removes only paths it created. It keeps
  its records in files, never in word-split variables, so a `TMPDIR` with spaces
  or glob characters is safe. It never finds a process or directory by pattern.
- `tests/breaks/daemon_boundary_safety.sh FEATURE_PREFIX DEFAULT_MARSH` (macOS):
  runs the boundary script with `TMPDIR="<base>/safety case"` next to a separately
  owned decoy daemon whose home is `<base>/safety/home` (the path truncated at
  the space). S1 (normal run) and S2 (CLI failure after an auto-start) must leave
  the decoy running and answering and remove everything the run created.
- `tests/breaks/daemon_boundary_preflight.sh FEATURE_PREFIX DEFAULT_MARSH`
  (macOS, no real daemon, no sbx): both boundary scripts refuse (exit 2) when
  the runtime directory their first daemon would create already exists (a
  dangling symlink counts), and neither removes a runtime that another actor
  creates first. The test points the scripts at a fake runtime root inside its
  own temporary root (`BREAKS_RUNTIME_ROOT`) and at fixed work directories
  inside it (`BOUNDARY_FIXED_WORK`, `SAFETY_FIXED_BASE`), creates its fixtures
  exclusively (noclobber, `mkdir` without `-p`, `ln` without `-f`), and asserts:
  the refusal, every pre-existing file unchanged, and, when a stub `marshd`
  creates the runtime after the preflight and then fails to start, that the
  other actor's four files (`s`, `l`, `t`, the startup lock) are untouched.

- `tests/breaks/guest_env.sh GUEST_DIR` (Docker, linux/arm64): a feature-built
  guest `marshd` launched with `MARSH_TEST_BREAK=demo` and the flag, and a guest
  `marsh` status, report no modes.
- `scripts/test-release-refusal.sh` (Docker and cargo): see "Releases".

## Ownership rules for the test scripts

Every script under `tests/breaks/` and `scripts/test-release-refusal.sh` follows
the same rules:

1. All state (homes, scopes, decoys, fixtures, artifacts, logs) lives under
   **one root created atomically by `mktemp -d`** by that run. Removing that
   root, after its device and inode are checked against the ones recorded at
   creation, is the only deletion the script ever performs.
2. **No script deletes daemon runtime files** (`/tmp/marsh-UID/...`). Owned
   daemons are stopped by recorded pid (start time and exact command line
   checked before each signal). A clean stop removes the daemon's socket, lock
   and token; an empty runtime directory and a zero-byte `.startup-lock` can
   remain (a few per run, in `/tmp/marsh-UID/`). Remaining paths are reported;
   the run fails only if a daemon it started is still alive.
3. Collision fixtures are created exclusively, inside the root, and are never
   recorded for cleanup: the root's removal is the only cleanup.
4. **Assumption.** A runtime path is derived from a home path that lives inside
   the run's mktemp root (`/tmp/marsh-UID/` + the first 16 hex digits of the
   SHA-256 of the canonical home). For another actor to create that runtime
   before our daemon does, it would have to guess the mktemp root's random name.
   The scripts therefore treat an existing runtime path as a refusal (exit 2)
   but do not try to defend against a guessed one beyond never deleting it.

## Adding a mode

1. Pick a short, specific name, for example `daemon-skips-cleanup`.
2. Add it to `KNOWN_TEST_BREAKS` in `crates/marsh-daemon/src/lib.rs`, and add a
   row to the table below.
3. At the one code location that implements the behavior, guard the broken path
   with `if marsh_contracts::break_mode("name") { ... }`. Only the daemon may
   call it, because the set is initialized only in the daemon.
4. A mode that must act inside a guest is selected by the daemon from its own
   set and sent to the guest in a dedicated field of the daemon-to-VM protocol.
   Never forward environment into a job or a guest.
5. Add an E2E test that starts the daemon with `MARSH_TEST_BREAK=name --test-breaks-from-env` and
   asserts the behavior is violated, plus a run without it that asserts the
   behavior holds. Unit tests that only restate the function are not needed.

## Modes

| Mode | Where | Breaks | Checked by |
|---|---|---|---|
| `demo` | daemon status (`marsh-daemon`) | Reports `"test_breaks":["demo"]` in `marsh status --json`. Plumbing proof only, not a behavior check. | `tests/breaks/daemon_boundary.sh` P1 (plumbing only) |
| `vm-per-scope` | `marsh-sbx` `services_vm.rs` `services_key` | The ownership key of a services VM is the daemon scope, not the project root: two projects share one VM, and the second project's folder is mounted into it. | SVC-VM-02 c1; SVC-SEC-06 c1 |
| `lease-none` | `marsh-backend` `services.rs` (`services_start`) | A lease holder is not recorded, so nothing keeps the VM from its idle timer while it is held. | SVC-VM-05 c1 |
| `idle-stop-off` | `marsh-backend` `services.rs` (`services_idle_stop`) | The idle timer expires but never stops the VM. | SVC-VM-05 c2 |
| `services-owner-uuid-check-off` | `marsh-sbx` `services_vm.rs` (`ensure_services_vm_inner`) | A VM that holds the recorded name with another UUID (an impostor) is adopted instead of refused, and used. | SVC-VM-10 c1 |
| `services-stop-deletes-data` | `marsh-backend` `services.rs` (`services_scope_teardown`) | `marsh stop` removes the services VMs (and their data) it should only stop. | SVC-VM-10, stop keeps data (D18; cell per the revised test plan) |
| `reset-skip-confirmation` | `marsh-sbx` `services_vm.rs` (`remove_confirmed_services_vms_before`) | `marsh reset --include-services` removes every services VM, including projects the user did not confirm. | SVC-VM-10, only the confirmed projects go (cell per the revised test plan) |
| `services-extra-mount-ok` | `marsh-sbx` `services_vm.rs` (`services_mounts_problem`) | Readiness tolerates a host mount other than the project (the check that `sbx inspect` lists exactly the project is skipped). | SVC-VM-02 (only the project is mounted; security review L3) |
| `services-disk-size-env-off` | `marsh-sbx` `services_vm.rs` (`create_services_vm`) | The create does not carry the Docker disk size, so the wrong-size removal path runs for real. | S1 disk-size gate (no test-plan row; Opus L-a: the removal leaves no dummy secrets) |
| `services-warm-reconcile-off` | `marsh-sbx` `services_vm.rs` (`ensure_services_vm_inner` warm path, `reconcile_services_secrets`) | A running VM's dummy secrets are reconciled only at a cold start: neither a warm ensure nor `services status` shadows a global secret that appeared meanwhile. | S1 credential shadowing (A12; Sol's round 3-4 blocker) |
| `services-enable-per-home` | `marsh-backend` `services.rs` (`Settings::enabled_for`) | One enabled project enables services for every project of the home. | SVC-VM-01 / SEC-09 (per-project opt-in, A11) |
| `reset-include-services-noop` | `marsh-backend` `services.rs` (`services_remove_confirmed`) | `--include-services` is accepted, confirmed, and removes nothing. | SVC-VM-10, the flag removes the confirmed projects (cell per the revised test plan) |
| `services-default-on` | `marsh-sbx` `services_net.rs` (`InternetGrants::is_on`) | A project nobody toggled starts with Internet on: `engine-up` opens the gate and `status` says `on:true`. | SVC-VM-08 c1; SVC-SEC-17 c2 |
| `internet-noop` | `marsh-backend` `services.rs` (`services_internet`) | `internet on` and `internet off` report success and change nothing. | SVC-VM-08 c2, c3 |
| `internet-off-noop` | `marsh-backend` `services.rs` (`services_internet`) | `internet off` reports success and changes nothing: the gate stays open. | SVC-VM-08 c3 |
| `internet-ignore-governed` | `marsh-backend` `services.rs` (`services_internet`) | The governance check is skipped: `internet on|off` goes ahead although an sbx rule governs the VM. | SVC-VM-09 c2 |
| `governance-any-deny` | `marsh-sbx` `services_net.rs` (`assess`) | Any global deny counts as governing, however narrow: a single-host deny makes `internet on` refuse and `governed:true`. | SVC-VM-09 c4 |
| `gate-by-name` | `marsh-sbx` `services_net.rs` (`engine_internet`) | The Internet gate is addressed as `docker exec marsh-svc-engine`, by name, not by the container id the daemon recorded at creation: a container the VM renamed into that name answers for the engine. | HF1 `gate-by-id` |
| `services-cli-in-job` | `marsh-daemon` `lib.rs` (`handle`, capability branch) | A job's capability socket is served host-only `services` requests (a job can run `marsh services internet on`). | SVC-SEC-09 c1 |
| `relay-drop-chunk` | `marsh-services` `host.rs` (`handle_data`, `writer_loop`) | One data chunk in 700 is lost in each direction, so bytes are wrong but flow control stays in step. | SVC-EP-06 c1 |
| `slow-relay` | `host.rs` (`writer_loop`) | Every frame the daemon writes waits 2 ms: a latency control. | SVC-PERF-05 |
| `backend-services-disconnected` | `marsh-backend` `services_snapshots.rs` (`services_capture`) | The backend has no services connection: a checkpoint of a running project records no `services` at all. | SVC-CAP-01 c1 |
| `capture-ensure-vm` | `services_snapshots.rs` (`services_capture`) | A checkpoint makes sure the services VM exists and runs before it looks, so a project whose VM was never created gets one from a checkpoint. | SVC-CAP-01 c5 |
| `reuse-last-snapshot-off` | `services_snapshots.rs` (`services_reuse_decision`) | A checkpoint made while the VM is stopped finds nothing to reference: `services: null` instead of `reused_from`. | SVC-CAP-08 c1 |
| `idle-final-capture-off` | `services_snapshots.rs` (`services_cutover`) | The idle stop takes no final capture and records nothing: the reuse source is the old last capture (with the stopped-outside-marsh warning). | SVC-CAP-08 c2 |
| `retention-keep-snapshots` | `marsh-daemon` `checkpoint.rs` (`retain`) | Retention removes the checkpoints and keeps their snapshots (only the orphan collector still finds them). | SVC-CAP-04 c1 |
| `orphan-gc-off` | `checkpoint.rs` (`collect_orphans`) | The orphan collector never drops a snapshot no checkpoint references. | SVC-CAP-04 c4 |
| `reuse-lease-off` | `checkpoint.rs` (`in_use`) | Garbage collection ignores `reused_from` references, so a snapshot only a reusing checkpoint names is dropped; the field the checkpoint returns stays intact. | SVC-CAP-08 c3 |
| `admit-ignore-pool` | `services_snapshots.rs` (`reserve_bytes`) | The reserve sent with the capture request is zero: a capture (a checkpoint's, or the stop's) below the reserve is still taken. | SVC-CAP-05 c1; C12 stopfail, space |
| `space-check-stale` (agent) | `marsh-services` `capture.rs` (`check_reserve`) | The agent reuses its first free-space reading for every later capture, as a reading taken before a wait would be. | C12 space (`hold-checkpoint-before-space-check`) |
| `fault-pool-unreadable` / `unreadable-pool-ok` (agent) | `capture.rs` (`check_reserve`) | Fault: the agent's `fstatvfs` on its pool descriptor fails. Defect: an unreadable pool is captured anyway. | C12 space |
| `hold-checkpoint-before-space-check` | `services_capture` | A hold (`test-holds/checkpoint-before-space-check.reached` / `.release` in the daemon home) just before the space check. | C12 space |
| `services-feature-off` (alias `feature-hidden`) | `marsh-backend` `services.rs` (`services_feature`) | `services-v1` is never listed in `marsh status --json`, though the build can run services. | SVC-VM-01 c1 (C4 witness) |
| `ignored-skip` (alias `ignored-capture-off`) | `marsh-daemon` `ignored.rs` (`capture`) | A checkpoint saves no git-ignored state: `ignored/` stays empty and the record says `files: 0`. | SVC-CAP-09 c1 (C5 witness) |
| `ignored-follow-symlink` (alias `ignored-follow-outside-symlink`) | `marsh-daemon` `ignored.rs` (`take`) | A symbolic link (a `.env*` file, a referenced folder or a link on the way) is followed by path and what it points at is copied into `ignored/`. | SVC-CAP-10 c2 (C5 witness) |
| `capture-no-skip` (alias `capture-block-next-prompt`) | `marsh-backend` `services_snapshots.rs` (`capture_through_within`) | A checkpoint's services capture has no budget: it is waited for however long it takes (with the agent's `slow-capture`, the checkpoint is held up past the budget and records no skip gap). | P2 (budget witness) |
| `stop-pause-off` | `services_cutover` | The containers are not paused and nothing is counted: containers write between the final capture and the stop. | C12 fresh (continuous writer) |
| `stop-fence-off` | `services_cutover` | The agent's write fence (the filter's freeze gate on every tracked workspace) is not taken: a container started or unpaused through the endpoint is not refused. | HF2 fence (both windows) |
| `hold-cutover-after-capture` / `hold-cutover-after-count` | `services_cutover` | Holds (`test-holds/cutover-after-capture.reached` / `.release` and `cutover-after-count`) after a workspace's capture and after the engine count. | HF2 fence |
| `fault-engine-query` / `engine-query-fail-open` | marsh-sbx `services_net.rs` (`services_running_count`) | Fault: the count's `docker ps` cannot reach the engine. Defect: the count is read through `\| wc -l`, so the failure reads as zero. | HF2 enginequery |
| `capture-gate-ignored` | `services_snapshots.rs` (`services_stop_prepare`) | An explicit stop goes ahead although the capture exclusion could not be acquired. | HF2 exclusion |
| `manifest-dangling-ref` (agent) / `manifest-refs-unchecked` | `capture.rs` (`group_containers`) / `checkpoint.rs` (`from_manifest`) | Fault: the agent's manifest names a volume it does not list. Defect: the record accepts a container's reference to an unlisted volume. | HF2 manifest |
| `lane-reconcile-off` | `services_snapshots.rs` (`services_reconcile_lanes`) | Recovery does not untrack a lane capture whose checkpoint record is gone: the journal protects it for ever. | HF2 laneorphan |
| `stop-base-only` | `services_cutover` | Only the base workspace is captured at the stop; every lane is recorded as captured anyway. | C12 fresh (lane) |
| `reuse-ignores-failure` | `services_reuse_decision` | A failed final capture is ignored: the old snapshot is offered as this turn's data, with no gap. | C12 stopfail |
| `idle-ignores-inflight` | `services.rs` (`services_idle_stop`) | The idle stop starts under a checkpoint capture in flight. | C12 inflight (`hold-capture-frozen`) |
| `capture-admission-blocks` | `checkpoint.rs` (`services_for`) | Capture admission waits for the project's other capture (a second checkpoint is held behind the first, not answered by the agent's busy gap). | SVC-CAP-03 c3 through the product |
| `journal-write-fail` | `services_snapshots.rs` (`persist_journal`) | Fault: the journal cannot be written (the old file is left as it is and a durable marker invalidates it). | C12 journal; HF2 journal (stale file + marker) |
| `journal-marker-fail` | `persist_journal` | Fault: the marker cannot be written either: neither outcome nor invalidation is persistable, so a stop is refused and a capture is a gap. | HF2 journal |
| `journal-neither-ignored` | `record_stop`, `remember_capture` | The defect for it: the stop goes ahead and the capture counts although nothing was persisted. | HF2 journal |
| `journal-crash-after-intent` / `journal-intent-off` | `persist_journal` | Fault: the daemon dies after the marker is durable and before the journal is rewritten. Defect: no marker is written before the journal changes. | HF2 journal (restart at the boundary) |
| `journal-loss-silent` | `services_reuse_decision` | A lost or invalidated journal is read as "nothing was ever captured", and an entry that survives it is trusted, instead of the warning reuse. | C12 journal; HF2 journal |
| `journal-vm-unbound` | `services_snapshots.rs` (`project_entry`, `services_reuse_decision`) | A journal entry is used for whatever VM the project has now, not only the VM whose UUID it names. | C12 rm/recreate |
| `orphan-roots-from-records` | `checkpoint.rs` (`collect_orphans`) | The collector scans only projects with a checkpoint record: an orphan-only project is never scanned. | C12 orphan |
| `lane-capture-root` | `checkpoint.rs` (`services_for`) | A lane's checkpoint captures the base workspace instead of the lane's. | SVC-CAP-07 c1 (product: `checkpoint --lane`) |
| `startup-recovery-off` | `marsh-sbx` `services_relay.rs` (`establish`), `marsh-backend` `services.rs` (`services_recover_once`) | After a daemon restart the recovery pass is skipped and a project found at startup never gets its relay back. | SVC-VM-06 c2 |
| `relay-accept-stale-generation` (alias `ready-accept-stale-generation`, the test plan's name) | `host.rs` (`reader_loop`) | A `ready` for another generation is accepted. | SVC-SEC-12 c1 |
| `relay-unbounded` | `host.rs` (`handle_data`, `Stream::write`, `pick`) | Credit is returned on arrival and queues have no bound: a reader that stops reading makes the daemon buffer without limit. | SVC-SEC-19 (bounds) |
| `channel-limit-off` | `host.rs` (`open`) | The per-attempt, per-project and per-daemon channel caps are not enforced. | SVC-SEC-19 c1 |
| `relay-fairness-off` | `host.rs` (`pick`) | The channel at the head of the ring is served to completion, waiting for its credit, before any other channel's data. | SVC-SEC-19 c2 |
| `tcp-map-check-off` | `host.rs` (`open_tcp`) | The daemon's check of a `tcp` open against the workspace's published-port map is skipped (the agent still checks its ledger). | SVC-SEC-18 c1 |
| `vm-frame-validation-off` | `host.rs` (`violation`, `reader_loop`) | A frame the VM must never send (unknown or stale channel, a duplicate `opened`, bad ports) is ignored instead of ending the relay; an oversized frame still ends it (the stream cannot be followed). | SVC-SEC-21 c1 |
| `fault-forge-tcp` (fault) | `host.rs` (`open_tcp`), input `$MARSH_HOME/test-holds/forge-tcp-port` | A `tcp` open is rewritten to another port, as a daemon bug would. | SVC-SEC-18 |
| `fault-agent-override` (fault) | `services_relay.rs` (`peer_override`), input `$MARSH_HOME/test-holds/agent-override` (the project root) | `sbx exec -i -u root VM python3 /run/nfx/peer.py` replaces the agent for that project only. | SVC-SEC-12, SVC-SEC-21 |
| `ports-off` | `marsh-backend` `endpoint.rs` (`watch`) | No port is ever sent to the job's or the shell's forwards: published ports are not reachable on `localhost`. | SVC-EP-01 c4; SVC-EP-02 c2 |
| `ports-keep-stale` | `marsh-worker` `forward.rs` (`manage`), taken from the job's `services.breaks` | A forward is not revoked when its port stops being published: the listener stays in the job's namespace. | SVC-EP-02 c7 |
| `slow-ports` | `marsh-backend` `endpoint.rs` (`watch`) | Each ports update reaches the forwards three seconds late (a measurement control). | SVC-PERF-06 |
| `listener-before-setns` | `marsh-worker` `forward.rs` (`listen`) | The sockets are made before the namespace is entered, so they live in the worker's own namespace (and stay there when `setns` then fails). | SVC-EP-07 c1 (step 4) |
| `listener-retain-on-end` | `marsh-worker` `forward.rs` (`manage`) | The job's listeners are not closed when its attempt ends. | SVC-EP-07 c2 |
| `shell-no-endpoint` | `marsh-backend` `lib.rs` (shell run, `services_root`) | The shell gets no Docker endpoint, no `DOCKER_HOST` and no forwards. | SVC-EP-05 c1 |
| `shell-forward-last-claimant` | `marsh-daemon` `relay/forward.rs` (`listen`), taken from the relay's `Initialize` frame | A later claimant takes a shell VM port over from the earlier one instead of reporting a collision. | SVC-EP-08 c1 |
| `docker-env-open` | `marsh-contracts` `lib.rs` (`reserved_exported_environment_name`), `marsh-runtime` `lib.rs` (`job_environment`), taken from the job's `services.breaks` | `DOCKER_*` is not refused from any exported source, and marsh does not override a value the job already has. | SVC-SEC-08 c2 |
| `fault-setns-fail` (fault) | `forward.rs` (`enter_namespace`), taken from the job's `services.breaks` | The worker's `setns` returns `EPERM`: no listener may exist anywhere, and the failure is reported. | SVC-EP-07 step 4 |
| `hold-netns-attach` (hold) | `forward.rs` (`open_namespace`), taken from the job's `services.breaks` | Waits (`/run/marsh-test-holds/netns-attach.reached` / `.release`) between reading the container's namespace key and opening it, once per publish batch, for at most 120 s or until the attempt ends. The directory is root's: a harness releases with `sbx exec -u root`. | SVC-EP-07 step 3 |
| `reap-off` | `marsh-backend` `services_reap.rs` (`services_reap_track`, `services_reap_attempt_ended`) | The daemon keeps no reap record and asks for no reap: a job's one-off containers outlive its end (and a restart). | SVC-EP-03 c3; `docs/model/ServicesReap.tla` `reapOff` |
| `reap-persistent` | `marsh-services` `reap.rs` (`reap`), agent mode | A reap also removes Compose and restart-policy containers. | SVC-EP-03 c4 |
| `reap-all-scope` | `marsh-services` `reap.rs` (`reap`), agent mode | A reap takes every container of the workspace, whatever attempt made it: a concurrent job's and the shell's containers go too. | SVC-EP-03 c3 (witness: the concurrent job and the shell are untouched) |
| `shell-reap-on-exit` | `marsh-backend` `endpoint.rs` (`ShellEndpointGuard::drop`) | The shell's objects are reaped when the shell exits. | SVC-EP-09 c2 |

The job-endpoint modes (E2) the worker and the guest relay act on are chosen by the daemon from its own
set and sent in the job's `services.breaks` field or the relay's `Initialize` frame, never read from their
environment (DECISIONS 22); a release daemon sends none.

The relay modes above are the daemon's (`marsh_sbx::services_relay::breaks::ALL`); the agent's modes
(`capture-*`, `filter-*`, `output-unbounded`, ...) are handed to it through its `--break-modes` argument.

The services VM modes above (all but `demo`) belong to task S1 and are listed in `marsh_sbx::services_breaks::ALL`. The engine (S2, S2b) and policy (E3) modes are documented in `docs/design/services-engine.md` and `docs/design/services-policy.md`, and restore's (S4) in `docs/design/services-restore.md`.

**C3 (app data in lanes).** The daemon's modes are in `marsh_daemon::lane_services::breaks` (and `KNOWN_TEST_BREAKS`), `lane-scope-root` is the backend's; the agent's (`restore-files-from-agent`, the `fault-restore-*` faults, and S4's `restore-*`, `slow-restore`, `hold-restore-*`) travel in `--break-modes`.

| Mode | Where | What it breaks | Caught by |
|---|---|---|---|
| `restore-files-from-agent` | `marsh-services` `restore.rs` (`finish_stack`), `marsh-daemon` `lane_services.rs` (`write_lane_files`), `AGENT_MODES` | The agent writes `compose.fork.yaml` and `.env` into the lane itself, and marshd skips its write (A9). | SVC-SEC-22 (the lane has no generated file at `hold-lane-files-write`) |
| `restore-line-trust-vm` | `lane_services.rs` (`line`) | The agent's own reason text goes into the restore line instead of the Mac's words. | SVC-RST-07 c5 |
| `lane-scope-root` | `marsh-backend` `services_lanes.rs` (`services_job_scope`) | A job in a lane gets the project's root scope (workspace `base`) instead of its lane's. | SVC-RST-11 c1; SVC-SEC-05 c1 |
| `lane-drop-off` (alias `drop-noop`, the test plan's name) | `lane_services.rs` (`LaneDrops::drop_lane`) | `close`, `cancel` and `rm` drop no lane's services. | SVC-RST-09 c1 |
| `restore-silent-fail` | `lane_services.rs` (`assemble`) | A partial restore reports `ok: true`; its line still says what went wrong. | SVC-RST-08 c1 |
| `line-always-ok` | `lane_services.rs` (`assemble`, `line`) | Every restore reports `ok: true` with no failure clause. | SVC-RST-08 c1, c2 |
| `line-omit-fresh` | `lane_services.rs` (`line`) | Services that started fresh or were not restored are left out of the line. | SVC-RST-07 c2 |
| `line-internal-words` | `lane_services.rs` (`line`) | The line carries internal words (volumes, snapshot, pool). | SVC-RST-07 c5; NW-06 c1 |
| `lane-ports-unmapped` | `lane_services.rs` (`restore_lane`) | The restore's port map is dropped: inside the lane the original port numbers reach nothing. | SVC-RST-03 c1 |
| `lane-files-follow-symlinks` | `lane_services.rs` (`write_lane_files`) | The lane files are written through whatever the names point at (a plain `fs::write`). | SVC-SEC-22 c1 |
| `apply-include-generated` | `marsh-daemon` `split.rs` (`generated`) | `split apply` carries the generated lane files into the project. | SVC-RST-09 c3 |
| `hold-lane-files-write` (hold) | `lane_services.rs` (`restore_lane`), `DaemonBackend::test_hold` | Waits (`$MARSH_HOME/test-holds/hold-lane-files-write.reached` / `.release`) after the result is validated and before the lane files are written. | SVC-SEC-22 |
| `hold-restore-after-lease` (hold) | `marsh-daemon` `split.rs` (`restore_lanes`) | Waits (same files) after the checkpoint is leased and before the restore starts. | SVC-CAP-04 c3 |
| `fault-restore-bad-file-name`, `fault-restore-huge-file`, `fault-restore-nul`, `fault-restore-bad-port`, `fault-restore-bad-name` (faults) | `marsh-services` `restore.rs` (`hostile_reply`), `AGENT_MODES` | The agent answers `restore_prepare` as a hostile VM would: a file named `../../escape/.env`, a 200 KiB `.env`, a NUL in `.env`, port 70000, a service named with an escape and a newline. | SVC-RST-08 (hostile result; the daemon refuses it: `ok: false`, a gap, nothing written, the fork dropped) |
| `fault-restore-markup` (fault) | same | Display text with ANSI, markup and a newline in a reason. | SVC-RST-07 (the line is plain) |
