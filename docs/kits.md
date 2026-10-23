# Commands and Kits

A Kit packages a command's image, network policy, and credentials for Docker
Sandboxes. marsh registers it under a shell name and runs each invocation in
a fresh nonroot container. For example, in your [marsh shell](shell.md):

<!-- doc-test: shell stdout="Linux aarch64" -->
```sh
shell -c 'uname -sm'
```

`shell` is a packaged command. To add your own, use
`marsh kit install NAME REPOSITORY:TAG`, substituting its published
image. marsh uses the native Docker Sandboxes Kit v3 format. Commands update
themselves; see [Updates](#updates).

## The default commands

| Command | Kit | What it is |
|---|---|---|
| `claude` | `marsh-claude` | Claude Code, and its ACP adapter |
| `codex` | `marsh-codex` | OpenAI Codex CLI, and its ACP adapter |
| `pi` | `marsh-pi` | The Pi coding agent, and its ACP adapter |
| `shell` | `marsh-shell` | A plain `/bin/sh`, with no credentials |
| `pix` | `marsh-pix` | Upstream Pi with public skills, orchestrated by marsh |
| `hermes` | `marsh-hermes` | Hermes Agent, sharing the marsh home's skills folder |

Releases pin the first four images by digest
(`docker.io/mcavage/marsh-*@sha256:...`); the list is in the installed
`libexec/marsh/commands.json`. `pix` and `hermes` follow their `:stable` tag:
a release build adds each one shortly after the daemon first starts, which
needs the network. Until then the name is not there yet; a problem shows in
`marsh status`. If you already have your own command with one of these names,
marsh leaves it alone. `marsh kit uninstall pix` removes it for good, until you
run `marsh kit install pix` again.

A foreground call that boots a Kit VM prints `[starting NAME worker VM…]`.
To prepare VMs before the prompt, start marsh with `marsh --load KIT,...` or
`marsh --load all`.

### Signing in

marsh has no login of its own. Docker Sandboxes (`sbx`) holds the credentials
on your Mac. Run `sbx login` once, then give `sbx` a credential for each
provider:

| Host command | Used by |
|---|---|
| `sbx secret set anthropic` | `claude` and `pi` |
| `sbx secret set openai` | `codex` |

`sbx secret import` takes keys from your environment variables instead. The
Claude and Codex Kits also accept an OAuth login that `sbx` holds. The `sbx`
proxy adds the credential to the agent's requests, and the container sees a
placeholder.

If nothing is bound, an agent can still sign in from inside its container
(`codex login`, or Claude's `/login`). That login is stored in the guest home
(`~/.marsh/home` on the Mac) as ordinary files, and every job in the scope can
read it. Prefer `sbx secret`.

The Pi Kit declares an Anthropic API key only. `shell` has no credentials.

### Updates

Each Kit image pins its agent's version. Each time the marsh daemon starts,
and once an hour while it runs, marsh checks in the background whether a
command's tag names a newer image, so you get a new Claude Code, Codex, Pi,
Pix or Hermes without waiting for a marsh release. It never delays what you
are doing, and an offline check is reported only in `marsh status`.

- The packaged commands follow the image tag of your release channel
  (`:latest` for stable, `:nightly` for nightly) on a release build. A Kit
  you installed from a tag follows that tag.
- The new image is prepared first. A command whose Kit is running jobs waits
  and switches when they finish. A failure keeps the old image. An image
  that needs a newer marsh (its `org.marsh.kit.contract` label) is refused.
- After a switch, the next time you run that command at a terminal, or open a
  marsh shell, marsh prints one line on stderr, once:

  ```text
  marsh: claude updated (2.1.305 → 2.1.307)
  ```

  A rebuild with the same version switches without a notice.
- `marsh status` shows when the last check ran, any problem, commands waiting
  for their jobs, and recent updates (`kit_updates` in `marsh status --json`).
- To update right now, run `marsh stop` when nothing is running, then use
  marsh again.
- To turn updates off for a `MARSH_HOME`, run `marsh config auto-update off`,
  or start the daemon with `MARSH_AUTO_UPDATE=0`. That also stops `pix` and
  `hermes` from being added.

A command you installed by digest never changes, and neither does a packaged
command you pointed at a different image or a Kit folder in `commands.json`.
A packaged command still updates from any image of its own repository.
ACP agents for a packaged command, or for a command installed from a tag,
follow its updates. Upgrading marsh itself also brings the versions it ships;
see [Upgrade](install.md#upgrade).

Inside marsh, `claude --version`, `codex --version`, and `pi --version` show the
installed versions. Resetting a VM alone does not update its pinned image.

The packaged Claude Kit disables self-updates in CLI and ACP sessions;
`claude update` and `claude upgrade` exit 125.

## How a call runs

When you invoke a registered command:

1. The shell asks the marsh daemon on your Mac to start a job.
2. The daemon finds a warm VM for the command's Kit, or creates one.
3. Inside that VM, a fresh container starts from the Kit image. It runs as a
   nonroot user with your UID.
4. Your project is mounted read/write at its Mac path. In a split, the job
   instead mounts its fork at the fork's own host path. Your guest home is
   mounted read/write as `$HOME` in both cases.
5. stdin, stdout, stderr, signals, and the exit status pass through as for a
   local program. A child job started by another job gets no terminal
   ([Agents running agents](agents.md#limits-and-messages)).
6. When the program exits, the container is deleted and the deletion is
   checked. The VM stays warm for the next call.

Each call gets its own container, including concurrent and background calls.
Up to 8 jobs run at once per daemon.

An agent can edit or delete the project files its container mounts. The
packaged agent Kits disable their agents' own sandboxes and permission
prompts ([details](agents.md#agent-sandboxes-and-permission-prompts)). Use
[`split`](split.md) to give an agent a private project copy. The guest home
remains shared even in a split.

### Network and credentials

Credentials go through the Docker Sandboxes proxy; see
[Signing in](#signing-in). Each Kit VM has the network policy its Kit declares.
The `codex` Kit, for example, allows the listed OpenAI endpoints, GitHub
download hosts, npm, and package repositories. The exact hosts and ports are
in the Kit's descriptor
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

```sh
marsh kit install mytool registry.example.com/me/mytool-kit:stable
marsh kit uninstall mytool
```

`marsh kit install NAME REPOSITORY:TAG` asks `sbx` which image the tag
names now, prepares that image, and registers NAME. marsh records the digest,
never the tag, so the command runs exactly that image until an
[update](#updates) moves it to a newer one. You can also install a digest
(`REPOSITORY@sha256:DIGEST`); such a command never changes. The shell you run
it from sees the name immediately; other open shells see it once reopened. An
existing name, including a packaged one, is refused. `marsh kit install pix`
or `marsh kit install hermes`, with no reference, installs that Kit right
away.

### Removing a command

`marsh kit uninstall NAME` removes a command you installed, including `pix`
or `hermes`. For a packaged command that was updated, it returns to the
version your marsh shipped.

The tags installed commands follow are recorded in `kit-sources.json` next to
`commands.json`. Updates keep their record in `kit-updates.json` in the same
directory.

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

- an image reference pinned by digest (tags are refused here; `marsh kit install`
  resolves a tag to its digest and updates follow it);
- the absolute path of a Kit source directory.

Manual edits take effect when the daemon starts: finish active work, exit
open shells, run `marsh stop`, then launch marsh again. A worker reset alone
does not reload `commands.json`.

A malformed `commands.json` stops the daemon from starting, and the error
names the file.

### Rules for names and sources

- Names are 1 to 128 letters, digits, `_`, or `-`, and do not start with `-`.
- These names are reserved: `marsh`, `fanout`, `collect`, `acp`, `mcp`, `ps`,
  `top`.
- A source directory must be outside your project and guest home, so the
  normal project and home mounts do not expose it to guest code.
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
nonroot user in the caller's directory, with these mounts and variables:

- `HOME` set to the mounted guest home. The same path is in
  `MARSH_SELECTED_HOME`.
- `/run/marsh/bin` first on `PATH`. It holds the registered commands and an
  in-job `marsh`.
- `/run/marsh/context.md`, a short text telling an agent what it may start,
  followed by the Kit's own instructions (`agent-context`).

The same image also boots the outer Kit VM. That VM must provide Docker Engine
and the stock sandbox runtime plumbing for marsh's fresh job containers; a
standalone minimal Python or Alpine image is not enough. Derive from a suitable
packaged marsh workload/base rather than putting host Docker sockets into jobs.

Keep tool state under `$HOME` so it persists between calls. Other jobs in the
scope can read it.

The packaged Kits are in
[`kits/`](https://github.com/mcavage/marsh/tree/main/kits). They show how the
agent Kits pass `context.md` to their agent.

### Adding skills and instructions with mixins

To add to an existing agent without changing its image, publish a mixin with
your skills and instructions, then combine it with the agent in a
[kit set](https://docs.docker.com/ai/sandboxes/customize/author/kit-sets/):

```yaml
# syntax=docker/sandbox-kit:3
schemaVersion: "3"
kind: set
displayName: Team Pix
kits:
  - ref: docker.io/mcavage/marsh-pix
    digest: sha256:…
  - ref: registry.example.com/team/pix-skills
    digest: sha256:…
```

Publish the set with Docker Buildx and install it like any other Kit
(`marsh kit install team-pix registry.example.com/team/pix:stable`). Each job
receives what the set declares:

- **Instructions** (`agent-context`) are added to `/run/marsh/context.md`.
- **Skills** (`agent-skill@1`) are linked, read-only, into each skills
  folder the agent declares (`agent-skills@1`). A folder inside your home or
  project is skipped, so marsh never creates files there.

marsh reads these from the image's published descriptor the first time a
command runs, and keeps the result. A Kit run from a source directory has
none.

### Publishing a Kit

Build and push the Kit with Docker Buildx to get a digest reference. In the
repository, `make kit-publish` does this for the packaged Kits
([Kit publication](design/kit-publication.md)).

To share a Kit, post it in
[Show and tell](https://github.com/mcavage/marsh/discussions).

## Removing VMs

| Command | Removes |
|---|---|
| `marsh workers reset codex` | The idle Codex Kit VM |
| `marsh workers reset all` | All idle Kit VMs |

The next call creates a new VM. Use this to clear a quarantined VM
([Troubleshooting](troubleshooting.md#cleanup-uncertain-and-quarantine)).

Next: [Troubleshooting](troubleshooting.md), [Configuration](configuration.md).
