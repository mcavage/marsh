# marsh architecture

This describes the current implementation. The user-visible contract is
`plan/local-product-contract.md`; cross-component state is specified in
`model/README.md`.

## Topology

```text
macOS host
  marsh client(s) ──► marshd (one per MARSH_HOME, same user)
                        │ registry · ownership map · receipts · grants
                        │ public `sbx` CLI only
          ┌─────────────┴──────────────┐
  shell VM (marsh-s-xxxxxxxx)    Kit VM (marsh-k-xxxxxxxx), one per Kit identity
    relay ◄─► Brush shell          marsh-worker (one retained `sbx exec -i`)
    project + home at                └─► fresh nonroot container per invocation
    natural Mac paths                    (VM-private Docker; no sockets passed in)
```

All shells for one user and selected `MARSH_HOME` share the daemon and its
warm VMs. Session, job, attempt, worker, VM, and container identities are
distinct and are never derived from each other.

## Data flow

1. The `marsh` client parses its own launch flags, captures the host identity
   and natural paths, and connects to the daemon over an owner-only Unix
   socket with a bearer token. It starts `marshd` if none is running.
2. The daemon admits the project and home sources through no-follow directory
   descriptors, ensures an owned shell VM, mounts the sources (kept while the
   VM is warm), and starts the guest relay over one long-lived `sbx exec`.
3. The guest runs Brush (vendored upstream plus `docs/upstream/brush/`
   patches), or, if the user chose it, the VM's own bash or zsh as a child of
   the guest marsh with the same session links first on `PATH`
   (`shells.md`, `marsh/src/shell_choice.rs`). Registered command names resolve through Brush's process shim and
   private links on `PATH`; each call goes back to the daemon over the relay.
   `marsh split` / `join` / `splits` (and the Brush sugar
   `split { ... } | join | CMD`, patch `0035`, a thin client) are daemon
   requests (`docs/workspaces.md`): `marsh-daemon/src/split.rs` journals the
   split, `split_fs.rs` snapshots the caller's tree by stat-checked clone into
   `<root>/.marsh/split/<id>/base`, clones one fork per branch with
   daemon-written Git metadata, and runs each branch through the daemon's own
   endpoint (a fresh session shell for `-b`, `Execute` for `:::`). A Kit job
   in a fork mounts only the fork plus read-only Git metadata
   (`split_confinement.rs`). Results are `git apply`-compatible patches in
   `out/`; the user's tree and `.git` are never written.
4. Every Kit job gets a read-only `/run/marsh` (`docs/processes.md`): the
   per-attempt capability socket `cap.sock` (worker `capability.rs`, backend
   `CapabilityBridges`), `job.json`, `context.md`, and `bin/` links
   (`#!/run/marsh/marsh --link=NAME`) to the static musl artifact
   (`marsh-local`, `marsh/src/job.rs`) bound at `/run/marsh/marsh`. A link
   runs the entry rule, keeps an image-provided name local, or sends
   `ProcessRun`; the daemon (`process.rs`) admits it under one lineage tree
   and runs it through its own `Execute` with the parent's mounts. The
   capability admits only `ProcessRun`, `ProcessShow`, the job's own
   `jobs`, `SplitCreate`, and `SplitJoin`.
5. The daemon resolves the Kit, ensures an owned Kit VM, and sends the attempt
   over that VM's retained worker transport. The worker creates one fresh
   container from the exact Kit image with nonroot identity and resource
   limits, attaches bytes or a PTY, forwards signals and resize, waits, deletes
   the container, and verifies its absence.
6. The daemon writes a structural receipt (identities, state, exit cause,
   cleanup, phase timings) to an owner-only journal under the host control
   directory. No prompt or output bytes are stored.

Transport loss with active attempts marks them `cleanup_uncertain` and
quarantines the VM; jobs are never replayed. `marsh workers reset KIT` retires
a quarantined Kit VM. `marsh reset` and `marsh stop` clean up the whole
scope once nothing is active.

## VM ownership

