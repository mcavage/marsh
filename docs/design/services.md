# Services VM

Status: the VM, its lease, idle stop and readiness are built (task S1). The
engine container inside it, the Docker endpoint for jobs, snapshots and
checkpoints are later tasks. Nothing runs in the VM yet except marsh's own
keep-alive, so a services VM is useless until those land; the setting is off
by default.

A project that wants Docker services (a database, a queue) gets its own
**services VM**: one stock SBX VM per canonical project root and daemon scope.
It is created from the marsh shell image, which already carries dockerd,
Compose and the marsh binary. Kit VMs and the shell VM are unchanged.

## On by default, and the settings

**Docker is on for every project by default** (DECISIONS 31), per project and
overridable. Nothing starts because of that: a project's VM is still created only
when an agent first uses the endpoint (or `marsh docker start`), idle-stops as
before, and a checkpoint of a project whose VM is stopped or was never created
captures nothing and starts nothing (`ServicesCapture::Stopped` / `Off`:
`services_capture` reads the VM's state and never creates or wakes it). What
"enabled" does is offer the endpoint to jobs and shells of the project (the socket,
`DOCKER_HOST`, the `context.md` section); a job that never runs `docker` never
reaches the relay, so no VM is created.

*Naming (DECISIONS 28): the feature is called **Docker** in everything a user reads. The CLI is
`marsh docker …` and `marsh config docker on|off|ls|default`; `marsh services …` and `marsh config
services …` are kept as aliases. JSON field names, `config.json` keys (`services_*`), Rust
names and this document's file name keep "services": Narwhal reads them.*

Whether Docker is on for a project (`Settings::enabled_for`): the project's own
setting wins, **off beats on**, else the home's default. `marsh config docker on|off`
acts on the **current project** (the canonical path of the folder it runs in):
`on` adds it to `services_projects` and removes it from `docker_projects_off`; `off`
does the opposite. `marsh config docker default on|off` sets `docker_default_on`
(default `true`); with it `false` Docker is opt-in and only `services_projects` are
on. `marsh config docker` shows the effective value for this project; `ls` prints
`default: on|off` and `PATH: on|off` for every project with a setting of its own.
The daemon reads the file on every request and the setting is host only: a job
cannot change it or the default. For a project that is off, `marsh docker start` and
`hold` refuse with `marsh: Docker is off for PROJECT; ...`, even when another
project of the home is on. Turning a project off **revokes its lease holders and stops
its running Docker VM within a second, keeping its data** (DECISIONS 13).
`status`, `stop`, `rm` and `release` never need it on: turning a project off must not
strand its VM. `marsh docker status --json` has `enabled_projects` (the effective
value: explicit-on projects plus every project it lists, i.e. this one, tracked ones and
those with a VM, for which Docker is on), `default_enabled`, `disabled_projects`, and
each project's `enabled`. An organization or global network policy still governs the
project's containers whatever this setting says.

| Setting (`config.json`) | `marsh config` | Default | Meaning |
|---|---|---|---|
| `docker_default_on` | `docker default [on\|off]` | `true` | Docker is on for a project with no setting of its own. |
| `services_projects` | `docker on` | none | The canonical roots Docker was turned on for (beats the default; matters when it is off). |
| `docker_projects_off` | `docker off` | none | The canonical roots Docker was turned off for (wins over everything else). |
| `services_disk_size` | `services-disk-size [SIZE]` | `40g` | Docker disk of a new VM (`1g` to `4096g`). Existing VMs keep theirs. |
| `services_idle_seconds` | `services-idle-seconds [N]` | 900 | Seconds with no lease holder before the VM is stopped. `MARSH_SERVICES_IDLE_SECONDS` at daemon start overrides it. |
| `services_capture_budget_ms` | (edit `config.json`) | 2000 | The most a checkpoint's services capture may add (100 to 60000), see "Capture budget". |
| `services_ignored_max_files` | (edit `config.json`) | 20000 | File-count bound of the folder local data a checkpoint saves, see "Folder local data in checkpoints". |
| `services_ignored_max_bytes` | (edit `config.json`) | 2 GiB | Size bound of the same (a byte count or a size such as `2g`). |

## Identity

`VmPurpose::Services` (ownership label `services`, name prefix `v-`). The name
is `<vm_prefix>v-xxxxxxxx`, random, and recorded in the ownership map with the
project root as its key **before** `sbx create`, exactly as for Kit and shell
VMs. After the create the observed UUID is recorded. Every stop and removal is
by that recorded UUID; a VM that is not in the map, or whose name carries
another UUID, is never touched.

If the recorded VM was removed behind marsh's back and another VM now holds its
name, marsh drops its stale record and gives the project a new random name;
the impostor is neither used nor removed.

A services VM holds a project's data, so it is removed only on purpose
(DECISIONS 18):

| Command | Services VMs and their data |
|---|---|
| idle timer | stopped, kept |
| `marsh services stop` | that project's VM stopped, kept |
| `marsh stop` | every running one stopped, all kept (the daemon exits; no VM would idle itself) |
| `marsh reset` | skipped entirely: still running, still leased |
| `marsh reset --include-services` | lists the projects whose services VM, Docker disk and data would be deleted and asks first (`--yes` skips the question, a terminal is otherwise required). The request carries exactly the confirmed project keys; the daemon removes those VMs by recorded UUID and keeps any other |
| `marsh services rm` | removes one project's VM and disk (asks first, or `--yes`) |
| `marsh gc`, the hourly pass | never considers a services VM |

## What the VM has

Created with `sbx create --name N --pull P --skills off --template <shell image> shell`
(no workspace argument) under the digest check the shell VM uses, then:

| In the VM | |
|---|---|
| Host mounts | The project, read-write, at its Mac path, added with `sbx mount` through the same grant and mount machinery as shell VMs (identity checks, overlap rules). Nothing else. |
| Guest home | None. No `$MARSH_HOME/home`. |
| Other projects | None. |
| Skills | None marsh adds (`--skills off`). |
| MCP servers | None marsh loads, and the host's static MCP set is not attached (`--static-mcp none`). **Stock still attaches its MCP gateway** (see below). |
| Credentials | None marsh adds. **Stock injects the host's global service secrets** unless marsh shadows them (see below). |
| Docker or SBX control | None for jobs or shells except the filtered endpoint of that project's own services (E2, below). The VM is not reachable from them; `marsh services` is refused to relay and job-capability tokens. |
| Dev broker | None. |
| marsh | The marsh guest binary at `/usr/local/bin/marsh`, and the ownership marker `/var/lib/marsh/worker.json`. |

### Credentials and the MCP gateway (SECURITY-DESIGN A12)

On `sbx v0.51.0` stock attaches every global service secret and its MCP
gateway to **every** sandbox, services VMs included: `sbx inspect` lists the
secrets (names only here), the VM's environment carries the credential proxy,
its CA and a placeholder `GH_TOKEN`, and the gateway is reachable through the
proxy at `gateway.docker.internal:3128`. Any process in the VM that sets that
proxy itself acts as the host user on the secrets' hosts (GitHub, Anthropic and
OpenAI today) and can drive the gateway's tools, including the `narwhal_*`
tools that act on the Mac. sbx network policy does not change this (the
gateway ignores it), and sbx has no create option to turn the injection off.
Measurements: `reviews/credential-exposure.md` in the Narwhal Fork spike.

