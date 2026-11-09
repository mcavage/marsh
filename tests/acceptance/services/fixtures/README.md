# Services E2E fixtures

Synthetic data only; every image is pinned by digest (except the harness-built
`nfx-tools:1`); nothing here needs or reads a credential. Test plan: `sdlc/TEST-PLAN.md`
§A.5 and the harness deltas (H-4, H-5, H-6, H-11, H-12 and the fixture side of 8, 9, 11,
12, 13 and HARNESS-03).

| Path | What |
|---|---|
| `stage.py FIXTURE DEST [--with FIXTURE]` | copies a fixture into a project folder; writes stack A's git-ignored `.env`/`.gitignore` from `env.template`/`gitignore.template`; runs `stage-hook.sh` (`NFX_FIXTURE` names the fixture) |
| `stack-a/` | Postgres 17, Redis (no persistence, key `k`), mailpit, `app` writer (`LOAD=1` ≈ 10 tokens/s; 1000 items, marker rows, deterministic `seed.bin`); ignored `data/app.sqlite`, `public/uploads/a.bin` and a 200 MiB `big.bin`; `sentinels.json` declares each service's class (restorable / fresh / unsupported) and sentinels |
| `stack-a-mixed/` | stack A + `anon` (anonymous volume only) + `writable` (writable layer) |
| `stack-a-blockers/` | `container_name: nfx-mail`, external volume `nfx-ext` (`setup.sh`), volume named `nfx-fixed` |
| `stack-a-local-image/` | stack A + `localapp` (`pull_policy: never`; `setup.sh` builds `nfx/local-app:1`) |
| `stack-canary/` | `restart: always` service logging `OK`/`EMPTY` to `canary-log/boots` on each start |
| `stack-b/` | Supabase CLI 3.x project and `load.sh` |
| `devserver/` | overlay (`--with devserver`; `node_modules` installed on demand like `tc/`): `npm run dev` on :3000 reading `DATABASE_URL` from `.env`; `/items` returns `{count, row1, markers}`; `held-job.sh` is RST-13's held job |
| `ticker/` | `start.sh` / `stop.sh`: a container printing `tick N` every 0.5 s |
| `hostile-compose/` | SEC-13 variants (`manifest.json`, `normal.yaml`, `variants/*.yaml`) and the outside-the-lane canary files |
| `hostile-names/` | valid service and volume names with hostile text in labels (`names.json`, generated `compose.yaml`) |
| `tc/` | Testcontainers 10 + `pg` (Node 22), `node_modules` installed on demand by `stage.py` (`npm ci --ignore-scripts`, git-ignored): `tc.test.js`, `tc-parallel.test.js`, `tc-hold.js`, and `tc-ryuk.test.js` (Ryuk on, DECISIONS 15) |
| `build-ctx/` | multi-stage Dockerfile with a `RUN sleep 5` layer, 50 MiB random context file, `build.compose.yaml` |
| `many-files.sh [VOLUME [N]]` | fills a named volume with N small files (default 300000) |
| `rawdocker.py` + `cases/` | raw-socket client with per-case expectations (`benign`, `encodings`, `framing`, `canonical`), EOF observation, tri-state observer (`--observer-cmd`), `--events-cmd` create-event witness (proven by a probe, a bounded `--event-settle` window of 3 s after each case with forbidden objects plus a post-case barrier probe; an event delayed beyond the window is not caught), recording upstream and the revision-3.1 SEC-23 `--compare-recording` oracle (exact form, harness-computed `--authority`); `cases/generate.py` regenerates the JSON |
| `ptyrun.py` | PTY helper for `-it` controls (stdlib only) |
| `race/` | symlink-flipper container script and attack loop for SVC-SEC-04 |
| `decoys/` | `fifo_witness.py` (a Mac FIFO that detects any reader) and `ssh_agent_stub.py` (a request-logging synthetic ssh-agent) |
| `vm-peer/` | the controlled VM peer for SEC-12 / SEC-21: `peer.py` (runs in the VM under `fault-agent-override`), `peer_install.py` |
| `nfx-kit/` | the `nfx`, `nfx-acp`, `nfx-acp2` Kits (`stage.py` stages them as local Kits and writes `commands.json`); `versions.json` is HARNESS-03 c1's expectation |
| `nfx-tools/` | the image of SEC-15 (docker CLI + python3 + rawdocker): a reproducible build whose image ID is pinned in `local-images.json` (use its digest-qualified `ref`); `image_cache.py save --rebuild-local` rebuilds and checks the pin |
| `image_cache.py`, `images.lock`, `local-images.json` | save the image set to `target/nfx/images/*.tar` and load it into a services engine through `vx` |
| `egress.env`, `egress_preflight.py` | the controlled egress endpoints (DECISIONS 17) and their Mac-side preflight |
| `egress-injection.json`, `synthetic_injection.py` | the Kit-scoped synthetic header-injection rule of SEC-17: a plain `curl` in a Kit job (the Kit's curl wrapper) sends a placeholder that a Kit-scoped sbx secret substitutes; the same trigger from the services side must stay unsubstituted (`verdict`) |
| `sentinels.py` | reads a fixture's sentinels (confined, no-follow file reads), compares them with the manifest, builds `M(C)` (restorable never reduced; `missing` separate; unreadable data raises) |
| `redaction.json`, `redact.py` | synthetic-secret patterns applied to evidence bytes |
| `doc_inventory.py` | DOC-01: `docs/services-examples.ids` against the `services-example` markers (c1), the markers' machine-readable contract (`fixture`, `run`, `after`, `status`, `stdout`, `stderr`; documented in the file), `evaluate()` (c2's comparison) and `mutate_example()` (the `--mutate-doc` break). `tests/acceptance/test_services_docs.py` is its VM-free check |
| `harness03.py` | HARNESS-03's check logic as `run(ctx)` (verdict `pass`/`fail`/`verified_break`/`invalid_negative_control`); `REGISTRATION` is the hook for `services_uat.py` (H1a owns it); a standalone runner with bounded I/O and verified cleanup proves it |
| `uat-app-a/b/c/` | UAT fixtures: seed/reset scripts, sentinel manifests with service classes; exercised to a running state (see the H1b result file) |

