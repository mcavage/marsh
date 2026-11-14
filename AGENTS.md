# marsh development guide

marsh is a Linux shell for Apple Silicon Macs. Shell behavior comes from
upstream Brush, vendored under `vendor/brush/` with a small patch queue in
`docs/upstream/brush/`. VMs come from the stock Docker Sandboxes CLI (`sbx`).
marsh must never invoke Bash as its implementation.

## Scope

In scope:

- the local Brush shell, opened at the project's natural Mac path in a stock
  SBX shell VM;
- registered native Kit v3 commands, each run as a fresh nonroot container in
  a warm per-Kit VM, from the shell or from inside any job (nested processes,
  one lineage tree and one admission; `docs/design/processes.md`);
- `fanout { ... } | collect` composition and structural `marsh results`;
- daemon-owned filesystem snapshots and private split forks, with patch results
  (`docs/design/workspaces.md`);
- ACP agent sessions and MCP publication/load;
- explicit cloud placement (`docs/design/cloud-placement.md`): session-origin
  `cloud { ... }` / `marsh cloud` ships as a preview (`docs/cloud.md`,
  `docs/plan/cloud-dispatch.md`, live gate `tests/acceptance/cloud_uat.py`);
  registered Kits in cloud blocks, job-origin `marsh cloud` (in the job's own
  Kit), and the interactive cloud shell are part of that preview; cloud ->
  laptop crossings and `marsh run --place` are v2 work.

Out of scope: a Brush fork, cross-daemon source ledgers,
build-receipt authority binding, general M:N scheduling, workload restart
recovery, and multi-device history. Third-party
agent Kits live outside this repository (`docs/plan/custom-agent-kit.md`). The
flight recorder was removed; `marsh results` (structural receipts) is the job record.

## Reading order

1. `README.md`
2. `docs/plan/local-product-contract.md` - user-visible contract
3. `docs/architecture.md` - crates, data flow, trust boundaries
4. `docs/model/README.md` - TLA+ spec of cross-component state
5. `tests/acceptance/CONTRACT.md` - black-box observations

## Dev loop

```bash
make dev                              # build everything; install ~/.marsh-dev
~/.marsh-dev/bin/marsh                # run the dev install from any project
make fixture-ref                      # once: published fixture ref -> target/fixture-ref
make test                             # VM-free gate, the same targets CI runs (needs cargo-nextest)
make dev && make check                # the loop: fast real smoke, warm scope in target/check
make check-reset                      # before removing a worktree (make check-gc recovers if you forget)
make verify                           # before pushing: check + high-signal suites, concurrent (tests/regress.py)
make regress                          # every sbx-backed suite, then make test with the full model (manual)
make regress ONLY=smoke               # one suite, family, or scenario id (W01-...); REGRESS_JOBS=1 serializes
python3 tests/perf/warm.py --marsh ~/.marsh-dev/bin/marsh \
  --guest-artifacts ~/.marsh-dev/libexec/marsh --kit <fixture-ref>
```

## Gates

```bash
make test                             # fmt, clippy, every workspace test, harnesses, quick model
make verify                           # on a Mac, against the dev install
make test-model MODEL_FLAGS=          # every TLA+ config, when cross-component state changes
```

Add focused tests for the crate you changed. Avoid whole-workspace test churn.
`make acceptance` builds a fresh candidate and runs the full acceptance gate
(manual). A release runs every CI lane on the tagged commit, not these.

Prioritize real-caller end-to-end evidence over unit-test volume. Exercise the
built product through its actual CLI/transports on stock SBX (and cloud when
changing cloud behavior), checking output, authority, data preservation,
receipts, cancellation, and verified cleanup. Unit tests support targeted
validation/failure boundaries; they do not substitute for a working user
journey. Do not grow mock suites or a parallel test framework to claim progress.

## Rules

- Documentation edits must preserve current positioning and distinguish shipped
  behavior from historical plans. Do not invent benchmarks or transcripts, or
  make broad rewrites without user approval.
- Preserve ordinary Bash behavior through Brush. Classify and test every
  divergence. Keep Brush changes as small patches in `docs/upstream/brush/`.
- Host `sbx` is trusted. Use only its public CLI; never ship a patched SBX.
- VM names are random (`marsh-k-xxxxxxxx`, `marsh-s-xxxxxxxx`) and recorded in
  the per-daemon ownership map before `sbx create`. Never stop, remove, mount,
  or exec into a VM that is not in that map with its recorded UUID.
- Keep session, job, attempt, worker, VM, and container identities distinct.
- Never replay a job after transport loss; mark it cleanup-uncertain and
  quarantine the VM, with its reason. The daemon retires it once it verified
  no container can still run there; otherwise `marsh workers reset KIT` does.
  Failures before any job ran (timeouts, slow pulls, busy bookkeeping) fail
  the start cleanly instead of quarantining.
- Jobs get no containerd or SBX socket and no raw credentials, and never a Kit
  VM's Docker engine. Their daemon channel is the scoped job capability socket
  (`/run/marsh/cap.sock`, `docs/design/processes.md`). **Jobs of a services-enabled
  project get a filtered endpoint for that project's own services VM, never a Kit
  VM's engine** (`/run/marsh/docker.sock`, `DOCKER_HOST` and
  `TESTCONTAINERS_HOST_OVERRIDE` from a dedicated contract field, never from the
  exported environment; `docs/design/services.md`). Every other mount and
  `DOCKER_*`/`CONTAINERD_*` environment refusal stays. When a job's attempt ends,
  marsh removes that attempt's containers in the services VM, except Compose and
  restart-policy containers, which persist with the workspace (DECISIONS 11;
  `docs/design/services.md`, "Reaping at attempt end").
- Prefer real-caller end-to-end tests that check output, authority, data
  preservation, receipts, and cleanup. Do not add unit tests that mirror the
  implementation.
- No real credentials in unit tests.
- Keep `docs/model/` in step with changes to what marsh deletes, kills, or
  reassigns: VMs, containers, jobs, mounts, grants, and ownership records,
  including when that may happen (cancellation, restart, retirement). Update
  the invariant, the negative control, and the code-location table together.
  Other changes (CLI, help and docs, configuration, Kit settings, detection,
  output formats) need tests, not model changes. A TLC pass is not evidence
  about the Rust code.