What marsh does, in layers (none of them is "no secrets are attached", which
sbx cannot deliver today):

1. **The engine firewall is the primary control** (task S2b, owned by
   `engine-up`): a firewall in `marsh-svc-engine` that drops every TCP
   connection to the sbx gateway except DNS, installed before the production
   engine starts and checked for readiness. It also closes the MCP gateway,
   which is reachable only through the proxy. Until it lands this layer is
   absent; S1 does not provide it.
2. **Sandbox-scoped dummy secrets** (S1): on every ensure (cold, restart and
   the warm fast path alike) and on every `marsh services status` of a ready
   VM, before `ready` is reported, for each global
   service secret named by `sbx secret ls --global --json` (names only; no
   value is ever read or printed), marsh runs
   `sbx secret set NAME --sandbox VM --token <constant dummy> --force`. The
   sandbox-scoped dummy shadows the global one in the proxy: with it, a request
   through the proxy gets `401` instead of acting as the user (checked live:
   `curl -k -x gateway.docker.internal:3128 https://api.github.com/user` from
   inside the VM; `-k` because the proxy re-signs TLS with its own CA). On the
   measured host stock accepted a sandbox-scoped dummy for all six global
   secrets (`anthropic`, `bamboohr`, `github`, `gog`, `google`, `openai`). A
   secret stock refuses stays unshadowed and is reported as `unshadowable`. The
   dummies are removed with the VM (`marsh services rm`, `reset
   --include-services`, and the wrong-disk-size removal at create) and only
   those: the global secrets are never written. It is a denylist: a secret
   added on the host while the VM runs is shadowed by the next ensure or
   status (a running VM is reconciled, not only a restarted one), and a
   status that cannot reconcile is `unavailable`. The listings are read by
   scope: only `--global` rows with `scope == "global"` count as global and only
   `--sandbox VM` rows with `scope == VM` count as that VM's dummies; a
   service row with no `scope` fails the listing closed.
3. **`--static-mcp none`** at create: the host's registered MCP servers are not
   attached. It does not remove the gateway's built-in tools or the `narwhal_*`
   tools; layer 1 does.
4. The production engine has no proxy configured (S2), so a compose app does not
   inherit one by accident; a hostile container can still set one, which layer 1
   stops.

`marsh services status --json` reports it, per project with a VM (names only):

```json
"credential_exposure": {"shadowed": ["anthropic", "bamboohr", "github", "gog", "google", "openai"],
                        "unshadowable": [],
                        "mcp_gateway": true}
```

(`"error"` instead when stock cannot be asked.) Readiness does **not** require
the lists to be empty. When the engine firewall lands, readiness requires its
check and this shadowing report. Asks to sbx (release blockers): a create option
for no credential injection and no MCP gateway; the gateway obeying sandbox
policy and `--static-mcp none` meaning zero tools; injection opt-in per sandbox or
tied to the requesting process; operations by sandbox id.

`sbx inspect` of a services VM lists exactly one `host_mounts` entry: the
project. (Stock SBX also shares an empty `/mnt/host` staging root that holds
only mount targets.)

Because the project is mounted at its Mac path, a shell opened in a *different*
path that overlaps it (a subfolder of a project whose services VM is running,
or its parent) is refused by the usual overlap rule until the services VM is
stopped. The same project shares the mount.

## Docker disk size

Stock SBX sizes a sandbox's `/var/lib/docker` from the setting
`sandbox.disk.dockerVolume`, and for one sandbox from the environment variable
`DOCKER_SANDBOXES_DOCKER_SIZE` of the `sbx create` that makes it. marsh sets
that variable on the services create invocation **only** (`services_disk_size`,
default `40g`). The host-global setting is never read or changed, so other
homes, other tools and concurrent creates are unaffected and there is nothing
to restore after a crash.

After a cold create marsh reads the Docker filesystem's size in the guest (it
is reported as `docker_disk_bytes` by `marsh services start --json`) and
requires at least 90% of the size asked for (ext4 keeps about 5%); a VM that
comes out smaller is removed and the start fails, so a stock that stops
honoring the variable is noticed instead of ignored. An existing VM keeps the
size it was created with: `services_disk_size` applies to VMs created after the
change, as stock's own setting does.

## Lease, holders and idle stop

While a VM is ready, marsh keeps a *lease* on it: the same retained
`sbx exec -i -u root VM marsh --internal-supervisor` transport a warm shell VM
has. Holders (later: endpoint channels and captures) keep the VM awake:

* `marsh services hold SECONDS` acquires a holder that expires on its own;
  `marsh services release HOLDER` releases it. They exist to exercise the
  lease until real holders arrive.
* When the last holder is gone, an idle timer of `services_idle_seconds`
  starts (`marsh services start` also arms it, so a VM nobody holds does not
  run for ever). When it expires the lease is dropped and `sbx stop` runs.
* **Idle never removes a VM**; its disk and data stay, and the next start
  boots the same VM.
* A daemon that finds a services VM running at start has no holders (they died
  with the old daemon) and arms an idle timer for it.
* Turning services off revokes every holder and stops the VM at once.

## Readiness

Per VM the daemon tracks a generation: the guest's boot id plus a counter that
advances whenever the VM is made ready again. The state, per project, is one of
`off | stopped | starting | ready | unavailable(reason)`:

* `off`: no services VM exists. `stopped`: it exists and is not running.
* `starting`: a start is in progress.
* `ready`: the VM runs, its lease is live, and the project mount was verified
  inside the VM (`findmnt`, read-write) in the current generation, **and
  `sbx inspect` lists exactly one host mount: the project, at its Mac path,
  read-write**. Stock keeps a stopped VM's saved mounts, so a mount added behind
  marsh's back would survive every restart; any extra (or different) mount makes
  the state `unavailable` with the mounts stock lists, and `start`/`hold` fail
  with the same reason. The check runs on every ensure (including a warm one) and
  on every `status`.
* `unavailable`: a start failed, or the VM runs without a live lease.

The state is derived from stock SBX and the adapter on each call, never
remembered. Nothing else may treat the VM as usable unless the state is `ready`
for the current generation. S2 extends readiness to the engine container.

## Internet

A new project's services have **no Internet** (SECURITY-DESIGN A5): a service
container cannot start a connection out, a name lookup from it fails, and image
pulls from the default registries still work. The user grants Internet to a
project with `marsh services internet on` (and takes it back with `off`); a job
or a shell cannot (the request is host-only, and a job's capability socket
does not admit it).