The fake agent's `exec` verb lives in `tests/acceptance/acp-fixture/agent.mjs` (the
`nfx` Kits stage a copy): prompt `exec <sh>` runs `sh -c` in the agent's cwd (300 s
limit) and replies `fixture:exit:<n>` (the shell's status; 128+signal when killed, 124 on the time
limit), then `fixture:out:<base64 stdout>` with ALL of stdout, split over several `fixture:out:`
updates of at most 192 KiB each (concatenate the decoded chunks); output over 32 MiB kills the command
and replies `fixture:exit:125`, `fixture:error:output-limit`, stopReason `refusal` (never a truncated
success). `tests/acceptance/test_acp_fixture_exec.py` covers the verb. `NFX_FAKE_EXEC=off`
in the agent's environment, or a `.nfx-fake-exec-off` file in its cwd, turns the verb
off (HARNESS-03's break).

## Scope limit of the egress control

The SEC-17 injection control (`synthetic_injection.py`) uses a **placeholder**: the Kit's `curl`
wrapper sends `X-Nfx-Synthetic: nfx-ph-kit` and a Kit-scoped `sbx secret set-custom` substitutes
`nfx-<run>`; the services side sends the same trigger and must get neither the value (anywhere in
the echo) nor a substitution. That proves the **local Kit-scoped secret-substitution boundary**
only. It is not evidence about unrelated credential or header-injection mechanisms (for example
sbx's native header injection, which exists only for cloud sandboxes), and the "other scope" of
the supporting evidence was a plain sandbox. E5 reruns the control against the real services VM.

## The VM lock

`vmlock.py` is only a shim over the shared helper `harness/vmlock.py` (located through the
environment variable `NFX_VMLOCK`, default `~/dev/narwhal-fork-spike/harness/vmlock.py`; these
fixtures are test-only and ship no lock implementation). Its `run(cmd, ...)` is the only way these tools hold the lock around work done by a child (the lock is kept until the child is reaped; INT/TERM/HUP are forwarded to it); `held()` is for in-process work without children. `harness03.py` re-runs itself under `vmlock.run` (the working child unwinds through its cleanup on a signal and keeps its evidence), and `selfcheck.py --live` runs HARNESS-03 under `vmlock.run`. The shim adds `LockGuard`, a read-only
snapshot of the lock file's inode and contents that HARNESS-03 checks **before** it decides
whether to delete its scratch evidence. `harness03.py --lock FILE` is test-only (a synthetic lock).

## Negative self-checks

`selfcheck.py` feeds known-bad inputs to the checkers (truncated or unreadable checkpoint data,
a `null` SEC-23 body, a dead, silent or late event stream, malformed ownership maps, a leaked synthetic value, an agent JSON-RPC
error, a missing ownership record, a failed `marsh stop`) and shows each is caught;
`tests/acceptance/test_services_fixtures.py` runs it in `make test-tools`. `selfcheck.py --live MARSH`
adds HARNESS-03 on a real install with injected failures (VM lock taken by `harness03.py`).

## Notes

- Images chosen by tools rather than by us (Supabase CLI's service images) are not in
  `images.lock`; pass them with `image_cache.py save --extra FILE`.
- `docker save` of a digest reference writes an unnamed image that
  `image: postgres:17@sha256:…` cannot find after `docker load`; `image_cache.py`
  adds the name and tag to the tar, keeping the index digest.
- `image_cache.py load` refuses a `DOCKER_HOST` that names a job's endpoint:
  images go in through `vx`, recorded with `--record`.
