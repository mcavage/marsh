# Security model

A marsh job runs an agent or tool in a fresh container inside a Docker Sandbox
VM. It can change or delete the project files it mounts and read credentials
saved in the guest home. A registered command in a split mounts a private
project copy instead. Ordinary jobs have no host-command channel; MCP tools
and development shells have the explicit grants described below.

| Can a job ... | Answer | What enforces it |
|---|---|---|
| Run a command directly on your Mac | Not through ordinary job execution | The VM boundary; jobs receive no host-command relay or Docker, containerd, or `sbx` control socket |
| Read your real home directory | Not implicitly | Only the project, guest home, and explicitly configured extra directories are mounted |
| Read proxy-managed provider keys | No | The `sbx` proxy holds them; the job sees placeholders |
| Change or delete project files | Yes | The project is mounted read/write |
| Read other jobs' logins and history | Yes, when they use the same guest home | The persistent guest home is shared within a scope |
| Start other registered commands | Yes, within limits | The job capability socket; `MARSH_SPAWN` narrows it |
| Reach the network | As its Kit's policy allows | `sbx` enforces the policy; marsh does not inspect traffic |

marsh has not been independently reviewed or audited. To report a
vulnerability, see [SECURITY.md](../SECURITY.md).

## Keeping your files safe

Direct agent calls can change or delete the project. Keep your work in Git.
To inspect an agent's changes before applying them to your working tree, run it
in a [`split`](split.md): that agent mounts a private copy of the project.
Your own shell commands in a branch are not confined to that copy
([split branches](#split-branches)).

## What marsh trusts

- Your Mac account. The marsh daemon runs as you, keeps its files readable
  only by your account, and can do only what your account can do.
- `sbx`. Docker Sandboxes creates the VMs, enforces each VM's network policy,
  and holds proxy-managed provider credentials. marsh uses its public command
  line and ships no modified copy. Docker Sandboxes supplies the VM boundary.
- The Kits you register. A Kit's image and descriptor decide what runs and what
  network access it gets. marsh does not verify that a registered Kit does what
  it claims.

## Boundaries

### Mac

The guest does not mount your real home as its home directory. By default,
it shares two directories with the Mac:

- the project you started in, read/write, at its Mac path
- the guest home (`$MARSH_HOME/home`, normally `~/.marsh/home`)

`--ephemeral-home` uses a private disposable home and separate VMs for that
session. It does not make the project read-only.

MCP integrations and `--dev` can add access beyond these mounts, as described
[below](#what-is-not-isolated).

Native `sbxenv.yaml`/`sbxenvs/NAME.yaml` additions can explicitly grant other
directories and MCP sources ([home setup](guides/set-up-your-home.md)). Extras
use normal source admission, appear in receipts, propagate only through a
parent's recorded view, and are omitted from split Kit jobs. Environment files
and local MCP executable paths are protected host inputs. Local MCP servers
execute on the Mac with your privileges; their code and dependencies must be
trusted and kept outside writable guest mounts. Connections are per Kit VM,
not per project, and do not imply permission to send messages or mutate data.

The host control directory, the install, and Kit sources are never mounted.
marsh refuses to start in a directory that contains them.

### Shell VM

Your shell runs here as your user, with passwordless `sudo` and a private
Docker Engine. That is root inside this VM only. By default, the VM has no Mac
Docker socket or `sbx` control. Root here can still read the project and the
guest home.

All projects in one scope share one shell VM, one user, and one guest home.
They can read each other's files. For work that must stay private from other
work, start marsh with a separate home: `MARSH_HOME=~/.marsh-work marsh`.

### Kit VMs and job containers

Each registered command runs as a nonroot user in a fresh container, inside a
VM for its Kit. A job sees:

- the project and the guest home
- its Kit's network policy
- placeholders for proxy-managed credentials; the provider keys stay with the
  `sbx` proxy. Saved agent logins live in the shared guest home.

A job has no Docker, containerd, or `sbx` socket. Its one channel to the daemon
is a socket that lets it:

- start registered commands and splits, within the
  [limits](agents.md#limits-and-messages) and `MARSH_SPAWN`
- list its own jobs

Containers for the same Kit share a VM kernel, so a kernel escape can expose
other jobs in that VM. Different Kits run in separate VMs.

### Split branches

An argv branch (`::: LABEL CMD ...`) runs one registered command. Its project
mount is only its fork of the project, so it cannot write your real files or
`.git`.

A shell branch (`-b`, or `split { }`) runs your own shell code in the shell VM.
That code starts in its fork but can write anything you can write there. Only
the fork's changes are captured. A registered command the branch starts, such
as `claude`, runs in a container that mounts only the fork, like an argv
branch.

Agents in split branches still share the guest home read/write. A private
project copy does not isolate their settings, login files, or history.

## What is not isolated

The project
: A direct job can read, change, or delete project files. Registered agents in
  a split mount their private project copies instead.

The guest home
: Jobs using the same guest home can read and write one another's settings,
  history, and saved logins, even across Kits. Provider credentials bound with
  `sbx secret` stay behind the proxy. An agent's own login flow can instead save
  credentials here as ordinary files, readable by other jobs.

Agents' own sandboxes
: The packaged Kits turn off Claude Code's and Codex's own sandboxes and
  permission prompts because those sandboxes cannot create the namespaces
  they need inside these containers. The Kits run Claude Code with
  `--dangerously-skip-permissions` and Codex with
  `--dangerously-bypass-approvals-and-sandbox`. Pi has no OS-level sandbox. The
  job's container and VM enforce access to files and the network.

Network
: Docker Sandboxes enforces each Kit's network policy. The agent Kits can reach
  their providers and the hosts their policy allows. marsh does not inspect
  traffic.

Agents running agents
: A job can start other registered commands. Each gets its own Kit's network
  and credentials. Narrow this with `MARSH_SPAWN=codex,pi` (a comma-separated
  list, or `none`) or `marsh run --spawn a,b NAME`. Nothing inside a job can widen it.

MCP and ACP publications
: Every client that loads a published pipeline can run it with your project's
  shell privileges. A published session gives its callers control of that
  agent. A server loaded into a Kit VM is available to every job there.
  Publishing registers the tool with the host broker, and loading makes it
  available to clients in the target VM. Jobs get no general host-command relay,
  but can use whatever operations a loaded MCP server offers, including host
  access if that server provides it.
  `marsh mcp install` gives a client control of the project's marsh shell and
  managed VM lifecycle. The optional `--allow-full-sbx-control` server flag
  also permits fixed repository test gates on the Mac, which execute the
  checkout's Makefile. See [MCP](mcp.md) and [ACP](acp.md).

`marsh --dev`
: The development shell's `sbx` is a broker on the Mac. It is limited to VMs
  with a random prefix and a few directories, but it is real host authority.
  Kit jobs do not receive this broker relay. Use it only for developing marsh.

## What marsh records

For each job marsh records:

- command name and IDs
- mount targets
- exit status and cleanup result
- timing

The results journal does not capture terminal streams, command output, or ACP
conversation content. It does store command arguments and paths, including a
prompt passed as an argument. These appear in `marsh results` and `marsh jobs`;
do not put secrets in them. Agents can also keep their own logs in the guest
home.

## Known limitations

- marsh opens project and home directories without following symlinks and
  checks their device and inode identities before and after mounting. Because
  `sbx mount` reopens a pathname rather than using marsh's directory descriptor,
  a process on the Mac that can replace that directory could swap it during
  the mount. marsh accepts this race under its trusted-host assumption.
- When marsh cannot confirm that a container was removed, it does not retry the
  job. It marks the job `cleanup uncertain` and quarantines the VM until you
  run `marsh workers reset KIT`.
- Binaries are not signed or notarized.

Next: [Split and join](split.md), [Troubleshooting](troubleshooting.md).
