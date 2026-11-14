# Security model

A marsh job runs an agent or tool in a fresh container inside a Docker Sandbox
VM. It can change or delete the project files it mounts and read credentials
saved in the guest home. A registered command in a split mounts a private
project copy instead. Ordinary jobs have no host-command channel; MCP tools
and development shells have the explicit grants described below.

| Can a job ... | Answer | What enforces it |
|---|---|---|
| Run a command directly on your Mac | Not through ordinary job execution | The VM boundary; jobs receive no host-command relay, no containerd or `sbx` control socket, and no Kit VM's Docker engine |
| Read your real home directory | Not implicitly | Only the project, guest home, and explicitly configured extra directories are mounted |
| Read proxy-managed provider keys | No | The `sbx` proxy holds them; the job sees placeholders |
| Change or delete project files | Yes | The project is mounted read/write |
| Change what your Mac's `git` runs in the project | Not through `.git`'s own config, hooks or Git dirs; yes through a repository it creates in the project ([Git in jobs](#git-in-jobs)) | Read-only binds of `.git/config`, `config.worktree`, `hooks/`, `info/`, `modules/`, `worktrees/` and `commondir`, under `.git` bound onto itself |
| Change marsh's split copies and checkpoints | No | `<project>/.marsh` is created before each job and bound read-only into it |
| Read other jobs' logins and history | Yes, when they use the same guest home, unless the job's Kit takes no home | The persistent guest home is shared within a scope; `org.marsh/home@1` keeps it out of a Kit's VM ([Mounts](kits.md#mounts)) |
| Start other registered commands | Yes, within limits | The job capability socket; `MARSH_SPAWN` narrows it |
| Reach the network | As its Kit's policy allows | `sbx` enforces the policy; marsh does not inspect traffic |
| Use Docker | In any project Docker is on for (every project, unless you turned it off), and only that project's own Docker | A filtered endpoint to the project's own services VM; [Docker services](#docker-services) |

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

MCP integrations can add access beyond these mounts, as described
[below](#what-is-not-isolated).

You can mount extra Mac directories through [environment files](configuration.md#servers-and-extra-directories).
Child jobs inherit their parent's extra directories; split Kit jobs omit them.
Use `readOnly: true` for reference material. MCP connections can grant access
beyond mounted files, as described [below](#mcp-connections).

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

- the project and the guest home (no guest home if its Kit declares
  [`mount: none`](kits.md#mounts))
- its Kit's network policy, through the `sbx` proxy. Only the container's own
  loopback (`localhost`, `127.0.0.1`, `::1`) skips the proxy (`NO_PROXY`), so a
  test can reach a server it started; the container has its own network
  namespace, so that reaches nothing else
- placeholders for proxy-managed credentials; the provider keys stay with the
  `sbx` proxy. Saved agent logins live in the shared guest home.

#### Git in jobs

A job (and every child job) mounts the project read-write, but what your Mac's
`git` would read code or another Git directory from is read-only in it:
`.git/config`, `.git/config.worktree` (when `extensions.worktreeConfig` is on),
`.git/hooks/`, `.git/info/`, `.git/commondir`, and the submodule and linked
worktree Git directories `.git/modules/` and `.git/worktrees/`. `.git` itself is
bound onto itself so it can't be renamed away. A `.git` file (the project is
itself a worktree or submodule, or was made with `--separate-git-dir`) is
read-only. When the Git directory it names (or that directory's common
directory) is inside the project, its controls are guarded the same way;
when the path to it passes through a symlink inside the project (which a job
could re-point), the job is refused. When it is outside the project, only
the `.git` file is guarded: the job doesn't see that directory unless it is
under an extra folder or the guest home, where it is not protected; and in
the job `git status` says `not a git repository`. A `.git` that is a
symlink refuses the job. Before each job marsh makes
missing ones, so a job can't plant them: empty `hooks/`, `info/`, `modules/`
and `worktrees/` directories, empty `config` and `config.worktree` files, and
a `commondir` containing `./` (the Git directory itself) in a repository that
had none. `marsh` leaves them in place. The job is refused when one of them
is a symlink or of the wrong type, or when `commondir` names another
directory (Git would read config and hooks there).
With that `commondir`, `git rev-parse --git-common-dir` prints an absolute
path instead of `.git`; a script that compares it with `--git-dir` to detect
a linked worktree is fooled. `.marsh` must be a directory marsh can make, or
the job is refused.

Inside a job, `git commit`, `branch`, `switch`, `fetch`, `status` and `diff`
work. These fail: `git config` without `--global` (your name and email are
already set in the guest home's `~/.gitconfig` from your Mac's, and
`git config --global` writes there); `git branch -m` (exit 128 after renaming);
`git branch -u`, `push -u`, and the tracking `git switch` sets up for a
remote-only branch (it says so, but nothing is recorded); any change inside a
submodule or an existing linked worktree, `git submodule update --init`, and
`git worktree add`; `git lfs install`, `pre-commit install` and other tools
that install hooks.

**Known gap, not prevented: nested repositories.** `.git/index` stays
writable so a job can commit, and so a job can create a repository anywhere
in the project's files and stage or commit it as a submodule, or put a
repository of its own in a submodule's directory (replacing its `.git` file,
or in an uninitialised one). Your Mac's `git status`, `diff`, `add -A`/`-u`,
`commit -a`, `stash`, `merge`, `diff-files` and `diff-index`, and any IDE or
prompt that runs them, then run Git inside that repository with its config:
its `core.fsmonitor`, or a `filter.<name>.clean` its `.gitattributes` names.
The only full prevention is a private index per job, which marsh does not
have yet (future work). Until then:

- No setting of your Mac's Git fully helps. `diff.ignoreSubmodules` is
  overridden by `ignore = none` in the job-written `.gitmodules`, and doesn't
  cover `add -A`, `stash`, `merge`, `diff-files` or `diff-index`; only
  `--ignore-submodules=all` on a single command skips submodules.
- Run `git status` (and let your IDE or prompt run it) only in repositories
  whose contents you trust; after an agent has worked in a project, treat it
  as untrusted until you have reviewed its changes.
- Consider reviewing a job's work with host Git in a separate checkout (a
  fresh clone that fetches the job's branch), not in the project it ran in.

Also not covered:

- An IDE that scans one level down (VS Code by default) opens a nested
  repository the job created even when nothing stages it.
- A job can write interactive-rebase state (`.git/rebase-merge/`) whose
  `exec` lines run at your next `git rebase --continue`.
- Hooks your config points into the project (`core.hooksPath`, as husky
  sets), which a job can edit.
- Your own shell commands (the shell VM mounts the whole project).

A job has no containerd or `sbx` socket and never a Kit VM's Docker engine.
**Jobs of a services-enabled project get a filtered endpoint for that project's
own services VM, never a Kit VM's engine**
(`/run/marsh/docker.sock`; [Docker services](#docker-services)). Its one
channel to the daemon is a socket that lets it:

- start registered commands and splits, within the
  [limits](agents.md#limits-and-messages) and `MARSH_SPAWN`
- list its own jobs
- ask the Mac to open a web link

### Docker services

In a project [Docker](docker.md) is on for (the default for every project), each job gets a Docker
socket, `/run/marsh/docker.sock` (mode 0600, owned by the job's user). It leads
to **that project's own services VM**: a VM that holds only that project's
containers, run by a Docker engine (the program that runs containers) that
exists only for that project. The endpoint never leads to a Kit VM's Docker, to
the shell VM's Docker, or to another project's services. Docker is on by default;
only you can turn it off or change that default, and a job can neither grant
itself the socket nor change the setting. Having it on starts nothing: the VM is
created when a job first uses the endpoint. A network policy from your organization
or global rules still applies to the project's containers. marsh sets
`DOCKER_HOST` and `TESTCONTAINERS_HOST_OVERRIDE` from its own record of the job
and refuses or drops `DOCKER_*` values from the exported launch sources (your
shell's environment, a Kit's settings). A process inside a job can still change
its own environment; that changes where that process's client connects and
gives it no other route to an engine.

**A filter sits between the job and Docker.** The filter runs inside the
services VM. It rebuilds each request from parts it has checked, allows a fixed
list of requests, and refuses everything else with a `marsh:` message. On the
Docker API it refuses:

- privileged containers, host networking, host PID or IPC, devices, and
  capabilities beyond a short list (`NET_BIND_SERVICE`, `NET_RAW`, `NET_ADMIN`,
  `SYS_NICE`, `SYS_PTRACE`);
- bind mounts of anything outside the project. Inside the project, `.git` and
  `.marsh` are mounted read-only **in the ordinary case**; there are exceptions
  (see [Services risks marsh accepts](#services-risks-marsh-accepts)). A bind
  source is pinned when the container is created, so swapping a folder for a
  symlink afterward does not redirect it;
- pushing, logging in, loading or importing images, builds from a remote
  context, plugins, swarm, and every other request not on the list;
- another workspace's objects: lists and events show only the caller's own
  containers, volumes, and networks, and joining a network or taking volumes
  from another scope is refused;
- labels that start with `dev.marsh.`, which marsh uses to tell whose object
  is whose;
- ports that are not on loopback: a request to publish elsewhere is rewritten
  to `127.0.0.1`.

**The filter does not read BuildKit's own stream.** `docker build` talks to the
engine over a second channel (gRPC) that the filter relays without parsing. A
remote context or frontend that BuildKit fetches, and a push BuildKit makes
inside that stream, are therefore not refused by the filter, and the engine's
Internet-off gate does not apply to them (the gate drops connections from
containers; the engine's own traffic, which is also how pulls work, is exempt).
What applies is the services VM's Docker Sandboxes network policy and the
firewall below. BuildKit's extra permissions (`security.insecure`,
`network.host`) are off. See [Builds and the cache](docker.md#builds-and-the-cache).

A container that mounts the Docker socket gets a different socket with the same
filter and the same limits as the job that made it. It never gets the real
engine socket.

**Containers get default limits** (4 GiB of memory, 4 CPUs, 4096 processes) and
caps on how many connections a job, a project, and the daemon may hold open.

**What the services VM can touch on your Mac.** It mounts the project folder
read/write at its Mac path, so a container, and an agent driving Docker, can
read, change, and delete project files, as a direct job can. It does not mount
your home or the guest home. The daemon relays opaque bytes between the job and
the services VM, does not parse Docker requests, and dials nothing on your Mac
for the services VM. Port forwards into a job are made inside that job's own
network, opened before any listener exists and only into that network; if that
step fails, nothing listens anywhere. A forward goes into a job's network only
when the namespace has IPv6 for `::1`; `127.0.0.1` is always used.

**Internet is off by default** for a project's Docker. The gate drops new
connections from containers to anywhere outside the project's container
networks, and only you can open it (`marsh services internet on`). A policy that
governs the project's VM wins over your choice. A job cannot turn it on. The
gate does not cover the engine's own traffic (pulls, and BuildKit's own fetches
and pushes, above). See [Internet](docker.md#internet).

#### Your credentials

Docker Sandboxes attaches every credential it holds, and its MCP gateway, to
every sandbox it creates, a services VM included. Left alone, a container could
send its traffic through the sandbox proxy and act as you on GitHub, Anthropic,
and OpenAI, and reach MCP tools that act on your Mac. marsh limits that in two
layers:

1. **A firewall inside the VM's engine** drops TCP and UDP from every container,
   and from the engine itself, to the sandbox's gateway addresses, except DNS on
   port 53. It is installed before any container can run. If the rules go
   missing, or the gateway address changes so they no longer cover it, the
   engine is treated as lost and its services stop. The proxy is blocked by name,
   by IPv4 and IPv6 address, and on other ports.
2. **Dummy secrets.** When it creates a services VM, marsh gives that VM its own
   throwaway value for each credential Docker Sandboxes lets it shadow (GitHub,
   Anthropic, and OpenAI at least), so a request through the proxy gets a 401
   instead of acting as you. marsh never reads or prints a secret value, and
   never changes your real ones. A credential Docker Sandboxes does not let it
   shadow stays exposed to whatever gets past the firewall.
   `marsh services status --json` reports under `credential_exposure` which
   secrets were shadowed and which could not be.

marsh also creates the VM with no MCP servers of its own. These controls are
about Docker Sandboxes' credentials. They do not protect secrets that sit in
your project (a `.env` file a container can bind-mount), secrets you pass into a
container or a build, or anything once root in the VM is compromised (next
section).

#### Services risks marsh accepts

- **Root in the services VM defeats the filter, and the firewall, for that
  project.** Anything that reaches root there already owns that project's
  containers, data, and mounted project folder. The firewall lives inside the
  VM, so root can also remove it. Docker Sandboxes' proxy and MCP gateway stay
  attached to the VM: root can use them, and the gateway's tools can act on your
  Mac, and your credentials act wherever they were not shadowed. The VM
  boundary still separates it from other projects' services VMs, but it is not a
  complete barrier to your Mac. marsh asked Docker Sandboxes for a way to create
  a sandbox with no credential injection and no MCP gateway; until that exists
  this is a defense, not an absence.
- **BuildKit's own stream is not inspected.** A remote context, a frontend, or a
  push made inside a BuildKit build is not refused by the filter and is not
  stopped by the Internet-off gate. The services VM's Docker Sandboxes network
  policy and the credential firewall still apply. See above.
- **The BuildKit cache is shared across the lanes of one project.** A cache a
  build in one lane writes is readable by a build in another lane of the same
  project. It is the same user and the same project, and lanes are copies of one
  tree. It does not cross projects. BuildKit's SSH and secret mounts are served
  by the client in the job, never from your Mac.
- **The `.git` and `.marsh` protection has exceptions.** The filter knows the
  project's `.git` and `.marsh` by identity, not by name, so an ordinary rename
  or a different letter case does not hide them. These cases are not covered,
  and all need a job that can already write your project folder while it runs
  (the same residuals are in
  [the policy design](design/services-policy.md#pins-a3)):
  - the first time the filter sees the project, it trusts what it finds. If
    `.git` was renamed before the project's first Docker request, the renamed
    folder is never recorded;
  - a protected folder moved deeper into a subfolder is protected when a mount
    targets it or sits inside it, but a mount of that subfolder, or of any folder
    above it other than the project root, does not carry it as read-only;
  - a mount of a single file works out its parent folders by name after opening
    it, so a rename at that moment can hide a protected parent;
  - a repository created after a container has mounted the project root is not
    protected in that container. A container started afterward mounts the root
    again and does protect it; the one already running is not repaired;
  - if the filesystem ever gives a recorded folder's identity to an unrelated
    folder, that folder is read-only when mounted.
- **A container with `NET_RAW` can disturb the engine's DNS check.** The
  engine looks up the gateway address again from time to time, and a container
  that may forge network packets can forge that answer. It cannot weaken the
  firewall. The worst it can do is make marsh decide the engine is lost, and
  the project's services stop until you start them again. It is an
  availability risk only.
- **Anyone in the shell VM can use a forwarded port.** One shell VM serves all
  projects of a home, and its forwards do not check who connects. Another
  project's shell can reach a port you forwarded there, just as projects in one
  home can already read each other's files. Use a separate `MARSH_HOME` to keep
  projects apart. A job's forwards are in its own network and are not shared.
- **Images are shared by the lanes of a project.** An image one lane pulls or
  builds is there for the others.

### Opening links on the Mac

`xdg-open URL` in the shell VM or a job is the one action on your Mac that a
guest can request without a registered command. The daemon runs
`/usr/bin/open URL` directly, with no shell, and only when all of these hold:

- the link is `http://` or `https://`, with a host, in plain ASCII, at most
  4096 bytes; files, paths, and other link types are refused;
- your terminal is attached to the session, and the request comes from the
  session's shell or a job started from that terminal (or its children), not
  from ACP, MCP, a split or fanout branch, or `cloud`;
- the session has opened fewer than 3 links in the last 10 seconds;
- `marsh config open-urls` is on and the daemon did not start with
  `MARSH_OPEN_URLS=0`.

Otherwise the link is only printed. The browser then loads the page as you,
with your cookies, so treat a link from an agent like a link in an email. A job
that opens a link records its site (`scheme://host`, no path or query) in its
receipt.

The shell and its jobs also see a few variables naming your terminal (such as
`TERM_PROGRAM`; [the list](agents.md#your-terminal)). They name the terminal
application, not you, and carry no credentials.

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

### MCP connections

A server loaded into a Kit VM is available to every job in that VM, across
projects in the scope. Its tools and account grants determine what it can
read or change. The per-Kit YAML files select automatic loading; they do not
restrict access to Docker Sandboxes' host-wide registry or prevent a client
from using the gateway's discovery and loading features.

Local MCP `command` servers run on your Mac with your account's permissions.
Keep their executable, scripts, and configuration outside guest-writable
directories. marsh prevents mounting the environment YAML and resolved
executable, but does not discover every file that executable may load.

Removing a declaration or stopping marsh does not remove its Docker Sandboxes
registration or OAuth grant. Manage those with `sbx mcp` on the Mac.

## What is not isolated

The project
: A direct job can read, change, or delete project files. Registered agents in
  a split mount their private project copies instead.

The guest home
: Jobs using the same guest home can read and write one another's settings,
  history, and saved logins, even across Kits. Provider credentials bound with
  `sbx secret` stay behind the proxy. An agent's own login flow can instead save
  credentials here as ordinary files, readable by other jobs. A Kit that
  declares [`mount: none`](kits.md#mounts) never gets it.

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
  traffic. A project's [Docker](docker.md) has its own setting, off by
  default; once you turn Internet on, its containers reach whatever your
  Docker Sandboxes policy allows.

Docker services
: A job of a project with services on can run containers in that project's
  services VM, read and write that project's data there, use any port those
  containers publish, and change project files through a bind mount. It cannot
  start privileged containers, and its Docker endpoint leads to that project's
  services only. The filter does not read BuildKit's own stream, and root in the
  services VM defeats it. See [Docker services](#docker-services) for what the
  filter refuses and the risks marsh accepts.

Agents running agents
: A job can start other registered commands. Each gets its own Kit's network
  and credentials. Narrow this with `MARSH_SPAWN=codex,pi` (a comma-separated
  list, or `none`) or `marsh run --spawn a,b NAME`. Nothing inside a job can widen it.

Detached cloud runs with `cloud-delegate` on
: Off by default. On, each detached cloud run that can call a registered Kit
  carries your Docker access token (Allow Cloud Sandboxes access) in a
  root-only file in its sandbox, so its supervisor can start agent sandboxes.
  That access is all or nothing: anyone holding the token can create, use,
  and delete every cloud sandbox on your account and spend its quota. The
  run's commands run as an unprivileged user without `sudo` and cannot read
  the file; they can only ask the supervisor to start one of the run's own
  Kits, within a per-run cap and the run's time limit. What remains exposed
  is the sandbox itself (its disk holds the file until the sandbox is
  deleted) and the supervisor. marsh reads the token from your Keychain only
  at a run's start and passes it on one root-only step's stdin; it is not in
  requests, records, receipts, logs, or command lines. See
  [Docker Cloud](cloud.md#agents-calling-agents-with-the-lid-closed).

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

## What marsh records

For each job marsh records:

- command name and IDs
- mount targets
- exit status and cleanup result
- timing
- the sites (`scheme://host`) of links it opened on the Mac

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
