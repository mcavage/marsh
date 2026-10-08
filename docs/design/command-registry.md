# Command registry

Native Kit v3 deliberately does not define the shell word a user types. marsh
adds only that mapping:

```json
{
  "claude": "kits/marsh-claude",
  "codex": "kits/marsh-codex",
  "pi": "kits/marsh-pi",
  "shell": "kits/marsh-shell"
}
```

Release packaging installs `libexec/marsh/commands.json`. An optional
host-control `commands.json` overlays packaged entries by command name.
`packaging/commands.json` names local authoring sources. For a releasable
candidate, `make kit-publish KIT_REPOSITORY_PREFIX=docker.io/YOU`
builds and pushes each native v3 Kit through Buildx, then writes
`target/kit-release/commands.json` with immutable OCI manifest references.
Install that file with `make install KIT_COMMANDS=target/kit-release/commands.json`.
Both stock SBX and the worker's private Docker Engine then pull the same
published image. Stock SBX must allow the registry in `kit.allowedSources`.

The loader and live `kit install` use the same declaration rules in
`marsh-contracts::command_registry`. These rules are declared in
`crates/marsh-contracts/src/command_registry_rules.json` and embedded by Rust;
this is a small validation rule set, not another Kit manifest. A registry
has at most 251 commands, and each registry file is at most 1 MiB. Names are 1–128 ASCII bytes,
containing letters, digits, `_` or `-`, and cannot start with `-`. Reserved names
are `marsh`, `fanout`, `collect`, `acp`, `mcp`, `ps`, and `top`.
Public MCP tool/publication names are a different semantic type; their dot
syntax does not make dots valid Kit command names.

The loader accepts a JSON object whose keys obey those rules. Each value
is either a native v3 source directory or an immutable
`repository@sha256:<64 lowercase hex>` OCI reference. Relative directories are
resolved from the registry file, so packaged `kits/marsh-claude` names the
installed source while the same value in host-control `commands.json` resolves relative to that
host-control directory. Use an absolute path to a Kit under `$MARSH_HOME/kits/`. Directories are canonicalized and must exist.
Duplicate keys (including escaped spellings of the same key), path-like or
empty command names, mutable image tags, non-string values, and malformed
documents are rejected. Registry files must be regular files, not symlinks or
FIFOs. Both declarations and their combined size are validated before any Kit
resolution or stock call. A host-control entry explicitly overrides a packaged
entry of the same name; an empty `{}` retains packaged commands.

Keep every registered Kit source outside the project and selected guest home.
The daemon protects these trusted sources from guest writes and rejects a
project/home mount that contains a source directory or is inside one. To edit
a Kit in the project, copy or install its candidate source into a separate
host-owned directory, then map that snapshot. When developing the packaged
Kits in this repository, `make install` copies them outside the checkout; see
[the self-development workflow](self-development.md).

The referenced native workload is the sole authority for the image config,
entrypoint, command, static environment, user, workdir, capabilities, network
policy, and services. marsh supplies only invocation arguments plus the
host/session bindings required by its product contract: nonroot Mac UID/GID,
natural working directory and home path, selected home backing, terminal
mode, and exact project/home grants. It does not define another image, argv,
policy, resource, capability, mixin, or service schema.

Directories under `kits/` are native v3 source for stock Docker tooling. The
local-source runtime path currently cannot prove that its independent Buildx
build and stock SBX's assembled workload are the same manifest. Use published
digests for packaged acceptance and release; local-source mappings remain an
authoring path without a qualification claim.

For local v3 source, stock SBX derives Kit identity from the source directory
basename. Packaged sources therefore use the `marsh-*` namespace so they do
not collide with stock built-in agent identities. This identity is independent
of the command registry key: users still invoke `claude`, `codex`, `pi`, and
`shell`.

The command is registered in Brush through a generic process shim. The shell
also prepends a private, session-lived directory of command links to exported
`PATH`. Descendants such as `/bin/sh`, scripts, `make`, and agent subprocesses
that resolve a registered name through `PATH` therefore return to the same
daemon and create a fresh container. Brush functions and builtins retain their
normal precedence, and explicit executable paths bypass name resolution.
Every foreground, pipeline, background, or concurrent invocation returns to
the shared daemon and creates a new container. The shell command name has no
hard-coded provider behavior. The generic `shell` workload uses `/bin/sh` as
its native image entrypoint. A bare noninteractive invocation reads stdin as a
script, `shell -c ...` passes the remaining argv directly to `/bin/sh`, and a
bare PTY invocation is an interactive shell. The normal worker lifecycle gives
each call a fresh container while reusing the exact shell Kit VM.

To add a published alternate Kit while the daemon is running, use
`marsh kit install NAME --from repository@sha256:DIGEST`. The daemon checks
the immutable reference through stock SBX, prepares its worker, and then
registers the command in its current scope. It writes the pinned reference
to that scope's private control registry for the next daemon start. Inside an
attached project shell, the new name becomes available on `PATH` immediately.
Failed preparation leaves the registry unchanged. Installation rejects an
existing name rather than treating it as an override. It validates the existing
control registry before any stock preparation or file write; malformed or
duplicate-key configuration must be fixed, not silently rewritten. Concurrent
installs are serialized, and a host edit detected during preparation is not
overwritten.

After changing a registry entry, local Kit source, or stock-SBX service secret,
the host can discard idle cached workers with
`marsh workers reset KIT[,KIT...]` or `marsh workers reset all`, then recreate
them with the matching `marsh --load` selection. Reset resolves names through
this registry, acts only on Kit VMs in the daemon's ownership map with their
recorded UUID, refuses any selected worker with an active reservation or
container, and removes only those Kit worker VMs. A quarantined Kit VM is
retired the same way; the next use creates a new VM with a new random name. It does not remove the project shell VM, selected home, receipts,
authentication state, or unrelated Docker Sandbox VMs. Guest relay tokens are
not authorized for this host lifecycle operation.

## ACP adapter configuration

`agents.json` has deliberately different overlay semantics: a host-control file
**replaces** the complete packaged adapter set. An explicit `[]` disables all
ACP adapters; omitting the file selects packaged defaults. A nonempty replacement
must list every adapter the user wants to keep. No implicit merge resurrects a
packaged adapter after an explicit disable. Duplicate adapter names or fields,
invalid declarations, missing referenced commands, or mismatched Kit generations
are fatal startup configuration errors, not a silent fallback to disabled ACP.
The startup diagnostic names the configuration paths without echoing contents.
These checks happen before stock version/preparation calls. The same regular-file
and 1 MiB input bounds apply.

Each declaration names a registered `command` and may add a fixed
`arguments` tail (at most 8 printable values of at most 128 bytes) that the
daemon appends to that Kit's own entrypoint when the session starts, exactly
like typed arguments. The packaged agents use it to select ACP mode in the
one-per-agent Kits:

```json
{"schema_version":1,"name":"claude-session","protocol":"acp_v1","command":"claude","arguments":["--acp"]}
```

The tail cannot change the Kit's image, entrypoint, mounts, network, or
credentials.

The host-control directory is scoped by the SHA256 of the canonical
`MARSH_HOME` path under `~/Library/Application Support/marsh/control/`.
`MARSH_CONTROL_HOME` selects another private control root with the same
per-scope suffix. This directory is never mounted as the guest home.
