# Services engine (`marsh-svc-engine`)

Design: `sdlc/SECURITY-DESIGN.md` A1 in the Narwhal Fork spike repository. This page
records what is built and where.

A project's services VM runs the stock ("boot") Docker engine that `sbx` starts. That
engine never holds a user object. It runs exactly one container, `marsh-svc-engine`,
and the user's containers live in a **production dockerd inside that container**, on
an XFS (`reflink=1`) pool that the container mounts before dockerd starts.

## Pieces

| Piece | Where |
|---|---|
| Image recipe | `packaging/svc-engine/Dockerfile`: dockerd and containerd at the shell image's pinned versions, the Compose plugin (`docker-compose-plugin`, pinned; restore runs the compose client here, `docs/design/services-restore.md`), `xfsprogs`, `util-linux`; the entrypoint is `/marsh/marsh --internal-services engine-up` |
| Build | `scripts/prepare-svc-engine-image.py` (`make prepare-svc-engine-image`, part of `make build-linux` and `make dev`): writes `svc-engine.tar` (an unnamed OCI archive) and `svc-engine-image.json` (the pin: image id, config id, archive sha256 and size) into the guest artifacts. It refuses when the dockerd/containerd pins differ from `packaging/shell/Dockerfile`. CI's image job (`.github/workflows/ci.yml`, "Kit and shell images (build, no push)") runs the same build, without pushing, whenever the Dockerfile, this script or the shell image's pins change (`scripts/ci-changes.sh`) |
| Shipping | `make install`, `make dist` and `make dev` copy both files to `libexec/marsh/` beside `shell-image` |
| In-engine setup | `crates/marsh-services/src/engine.rs`, `engine-up` |
| Egress firewall | `crates/marsh-services/src/firewall.rs` (rules, canonical verification, the `ready` report) |
| Internet gate | `crates/marsh-services/src/internet.rs` (the `MARSH-INTERNET` chain; `engine-up --internet on|off`; `marsh --internal-services internet on|off|status`); the policy around it is in `services.md`, "Internet" |
| Daemon lifecycle | `crates/marsh-sbx/src/services_engine.rs` (`ServicesEngine::start`, `EngineHandle`) |
| Live checks | `tests/acceptance/services_s2_engine.py` and `tests/acceptance/services_s2b_firewall.py`, with the `services_engine_drive` example |

## Start of one generation (daemon side)

1. Read the VM's boot id. Copy the tar and the Linux `marsh` binary to
   `/var/lib/docker/marsh-svc/artifacts/` on the VM's Docker disk with `sbx cp`, unless
   the files there already have the pinned sha256 (checked after every copy).
2. `docker load -i` the tar (skipped when the pinned id is already present). The loaded
   id must equal the pin's image id or config id. The image is **never pulled, never
   tagged and never run by tag**: the container is created from the verified image id,
   and its `.Image` is checked against the pin again.
3. `docker rm -f marsh-svc-engine`, then `docker create` it: privileged, `--init`, no
   restart policy, default bridge (no published ports, no host network), the VM's
   project mounted read-write at its Mac path, `marsh` read-only at `/marsh/marsh`, and
   `/var/lib/docker/marsh-svc/pool` at `/var/lib/marsh-svc` (the pool image).
4. `docker start`, then follow `docker logs` through `sbx exec`. The container prints
   JSON events on stdout; `ready{generation}` is the only way to `Ready`.
5. On `ready`, the generation must equal (boot id, the container id from `docker
   create`), the `ready` must carry a verified firewall report (below), the daemon's own read of
   the engine's rules (`iptables -S` / `ip6tables -S` through `docker exec <engine id>`)
   must equal the canonical expected set, and `docker ps -a` must list only `marsh-svc-engine`; otherwise the VM is
   `unavailable` with the reason.

## Inside the engine (`engine-up`)

