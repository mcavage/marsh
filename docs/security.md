# Security model

A job is an agent or tool run by marsh in a fresh container inside a Docker
Sandbox VM. A direct job can change your project files and the shared guest
home; a registered agent in a split mounts its private project copy instead.
Ordinary job commands execute in Linux, not on your Mac. Proxy-managed provider
keys stay behind the Docker Sandboxes proxy, but saved guest logins are readable
files. Explicit integration and development grants are described below.

| Can a job ... | Answer | What enforces it |
|---|---|---|
| Run a command directly on your Mac | Not through ordinary job execution | Docker Sandboxes VM boundary; no Docker, containerd, or `sbx` socket is mounted; explicit grants can add authority |
| Read your real home directory | No | Only the project and the guest home are mounted |
| Read proxy-managed provider keys | No | The `sbx` proxy holds them; the job sees placeholders |
| Change or delete project files | Yes | The project is mounted read/write |
| Read other jobs' logins and history | Yes | All jobs share the guest home |
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
  and holds your credentials behind a proxy. marsh uses its public command line
  and ships no modified copy. The VM boundary is `sbx`'s; marsh adds none.
- The Kits you register. A Kit's image and descriptor decide what runs and what
  network access it gets. marsh does not verify that a registered Kit does what
  it claims.

## Boundaries

### Mac

By default, guest commands execute inside Linux VMs, without a host-command
channel. MCP publications grant access to the declared pipeline or agent
session, not unrestricted host execution. The `--dev` broker grants bounded
host authority to the development shell; Kit jobs still receive no broker
relay ([below](#what-is-not-isolated)). The real home directory is not mounted
as the guest home. Two directories are shared:

- the project you started in, read/write, at its Mac path
- the guest home (`~/.marsh/home`), the home directory every job sees

The host control directory, the install, and Kit sources are never mounted.
marsh refuses to start in a directory that contains them.

### Shell VM

Your shell runs here as your user, with passwordless `sudo` and a private
Docker Engine. That is root inside this VM only. The VM has no Mac Docker socket
and no `sbx` control. Root here can still read the project and the guest home.

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

Containers in one Kit VM share that VM's kernel. A kernel escape from one job
reaches the other jobs in the same Kit VM. The VM separates Kits from each
other. The container does not.

### Split branches

An argv branch (`::: LABEL CMD ...`) runs one registered command. Its project
mount is only its fork of the project, so it cannot write your real files or
`.git`.

A shell branch (`-b`, or `split { }`) runs your own shell code in the shell VM.
That code starts in its fork but can write anything you can write there. Only
the fork's changes are captured. A registered command the branch starts, such
as `claude`, runs in a container that mounts only the fork, like an argv
branch.

Every branch mounts the guest home read/write, because agents keep settings
and history there.

## What is not isolated

The project
: A direct job can read and write it. An agent that deletes files deletes them.
  Registered agents in a split mount their private project copies instead.

The guest home
: Every job reads and writes it. Agents keep settings and history there, so an
  agent in one Kit can read another agent's settings. Credentials bound with
  `sbx secret` go through the `sbx` proxy and are not stored here. A login an
  agent makes from inside its container is stored here as ordinary files.

Agents' own sandboxes
: Claude Code and Codex sandboxes do not work inside a container, so the
  packaged Kits turn them off. They run Claude Code with
  `--dangerously-skip-permissions` and Codex with
  `--dangerously-bypass-approvals-and-sandbox`. Pi has no OS-level sandbox. The
  only enforcement is the container and its VM.

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
  privileges. Every client that loads a published session can steer that agent
  the same way. A server loaded into a Kit VM is available to every job in that
  VM. `marsh mcp install` gives a client full control of the project's marsh
  shell. See [MCP](mcp.md) and [ACP](acp.md).

`marsh --dev`
: The development shell's `sbx` is a broker on the Mac. It is limited to VMs
  with a random prefix and a few directories, but it is real host authority.
  Use it only for developing marsh.

## What marsh records

For each job marsh records:

- command name and IDs
- mount targets
- exit status and cleanup result
- timing

It does not record prompts, terminal content, or command output.

Command arguments and paths appear in `marsh results` and `marsh jobs`. Do not
put secrets in them.

## Known limitations

- `sbx mount` takes a path, so the path could be swapped between marsh's check
  and the mount. marsh checks the directory's identity before and after. A gap
  remains. Exploiting it needs a process that can replace the directory. marsh
  accepts it because `sbx` is trusted.
- When marsh cannot confirm that a container was removed, it does not retry the
  job. It marks the job `cleanup uncertain` and quarantines the VM until you
  run `marsh workers reset KIT`.
- Binaries are not signed or notarized.

Next: [Split and join](split.md), [Troubleshooting](troubleshooting.md).
