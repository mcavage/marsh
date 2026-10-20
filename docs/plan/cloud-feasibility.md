# Cloud placement: live feasibility findings

Date: 2026-10-16. Status: **platform probe evidence, not shipped cloud support**.
The user approved billable probes. No model-provider calls, account-global
policy changes, or project/home uploads were made. Production Rust and Brush
are unchanged. The [placement proposal](../design/cloud-placement.md) remains
the contract; these experiments do not weaken its boundaries.

## Outcome

Kit v2 boot, binary streaming, trusted bootstrap, and fresh nonroot container
execution are viable on the tested stock CLI. Basic live filesystem sharing
between two incompatible native policy domains worked over retained
`sbx exec` streams, without publishing a port or giving jobs mount privileges.

Two issues need production work, not a transparent local/cloud flag:

1. The existing worker rejects cloud's proxy endpoint under its local-only
   validation. Cloud needs an explicit trusted runtime profile.
2. The zero-cache SSHFS prototype rejects shared `mmap`. It proves a transport/
   topology, not an acceptable general-purpose workspace client yet.

## Inputs and evidence boundaries

- Stock CLI: `v0.48.0-683-g7167374c8`, commit
  `7167374c89559fadc831878d2c2ef8426df417f9`.
- Cloud descriptors: `schemaVersion: "2"`, `kind: sandbox`, no credentials;
  ARM64 image
  `docker.io/docker/sandbox-templates:shell-docker@sha256:b2c007bbb5dcdb60a29f3019c7160ebe0c50c968e08854fbba6093834f8f6a1b`.
  Successful creation proves v2 acceptance, **not that v3 is unsupported**.
- Nested fixture: BusyBox ARM64 base
  `sha256:d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c`,
  nonroot image user and `/bin/sh` entrypoint; no agent/inference calls.
- Installed static ARM64 dev artifacts, not a fresh build from this branch.
  Worker SHA256:
  `ba64577b2bbd3bec72833a58f491650577367f405561066b66c070614b548c08`;
  byte-exec:
  `364d59f3481cef0be7d00c6c7529e64f5c9021ac4f34c8d4527823e0ec8fb1b9`;
  marsh-local:
  `e2a0ef598b9db0f8650462955476b9b3ee303099239bc7a6987f8749ac086eaa`.
  Uploaded bytes were hashed remotely too. This is not candidate-bound proof
  for future Rust changes.
- Disposable stdio server: `github.com/pkg/sftp` v1.13.11, static ARM64 wrapper
  chrooted to a harmless export, then dropped to uid/gid 1000 without
  supplementary groups. Binary SHA256:
  `cfc439ad6ab117b0f39dca056f641452a4c7980cf1cf2fba54cd2c6808e92b19`.
  Client: SSHFS 3.7.3 / libfuse 3.17.2, supplied as isolated trusted helpers,
  not installed into jobs or added as product dependencies.

Scratch descriptors/helpers, command/stdout/stderr/result logs, artifact
identities, and ownership maps remain locally under
`.pi-agent/deliver/cloud-feasibility/` (git-ignored, owner-only evidence).
They are **not** a new acceptance framework or implementation to ship. Failed
experiments remain recorded; only the observations below are claimed.

## Observations

### V2 lifecycle and bootstrap

Credential-free v2 Kits created running ARM64 sandboxes with 2 CPUs / 4096 MiB,
30-minute TTL, and delete-on-timeout. Exec ran as `agent` uid/gid 1000, with
noninteractive sudo. The private Docker Engine reported 29.8.1 and cgroup v2.
The uploaded static marsh artifact evaluated a Brush command.

Stock creation also created temporary bake sandboxes and saved cloud templates.
The bake sandboxes disappeared afterward; templates remained and an identical
Kit launch reused its template. These are additional lifecycle resources.
Production ownership must journal preparation before invoking stock, persist
observed auxiliary IDs as they arrive, and distinguish reused templates from
ones the operation demonstrably created. Crash/ambiguous bake reconciliation is
not solved by this probe.

### Streams and file transfer

- Direct cloud exec: 32,780 input bytes, including all byte values, returned
  byte-for-byte on stdout, with separate exact stderr and exit 17.
- A one-byte marker arrived before stdin EOF; subsequent binary input returned
  exactly. This establishes streaming, not a performance benchmark.
- A worker binary copied to cloud and back through stock `sbx cp` retained its
  SHA256. This does not establish safe production manifest/delta application.

Later finding (2026-10-16, session-origin dispatch): continuously writing
stdin to `sbx --cloud exec -i` while the remote command did not read it made
later exec calls to that sandbox fail with `internal: upstream unavailable`.
Output has the converse limit: with stock `sbx --cloud exec -i` alone, 512 KiB
of output produced at once arrived, while 1 MiB or more failed with
`failed_precondition: requested output no longer retained` and delivered
nothing, even to a fast reader; the same total paced in 256 KiB bursts arrived
intact. The exec channel must not be treated as backpressured in either
direction; dispatch adds its own acknowledged windows
([cloud dispatch](cloud-dispatch.md)).

### Worker and proxy boundary

The unmodified installed worker failed its first create with
`invalid trusted SBX proxy environment variable HTTP_PROXY`. Cloud supplied
`http://192.0.2.1:8889`; local validation accepts only
`http://gateway.docker.internal:3128`. No container was observed after failure.
Its uncertain/quarantine report was honored by retiring the sandbox before
another worker attempt.