First, **before anything of the production engine exists**, the egress firewall (next
section), then the Internet gate (`--internet on|off`, closed when the argument is absent;
`docs/design/services.md`, "Internet"), which the supervisor re-reads every second like the
firewall. Then cgroup delegation (below). Then `mknod` the loop nodes; create the pool image on first use (refusing with a `marsh:`
message when the disk cannot hold it plus a reserve; the image is renamed into place
last, after its identity record); mount it as XFS; check the pool's filesystem UUID
against the identity record and probe reflink; write the hardened `daemon.json`
(BuildKit `network-host` and `security-insecure` entitlements off, loopback default
publish address, bounded logs and downloads) and the engine's own configuration
(`/run/marsh-svc/engine-config.json`, the registry policy below); recreate the bind pins
(E3 `pins::restore_all`); start dockerd with `data-root` `/pool/docker` and its
socket at `/run/marsh-svc/docker.sock` (inside the engine only); wait for `/_ping`;
print `ready`. Then it supervises dockerd and the pool mount and exits with `lost`
when either goes away.

## Published ports and cgroup delegation (S2c)

**Loopback-only publishes.** The production `daemon.json` has `"ip": "127.0.0.1"`, so a
published port without a host IP binds to loopback inside the engine. The E3 filter
forces loopback too; this is the second layer, for anything that reaches the engine
without the filter. Break `engine-ip-any` writes `"ip": "0.0.0.0"`.

**cgroup v2 delegation.** The engine container is created with `--cgroupns private`, so
`/sys/fs/cgroup` is its own cgroup. `engine-up` refuses to touch cgroups unless
`/proc/self/cgroup` is `0::/` and `/sys/fs/cgroup` is cgroup2. It then moves every process
in the root cgroup (itself included) into a nested `init` cgroup and enables the
`cpu cpuset io memory pids hugetlb` controllers (those available; `cpu`, `memory` and `pids`
are required) in `cgroup.subtree_control`, retrying while the kernel answers EBUSY. This is
what `docker:dind`'s entrypoint does. Without it every container created with the policy's
forced default memory, CPU and PIDs limits fails to start. Break
`engine-cgroup-no-delegate` skips the step. Failure is `failed{kind: cgroup}`.

## Engine configuration and the registry policy (S2c)

`EngineConfig` (`marsh_services::engine`) is the engine's configuration for the filtered
endpoint, written to `/run/marsh-svc/engine-config.json`. Its one input is the project's
Internet gate (E5, `docs/design/services.md` "Internet"): `engine-up --internet on|off`
(closed when absent) installs the gate and writes the file in one call
(`internet::set`), and the registry allowlist is `registry_allow` of the gate state: Docker Hub
and `ghcr.io` (`DEFAULT_REGISTRIES`) while closed, every registry while open
(`EngineConfig::for_internet`). There is no separate registry argument or setting
(S2c's `--registry-allow` and `MARSH_SERVICES_REGISTRY_ALLOW` are gone). The relay agent
(`relay::production_services`) loads the file to start (unreadable: it refuses to serve) and
its filter re-reads it for every image reference (`Config::registry_config`), so a toggle
reaches a running agent at once; the supervisor counts a file that disagrees with the gate as
a lost gate. **TODO(E1):** the daemon-side relay host calls `Services::restore_sockets()` once the
agent listens; the call site is marked in `engine.rs` (`bring_up`).

## Egress firewall (A12)

sbx attaches Mark's global credentials and the MCP gateway to every sandbox through one
proxy, `gateway.docker.internal:3128`, and any container that sets it acts as Mark. The
production engine needs no proxy, so `engine-up` closes the gateway for the engine and
everything below it:

- `gateway.docker.internal` is resolved (A and AAAA) at every start, since the address is
  per VM. No address, or loopback/unspecified/multicast, fails the start.
- A chain `MARSH-EGRESS` in the `filter` table of `iptables` and `ip6tables` holds, per
  gateway address and in this order, `-d ADDRESS -p tcp ! --dport 53 -j DROP` and
  `-d ADDRESS -p udp ! --dport 53 -j DROP`. Jumps to it sit first in `OUTPUT` (the engine
  and anything sharing its netns, including host-network containers of the production
  engine), `FORWARD` and `DOCKER-USER` (every container network, including user-defined
  bridges; Docker keeps a `DOCKER-USER` chain in both families). `DOCKER-USER` is created
  before dockerd, which keeps it. Port 53 (DNS) stays open over both protocols; every other
  destination is untouched.
- **Verification is equality with a canonical rule set**, not a search. The `-S` listing is
  normalized (the host mask of `-d` and the `-m tcp` / `-m udp` the kernel adds are removed;
  nothing else is tolerated) and the chain must equal the expected rule list exactly,
  including order. So a rule narrowed by a source, an interface, a source port, a state or a
  comment match, a negated or widened destination, a wrong protocol or target, a reordered
  or extra rule, all fail. `OUTPUT` and `DOCKER-USER` must start with exactly the jump;
  `FORWARD` must start with exactly that jump or exactly the jump to `DOCKER-USER`.
- `ready{generation, firewall}` carries `{addresses, verified, connect_blocked}`: the rules
  were read back and compared (above), and a TCP connect from the engine to the gateway's
  proxy port did not connect. The check runs once before dockerd starts and again after it
  answers, since dockerd edits the same tables.
- **The daemon audits independently.** It refuses a `ready` with no report, no address,
  `verified: false` or `connect_blocked: false`. It then reads `iptables -S` and
  `ip6tables -S` itself through the boot engine, from the engine container addressed by the
  generation's recorded container id, and applies the same canonical comparison on its own
  side, so the engine's own check cannot vouch for itself. The addresses to cover are the
  reported ones, and the VM's own resolution of the gateway (`getent ahostsv4/v6`, outside
  the engine) must be a subset: an engine cannot shrink what must be dropped.
