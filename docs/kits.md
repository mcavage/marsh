# Commands and Kits

A Kit turns a container image into a shell command. Install one and type its
name. Your project is mounted and output comes back to your terminal, but the
program runs in a fresh container in a microVM, not on your Mac.

```sh
marsh kit install mytool --from registry.example.com/me/mytool-kit@sha256:DIGEST
mytool --help
```

The daemon checks the image through `sbx`, prepares a VM, and registers
`mytool`. Open shells see it at once, and it is saved for the next start. The
first `mytool` call prints `[starting mytool worker VM…]` if the VM is not
already running.

`claude`, `codex`, `pi`, and `shell` are registered the same way. A *registered
command* is a shell name that runs in its own container. A *Kit* is what it
runs: a Docker Sandboxes Kit (version 3), which is an image plus a small
descriptor naming its network policy, credentials, and entrypoint. marsh maps
names to Kits and defines no Kit format of its own.

## The default commands

| Command | Kit | What it is |
|---|---|---|
| `claude` | `marsh-claude` | Claude Code, and its ACP adapter |
| `codex` | `marsh-codex` | OpenAI Codex CLI, and its ACP adapter |
| `pi` | `marsh-pi` | The Pi coding agent, and its ACP adapter |
| `shell` | `marsh-shell` | A plain `/bin/sh`, with no credentials |

Releases pin the images by digest (`docker.io/mcavage/marsh-*@sha256:...`).
The list is in the installed `libexec/marsh/commands.json`.

The first call to a Kit boots its VM and prints `[starting NAME worker VM…]`.
To boot VMs before the prompt, start marsh with `marsh --load KIT,...` or
`marsh --load all`.

### Signing in

marsh has no login of its own. Docker Sandboxes (`sbx`) holds the credentials
on your Mac. Run `sbx login` once, then give `sbx` a credential for each
provider:

```sh
sbx secret set anthropic     # claude, and pi
sbx secret set openai        # codex
```

`sbx secret import` takes keys from your environment variables instead. The
Claude and Codex Kits also accept an OAuth login that `sbx` holds. The `sbx`
proxy adds the credential to the agent's requests, and the container sees a
placeholder, never the key.

If nothing is bound, an agent can still sign in from inside its container
(`codex login`, or Claude's `/login`). That login is stored in the guest home
(`~/.marsh/home` on the Mac) as ordinary files, and every job in the scope can
read it. Prefer `sbx secret`.

The Pi Kit declares an Anthropic API key only. `shell` has no credentials.

## How a call runs

Running `codex exec 'hello'` does the following:

1. The shell asks the marsh daemon on your Mac to start a job.
2. The daemon finds a warm VM for the `codex` Kit, or creates one.
3. Inside that VM, a fresh container starts from the Kit image. It runs as a
   nonroot user with your UID.
4. Your project is mounted at its Mac path. Your guest home is mounted as
   `$HOME`.
5. stdin, stdout, stderr, signals, and the exit status pass through as for a
   local program. A child job started by another job gets no terminal
   ([Agents running agents](agents.md#limits-and-messages)).
6. When the program exits, the container is deleted and the deletion is
   checked. The VM stays warm for the next call.

Each call gets its own container, including concurrent and background calls.
Up to 8 jobs run at once per daemon.

VM names are random (`marsh-k-xxxxxxxx`) and recorded before creation. marsh
only stops, removes, or runs commands in VMs on that record.

The container can read and write your project, and an agent can delete files
there. The guest home is shared by every job in the scope. The packaged agent
Kits turn off each agent's own sandbox and permission prompts, because the
container is the sandbox ([Agents running agents](agents.md)). To keep agents
off your files, use [`split`](split.md).

### Network and credentials

Credentials go through the Docker Sandboxes proxy; see
[Signing in](#signing-in). Each Kit VM has the network policy its Kit declares. The `codex` Kit, for
example, allows OpenAI hosts, GitHub, npm, and the Ubuntu package mirrors, and
nothing else. The list is in the Kit's descriptor
(`kits/marsh-codex/codex.yaml`).

### Limits per job

| Limit | Default |
|---|---|
| CPUs | 4 |
| Memory | 8 GiB |
| Processes | 4,096 |
| Writable layer | 10 GiB |
| Output | 256 MiB |
| Wall time | 24 hours |

To change these, see [Configuration](configuration.md#job-limits).

## Agents inside jobs

Inside a job, every registered name is on `PATH`. An agent can start another
agent as a child job. See [Agents running agents](agents.md).

## Adding a command

`marsh kit install NAME --from REPOSITORY@sha256:DIGEST` registers a published
Kit in the running daemon, as shown at the top of this page. The reference
must be pinned by digest. A name that already exists, including a packaged
one, is refused. To replace a name, edit `commands.json` below.

### Changing or replacing a command

Edit `commands.json` in the scope's host control directory. `marsh status`
prints that directory. Entries there override the packaged ones by name:

```json
{
  "mytool": "registry.example.com/me/mytool-kit@sha256:0123…",
  "local-tool": "/Users/you/kits/local-tool"
}
```

Each value is one of:

- an image reference pinned by digest (tags are refused);
- the absolute path of a Kit source directory.

After changing a mapping, reset the old Kit VM so the next call uses the new
Kit:

```sh
marsh workers reset mytool
```

A malformed `commands.json` stops the daemon from starting, and the error
names the file.

### Rules for names and sources

- Names are 1 to 128 letters, digits, `_`, or `-`, and do not start with `-`.
- These names are reserved: `marsh`, `fanout`, `collect`, `acp`, `mcp`, `ps`,
  `top`.
- A source directory must be outside your project and your guest home. Code
  in the shell then cannot rewrite the Kit.
- Running a Kit from source needs Docker Desktop with Buildx on the Mac.
- Docker Sandboxes must allow the image's registry in its Kit sources policy.

## Writing a Kit

A Kit is a directory with a descriptor and a Dockerfile. The `shell` Kit is a
small example:

```yaml
# syntax=docker/sandbox-kit:3
schemaVersion: "3"
displayName: Generic shell
kind: workload
dockerfile: ./shell.dockerfile
capabilities:
  - type: com.docker.sandbox/sbx@1
```

The image's `ENTRYPOINT` receives the command's arguments. It runs as a
nonroot user, in the caller's directory, with this environment:

- `HOME` set to the mounted guest home. The same path is in
  `MARSH_SELECTED_HOME`.
- `/run/marsh/bin` first on `PATH`. It holds the registered commands and an
  in-job `marsh`.
- `/run/marsh/context.md`, a short text telling an agent what it may start.

Keep tool state under `$HOME` so it persists between calls. Other jobs in the
scope can read it.

The packaged Kits are in
[`kits/`](https://github.com/mcavage/marsh/tree/main/kits). They show how the
agent Kits pass `context.md` to their agent.

### Publishing a Kit

Build and push the Kit with Docker Buildx to get a digest reference. In the
repository, `make kit-publish` does this for the packaged Kits
([Kit publication](design/kit-publication.md)).

To share a Kit, post it in
[Show and tell](https://github.com/mcavage/marsh/discussions).

## Removing VMs

```sh
marsh workers reset codex       # one Kit's idle VM
marsh workers reset all         # every idle Kit VM
```

The next call creates a new VM. Use this after changing a mapping, or to clear
a quarantined VM
([Troubleshooting](troubleshooting.md#cleanup-uncertain-and-quarantine)).

Next: [Troubleshooting](troubleshooting.md), [Configuration](configuration.md).