**Why this is not sbx rules.** sbx network policy can only narrow: "a deny rule
from any source takes precedence over an allow rule" (sbx documentation, and
`sbx policy check` agrees: a sandbox-scoped deny of `**` beats a sandbox-scoped
allow of `registry-1.docker.io`, an HTTP allow with `--method ANY` included). A
per-sandbox "deny everything except image pulls" therefore cannot be written, on
an allow-all host, as rules. A deny that names the hosts a pull needs would also
stop the pull. So **marsh adds no sbx rule at all**; there is nothing to
remove.

**Measured on sbx 0.51 (this host).** A container in the production engine
with a sandbox-scoped deny of `**`: HTTPS to `example.com` and to an IP literal
fail, but **name resolution still succeeds** (sbx does not filter DNS), so a
resolution failure cannot come from sbx rules either. A Docker Hub pull of a
small image touched `registry-1.docker.io`, `auth.docker.io` and
`production.cloudfront.docker.com` (`sbx policy log`); the CDN host is the kind
that changes, which is one more reason not to hard-code pull hosts into rules.

**The gate.** The engine carries the grant out (`marsh --internal-services
internet`, `crates/marsh-services/src/internet.rs`): a marsh-owned chain
`MARSH-INTERNET` (both address families), jumped to from `DOCKER-USER` behind
the sbx-gateway chain of A12. Closed, it returns traffic toward container
networks (container to container, published ports) and drops every NEW
connection forwarded anywhere else. Open, it is empty. Pulls are made by the
production dockerd itself (`OUTPUT`), so they are not affected, and every packet
the gate passes is still subject to sbx policy. `engine-up --internet on|off`
installs it before any container exists (absent means off); the daemon passes
the grant at every engine start (`EngineSettings::internet_open`, read from
`InternetGrants`), so a new generation never inherits an open gate. The
supervisor compares the rules with the intended state every second and a gate
that differs twice in a row is a lost engine, like the firewall.

**Governance wins.** A policy that marsh did not create governs the project's
services VM when `sbx policy ls --json VM` lists an active network rule that is:
from an external layer (organization, remote, kit); scoped to the VM; a global
deny that is not an exact host (a glob, a CIDR, `**`); or when the global policy
has no allow-all rule (a restrictive preset). Then `internet on` and `internet
off` both refuse with a message naming the rule and change nothing, and `status`
reports `internet: {on, governed: true, effective: <the rules>}`. The built-in
default rules are the baseline, not governance. A global deny of one exact host
(for example a telemetry host you block everywhere) only narrows what Internet
reaches: it is listed in `effective` and does not govern, so one blocked host
does not take the toggle away from every project. Governance that cannot be read
(`sbx policy ls` fails or answers something unexpected) counts as governed:
fail closed, and nothing is ever opened to look. The check runs before every
toggle and on every `status`.

**Order of a change.** `on` opens the running engine's gate first, verified by
reading the rules back inside the engine, and records the grant after; `off`
records first and closes after. A failure never claims `on`, and a restart
after a half-done `off` comes up closed. With no engine yet the grant is
recorded and `engine-up` applies it. The grant is a daemon-owned file
(`services-internet.json` in the scope, private to the daemon); `marsh services
rm` removes the project's entry.

**Status.** `internet.on` is the grant; `governed` and `effective` are as above
and `effective` also says what the running engine's gate reads back (`on`,
`off`, or that it disagrees with the grant).

**Registries.** The pull allowlist is the filter's (`Config::registry_allow`,
E3), not the network's, and it follows the gate: `EngineConfig::for_internet`
(`docker.io` and `ghcr.io` while closed, every registry while open) is written
with the gate by `internet::set`, read by the relay's filter for every image
reference, and checked by the gate's supervisor
(`docs/design/services-engine.md`).

## Relay (daemon to the services agent)

One long-lived relay per ready services VM carries every channel between the
daemon and the project's services agent
(`crates/marsh-services/src/host.rs`, `crates/marsh-sbx/src/services_relay.rs`).
It is `sbx exec -i -u root VM docker exec -i ENGINE_ID /marsh/marsh
--internal-services relay`, started only after the engine reported
`ready{generation}` and bound to that generation. The engine is addressed by the
container id recorded in the generation, never by name. Losing the exec, the
engine or its readiness closes every channel and revokes the relay. A new relay
for the same still-ready generation replaces one that ended for a transport
reason; a relay that ended because of what the VM sent (a violation) puts the
project in **quarantine**, and only an explicit `marsh services start` brings
it back, with a whole new engine generation.

The daemon never parses HTTP. It relays opaque bytes and decides only whether a
frame is legal. Every `open` carries `{project, workspace, attempt, kind}` built
from the daemon's own state (`AttemptScope`): the job's attempt, its project and
workspace (the base workspace, `base`, until lanes), and the kind. The agent
validates `kind` as lowercase letters, digits and `-`, so the wire spelling of
`tcp:<port>` is `tcp-<port>`; the channel kind and port travel in their own
fields. A scope for another project than the relay's is refused on the Mac.

The typed API (for the job endpoint, E2, and for capture and restore, C1) is
`StockDaemonBackend::services_relay(root) -> Relay` (only for a project with
services enabled, A11), then `Relay::open_docker(&AttemptScope)`,
`open_tcp(&AttemptScope, port)`, `capture(id, workspace)`, `drop_snapshot(id)`,
`list()` and `ports(workspace)` (a subscription).

**Daemon restart.** Relays are re-established on first use, after a recovery
pass: the services VMs the ownership map holds when the daemon starts are marked
unrecovered (their relay and engine generation died with the old daemon), the
first services timer pass starts a fresh engine generation and relay for each
running, enabled one, and an unrecovered project refuses channels. Nothing from
the old generation is reused. `startup-recovery-off` skips the pass.

**Bounds (A7).** Channels per attempt 64, per project 256, per daemon 1024
(`Limits` and `DaemonBudget`; the first two are also sent to the agent in
`init`); credits of 16 KiB chunks with a 64-chunk window per channel and
direction; control frames before data and round-robin between channels on the
one write path; progress deadlines: a blocked write or a full queue 60 s, a frame
that starts and does not finish 4 s, an `open` 30 s, an idle channel 30 min, a
request 5 min. Buffers are bounded by the credit window, so a reader that stops
reading stalls its own channel only. Channels are multiplexed over the one
transport, so the Mac uses no file descriptor per channel.

**Hostile input (A8).** Frames are strictly typed (`deny_unknown_fields`) and
size-capped before the body is read. The relay refuses and tears down (only that
relay, with a recorded reason and the project quarantined): an unknown or
oversized frame, a frame that stops half way, a `ready` for another generation or
a second `ready`, an unknown, stale or duplicate channel id, `opened` or `refused`
twice, data beyond the window or out of order, credit beyond the window, a
response to a request the daemon never made (or of the wrong kind), a port list
with port 0, a repeated port, a port in two workspaces, an invalid workspace or
too many entries, and an invalid snapshot id in a listing. Frames that arrive for
a channel the daemon has just closed are ignored (a bounded memory of closed
ids). Display text from the VM (gaps, errors) loses its control characters. The
published-port map is used only to authorize `tcp` opens: the daemon refuses a
port that is not in the workspace's map before any frame is sent (the agent
checks its own ledger as well).

