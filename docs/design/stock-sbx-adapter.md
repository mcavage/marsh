# Stock-SBX adapter

`marsh-sbx` is the only boundary to the installed Docker Sandboxes runtime. It
runs an absolute, startup-resolved stock `sbx` binary and uses its public
`create`, `ls`, `inspect`, `mount`, `umount`, `cp`, `exec`, `stop`, and `rm`
operations. It does not start another SBX service, ship a patched binary, or
run a registered command directly in a VM. Host `sbx` is trusted.

## Version and installation

Startup requires stock SBX v0.45.0 or newer (stable or coherent nightly),
checks the exact reported version and commit, and probes the local Kit v3
create surface before creating product state. Smoke currently runs against
v0.46.0. The version check does not prove that a build has the EROFS
writable-layer discard fix; acceptance checks container cleanup directly.

Homebrew owns installation of `sbx`. marsh resolves `MARSH_SBX`, otherwise
`sbx` on `PATH`, and canonicalizes it. A running daemon keeps the executable it
resolved at startup, and its handshake includes a digest of that executable, so
a client restarts an idle daemon after an SBX upgrade. MCP registrations keep
their stored path; rerun them after an upgrade.

## Ownership map and names

Every VM marsh creates has a random name: `marsh-k-` plus 8 base36 characters
for Kit VMs, `marsh-s-` for shell VMs. `vm_ownership.rs` keeps a persisted map
from `(purpose, key)` to name and observed UUID in the host control directory.

- A missing entry gets a fresh random name, persisted as an intent before
  `sbx create`. After create the UUID is recorded.
- The next inventory adopts an intent whose name is present and drops one,
  loaded at startup, whose name is absent.
- A VM is usable only if its name is present with the recorded UUID. A name
  with a different UUID, or not in the map, is foreign and refused.
- Stop and remove target only a recorded UUID that is currently present.
  Removal drops the entry, so the next use gets a new name.

Warm requests use a cached `sbx ls --json` view, refreshed on cold start, after
a lifecycle change, or after a failure. `docs/model/Ownership.tla` specifies
this protocol.

## Kit VMs and fresh containers

A registry entry names a native Kit v3 source directory or an immutable Kit OCI
reference; mutable tags are rejected. The Kit's OCI config and descriptor own
entrypoint, command, environment, user, workdir, capabilities, network policy,
and services. The adapter does not translate or add to them. Local source is
an authoring path; the release run uses published digests.

Each Kit VM runs one retained `sbx exec -i ... marsh-worker --serve`
transport, which also keeps stock SBX from stopping an idle VM. The worker
reports its generation at startup and answers liveness pings. Per attempt the
daemon admits the sources, takes a reference on the retained mounts, sends
bounded attempt-keyed frames (start, input, EOF, signal, resize, cancel), relays
output, and records the terminal report. The worker creates one container from
the exact Kit image with the host user's nonroot UID/GID, natural cwd and home,
exact grants, and resource ceilings; reports Docker's container ID; waits;
deletes the container; and verifies its absence.

Jobs keep the image's static environment plus validated `HOME`, `USER`,
`LOGNAME`, and `MARSH_SELECTED_HOME`. When stock SBX provides them, the adapter
also passes only the fixed proxy endpoint, bounded credential-mode sentinels, a
read-only CA bundle, and the sandbox-local MCP Gateway URL and sentinel name as
a pair. Gateway tools loaded into a Kit VM are visible to every job in it. Jobs
receive no raw credentials, worker environment, or Docker/SBX socket.

`--load` prepares the VM, worker, and job image before returning.

## Shell VM and relay

The shell VM uses the packaged DHI Debian ARM64 template. The adapter installs
the Linux `marsh` and relay binaries, creates the Mac user's UID/GID, and
mounts the launch directory at its identical path and the selected home backing
(`MARSH_HOME/home`, or an ephemeral slot) at `/Users/<user>`. The real Mac home
is never mounted. Attachment uses `sbx exec -it` or `sbx exec -i`. The relay
runs as the shell user with a session-scoped socket and token in a 0700
runtime directory; the token never appears in argv, the home, or a job
environment.

## Mounts

Sources are opened with `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC` at admission and
their identity (volume UUID and persistent file ID on macOS, via
`marsh-host-identity`) is checked before and after `sbx mount`. Natural host
paths are only mount targets, never authority. A mount is made on first use
and retained while the VM is warm; concurrent users share it by reference. An
idle mount is replaced only when its source identity changed, and mounts are
released when the VM is retired. Stock SBX accepts only pathname mount
sources, so the reopen inside `sbx mount` is an upstream limitation accepted
under the trusted-host assumption.

## Failure behavior

Unknown state is never treated as absence or success. The adapter does not
create a second daemon or VM pool, replay a job, or fall back to direct VM
execution. Memory, PID, output, writable-layer, and wall limits produce typed
outcomes. Transport loss during active work, uncertain container cleanup, or a
missing create-boundary event within the bounded wait produces a nonzero
`cleanup_uncertain` receipt and quarantines the VM. A quarantined Kit VM takes
no new jobs until `marsh workers reset KIT` retires it; the next use creates a
fresh VM under a new random name. `marsh reset` and `marsh stop` clean up
the whole scope once no shells or jobs are active. Do not delete the journal or
ownership map as a recovery shortcut.