- While running, `engine-up` re-reads and compares the rules every second and re-resolves the
  gateway every five. A missing or altered rule, or a gateway address the rules do not cover,
  prints `lost` and ends the engine: the daemon treats it exactly like losing the production
  engine.
- `EngineHandle::firewall()` returns the report of the `ready` that made the generation
  ready, for `marsh services status`.

**Residual (L3).** A container with `NET_RAW` (in Docker's default capabilities and in E3's
allowlist) can forge DNS answers to the engine's periodic re-resolution. That can only make
an address appear that the rules do not cover, which ends the engine (`lost`) or fails an
audit; it can never remove coverage, since the installed set is fixed for the generation.
It is an availability issue only. If it shows up in practice, re-resolve only on the
daemon's audit cadence or only after a failed `:3128` probe.

The image declares `iptables` (and the build asserts `iptables` and `ip6tables`), so adding it
changed the engine image pin: rebuild with `make prepare-svc-engine-image`.

This is the primary control; the second layer (sandbox-scoped dummy secrets, `--static-mcp
none`) and the status report belong to S1. It does not make the VM itself safe: root in the VM
can still use the proxy.

## Readiness

`Starting -> Ready` only on a matching `ready`. The log stream ending (the exec), the
container stopping, the production engine dying, the pool unmounting, or a failed audit
makes the state `Unavailable`, which is terminal for that generation. Observers
(`EngineHandle::subscribe`) are told of every transition; whoever owns channels closes
them when the state leaves `Ready`, and calls `ready_generation()` before opening one.
A restart of the VM or of the container is a new `start`, so a new generation.

## Settings

`services.poolSize` (default 24 GiB; test override `MARSH_SERVICES_POOL_BYTES`, read by
`EngineSettings::from_env`). `EngineSettings::internet_open` is the project's Internet grant
(`InternetGrants::is_on`), passed to `engine-up` as `--internet on|off` at every start. The Docker disk size is S1's `services.diskSize`.

## Break modes and holds (test builds only)

`marsh-services`, `marsh-sbx` and the guest `marsh` crate each have a `test-breaks`
feature that forwards to `marsh-contracts/test-breaks`, the one feature that release
packaging refuses at compile time (`MARSH_RELEASE_PACKAGING=1`) and that leaves the
`marsh-feature:test-breaks` marker the packaging check looks for
(`tests/breaks/services_release_guard.sh` checks the refusal for all three crates).
The daemon fills `EngineSettings` breaks only from `marsh_contracts::break_mode`, and
passes the engine's modes through the dedicated `--break-modes` argument of
`engine-up`, which exists only in a feature build. Nothing reads job or project
environment.