`marsh services status --json` carries `relay: {state, reason, generation,
channels, agent_pid, recovery}` per project (`none`, `unrecovered`, `ready`,
`closed`, `quarantined`; `recovery` says how a quarantine is cleared). A VM that
is ready as far as S1 can tell but whose relay is not is **`unavailable`** at the
top level, with the relay's reason: losing the exec, the engine or the mount
revokes readiness (A1). `test_breaks_agent` is on the project entry only when the
agent reported break modes (test builds; a release daemon refuses an agent that
reports any, as a violation). A `ready` that is refused during the handshake
(another generation, an unsupported protocol, unknown fields) quarantines the
project too, so a hostile VM cannot make the daemon reconnect in a loop.

The relay exec's stderr is the VM's: it is read in bounded lines (`vm_text`, 1 KiB
a line, the rest discarded as it is read) and made plain text before it is kept,
and every reason the relay records or logs is made plain text too.

Restore (C3 calls these): `Relay::restore_prepare(id, workspace, project,
source, lane_dir, fork)` and `restore_up(workspace, project, source, lane_dir, fork)`
(`source` is the folder the checkpointed stacks' paths are relative to when that is
not `project`: a checkpoint of a lane). `Relay::drop_workspace(workspace)` drops a
workspace's containers, volumes, networks and ledger scope (the workspace is the only
argument). The
arguments are checked on the Mac as the agent checks them (the lane folder
strictly inside the project). The answers (`restore_prepared`, `restored`) must
match a pending request of that kind (anything else is a violation), must be JSON
objects of bounded depth, and come back as `RestoreReply { result }` unchanged:
the lane files' contents keep their newlines, so the caller validates every field
and writes the files with `O_NOFOLLOW` beneath a held directory (A9).

The Internet grant: the engine starts with `EngineSettings::for_project(grants,
root)`, so a recorded `internet on` opens the gate on start, restart and
recovery (DECISIONS 24).

## Job endpoint and port forwards (E2)

**Who gets it.** Only the jobs and shells of a project Docker is on for (A11; on unless the user turned it off, DECISIONS 31),
decided by the daemon from its own state (`StockDaemonBackend::services_job_root`), never by the
job: a job cannot grant itself the endpoint, and a project with services off gets no socket and no
variables. A job in an open lane gets the endpoint of that lane's scope (App data in lanes); a split branch's confined job that is not a lane gets none.

**The job's endpoint.** The worker serves `docker.sock` in the job's capability directory next to
`cap.sock` (mode 0600, the job's uid and gid; the container sees `/run/marsh/docker.sock`, which is
also the path Testcontainers' Ryuk mounts, DECISIONS 20). Each connection is a channel over the
worker transport (`ChanOpen`, then `CapData` with credit both ways: one chunk of 16 KiB per credit, a
window of 64, returned in batches of 8, plus `ChanEof` for half close). The daemon pairs it with
`services_relay(root).open_docker(scope)`, where the scope is `{project root, workspace base, the
job's attempt}` from the job record. A refusal reaches the Docker client as an HTTP error whose
`message` starts with `marsh:`. The daemon never parses HTTP. At most 64 channels per attempt, on the
endpoint and the forwarded ports together (A7).

**The environment** comes from a dedicated contract field, `JobCapability.services`, not from the
exported environment: `DOCKER_HOST=unix:///run/marsh/docker.sock` and
`TESTCONTAINERS_HOST_OVERRIDE=localhost`, added by the runtime after the image's own `ENV`.
`DOCKER_*` and `TESTCONTAINERS_HOST_OVERRIDE` in any exported source (the shell's environment,
`sbxenv.d`, a Kit, a descendant's `marsh run`) are still refused or dropped (SEC-08); no
`TESTCONTAINERS_RYUK_DISABLED` is set (DECISIONS 15).

**Port forwards in the job** (A6). The daemon follows the relay's published ports for the base
workspace (`Relay::ports`) and sends the worker the whole set whenever it changes (`Ports`). For
each port the worker, in a dedicated thread, `setns`es into **only** the job container's network
namespace (the descriptor opened from the Kit engine's inspect of that exact container,
`NetworkSettings.SandboxKey`, before any listener exists; `rustix`, no `unsafe`) and only then binds
`127.0.0.1:P` and `[::1]:P` (the second only where the namespace has IPv6; `SO_REUSEADDR`, never
`SO_REUSEPORT`). Any failure, `setns` included, is fatal for that port: no listener is made anywhere
and nothing falls back to the worker's or the Kit VM's namespace. A listener the job already has
wins: the collision is reported on the job's stderr as a `marsh:` line and nothing is replaced. Each
accepted connection becomes `open_tcp(scope, P)`, which the daemon first checks against the scope's
authorized port map (E1). A forward ends when its port stops being published (within about a
second), and everything closes with the attempt.

**The marsh shell** (DECISIONS 10, 19). The shell VM's guest relay serves a second owner-only socket
(`/run/marsh/<uid>/<session>/d`), and the shell gets `DOCKER_HOST=unix://` that path, with the
workspace base and attempt `shell`. The shell's private engine is untouched at its default socket for
anyone who unsets `DOCKER_HOST`. The ports the project publishes are forwarded by the guest relay in
the shell VM itself; the shell VM is shared by every project of the scope, so the first claimant of a
port wins, a later claim is reported (`marsh: port P is already in use in the shell VM ...`) and
replaces nothing, and forwards do not check who connects (a documented residual, as for files, D19).
A freed port is claimed by a waiting shell within a couple of seconds. At most 32 services
connections per shell (a descriptor pair each on the Mac).

**Lease.** A job or shell that uses the endpoint, or that follows a project's relay, holds the
project's services VM awake (a `job-N` holder) until it ends.

**Status.** `containers` and `ports` of `marsh services status --json` are the base workspace's
containers and published ports as the services agent lists them (through the same filter as a job,
attempt `status`): `containers: [{name, service, state, health}]`, `ports: [{service,
container_port, host_port, protocol}]`, sorted by service then name, display only. `forwards` lists
what the job and shell forwards did (`owner`, `kind`, `port`, `state`, `message`).

**Break modes** (`docs/design/test-breaks.md`): `ports-off`, `ports-keep-stale`, `slow-ports`,
`listener-before-setns`, `listener-retain-on-end`, `shell-no-endpoint`,
`shell-forward-last-claimant`, `docker-env-open`; fault `fault-setns-fail`; hold `hold-netns-attach`.
The worker and the guest relay take theirs only from the daemon, in the job's `services.breaks`
field or the relay's `Initialize` frame, never from their environment.

## Snapshots in checkpoints

With services on for the project, `marsh checkpoint` also snapshots the base
workspace's services objects (`checkpoint --lane S/L`: that lane's workspace
only, SECURITY-DESIGN A4). The checkpoint record gains `services`, printed by
`marsh checkpoint --json` and listed by `marsh checkpoints --json`:

```json
{"services":{"snapshot":"3f9c0a41b7d2","volumes":[{"id":"v…","name":"app_pgdata","captured":true,"files":1873}],
  "stacks":[{"kind":"compose","project":"app","containers":[{"name":"app-db-1","service":"db","image":"postgres:16","running":true,"volumes":["v…"]}]}],
  "standalone":["keeper"],"gaps":[],"frozen_ms":58}}
```

`services` is `null` when services are off, when the project's VM was never
created (a checkpoint never starts or creates it), or when it is stopped and
nothing was ever captured. The record is built on the Mac from the snapshot's
manifest with only known fields kept, opaque ids validated and every text
made plain and bounded (A8); the snapshot itself stays in the VM's pool.

- **VM ready:** captured under the checkpoint's id, after the pool's reserve is
  checked **by the agent at the capture boundary** (below, a pool that cannot be
  read counts as full). Checkpoint captures share a per-project gate: two overlap at the
  agent, which answers the second with its own busy gap ("skipped: another
  capture in progress"), and a stop cutover excludes them (a checkpoint then
  gets the gap "the services VM is stopping"). A capture that is skipped, fails,
  is over budget, or whose manifest does not validate leaves a `gaps` entry
  (`snapshot` is `null` when nothing was published); the file checkpoint
  succeeds either way. A manifest is validated whole on the Mac: a missing one,
  another id or workspace, a list that is not a list, or a volume or container
  without its identifying fields reject the capture (and the snapshot that
  published is dropped); every cap that cuts a list says so in a gap kept ahead
  of the others.
- **The stop cutover** (DECISIONS 12, fenced). Before any stop marsh causes (the
  idle timer, `marsh services stop`, `marsh stop`, turning services off), under
  the project's operation lock and the **capture exclusion, held until the VM
  has actually stopped** (not only through the capture): (1) the **write fence**:
  every tracked workspace is frozen through the agent's filtered endpoint's own
  freeze gate (a request that would change it, a container start or unpause
  included, is refused; the ones already admitted finish first), and stays
  frozen until the VM stops (the freeze lives in the agent, which exits with the
  VM); (2) every running container of the engine is **paused**; (3) **every
  tracked workspace** is captured (the base, and each lane that still has a
  checkpoint) with the reserve checked for each; (4) the engine is asked again
  what runs: **any engine-query error, and any running container, make the
  outcome a gap**, never `proven`; (5) each workspace's outcome (`proven` with
  the snapshot, or `failed` with why) is journalled durably, bound to the owned
  VM's UUID, before the VM stops. A stop that then fails thaws exactly what was
  frozen, releases the fence and forgets the outcome. If the exclusion cannot be
  acquired in time (a checkpoint capture is still running; 120 s for `services
  stop`, 20 s per VM for `marsh stop`, 0 for the idle timer, which tries again
  later) the stop is **refused** (`services stop` fails; `marsh stop` reports
  the VM as still running and leaves it); so is a stop whose outcome cannot be
  persisted. If the capture was skipped as busy it is retried (3 attempts).
  A direct start in the engine by something other than the filtered endpoint
  (nothing in the product does) is caught by the count in (4) only before it,
  not after: the fence is the filter's.