In a fresh sandbox, proxy/CA/MCP settings were cleared **only for a network-free
diagnostic worker**, not as a product fallback. That diagnostic:

- Ran two jobs with distinct real container IDs, each uid 1000, without runtime
  sockets at the tested Docker/containerd/SBX paths.
- Preserved empty/non-UTF-8 argv, binary streams, separate stderr, and exit 17.
  Project writes were visible in a subsequent fresh container.
- Observed admitted cgroup values: CPU `100000 100000`, memory `268435456`,
  PIDs `64`. Pressure/exhaustion behavior was not tested.
- Delivered an explicit INT frame; a fixture trap returned 130 with verified
  worker cleanup. Successful job absence was checked independently through
  Docker inspection/inventory.

This does **not** prove production credential/CA/MCP integration, daemon
admission, shell syntax, job capabilities, split, or write-back.

After killing the local exec carrier during an active job, 12 inspections still
reported the container running. A later inspection observed it absent, but no
terminal receipt arrived. The sandbox stayed quarantined and was removed with
verified absence. Local carrier exit is not remote cancellation or cleanup.

### Native policy and live remote sharing

Two v2 Kits declared opposite `permissions.network.allow` lists:

| Sandbox | Native mode | Allowed | Explicitly denied |
|---|---|---|---|
| Policy A | deny-all | `example.com` | `docs.docker.com` |
| Policy B | deny-all | `docs.docker.com` | `example.com` |

Root curl observed HTTP 200 for each allowed host and CONNECT 403 for each
denied host. No global policy was modified.

The kernel 9p-over-fd candidate failed: `9pnet: Could not find request
transport: fd`. A listed 9p filesystem does not prove that transport support.
The kernel was not modified.

The successful alternative used two SSHFS passive-mode clients, each connected
to a separate stdio SFTP session over the **same authoritative export in policy
A**. The Mac relayed bidirectional protocol bytes over retained public exec
channels. No SSH daemon, public port, service credential, or workload egress
exception was introduced. This was a remote filesystem, not live Mac sync.

Clients disabled directory/name/attribute/negative caching and used
`direct_io,sshfs_sync,no_readahead,allow_other,default_permissions,idmap=none`.
Trusted mounts were bound into fresh containers: uid/gid 1000, all capabilities
dropped, `no-new-privileges`, read-only image roots, no privileged mode. Jobs
received neither `/dev/fuse` nor runtime sockets; only project and read-only
public curl/runtime/CA tools were bound in.

With both containers still running:

- Negative lookup followed by the other client's creation became visible.
- Cross-client create, same-size overwrite, rename-over-existing, and deletion
  were visible on subsequent opens without exit/remount/reconciliation.
- A read-only bind read current bytes and refused file creation.
- HTTPS curl **inside each job**, with the native cloud proxy and certificate
  verification, succeeded for its own host and failed for the other's. Native
  policy inspection remained unchanged afterward.

An earlier BusyBox wget networking check failed on the allowed route with HTTP
500 and was not accepted as policy evidence. The successful check used curl
and the public CA bundle, without a TLS-verification bypass.

Private mmap copies worked; shared-read mmap failed with errno 19 (`ENODEV`).
This prototype is **not a transparent general-purpose filesystem**. Distributed
locking, executable mapping, durable flush, crash/loss, metadata completeness,
hostile RPCs, and representative Git/build/database tools remain gates. Do not
ship it unchanged or silently enable stale caching to recover mmap support.

## Cleanup

All four explicitly created sandbox IDs were independently absent after their
owned-ID removals:

- `sbx_001m52sykt41zgds9ww5rsbsfrg`
- `sbx_001m52t64c6etv5849adqf1f83w`
- `sbx_001m52tcqfmx33vfrsgjwftvfnc`
- `sbx_001m52tfvj0hjhzqmy7nf4zngw8`

Three bake sandboxes were absent after baking. These templates were inspected
by immutable ID, checked for remaining sandbox references, removed by ID, and
independently absent from a successful template inventory:

- `tmpl_001m52sy1s6wr3krnxb8n4pnw3h`
- `tmpl_001m52tc5bjaq3e1czn73cfsk7n`
- `tmpl_001m52tf93m5cs32wsr6e8mkz33`

Final cloud sandbox inventory was empty, as at the start. Two uniquely named
host probe-image tags were removed after matching recorded image IDs. Shared
build caches and unrelated resources were not pruned.

Inventory checks used the same signed-in CLI context throughout; no login or
context change was performed by the probes. Inspected public metadata did not
establish a stable account ID. The user subsequently chose ordinary current
stock sign-in behavior, not a separate account-switching feature or marker
protocol. Production ownership is ID-bound; failed/incomplete observations
remain uncertain rather than being converted to empty listings.

## Next implementation gates

1. Add a trusted placement-specific runtime profile for cloud proxy/CA handling.
   Keep local validation unchanged; no guest-selected URL or production proxy
   clearing. This primitive is now implemented and separately qualified in the
   [worker-profile slice](cloud-worker-profile.md); daemon wiring remains.
2. Account for v2 bake/template artifacts and ambiguous outcomes in ownership/
   recovery and the Ownership model extension.
3. Choose a workspace client with explicit mmap/locking/cache/loss semantics.
   The stdio topology works; direct-I/O SSHFS is not a final choice. Model pending
   filesystem RPCs/client generations before capture/teardown integration.
4. Build the real daemon/CLI transport and delta-return slices with candidate-
   bound callers. These probes do not implement marsh cloud syntax, lineage,
   nested scheduling, or automatic write-back.
