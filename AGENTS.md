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
- ACP agent sessions and MCP publication/load;
- `marsh --dev`: a lean dev broker so marsh can be developed from inside an
  marsh session (`docs/design/self-development.md`).

Out of scope: Cloud execution, a Brush fork, cross-daemon source ledgers,
build-receipt authority binding, general M:N scheduling, private split
workspaces, workload restart recovery, and multi-device history. Third-party
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
~/.marsh-dev/bin/marsh --dev -c pi-dev  # develop marsh from marsh with Pi
make fixture-ref                      # once: published fixture ref -> target/fixture-ref
make dev-smoke                        # acceptance smoke against the dev install (or DEV_KIT=<ref>)
make dev && make check                # the loop: fast real smoke, warm scope in target/check
make regress                          # every existing suite, sequentially, before release
python3 tests/perf/warm.py --marsh ~/.marsh-dev/bin/marsh \
  --guest-artifacts ~/.marsh-dev/libexec/marsh --kit <fixture-ref>
```

## Gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
make dev-smoke
docs/model/check.sh                   # when cross-component state changes
```

Add focused tests for the crate you changed. Avoid whole-workspace test churn.
`make acceptance` builds a fresh candidate and is the release run.

## Rules

- Preserve ordinary Bash behavior through Brush. Classify and test every
  divergence. Keep Brush changes as small patches in `docs/upstream/brush/`.
- Host `sbx` is trusted. Use only its public CLI; never ship a patched SBX.
- VM names are random (`marsh-k-xxxxxxxx`, `marsh-s-xxxxxxxx`) and recorded in
  the per-daemon ownership map before `sbx create`. Never stop, remove, mount,
  or exec into a VM that is not in that map with its recorded UUID.
- Keep session, job, attempt, worker, VM, and container identities distinct.
- Never replay a job after transport loss; mark it cleanup-uncertain and
  quarantine the VM. `marsh workers reset KIT` retires it.
- Jobs get no Docker, containerd, or SBX socket and no raw credentials; their only
  daemon channel is the scoped job capability socket (`/run/marsh/cap.sock`,
  `docs/design/processes.md`).
- Prefer real-caller end-to-end tests that check output, authority, data
  preservation, receipts, and cleanup. Do not add unit tests that mirror the
  implementation.
- No real credentials in unit tests.
- Keep `docs/model/` in step with changes to ownership, worker lifecycle,
  mounts, cancellation, or grants. Update the invariant, the negative control,
  and the code-location table together. A TLC pass is not evidence about the
  Rust code.