- **VM stopped:** what a checkpoint may reference depends on the outcome
  journalled for **its workspace**: `proven`: `reused_from` names that
  workspace's snapshot from the cutover, which holds its last write, and the
  record carries its volumes and stacks (no gap); `failed` (the final capture
  failed, was refused for space, or the engine could not be frozen): no
  reference and a gap saying so (the old snapshot is never offered as this
  turn's data); no outcome (a stop outside marsh, G13, or one that did not
  survive a restart): `reused_from` the workspace's last capture **with a
  warning gap** ("stopped outside marsh: changes after this snapshot are not
  saved"). A VM that was removed and recreated has no history: the journal
  entry names the VM's UUID and another VM's entry is never used (a crash
  between `services rm` and forgetting it is harmless). If what marsh
  recorded may be lost (journal unreadable, or a marker found: a write failed
  or was interrupted, even when an old journal file remains with a `proven`
  outcome in it), every entry is unknown: the checkpoint references the
  workspace's latest checkpoint snapshot with the warning "marsh lost its
  record of when the services VM stopped". The journal is written file
  `fsync`, rename, directory `fsync`; a change that matters for a stop
  (its outcome, its clearing when the VM starts) first makes the marker
  durable and removes it after the new journal is; a write that fails leaves
  the marker as the invalidation. When neither the journal nor the marker can
  be persisted, the stop is refused, the VM is not started, and a capture is
  a gap (its snapshot dropped). With no snapshot at all the field is
  `null`. The journal is `services-last-capture.json` (the daemon's home);
  `last_capture` in `marsh services status --json` is the base workspace's.
- **Low pool space** (DECISIONS 5): the reserve (512 MiB;
  `MARSH_SERVICES_POOL_RESERVE_BYTES` at daemon start overrides it, as a test
  setting) is checked when the capture is about to be taken, not at `admit`
  (a reading taken before the tree copy or any wait is stale): the reserve
  travels in the generation-bound capture request, and the agent reads the free
  space with `fstatvfs` on the pool descriptor it already holds open, after the
  capture is admitted and just before it writes (no `sbx exec`, no name lookup).
  Below the reserve, or unreadable, the capture is skipped
  with the gap "not enough space for app data" (or "could not be read"); the
  file checkpoint proceeds; exit 75 stays for the Mac volume rule. The stop's
  captures obey the same rule, so a stop below the reserve is a `failed`
  outcome.
- **Retention, `rm`, orphans:** a snapshot is in use while a live checkpoint
  of the project names it by `snapshot` or `reused_from`, while it is a
  workspace's last capture, or while a checkpoint being made holds it. Removing
  a checkpoint drops its snapshot only if nothing else uses it, so the last
  reference drops it; the decision and the mark that stops new references are
  one step under one lock, and nothing is held across a capture. A leased
  checkpoint (a restore reading it) is neither removed nor retired. A lane
  whose last checkpoint is removed is untracked. The daemon's services timer
  collects orphans (in the agent's list, used by nothing, older than 15 s)
  every five seconds for ready VMs, scanning every project with a journal entry
  or an **owned services VM**, with no checkpoint record needed, so a project
  whose last record is gone is still scanned the next time its VM runs. At
  daemon start the journal is reconciled with the records that survived: a lane
  capture the journal protects whose checkpoint record is gone (a crash between
  publishing it and recording the checkpoint) is untracked, and the collector
  reclaims it unless a record, a checkpoint being made or a lease names it.
  `marsh services rm` and `reset --include-services` forget the project's
  journal entry with the pool.
- **Events:** the `checkpoint` event carries `services`: `captured`, `reused`,
  `gap` or `none`.

The lane workspace name is `fork-SPLIT_ID-LABEL` (`checkpoint::lane_workspace`);
the lane's scope is created under that name (see App data in lanes).

## App data in lanes

`marsh split --open --from CHECKPOINT LABEL...` of a checkpoint that holds a
services snapshot gives each lane its own copy of the app data (C3). Everything
the services VM says is hostile (SECURITY-DESIGN A8, E1-R2): marshd validates the
whole restore result before it uses any of it, writes the lane's two files itself
(A9), and builds the restore line from validated fields and words of its own.

**What happens, per lane** (the lanes run in parallel):

1. The checkpoint's record names the snapshot (`services.snapshot`, or
   `services.reused_from` for a checkpoint taken while the VM was idle-stopped).
   The split holds a lease on the checkpoint from its plan until the restore is
   done, so retention and `checkpoints rm` cannot drop the snapshot meanwhile
   (`hold-restore-after-lease`). A record with no snapshot (a capture gap) is `ok: false` with
   "this turn's Docker data wasn't saved" and the record's gaps; a checkpoint with
   no `services` at all (Docker off, or never used: the VM was never created, which with
   Docker on by default is the usual case) restores no Docker data and is not a failure
   (its folder data still comes along, below).
2. `restore_prepare` through the relay, with the lane's own workspace
   `fork-SPLIT_ID-LABEL` and fork name, the lane folder
   (`<root>/.marsh/split/SPLIT_ID/LABEL`) and, for a checkpoint of a lane, the
   first lane's folder as `source` (the folder the checkpointed stacks' paths are
   relative to). The services VM is started if it is stopped.
3. **Validation** (`lane_services::validate`), of every field, before any is used:
   file names are exactly `compose.fork.yaml` and `.env`, in no folder but an
   ordinary relative one (at most four deep, never `.git` or `.marsh`); a file is
   at most 128 KiB of valid UTF-8 with no NUL; ports are integers 1 to 65535 and
   no two map to one; service and volume names are single tokens
   `[A-Za-z0-9][A-Za-z0-9._-]*` of at most 128 bytes; every text is made plain
   (`marsh_sbx::vm_text`: control, escape and direction-changing characters
   removed) and bounded, and one of more than 16 KiB is refused. Any defect:
   the fork is dropped, **nothing is written**, and the lane opens with
   `ok: false` and a gap saying the result was refused.
4. The lane files are written **by marshd on the Mac** (A9): the lane is opened by
   name below the split's held directory and its identity checked against the
   record, each file goes in as a temporary file (`O_EXCL | O_NOFOLLOW`) beside the
   target and an atomic rename (which replaces a symlink and never follows it),
   at the mode the file had. `hold-lane-files-write` is the harness barrier before
   the write. A lane folder that was replaced or became a link is refused. The two
   files are recorded as the lane's *generated* files: `split apply` leaves them
   out of the patch (`out/diff.patch`), so a lane's `compose.fork.yaml` and `.env`
   never reach the project.
5. `restore_up` brings the stacks up through the filter and runs the post-restore
   check; that result is validated the same way.

A restore that fails or is partial is **never silent**: `ok: false`, the gaps,
and a line that says so; the lane still opens with its files. `ok` is true only
when the agent says so, the check passed, nothing failed, and neither the
restore nor the checkpoint's record has a gap (TEST-PLAN A.4).

**The reply** of `marsh split --open --from`, on stdout, one JSON line:

```json
{"marsh_split":1,"id":"…","lanes":{"a":"/…/.marsh/split/ID/a"},
 "restore":{"a":{"ok":true,
   "line":"Local data restored: .wrangler/state, .env, app, db (Docker). Started empty: redis (no saved data), mail (no saved data).",
   "restored":[{"kind":"service","stack":"stack-a","service":"db","volumes":["pgdata"]},
               {"kind":"volume","name":"fork-ID-a_notes","from":"stack-a_notes","containers":["keeper"],"restarted":false}],
   "fresh":[{"kind":"service","stack":"stack-a","service":"redis","reason":"no saved data: it keeps nothing in volumes"}],
   "gaps":[],"ports":{"5432":15432},
   "check":{"ok":true,"containers":[{"service":"db","state":"running","health":"healthy"}]},
   "folder":{"files":14,"bytes":20480,"copied":[".wrangler/state",".env"],
             "skipped":[{"path":"node_modules","reason":"wasn't copied: tools rebuild it (dependencies, build output, caches). To copy it, add `include node_modules` to .marsh/branch"}]}}}}
```

- `restore` is present for an open split made `--from` a checkpoint that has local
  data of either kind, one entry per lane label. **Docker off, folder data copied:**
  there is still an entry (`ok: true`, no `restored`/`fresh`/`ports`, `folder` set) and a
  line (`Local data restored: .env, .wrangler/state.`). With neither (no `services` in the
  checkpoint of a project that expects none, and nothing copied from the folder),
  `restore` is `null` and `restore_note` says so. Without `--from` neither key is present.
- `folder` (new, optional; JSON names of the rest are unchanged): what came along from the
  folder: `files`, `bytes`, `copied` (the saved paths as the line names them) and `skipped`
  (`path`, `reason` in plain words, and `lost: true` when data the user may expect is
  missing, i.e. over a limit or unreadable, rather than left out on purpose: junk, an
  `exclude`, a link). A `lost` skip makes the lane's `ok` false and adds a gap
  (`not copied: PATH (over the size limit)`) and a `Not copied:` clause to the line.
- `line` is one line of at most 400 characters (Unicode scalars): the restored
  clause (`Local data restored: FOLDER PATHS, DOCKER SERVICES (Docker).`, or `No local data
  came back.`), a failure clause that is never dropped, and then what started empty or was
  not restored, each name cut to 32 characters, "and K more not restored" for
  what did not fit. The reasons are the Mac's words for the class of reason
  (`no saved data`, `it did not start`, `it can't be forked safely`, ...), never the
  agent's text; it never contains the words volume, snapshot, pool, VM, xfs, btrfs or
  reflink. `restored` and `fresh` keep the agent's `kind`/`service`/`stack` fields
  with validated names; a `reason` there is plain, bounded text.
- `ports` maps each original port number to the number the fork published. In the
  lane's jobs the **original** numbers work (A6): the job's listener on 5432
  reaches the fork's 15432 (the lane's rewritten `.env` names 15432, so the job
  listens on that number too). For a lane started from a checkpoint of a lane the
  map is composed with the source lane's (`Checkpoint.lane_ports`), so it names the
  project's original numbers again, not the first lane's.
