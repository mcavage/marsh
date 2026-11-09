# Services Docker policy

The services agent (`crates/marsh-services`, modules `policy`, `pins` and
`ledger`) runs inside the project's engine container and is the only way a job
reaches the production Docker engine. It is the security boundary of the
services feature: root in the services VM is out of scope, everything a job
can do through the Docker API is not.

The design is `SECURITY-DESIGN.md` B1, B2, A3, A3.1, A4, A5 and A7 in the
Narwhal Fork spike. This page records how the code meets it.

## Request path

1. **Strict parse** (`policy::http`). HTTP/1.0 and 1.1, CRLF only, token header
   names, no obs-fold, exactly one framing, origin-form target, 32 KiB and 100
   fields for a head, strict chunked decoding without extensions or trailers.
   `Expect:` gets 417. Ambiguity is a 400 and a close.
2. **Route allowlist** (`policy::routes`). Percent-escapes, `//`, dot segments
   and non-ASCII in the path are refused, not decoded. One spelling of the API
   version (`/v1.NN`, NN at least 44) or none. Unknown routes are a 403.
3. **Body schema** (`policy::json`, `policy::schema`, `policy::docker`). Bodies
   are parsed strictly (duplicate keys, trailing data, BOM and invalid UTF-8
   are errors), matched against a closed schema (keys are mapped to Docker's
   spelling, unknown keys refused) and re-serialized.
4. **Admission** (`policy::admit`, `policy::create`). Per-route query
   allowlists, scope checks through the ledger, bind pinning, socket
   substitution, reserved labels, resource defaults and limits, loopback
   publishes.
5. **Rebuild and relay** (`policy::endpoint`). The upstream request is built
   from the admitted pieces (method, canonical path, header allowlist, the
   validated body) and sent on a fresh connection to the engine. The client's
   bytes are never forwarded. A connection goes raw only after the engine
   answered `101`, or `200` with a Docker stream content type, on `attach`,
   `exec/start`, `/session` or `/grpc`.
6. **Streams have deadlines.** A raw stream is copied by one poll loop: a byte
   either way resets the idle deadline (`Config::hijack_idle`, 30 minutes), a
   peer that has fully gone (`POLLHUP`; a half-close does not raise it) ends the
   stream at once, writes time out after `HttpLimits::progress` so a client that
   stops reading is cut, and after the client half-closes the engine's output
   is still delivered (`docker exec -i < file`). The slot is freed when the
   stream ends.
7. **Connection caps** (A7). Per attempt (64), per project (256, every workspace
   root of one `Services` counts) and per process (1024, `PROCESS_CONNS`).
8. **Ledger failures are request failures.** Every ledger change is made on a
   copy, written, and only then adopted; an error is a `503`, the engine object
   that was just made is deleted, and no mutating request is admitted until a
   write succeeds again (reads still work). A pin that was mounted but could not
   be recorded is unmounted.
