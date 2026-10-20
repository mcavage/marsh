# Owned cloud lifecycle: implementation and real-caller evidence

Status: cloud adapter implemented on this branch, **not public marsh cloud
dispatch or workspace return**. Uses the current Docker sign-in; there is no
account manager, credential-store parser, or namespace-marker resource.

`marsh-sbx::cloud::CloudSbx` separates cloud `sbx_*` IDs/inventory from local
UUIDs. It persists private ownership before creation, validates fresh ID/name
observations before effects, and permits only immutable registry-qualified
images, bounded billable shapes, and finite delete-on-timeout TTLs. Its initial
shell/worker path uses stock `create --image-ref` without an agent argument;
this path creates no native Kit bake/template cache entry. Native v2 Kit
preparation remains a separate integration task, not a v3 translation.

## Persistence and failure behavior

- One owner holds the private map lock. Writes use atomic replacement and
  file/directory sync. Unsafe storage or uncertain persistence blocks effects.
- Create intent precedes the stock call. An ambiguous/missing reply retains the
  intent; reconciliation adopts a visible pending resource for cleanup only.
  Missing pending resources are never forgotten or created again automatically.
- A successful ID reply is recorded before control/copy operations. Ready also
  requires a matching positive inventory observation and the expected image.
- Quarantine blocks new submissions. Reopening the map makes prior Ready VMs
  cleanup-only, never implicitly reattaches or replays workloads.
- Identity/health changes and uncertain submitted uploads quarantine resources.
  The retained transport owner is responsible for quarantining lost work;
  killing a local carrier does not prove remote completion.
- Removal is addressed by the recorded ID. The record is released only after
  a complete successful inventory proves absence **and that ID was previously
  positively observed before that absence query began**. A later concurrent
  sighting cannot retroactively validate an older miss. A newly created but still invisible ID remains held,
  even after a deletion request, until its outcome can be established.
- File reads are bounded byte streams through stock exec, not archive
  extraction into the user's checkout. This low-level adapter does not itself
  authorize a workspace path or apply a delta.

## Run the real caller

Requires an Apple Silicon Mac, stock SBX with inline cloud-image/TTL support,
Docker sign-in and a Docker Cloud subscription with sandbox access. This is an
explicit opt-in cloud test; it provisions at most two small sandboxes, invokes
no model provider, and retains maps on failure.

```sh
cargo build --locked -p marsh-sbx --example cloud_lifecycle
# Create an owner-only, canonical control directory outside guest mounts first.
target/debug/examples/cloud_lifecycle /absolute/path/to/sbx CONTROL_DIRECTORY \
  docker.io/docker/sandbox-templates:shell-docker@sha256:b2c007bbb5dcdb60a29f3019c7160ebe0c50c968e08854fbba6093834f8f6a1b
```

On 2026-10-16 the fresh real caller passed with stock
`v0.48.0-683-g7167374c8`. Its successful journey verified:

- Distinct private ownership maps and exclusion of a second writer.
- Exact binary stdin/stdout, separate stderr, and remote exit 17.
- File roundtrip and rejection of an oversized read.
- Refusal to execute in or retire the other client's resource, followed by a
  successful echo through that other client.
- Intentional carrier teardown, quarantine, and refusal of new work.
- Reopening a still-Ready map forced quarantine independently of the explicit
  quarantine case; cleanup then retired only recorded IDs.
- Actual fresh inventory absence before releasing both ownership records.

Final successful normal-journey IDs were `sbx_001m53dzv0rj5mc8sgyn7q0qm60` and
`sbx_001m53e00gh0kgf76f8g27r5av2`; both maps ended with empty entries. Stable
candidate/command/artifact evidence is retained under
`.pi-agent/deliver/cloud-lifecycle/checks/live-4/`. These are SDK-boundary E2E
results, not claims that the daemon, job tree, `cloud {}`, or write-back works.

For an interrupted qualification, use the same control directory and append
`--cleanup`. It reconciles existing records for retirement; it does not rerun
the qualification or forget unresolved invisible intents.

Appending `--storage-failure` selects a separate real-cloud fault journey. It
replaces only its own locked ownership-file path with a directory, causing a
real atomic-replacement failure. The live owner must refuse subsequent exec
and inventory submissions even after the file is restored; reopening must
quarantine the recovered resource, then retire it by ID with verified absence.
That journey passed for `sbx_001m53e0nty0893ra2qgph395s2`, with evidence under
`.pi-agent/deliver/cloud-lifecycle/checks/storage-failure/`. Its map also ended
empty. The caller restores its owned file on error and retains recovery data
if restoration cannot be completed.

## Model and remaining integration

[`CloudOwnership.tla`](../model/CloudOwnership.tla) extends the ownership model
with create submission/reply/visibility loss, cleanup-only recovery, immutable
ID effects, and asynchronous deletion. Its two positive configs and ten
negative controls pass their expected verdicts. The
[model mapping](../model/README.md#cloud-ownership-extension) records assumptions
and code locations; TLC is not proof of Rust or the provider.

Next: wire the owned adapter and qualified worker into daemon placement, add
snapshot/delta return and real user-facing pipeline callers, and qualify native
v2 Kit policy/credential preparation. The existing local adapter is unchanged
apart from sharing the same bounded control-capture implementation with an
atomic admission callback.