| Mode | Where | Defect (scenario check) |
|---|---|---|
| `ready-before-pool` (deprecated aliases `boot-barrier-off`, `endpoint-before-pool`) | daemon | `Ready` as soon as the container starts (VM-03 c2) |
| `relay-dial-boot-engine` | daemon | `EngineHandle::relay_target()` names the boot engine's socket instead of the production socket inside the engine (VM-03 c1). The wave-2 proof needs the E1/S3 relay that consumes the target |
| `bootstrap-use-user-image` | daemon | runs `marsh-svc-engine:latest` if the boot engine has it, and copies that tag from a still-running production engine into the boot engine first (SEC-11 c1, c3), both unverified |
| `pool-skip-on-start` | `engine-up` | an existing pool is not mounted; the data-root lands on the container filesystem (VM-04 c1) |
| `entitlements-on` | `engine-up` | `daemon.json` grants `network-host` and `security-insecure`; the production builder then honors `--allow security.insecure` (SEC-07 c1) |
| `pool-no-size-check` | `engine-up` | no space check and a sparse image: a pool that cannot fit is accepted (VM-07 c1) |
| `engine-firewall-off` | `engine-up` (the daemon skips only its independent audit) | no rules are installed or checked, yet `ready` reports a verified firewall: a user container, the engine and a host-network container reach the proxy (`api.github.com/user` through it returns 200; MCP `tools/list` answers) (A12, `services_s2b_firewall.py`) |
| `engine-ip-any` | `engine-up` | `daemon.json` has `"ip": "0.0.0.0"`: a publish without a host IP listens on every address inside the engine (`ss -ltn`) (S2c, `services_s2c_engine.py`) |
| `engine-cgroup-no-delegate` | `engine-up` | no cgroup delegation: a container with the default memory, CPU and PIDs limits fails to start (S2c) |
| `engine-watch-unbounded` | daemon | the watcher reads the engine's log lines with no length cap and keeps VM text unfiltered (HF1: several MiB without a newline grow the daemon) |
| `slow-pool` | `engine-up` | 8 s before the pool mount (measurement control for PERF-07, not a hold) |

`ready-accept-stale-generation` is the relay's (E1, `docs/design/services.md`, "Relay"): the
daemon's relay refuses an agent `ready` for any generation but the one the engine reported.

The hold `hold-engine-before-ready` (not a defect) is S2's: with every setup step done
and the production engine answering `/_ping`, `engine-up` writes
`/run/marsh-svc/hold-reached` and waits for `hold-release` (go on) or `hold-abort`
(print `failed` and exit), for at most 300 s.

## Cancelling a start

`EngineHandle::detach` (and `Drop`) cancels at any time, including while `Starting`:
no step after it creates or attaches anything (the log stream is published under the
lock the cancellation takes), readiness is revoked, and it waits at most 5 s for the
worker, which ends at its next cancellation check. A start that has not reached
`docker start` never starts the engine; one that has leaves the container, which the
next generation replaces.

## Loss and revocation

When the log stream ends, the state becomes `Unavailable` and observers are told before
anything else runs. A diagnostic note (`EngineHandle::diagnostic`) follows; it looks at
the VM only if `sbx ls` shows it running, because `sbx exec` into a stopped VM starts
it. Observers run outside the state lock, in order.

## VM output and the engine's identity on the host (HF1)

Everything the engine container writes is VM-controlled. The watcher reads its stdout
and stderr in bounded lines (`crates/marsh-sbx/src/vm_text.rs`): an event line is read
whole up to 64 KiB and a stderr line up to 1 KiB, and the rest of a longer line is read
and thrown away, so a VM that writes MiB without a newline costs the daemon one
buffer. Text taken from the VM into a state reason or the stderr tail drops control and
escape characters and the characters that reorder text, and is capped (2 KiB for a
reason, 1 KiB per line of the tail).

The Internet gate (`services_net.rs`) reaches the engine by the container id the
daemon's own `docker create` returned for the generation, recorded in the adapter
(`StockSbx::services_engine`), never by the name `marsh-svc-engine`, which the VM can
move to another container. A daemon with no recorded generation for a VM (it started
after the engine, and has not started one since) reports no engine to reach: the grant
is applied by the next `engine-up`, as for a stopped engine.