9. **Capture freeze** (CAP-11, S3's hook `Services::freeze_scope`). A freeze waits
   for the state-changing requests already admitted for that workspace (from
   before their body is read until the ledger is updated) and then refuses new
   ones until its guard drops. Waits, attaches, resizes and exec start (long
   lived, or not container state) are not counted; image, build and exec
   routes are refused while frozen but not waited for.

## Pins (A3)

A bind source is resolved beneath a held project-root descriptor with
`openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV)`, cloned
with `open_tree(OPEN_TREE_CLONE)`, attached with `move_mount` under
`/var/lib/marsh/binds/<scope>/<id>` and made private. The request is rewritten
to that path. A missing `-v` source is created inside the project, as Docker
would.

**Protected paths.** `.git` and `.marsh` are protected by identity, not by
name: the project folder may be case-insensitive (APFS through virtiofs), so
`.GIT` is `.git`, and a renamed `.git` is still `.git`. The `(st_dev, st_ino)`
of each protected name is recorded in the ledger, per project root, the first
time the agent sees it (when it first opens the root, and at every pin; known
identities are never recorded twice, the earliest 64 are kept). A pin is
protected if its target, or any ancestor, is one of the identities recorded or
visible now. For a directory the ancestors are found by walking up from the
open handle (`..`), so renaming a parent after the open changes nothing; for a
file they are resolved by name. A pin of any of them (or inside) is read-only; a
pin of the project root carries the protected names, and every entry of the root
that is a recorded identity (a `.git` renamed to `.gx`), as read-only sub-mounts.
A root pin makes `.marsh` first (`mkdirat`) so that the `.marsh/checkpoints` and
`.marsh/split/*` created later are covered. A missing `.git` is not created (an
empty one would break `git init`); a repository initialised after a root pin is
made is a residual until the next pin. A bind with `NonRecursive`
(`bind-recursive=disabled`) would drop the sub-mounts and is refused, as is
`private` propagation (only `rprivate`).

Residuals of the identity record (a job with write access to the project can
already write `.git` while it runs; these only affect what survives the job):
- *First sight.* An identity is trusted from the moment the agent first sees
  it. If `.git` is renamed before the agent has ever looked at the project root
  (before the first Docker request of the project), the renamed tree is never
  recorded.
- *Moved deeper.* A recorded tree moved into a subdirectory is protected when
  the pin targets it or lies inside it, but a pin of the subdirectory (or any
  directory above it other than the root) does not carry it as a read-only
  sub-mount: only the root's own entries are scanned. Doing it for every depth
  would walk the whole tree on each pin.
- *Files.* A pin of a file resolves its ancestors by name, after the open, so
  a rename in that window can still hide a protected ancestor from the check.
- *Inode reuse.* A recorded identity whose inode the filesystem later gives to an
  unrelated directory makes that directory read-only when pinned. The shared
  folder's inodes are APFS's, which are not reused in practice.

**Leases.** The ledger records, per container, the pins it made and the pins of
every container it takes volumes from (`VolumesFrom`). A pin is released with
the last holder; the freed record (directory or file) goes with it so the
mountpoint is removed whatever it is. A periodic reconcile does the same for
auto-removed containers and sweeps pin paths no record names. A pin is mounted
and recorded under the read side of one lock and the sweep reads the ledger
under the write side, so it cannot meet a pin that is mounted and not yet
recorded; a pin's age is not consulted (the source folder's modification time
said nothing about the pin).

**Restore** (`engine.rs` `restore_pins` calls `pins::restore_all()` before the
production engine starts; an `Err` fails the boot). Each pin is re-resolved
beneath the reopened project root without following symlinks. A pin that
cannot be recreated is left as a dangling symlink (`/marsh-pin-not-restored`)
and recorded as failed; Docker cannot make an empty directory there, so a
`start` fails, and the filter refuses `start` and `restart` of any container
whose lease holds a failed pin. Removing the container clears it. The
substituted-socket paths are reserved as bound-and-dropped socket files, so
dockerd never creates a directory there. **TODO(E1):** the relay agent must call
`Services::restore_sockets()` once it listens (marked in `engine::bring_up`).

## Socket substitution (A3.1)

A bind of `/var/run/docker.sock` or `/run/docker.sock` becomes a socket the
agent serves under `/run/marsh/sockets/<workspace>/<attempt>/docker.sock` (root
only directory) with the creating attempt's authority. The set of such sockets
is in the ledger and is served again after an engine restart. The job
endpoint path E2 gives every job (`/run/marsh/docker.sock`, DECISIONS 20) is in
`Config::socket_aliases` by default and substitutes like the two standard
spellings.

## Ledger (A4)

`ledger.json` on the pool records the owner scope of containers, networks,
volumes and exec IDs, plus pins and substituted sockets. Every object route
resolves names, short IDs and full IDs inside the caller's scope only and
rewrites to the full ID; other scopes' objects answer exactly like missing
ones. Lists and prunes carry a scope label filter, list responses are filtered
again against the ledger (an engine answer that cannot be parsed is a 502, not
passed on), and events are filtered line by line. Each scope has a default
bridge network, `marsh-scope-<workspace>`, and the engine is always given the
network's full ID (never a name, which Docker resolves after an exact ID).
Container, network and volume names that look like a full ID (64 hex digits) are
refused for the same reason.

Volume ownership is one atomic step: `Ledger::claim_volume` reserves a name for
a scope (`New`, `Own` or `Foreign`), never replaces another scope's claim, and
runs under one lock with the engine's own lookup and create; an engine volume
nobody owns is not adopted. Anonymous volumes (`-v /path`, `Config.Volumes`,
image `VOLUME`s) are rewritten to labelled `volume` mounts, so they appear in the
creator's lists and prunes and only there.