`marsh-sbx/src/vm_ownership.rs` keeps a per-daemon map from `(purpose, key)`
to a random VM name and its observed UUID, persisted in the host control
directory. Create writes the name as an intent before `sbx create`, then
records the UUID; the next inventory adopts a present intent and drops an
absent one. A VM is usable only if its name is present with the recorded UUID.
Anything else is foreign and is never stopped, removed, mounted, or exec'd.
Warm requests use a cached `sbx ls` view, refreshed on cold start, lifecycle
change, or failure. Host `sbx` is trusted (assumption A1 in the model).

## Mounts

Project and home mounts are made on first use and retained while the VM is
warm. Concurrent attempts share a mount by reference. An idle mount is
replaced only if its source identity changed, and all mounts are released when
the VM is retired. Source identity is checked before and after `sbx mount`; the
pathname reopen inside `sbx mount` is an accepted upstream limitation.

## Crates

| Crate | Owns |
|---|---|
| `marsh` | Host CLI (`marsh`), guest Brush wiring and registered-command dispatch, the static job artifact (`marsh-local`: links and the in-job CLI in a job; Brush elsewhere), `marsh fanout`/`collect`, human output |
| `marsh-daemon` | Authenticated daemon endpoint, sessions, admission and the process lineage tree, receipt journal, guest relay, ACP session control, MCP publication, dev broker (`marsh --dev` grants, `DevSbx` relay calls) |
| `marsh-backend` | `marshd` binary; implements the daemon backend: prepares Kits and shell VMs, connects workers, shell attachment teardown |
| `marsh-sbx` | The only boundary to stock `sbx`: version probe, create/inspect/mount/exec, ownership map, inventory cache, source admission, ephemeral home slots |
| `marsh-worker` | Guest worker binary: attempt-keyed multiplexed frames, fresh-container supervision, output drain, cleanup |
| `marsh-runtime` | Attached process ownership, cancellation, the Docker CLI job runtime, `marsh-byte-exec` |
| `marsh-contracts` | Validated job, image, mount, limit, and registry values shared across crates |
| `marsh-acp` | ACP v1 client: framing, sessions, prompts, updates, permissions, phased cancellation |
| `marsh-mcp` | `marsh-mcp` binary: host MCP server, fixed pipeline exports, published ACP tools |
| `marsh-host-identity` | macOS descriptor-bound file identity used by source admission |

`marsh-backend` depends on the daemon interface, `marsh-sbx`, and the worker
protocol; the daemon does not depend on the backend. Brush imports no marsh
crate. Use `cargo tree --workspace --depth 1` for the current graph. Add a
module to the responsible crate before adding a crate or service.

## Trust boundaries

- **Host `sbx`** is trusted and used only through its public CLI.
- **Daemon** is same-user, owner-only, and scoped to one canonical
  `MARSH_HOME`. Its control directory is outside every guest mount.
- **Shell VM**: the shell user has passwordless `sudo` and the VM's private
  Docker, but no host Docker or SBX control. The relay grants only narrow,
  explicit operations (MCP publish/load, ACP publication, dev broker scope).
  Projects sharing a selected home share this VM.
- **Kit job container**: nonroot, resource-limited, fresh per call, with only
  the project and home grants (a child job: its parent's mounts). It
  receives no Docker, containerd, or SBX socket and no raw credentials; its
  only daemon channel is the scoped job capability socket. Stock SBX applies the Kit's network
  policy and credential proxy at the VM boundary. Containers in one Kit VM
  share a kernel.
- **MCP Gateway**: servers loaded into a Kit VM are available to every job in
  it. Do not load a host-control server such as `marsh-dev` into a Kit VM
  unless that authority is intended.

## Extending

- New command: add a registry mapping to a native Kit v3 source or immutable
  reference (`command-registry.md`). marsh has no Kit schema of its own.
- Shell behavior: change Brush through a small patch in
  `docs/upstream/brush/` and add a Bash differential test.
- Ownership, worker lifecycle, mounts, cancellation, or grants: update
  `docs/model/` (invariant, negative control, code-location table) and add a
  real-caller test at the affected boundary.