- The plain output is the same result as one line per lane on stderr
  (`restore LABEL: LINE`); the JSON stays on stdout. The `split` event of
  `marsh events` carries the same `restore` map, and `marsh splits --json` carries
  each lane's `restore` and `generated` in its record. Narwhal decodes `restore`
  (N1/N2): the keys above are stable, new keys may be added, none is renamed.

**Lane scope.** A job in a lane gets the endpoint with the authority
`{root: the lane's folder, workspace: fork-SPLIT_ID-LABEL, attempt}`: derived from the
job's recorded mounts (a child's are its parent's, so a child of a lane job that
`cd ~` is still in its lane's scope), never from its working directory, and only
for an **open** lane the daemon registered (`DaemonBackend::services_lane`): a job
of a closed lane, of a fork that is not a lane, or whose mounts name two places
gets no endpoint. Bind sources resolve beneath the lane's folder, so a lane cannot
bind the project or another lane. The relay accepts a scope for the project's root
or a folder strictly inside it. (A shell opened in a lane's copy still gets the
project's endpoint: it is the user's own, trusted shell.)

**Cleanup.** `close`, `cancel` and `rm` of a split drop its lanes' Docker objects: the
split is fenced, the daemon waits (up to 150 s) for a restore still running, forgets
the lanes with the backend, journals each drop (`lane-drops.json`) and sends
`drop_workspace` with the workspace and nothing else. The agent removes the
workspace's containers (force, with anonymous volumes), volumes and networks
through the filter *as that workspace* (a listing shows only its own objects, so
the drop cannot name another's), then forgets the scope's substituted sockets
(their listener threads end) and default network entry in its ledger. What is
left is reported and retried every 5 s; a drop with the VM stopped waits for it
(it never starts a VM only to drop). `apply` merges files only and leaves
services alone.

### Capture budget (P2, DECISIONS 16: skip and say so)

A services capture must not hold up the turn. The daemon gives the capture a
**budget** of `services_capture_budget_ms` (default 2000 ms, 100 to 60000) from
the moment it starts. Two things are checked against it: if the steps before
the request to the services agent (the gate, the pool's reserve read) already
used it, **no request is sent**; and the wait for the agent's answer ends with
the budget. Either way the capture is skipped with this gap, in plain words:

> Docker data was not saved this time: saving it would have held up your next step by more than 2.0 s; your files were saved

`services` then has `snapshot: null` and that gap; the file checkpoint (and
`ignored/`) is complete and the command exits 0. The freeze itself keeps its own,
tighter budget inside the agent (500 ms: a volume it cannot copy in time is a gap
of its own). A capture the daemon stopped waiting for may still finish in the VM;
nothing references what it publishes, so the orphan collector drops it. The stop
cutover (an idle or explicit stop) is not a turn and runs without this budget.
The test break `capture-no-skip` (alias `capture-block-next-prompt`) waits for the capture however long it takes.

### Folder local data in checkpoints (C5, DECISIONS 6, 26, 27)

The checkpoint's `tree/` leaves out what Git ignores, so a branch of it would start without
the user's `.env`, `.wrangler/state`, SQLite files and the like. `marsh checkpoint` therefore
also clones **every git-ignored path** of the project, except rebuildable junk, into
`<checkpoint>/ignored/` (same relative paths; APFS clones, so no extra space until a file
changes), whether or not Docker is on (there is no toggle: DECISIONS 26), and the record
gains `ignored`:

```json
{"ignored":{"files":14,"bytes":20480,"roots":[".env",".wrangler"],
            "skipped":[{"path":"node_modules","reason":"wasn't copied: tools rebuild it (dependencies, build output, caches). To copy it, add `include node_modules` to .marsh/branch"}]}}
```

`ignored` is `null` for a checkpoint made before this existed or by a backend that saves none;
`{files: 0, bytes: 0, skipped: []}` when there was nothing to save, and for a project that is
not a Git repository (everything is already in `tree/`).

- **What is taken:** every path the project's ignore rules exclude, decided as the tree
  snapshot decides it: the user's global excludes, `.git/info/exclude` and every
  `.gitignore`, read as data through descriptors (marsh does not run `git` against a
  repository whose config a job can write). The highest ignored path of each place is one
  saved path (`roots`: `.env`, `.wrangler/state`), and a saved folder is cloned as a tree
  with its empty folders. A path already in `tree/` (tracked) is not saved again. A nested
  repository is its own project and is not copied.
- **What is left out on purpose** (listed in `skipped`, `lost` false): the built-in junk
  list (`node_modules`, `.next`, `.nuxt`, `.svelte-kit`, `.angular`, `.nx`, `target`, `dist`,
  `build`, `out`, `.venv`, `venv`, `__pycache__`, `.pytest_cache`, `.mypy_cache`,
  `.ruff_cache`, `.tox`, `.turbo`, `.cache`, `.parcel-cache`, `.gradle`, `.terraform`,
  `coverage`, `.nyc_output`, and the files `*.log`, `*.pyc`, `*.tsbuildinfo`, `.DS_Store`)
  at any depth; what `.marsh/branch` excludes; a symbolic link (never followed, never
  copied); a pipe or device (never opened); anything named `.git` or `.marsh`. Log and
  cache files are counted in one entry rather than listed.
- **`.marsh/branch`** (per project, read at each checkpoint, at most 128 KiB; `.marsh` is
  not copied): one `include GLOB` or `exclude GLOB` per line, blank lines and whole-line `#`
  comments ignored, other lines said in `skipped`. Globs are gitignore-style (matched
  against the project-relative path; a folder pattern covers what is inside it). **An
  include beats the junk list** (`include node_modules/my-pkg` copies that folder although
  `node_modules` is left out; inside a junk folder only included paths are taken), **an
  exclude beats everything** (an include does not rescue an excluded path).
  The file only chooses among git-ignored paths: it cannot add a tracked or untracked file.
- **Safety:** unchanged from C5. The walk is by descriptor with `O_NOFOLLOW` at every step,
  using the helpers the tree snapshot uses; a file that becomes a link while it is cloned is
  removed again; a file that grows past the bound while it is cloned is removed.
- **Bounds (defaults):** 20000 files and 2 GiB in all (`services_ignored_max_files`,
  `services_ignored_max_bytes` in `config.json`). Single files first (so `.env` and the like
  are never crowded out by a big folder), then folders, each sorted. A saved path that does
  not fit is skipped from there on and said **once** (`over the size limit of 2 GiB for saved
  local data; the rest of it wasn't copied`, `lost: true`), at most 64 entries and then a
  count. A checkpoint never fails for this.
- **In a lane:** `split --open --from CHECKPOINT` copies `<checkpoint>/ignored/` into each
  lane's folder **before** any Docker restore runs (`ignored::copy_ignored_into_lane`).
  `.marsh` is guest-writable, so `ignored/` is read as untrusted: links, pipes and
  `.git`/`.marsh` are refused again, a file the lane already has is left alone, and nothing
  beyond the files and bytes the checkpoint recorded is copied. The result, with the
  checkpoint's own `skipped`, is kept on the lane (`BranchRecord.folder`) and **folded into
  the lane's restore result** (`folder`, the line, `ok` and `gaps`, see "App data in lanes"),
  so the one line says what came along across both places. The files are not part of the
  lane's base, so `split apply` does not carry them back (they are ignored files). They never
  leave the Mac (R17).
- **Test breaks:** `ignored-skip` (alias `ignored-capture-off`: nothing is saved),
  `ignored-follow-symlink` (alias `ignored-follow-outside-symlink`: a link is followed
  and what it points at is copied).

### Contract for clients

What a client (Narwhal) may rely on:

- **`services-v1`** in `features` of `marsh status --json` (and the event stream's
  `hello`) means this build can run app services: the engine artifacts are installed
  beside the daemon. It is absent from a build without them; a client then offers no
  services UI. The `services` subcommand and fields below exist only with it.
  `marsh status --json` lists the other features as before.
- **`services`** on a checkpoint (`checkpoint --json`, `checkpoints --json`): the
  record under "Snapshots in checkpoints", `null` when off.
- **`ignored`** on a checkpoint: the record above (`roots` and `lost` are new optional keys).
- **`restore`** on a `split --from` reply, per lane: `{ok, restored, fresh, gaps,
  ports}` (what was restored into the lane, what started fresh, and why anything
  is missing), and, new, `folder` (see "App data in lanes"). It comes with the restore (C3),
  not with this section; a client treats it as optional. The folder's git-ignored files are
  already in the lane by the time the Docker restore runs.
- **`summary`** on each project of `marsh services status --json`: one plain line
  to show as it is, instead of rewriting `reason` (which stays, and is for logs):
  `Off`, `On — nothing has been started yet`, `Starting`, `Ready — 4 containers`
  (`1 container`, `no containers running`), `Stopped — Docker data kept`, or
  `Unavailable: <short plain reason>` (`it could not start; try starting it again`,
  `it needs a restart`, `its setup changed; it needs a restart`, `it was stopped for
  safety; it needs a restart`, `marsh lost contact with it; it needs a restart`,
  `the sandbox service is not responding`, `this build of marsh cannot run Docker
  for projects`). It never names the VM, a volume, a snapshot, the engine or `sbx`. When
  the build lacks the engine artifacts the line is the last one whatever the project's
  `state` says (a project with no VM is still `off`).
- **There is no `marsh services down`** (DECISIONS 25): `marsh services stop` keeps
  the data, `marsh services rm` removes the containers and the data.

## Reaping at attempt end (E4)

Objects belong to their workspace, not to the job (DECISIONS 11). When an attempt ends, marsh
removes only the containers that attempt created **and** that are neither in a Compose project nor
set to restart. Compose stacks, restart-policy containers, named volumes and anything a shell made
persist until the user removes them.

- **What is selected.** The agent (`marsh-services` `reap.rs`) takes the containers the ledger
  records for the workspace and the attempt, and checks each one's inspect: the scope label must be
  the workspace and the attempt label the attempt. A mismatch is left alone and reported. Names never
  select anything. A container with the Compose project label, or with a restart policy other than
  `no`, is kept (the label is a hint about what survives, not authority; DECISIONS 11).
- **What is removed.** Stop with a two-second grace, then remove with force and **without** volumes,
  so named and anonymous volumes stay. Pins are released with the container. Idempotent: a container
  already gone is only forgotten.
- **Who asks.** The daemon, through the relay's `reap` frame (answered by request id, bounded like the
  others). Three triggers:
  - a job's end, on every path out of its response loop (normal exit, cancel, a crashed job, a lost
    worker), when the job used the project's services VM (its endpoint or its port watcher);
  - the recovery after a daemon restart, for the records a dead daemon left behind (only when the
    records file held some at start, once the project's relays are back);
  - a failed earlier reap, asked again at the next such trigger.
- **The record.** An attempt is recorded in `services-reaps.json` in the daemon's home **before** its
  first channel reaches the VM, so no container can exist without a record (`hold` in `endpoint.rs`,
  `services_reap.rs`). The record is running while the attempt is; the attempt's end marks it due. A
  record goes once the agent reports the reap complete. A failed reap keeps the record with its reason
  and is asked again at the next trigger (a later job's end, or the next daemon start), at least ten
  seconds after the failure. There is no periodic retry: the next job that ends in the project, or
  the next start, is the trigger. Nothing starts a services VM for a reap: a record waits until the
  project's relay is up. `services rm` and a confirmed reset drop the project's records. At most
  256 records; a job that would need one more is refused before it touches the VM.
- **Bounds.** At most 256 containers per pass, a 45-second deadline per pass, and a freeze of the
  workspace's state-changing requests while it runs: requests already admitted finish first, so a
  create in flight when the job ended is reaped too, and new ones are refused with a `marsh:` message
  until the pass is over (a concurrent job retries; the freeze waits at most 20 seconds for the first
  part, and a pass that could not freeze records a gap and is asked again).
- **Shells** are never recorded and never reaped at their exit. A shell's objects are removed by the
  user or by `marsh services rm`.

The model is `docs/model/ServicesReap.tla`.

## CLI

`marsh services status|start|stop|rm|hold|release|internet` (host only), schema
`marsh.services/v1` with `--json`:

```json
{"schema":"marsh.services/v1","enabled_projects":["/Users/me/app"],"idle_seconds":900,"disk_size":"40g",
 "projects":[{"root":"/Users/me/app","enabled":true,"state":"ready","reason":null,
   "vm":"marsh-v-1a2b3c4d","vm_uuid":"…","generation":{"boot":"…","counter":3},
   "summary":"Ready — 4 containers","holders":[],"idle_seconds_remaining":812,
   "containers":[],"ports":[],
   "internet":{"on":false,"governed":false,"effective":"off: containers cannot start connections out; image pulls from the default registries work"}}]}
```

Each project also has `summary`, one plain line for clients (see "Contract for clients").
`containers` and `ports` are filled as described under Job endpoint and port forwards;
they are `[]` unless the project is `ready`. `internet` is described under Internet. `last_capture` is `null` or `{id, at, reason}`.
`rm` asks for confirmation on a terminal; without one it needs `--yes`.
`marsh reset --include-services [--yes]` is described under Identity.

## Model

`docs/model/ServicesVm.tla` models the project-keyed VM lifecycle with the
distinct transitions of stop, default reset, confirmed `reset --include-services`,
explicit `rm`, idle expiry and turning services off, and a negative control for
each rule above (see `docs/model/README.md`). `docs/model/ServicesLanes.tla` models what marsh
deletes of a lane's services objects (by workspace only, after a restore still running, retried while the
VM is stopped). `docs/model/ServicesSnapshots.tla`
models what marsh deletes of snapshots and what a stopped VM's checkpoint may claim: references from checkpoints (`snapshot`
and `reused_from`), per-workspace last captures, leases, the fenced stop cutover and its journalled outcomes, and the orphan
collector.