`docker update` is validated on the limits the container would have after the
update (a patch is merged with what the container has now), and `PidsLimit`
0 or -1 and `MemorySwap` -1 are refused; a container that is unlimited now gets
the defaults when it is updated. The restart policy is not changed by an update.
Network create takes no options. `docker commit` may only name a repository under
`<workspace>/` (with or without `docker.io/`). Build `outputs` is a closed
schema (the `moby` exporter; local attributes only; no `push`), rebuilt before
it is forwarded.

## Compatibility notes

- Docker (API 1.44+) ignores a body on `start`, `stop`, `kill` and the other
  action routes, and dockerode sends `{}`; such bodies are read (bounded) and
  dropped, never forwarded.
- dockerode (Testcontainers) repeats create-body fields in the query string and
  puts `name` in the body. Both are ignored by Docker and dropped here; any
  other unknown query key is refused.
- Testcontainers mounts the job's own `DOCKER_HOST` path into Ryuk.
  `Config::socket_aliases` lists such caller-endpoint paths (for example
  `/run/marsh/docker.sock`) to substitute like `/var/run/docker.sock`; or the
  job sets `TESTCONTAINERS_DOCKER_SOCKET_OVERRIDE=/var/run/docker.sock`.
- Compose and Testcontainers address networks by name in `NetworkMode`,
  `EndpointsConfig` and `Links`; they are rewritten to the full IDs the ledger
  resolved, and the engine reports the names back as usual.
- **TODO (E5):** `Config::registry_allow` itself defaults to allow-all; the engine
  sets it from `EngineConfig` (`engine-up --registry-allow`, written to
  `/run/marsh-svc/engine-config.json`; default Docker Hub plus `ghcr.io`, see
  `docs/design/services-engine.md`). The default is E5's decision; the agent only
  enforces the list it is given.

## Engine requirements (for S2)

The production `daemon.json` must set `"ip": "127.0.0.1"` (so `-P` publishes on
loopback) and keep the BuildKit entitlements off. The engine container needs
cgroup v2 delegation as in `docker:dind`'s entrypoint, or containers with the
default memory, CPU and PID limits fail to start. `pins::restore_all()` must run
before the production dockerd. (Done by S2/S2c: `engine-up`, break modes
`engine-ip-any` and `engine-cgroup-no-delegate`; `Services::restore_sockets()`
runs in the relay agent once it listens: TODO(E1).)

## Break modes

Every check in `policy` and `pins` calls `crate::breaks::break_mode`, which asks
`marsh_contracts::break_mode` (the `test-breaks` feature forwards to
`marsh-contracts/test-breaks`, so H2's release guards apply) and the modes the
daemon hands the agent in its argv. Each mode removes one defense, so that the
scenario that names it fails at the named check:

| Mode | Defect | Scenario and check |
|---|---|---|
| `filter-off` | no inspection at all | SEC-01 c1 |
| `filter-no-upgrade` | hijack never honoured | EP-04 c1 |
| `filter-case-sensitive` | keys that differ in case are forwarded as written | SEC-02 c1 (W-case) |
| `filter-lenient-json` | unparseable JSON fails open | SEC-02 c1 (duplicate keys) |
| `filter-raw-after-upgrade-header` | any `Upgrade` header makes the stream raw | SEC-03 c1 |
| `filter-forward-original-bytes` | the client's head is forwarded | SEC-23 c1 |
| `filter-no-resolve` | bind sources opened by plain path | SEC-04 c2 |
| `bind-no-pin` | resolved path rewritten, not pinned | SEC-04 c1 |
| `bind-repin-unchecked` | pins recreated by plain path | SEC-14 c1 |
| `bind-pin-leak` | pins never unmounted | SEC-14 c2 |
| `protected-subpin-off` | `.git` and `.marsh` not read-only | SEC-05 c3 |
| `lane-authority-off` | no scope checks, lists and events unfiltered | SEC-05 c1, RST-11 c1 |
| `scope-reference-check-off` | inspect by full ID skips the scope check | SEC-16 c1 |
| `reserved-labels-allowed` | `dev.marsh.*` accepted from clients | SEC-16 c2 |
| `socket-substitute-raw` | docker.sock binds pass unchanged | SEC-15 c1 |
| `socket-refuse` | docker.sock binds refused | SUPA-01 c1 |
| `services-resource-limit-off` | no default limits | SEC-20 c1 |
| `publish-nonloopback` | published ports keep their host IP | EP-10 c2 |
| `capture-api-gate-off` | state changes allowed while a scope is frozen | CAP-11 c1 |
| `freeze-no-wait` | a freeze does not wait for admitted requests | freeze.sh (late body), unit `a_freeze_waits_for_*` |
| `ledger-persist-ignore` | a failed ledger write is logged and acknowledged | ledger.sh, unit `a_ledger_write_failure_*` |
| `volume-claim-racy` | the loser of a volume race replaces the owner | volumes.sh, unit `concurrent_claims_*` |
| `bind-nonrecursive-allowed` | `bind-recursive=disabled` accepted | protected.sh, unit `non_recursive_bind_mounts_*` |
| `protected-by-name` | `.git`/`.marsh` recognised by spelling | protected.sh (`.GIT`, `.Marsh`) |
| `protected-marsh-late-off` | `.marsh` not made at a root pin | protected.sh (`late`) |
| `pin-allow-xdev` | resolve without `NO_XDEV` | pins.sh (tmpfs inside the project) |
| `pin-inherit-off` | `VolumesFrom` pins not leased | pins.sh (`volumes-from`), unit |
| `pin-file-leak` | a file pin's mountpoint is not removed | pins.sh (`file pin`) |
| `restore-fail-open` | an unrestorable pin is left absent | pins.sh (`failed restore`) |
| `protected-identity-off` | recorded `.git`/`.marsh` identities ignored (only the names found now count) | HF1 `protected-identity` |
| `sweep-pins-by-source-age` | the sweep as it was: ledger read before the directory, "old" is the source folder's mtime | HF1 `sweep-race` |
| `slow-pin-record` | 200 ms between mounting a pin and recording it (widens the window the sweep must respect) | HF1 `sweep-race` |
| `fault-pin-orphan` | a pin is mounted and never recorded (the agent died in between) | HF1 `sweep-orphan` |
| `hold-pin-before-ledger` | harness barrier between mounting and recording | HF1 `sweep-race` |
| `ports-list-all` | a `tcp` open lists every container in the engine (the check as it was) | PERF-05 (b) |
| `update-limits-unmerged` | update validated on the patch alone | limits.sh, unit `update_limits_are_validated_*` |
| `commit-any-repo` | commit into any repository | sec01.sh, unit `commits_go_only_*` |
| `anon-volume-unlabeled` | anonymous volumes get no scope label | volumes.sh, unit |
| `project-cap-per-root` | the project cap counted per workspace root | unit `the_project_cap_counts_*` |
| `hijack-deadlines-off` | raw streams without deadlines or peer checks | unit `a_silent_hijacked_stream_*` and two more |
| `list-fail-open` | an unfilterable list is passed on | unit `a_list_that_cannot_be_filtered_*` |
| `scope-default-network-off` | the scope's default network is Docker's shared bridge | EP-10 c1 |
| `restart-update-allowed`, `network-mtu-allowed`, `private-propagation-allowed`, `fullid-name-allowed`, `outputs-open`, `network-name-forwarded` | the schema or reference tightening it names | sec01.sh / limits.sh and the unit table |

`e3/breaks.sh` runs the end-to-end rows and requires each to trip its own
witness and nothing else; `e3/witness.sh` runs the in-process rows
(`E3_TEST_BREAKS`, read only by the unit-test build) and requires the same.

## Known limits

- BuildKit gRPC (`/grpc`, `/session`) is relayed, not parsed (DECISIONS 21):
  cache mount IDs are not scoped (same-project cross-workspace cache sharing is
  accepted), and a remote context or frontend requested over gRPC is not seen
  here; the VM's policy and firewall govern it. Entitlements are denied by the
  engine's `daemon.json`.
- **A `/grpc` push is a residual** (DECISIONS 21): a BuildKit solve may export to
  a registry (`type=image,push=true`) inside the opaque gRPC stream, where the
  `outputs` query check does not see it. Refusing it would need parsing the
  h2c/gRPC `Control.Solve` request, which D21 moves out of E3; the egress is
  limited to what the engine's network policy lets the project's VM reach.
- The agent's argv carries break modes only in `test-breaks` builds (DECISIONS 22).
- Images are shared by every scope of a project.
- Network and volume names are global in the engine; a name used by another
  scope is refused.
