# Docker for your project

Many projects need a database, a queue, or a mail catcher to run or test.
Every project gets its own private Docker. Agents and
your own shell use it the way they would use Docker on a laptop: `docker run`,
`docker compose up`, Testcontainers. The containers they start serve
`localhost` ports to the jobs and shells that need them.

Docker is **on by default for every project**. Nothing starts until an agent
(or you) first uses it, and you can turn it off one project at a time, or turn
it off everywhere and opt projects in. The rest of marsh works the same with it
off. (This feature was called
"services"; `marsh docker ...` and `marsh config docker ...` still work as
aliases.)

## What you get

Each project with Docker on has its own **Docker VM**: a separate
Docker Sandboxes VM that runs only that project's containers. It is not the
shell VM and not a Kit VM. A project's jobs and shell reach it through a Docker
socket that marsh provides, a filtered endpoint that answers ordinary Docker
requests and refuses the ones that would reach out of the project (see
[What works and what is refused](#what-works-and-what-is-refused) and
[Security](security.md#docker-services)). The endpoint leads to this project's
containers only. It never leads to a Kit VM's Docker, the shell VM's Docker, or
another project's Docker.

The Docker VM mounts your project folder read/write, at its Mac path.
Containers can therefore read, change, and delete project files through a
bind mount, as a direct job can. That mount is the only part of your Mac the
Docker VM holds.

## Turn it off, or back on

Docker is on for a project unless you turn it off. In the project's folder:

<!-- services-example id=config-services-on fixture=stack-a run=host stdout="on\non\n" -->
```sh
marsh config docker on
marsh config docker
```

The first command prints `on` (it is the same as having done nothing). The
second shows the setting in effect for the current project (`on` or `off`).
`marsh config docker off` turns it off for this project only, and a project's
own setting always wins over the default. `marsh config docker ls` shows the
default and every project with a setting of its own. To make Docker opt-in
instead, `marsh config docker default off`: then only projects you
`marsh config docker on` have it (`marsh config docker default` shows the
default). The setting is yours alone: a job cannot turn Docker on or off, change
the default, or ask for the endpoint. It also does not widen what Docker Sandboxes
allows: an organization or global network policy still governs the project's
containers.

Having Docker on costs nothing until it is used. The Docker VM is created the
first time a job or shell of the project actually uses Docker (a `docker` command
that reaches the endpoint), so that first call is slower; a job that never runs
`docker` never starts it. A checkpoint of a project whose VM is not running
never starts it either. After the last user it stops by itself (see
[What survives](#what-survives-when-a-job-ends)). To create it ahead of time:

<!-- services-example id=services-start fixture=stack-a run=host stdout="is ready for" -->
```sh
marsh docker start
```

Turning Docker off (`marsh config docker off`) stops the project's
containers and its Docker VM at once and keeps the data.

The Docker disk of a new Docker VM is 40 GiB. Change it with
`marsh config services-disk-size SIZE` (`1g` to `4096g`); it applies to VMs
created afterwards. Docker needs a 24 GiB pool and a 512 MiB reserve on that
disk, and the VM's filesystem keeps about a tenth of the disk for itself (a
`25g` disk leaves about 22.4 GiB). marsh therefore refuses a size below `28g`
when it would create the VM, before any VM exists; the message gives the sizes
and the least size that passes, and nothing is left to remove. The `28g` is
an approximation of that overhead. If a size passes it and the engine still
finds too little free space when it starts, marsh removes the Docker VM that
start created (it holds nothing yet) and says so; a Docker VM that already
exists keeps the disk it was made with, is never refused for the setting and is
never removed for it.

## Use Docker from a job or the shell

In a project with Docker on, marsh starts every job (an agent, a script,
`marsh run`) with `DOCKER_HOST` pointing at `/run/marsh/docker.sock` and with
`TESTCONTAINERS_HOST_OVERRIDE=localhost`. marsh takes those two values from its
own record of the job. It refuses or drops `DOCKER_*` settings that come from
the places you configure a job (your shell's exported environment, a Kit's
settings). A process inside a job can still change its own environment, but that
only changes where that process's Docker client connects: the job has no other
route to a Docker engine. The [marsh shell](shell.md#docker-for-a-project-with-docker-on)
gets a socket of its own. Nothing else about Docker changes: the `docker` CLI,
Compose, and Testcontainers need no configuration.

Examples below use the image tags marsh's own tests use; any recent tag works.

Start a service that stays up after the job that started it, and reach its
port:

<!-- services-example id=docker-run-published-port fixture=stack-a run=job stdout="200" -->
```sh
docker run -d --name web --restart unless-stopped -p 8080:80 nginx:1.27-alpine
curl -s --retry 10 --retry-connrefused -o /dev/null -w '%{http_code}\n' http://localhost:8080
```

The `--restart` policy matters when a job starts the container: a one-off
container without it is removed when the job ends (see
[What survives when a job ends](#what-survives-when-a-job-ends)). From the
marsh shell the container stays regardless.

`exec`, `logs`, `cp`, and `attach` work as usual:

<!-- services-example id=docker-run-exec-logs fixture=stack-a run=job stdout="exec-ok" stdout="ready" -->
```sh
docker run -d --name hello busybox:1.37 sh -c 'echo ready; sleep 3600'
docker exec hello echo exec-ok
docker logs hello
docker rm -f hello
```

Or a Compose stack from the project folder (`compose.yaml` is yours; stack A
of marsh's tests has Postgres on 5432):

<!-- services-example id=compose-up fixture=stack-a run=job stdout="accepting connections" -->
```sh
docker compose up -d --wait db
pg_isready -h localhost -p 5432
```

Testcontainers needs nothing special. Run a test that uses it; its helper
container (Ryuk) stays on and cleans up after the run:

<!-- services-example id=testcontainers fixture=tc run=job stdout="\"ok\":true" -->
```sh
node tc.test.js
```

Named volumes and `tmpfs` work:

<!-- services-example id=named-volume-tmpfs fixture=stack-a run=job stdout="kept\nt\n" -->
```sh
docker volume create demo-data
docker run --rm -v demo-data:/data busybox:1.37 sh -c 'echo kept > /data/note'
docker run --rm -v demo-data:/data busybox:1.37 cat /data/note
docker run --rm --tmpfs /scratch busybox:1.37 sh -c 'echo t > /scratch/x && cat /scratch/x'
```

A bind mount of a folder inside the project works. It sees the project's files
live. In the ordinary case the project's `.git` and `.marsh` folders are
read-only inside the container, with a few documented exceptions (see
[Services risks marsh accepts](security.md#services-risks-marsh-accepts)):

<!-- services-example id=bind-mount-project fixture=stack-a run=job stdout="from-project" -->
```sh
mkdir -p data && echo from-project > data/note
docker run --rm -v "$PWD/data:/data" busybox:1.37 cat /data/note
```

A container may mount the Docker socket (`/var/run/docker.sock`), as
Testcontainers' helper and some dev tools do. It gets a socket with the same
limits as the job that started it:

<!-- services-example id=docker-socket-mount fixture=stack-a run=job stdout="socket-ok" -->
```sh
docker run --rm -v /var/run/docker.sock:/var/run/docker.sock docker:29.9.2-cli sh -c 'docker ps > /dev/null && echo socket-ok'
```

`docker build` works, with the limits listed under [Builds](#builds-and-the-cache):

<!-- services-example id=docker-build fixture=stack-a run=job stdout="built" -->
```sh
mkdir -p build && printf 'FROM busybox:1.37\nRUN echo built > /note\n' > build/Dockerfile
docker build -q -t app build
docker run --rm app cat /note
```

## Ports on localhost

A port a container publishes is reachable on `localhost` (`127.0.0.1`, and
`::1` where the network has IPv6) from inside the jobs and the marsh shell of
that project. A port appears within about a second of the container starting
and disappears within about a second of it stopping.

- **In a job:** each job has its own network, so a port is forwarded only into
  jobs of that project, and only for as long as the job runs. If the job
  already listens on that port, its own listener wins: marsh prints a
  `marsh:` line and does not replace it. If marsh cannot set a forward up, it
  says so on the job's stderr and does not listen anywhere else.
- **In the shell:** ports are forwarded into the shell VM. The shell VM is
  shared by all projects of one home, so the first shell to claim a port keeps
  it. A later claim prints `marsh: port N is already in use in the shell VM`
  and replaces nothing. See [the shell](shell.md#docker-for-a-project-with-docker-on).
- **Not on your Mac.** A published port is not on your Mac's own `localhost`.
  Docker runs in a VM; use it from jobs and the marsh shell.
- **Not on the network.** Docker publishes ports on loopback only. A request to
  publish on another address, such as `-p 0.0.0.0:5432:5432`, is rewritten to
  loopback instead of being refused.
- **Only `127.0.0.1` is forwarded.** Loopback is a whole range, and a container
  may publish on another loopback address (`-p 127.0.0.2:8080:80`) or on
  `::1`. Docker accepts that, and two workspaces of a project can each hold
  port 8080 on a different address. marsh forwards only what is published on
  `127.0.0.1`: a port published on any other address stays inside the Docker
  VM and does not appear in jobs or the shell.

`marsh docker status` tells you whether a project's Docker VM is up:

<!-- services-example id=services-status fixture=stack-a run=host after=services-start stdout="Docker: on for" stdout="ready" -->
```sh
marsh docker status
```

It prints one line naming the projects that have Docker on, then one line
for each project that has a Docker VM (and for the current project, even if
it has none yet): its folder, its state, the VM's name, and the reason in
parentheses when there is one. The state is `off`, `stopped`, `starting`,
`ready`, or `unavailable`. Plain `status` does not list containers, ports, or
the Internet setting. For those, use `--json`: each project has `containers`,
`ports`, and `internet` (with `on`, `governed`, and `effective`).

<!-- services-example id=services-status-json fixture=stack-a run=host after=services-start stdout="\"schema\":\"marsh.services/v1\"" stdout="\"governed\":false" -->
```sh
marsh docker status --json
```

## Internet

A project's Docker has **no Internet by default**. A container cannot open
a new connection to anywhere outside the project's container networks, and a
name lookup from inside one fails. Pulling images still works, because Docker
itself does the pulling and the filter allows it from Docker Hub (`docker.io`)
and GitHub's registry (`ghcr.io`) only. Containers reach each other, and a
container's published ports work, as usual.

Many apps need the Internet (a payment sandbox, an OAuth redirect, a package
install in a container build). You grant it to the project from your Mac:

<!-- services-example id=services-internet fixture=stack-a run=host after=services-start stdout="Internet is on for this project's Docker" -->
```sh
marsh docker internet on
```

With it on, a container reaches the Internet:

<!-- services-example id=internet-on-reaches-out fixture=stack-a run=job after=services-internet stdout="200" -->
```sh
docker run --rm curlimages/curl:8.11.1 -s -o /dev/null -w '%{http_code}' https://example.com
```

`marsh docker internet off` takes it back, and the same request fails:

<!-- services-example id=services-internet-off fixture=stack-a run=host after=services-internet stdout="Internet is off for this project's Docker" -->
```sh
marsh docker internet off
```

<!-- services-example id=internet-off-blocked fixture=stack-a run=job after=services-internet-off status=nonzero stdout="000" -->
```sh
docker run --rm curlimages/curl:8.11.1 -s -o /dev/null -w '%{http_code}' https://example.com
```

The command works only from your Mac terminal. A job cannot run it, and no job
can turn it on for itself. Turning it on also opens every registry for pulls.
It does not widen anything your Docker Sandboxes network policy blocks, and
traffic that gets through is still subject to that policy.

**Image pulls and builds are the exception to "off".** Pulls are made by Docker
itself, not by a container, so the Internet-off gate does not stop them; the
filter limits them to the two registries. A build through BuildKit has an
exception of its own, described under [Builds and the cache](#builds-and-the-cache):
the filter cannot see what a BuildKit build fetches or pushes. So Internet off
is not a promise that nothing from a project's Docker can reach the outside.

**Policy that governs wins.** If an organization, global, or per-VM network
policy that marsh did not create restricts the project's Docker VM, both
`internet on` and `internet off` refuse, name the rule, and change nothing.
`marsh docker status --json` then shows `internet.governed` as `true`, and
`internet.effective` lists the rules. A policy that blocks a single host (for
example a telemetry host you block everywhere) only narrows what Internet
reaches. It is listed but does not count as governing. If marsh cannot read
the policy, it treats the project as governed and opens nothing.

Your Kit's network policy and the Docker Internet grant are separate. marsh
does not combine them: the Docker grant is yours to give, per project.

## What survives when a job ends

Containers and volumes belong to the project's workspace, not to a job, so
that an agent's `docker compose up` keeps serving the next turn and you.

| Started by a job | At job end |
|---|---|
| A Compose stack | Kept |
| A container with a restart policy (`--restart`) | Kept |
| A one-off container (`docker run`) | Removed |
| Containers a test run started with Testcontainers | Removed |
| Volumes, images, networks | Kept |

marsh removes at job end only that job's own containers that are neither in a
Compose project nor set to restart. So a Compose label, or a restart policy,
does exempt a container a job created from that job-end cleanup. It gives the
container no extra rights: the container still belongs to its workspace and the
same filter limits it. Containers you start from the marsh shell are yours: they
stay after the shell exits.

Kept does not mean permanent. You can remove containers with Docker
(`docker compose down`, `docker rm -f`). Closing, cancelling, or removing a lane
removes the containers that belong to that lane. Removing the project's Docker
(below) removes everything.

## Stop, keep, and remove

| What you do | Containers | Data (volumes, images) |
|---|---|---|
| Nothing for 15 minutes | Stopped with the VM | Kept |
| `marsh docker stop` | Stopped | Kept |
| `marsh config docker off` | Stopped | Kept |
| `marsh stop` | Stopped | Kept |
| `marsh reset` | Left alone | Kept |
| `marsh reset --include-services` | Deleted | Deleted |
| `marsh docker rm` | Deleted | Deleted |

The idle time is 15 minutes with no job or shell using the project's Docker.
Change it with `marsh config services-idle-seconds SECONDS`. A stopped
Docker VM starts again the next time something needs it. marsh does not start
your containers again. A container with a Docker restart policy may come back
the way it would on any Docker host, and the others stay stopped: run
`docker compose up -d` again. The data is still in its volumes.

Turning Docker off and on again keeps the data too:

<!-- services-example id=config-services-off fixture=stack-a run=host stdout="off\noff\non\n" -->
```sh
marsh config docker off
marsh config docker
marsh config docker on
```

`marsh reset` removes the shell and Kit VMs but skips every Docker VM.
`marsh reset --include-services` lists each project whose Docker data it
would delete, then asks you to confirm on a terminal. With no terminal (a
script) it prints the list and stops with exit status 1 unless you pass `--yes`,
and deletes nothing:

<!-- services-example id=reset-include-services fixture=stack-a run=host after=services-start status=1 stderr="deletes the Docker VM, Docker disk and data of:" stderr="pass --yes to confirm" -->
```sh
marsh reset --include-services
```

`marsh docker hold` and `release` keep the VM awake for a number of seconds,
and let go of it. They exist to exercise the idle timer and are not needed in
normal use:

<!-- services-example id=services-hold-release fixture=stack-a run=host after=services-start stdout="released " -->
```sh
holder=$(marsh docker hold 60 | sed -n 's/^holder //p')
marsh docker release "$holder"
```

`marsh docker stop` stops the VM and keeps its data:

<!-- services-example id=services-stop fixture=stack-a run=host after=services-start stdout="its data is kept" -->
```sh
marsh docker stop
```

Docker data is deleted only on purpose. There is no command that removes just
one workspace's containers; use Docker for that. `marsh docker rm` removes the
project's whole Docker VM and its disk: every container, volume, and image,
every lane's containers, and the saved app data of the project's checkpoints. A
checkpoint keeps its files, but its app data is then gone. `rm` asks first on a
terminal and needs `--yes` otherwise:

<!-- services-example id=services-rm fixture=stack-a run=host after=services-stop stdout="removed the Docker VM and its disk" -->
```sh
marsh docker rm --yes
```

`marsh docker status`, `stop`, and `rm` work even when Docker is off for
the project, so turning Docker off never strands a VM. A project folder that was
moved or deleted still has its Docker VM and disk: `marsh docker status` marks it
as gone, and `marsh docker rm --project /old/path` (from any folder) removes
it. For a folder that is gone it only accepts the exact folder marsh has a
Docker VM or a Docker setting recorded for (a folder on a drive that is not
mounted looks gone too, so mount it first); for a folder that exists it acts
like `marsh docker rm` run there. Once a gone folder's VM is removed, `rm`
also forgets its setting, so `marsh docker status` and `marsh config docker ls`
stop listing it; a gone folder that only has a setting (no VM was ever made)
is forgotten the same way, and `rm` says there was no Docker VM to remove and
that it forgot the setting. `rm --project` asks for confirmation only when the
project has a Docker VM. If the setting cannot be forgotten (the config file is
unreadable or cannot be written), `rm` fails and says so; the VM is already
gone, and running the same command again finishes the job. Running it again for a folder marsh no longer has anything recorded for is
refused with a message saying so.

## Checkpoints and Branch from here

A [checkpoint](split.md#checkpoints-and-open-lanes) saves the project's **local
data** along with its code, in two places:

- **The folder.** Every git-ignored file the tools write (`.env`,
  `.wrangler/state`, SQLite files, uploads) is saved with the checkpoint,
  whether or not Docker is on. Rebuildable folders are left out: `node_modules`,
  `.next`, `.nuxt`, `target`, `dist`, `build`, `out`, `.venv`, `venv`,
  `__pycache__`, `.pytest_cache`, `.turbo`, `.cache`, `.parcel-cache`,
  `.gradle`, `.terraform`, `coverage`, and files such as `*.log`. A file in
  `.marsh/branch` changes that, one line each:

  ```text
  # copy this although node_modules is left out
  include node_modules/my-local-package
  # leave these out
  exclude .wrangler/tmp
  exclude *.sqlite
  ```

  The patterns are gitignore-style, and an `exclude` wins over an `include`. At
  most 20,000 files and 2 GiB are saved (the copies are APFS clones, so they
  cost little disk; `services_ignored_max_files` and
  `services_ignored_max_bytes` in `config.json` change the limits), small
  files first. What was left out, and why, is listed in plain words in the
  checkpoint's `ignored.skipped` and in the lane's `folder.skipped`. Links are
  never followed. Everything stays on your Mac.
- **Docker** (when it is on for the project). The checkpoint holds a copy of the
  contents of the project's volumes, taken while the project's containers are
  paused for a short time (the checkpoint records how long). Other lanes keep
  running.

Cloud state (a hosted database, a deployed Worker) is never copied.

A Docker capture can be skipped or incomplete. The checkpoint always keeps your
files, and records what was left out as a gap, for example when the pool has too
little free space or a service's data could not be saved. Do not assume a
checkpoint has Docker data unless its record says so.

If the Docker VM is stopped when you checkpoint, nothing is writing to it, and
the checkpoint points at the last saved copy instead of taking a new one:

- When the VM stops because it idled out, marsh takes a final capture first, so
  that copy is current. That capture can fail like any other.
- When it stopped another way (for example `marsh docker stop`, or from
  outside marsh) there may be no final capture. The checkpoint then reuses an
  older copy, which can be older than your last write, or has no app data at all
  if there was never a copy. The checkpoint and the restore line say so.

**Branch from here.** Starting a lane from a checkpoint (`marsh split --open
--from CHECKPOINT`, or Branch from here in Narwhal) gives the lane its own copy
of the code and of the saved local data: the folder's git-ignored files, and
Docker's data started in the lane's own containers, on its own ports. There is
no switch for it. Two lanes never write into one database, and the checkpoint
is never changed.

- marsh writes `compose.fork.yaml` and a rewritten `.env` into the lane's copy
  of each Compose project, on your Mac. Published ports are remapped to free
  ones, and `localhost:PORT` and `127.0.0.1:PORT` in the lane's `.env` are
  rewritten to match. Git-ignored files saved with the checkpoint are copied in
  first, so interpolation sees your real values.
- Jobs in the lane get their own Docker endpoint, scoped to that lane. A lane
  sees only its own containers, volumes, and networks: it cannot list, stop, or
  enter another lane's. Standalone containers in different lanes cannot reach
  each other. Lane jobs get `localhost` forwards for the lane's own ports.
- The result for each lane says in plain words what local data came back, across
  both places, and what did not. For example: `Local data restored:
  .wrangler/state, .env, db (Docker). Started empty: redis (no saved data).`
  Its `ok` is true only if every service that had saved data came back with that
  data verified and nothing the checkpoint saved from the folder was left out
  for lack of room. Treat `ok: false` as "look at the line". With Docker off, a
  lane still gets its folder's data, and the line says so.
- Closing, cancelling, or removing the lane removes its containers and its copy
  of the data.

What is not restored: a Compose stack that uses a fixed `container_name`, an
external or explicitly named volume or network, or a published port range is
listed as a gap rather than started. Testcontainers objects are reported as not
restored. A service with no saved data starts empty, and the line says so. A
standalone volume comes back with its data, but its container is not started.
A stack that fails to start is taken down completely, so a lane never has a
half-started copy.

## What agents are told

In a project with Docker on, each job's `/run/marsh/context.md` (the file agent
Kits hand to their agent) gains a short **Docker** section: `docker` and `docker
compose` work here and `DOCKER_HOST` is set, published ports are reachable on
`localhost`, local data (Docker volumes and git-ignored files) is saved with
each turn and comes along on Branch from here, containers have no Internet
unless you turn it on, and a refused operation is an error that starts with
`marsh:`. Jobs of a project with Docker off get no such section.

The job's image needs the `docker` command. If an agent's image has none, the
[`marsh-docker-cli` mixin Kit](https://github.com/mcavage/marsh/tree/main/kits#marsh-docker-cli-the-docker-cli-for-any-agent-image)
has the CLI and the Compose plugin to add to it.

## What works and what is refused

Every Docker request goes through a filter that allows a known set of requests
and refuses everything else. A refusal fails the Docker command with an error
that starts with `marsh:` and says why. The table is the short version for the
Docker API; BuildKit is different (see [Builds](#builds-and-the-cache)).

| Works unchanged | Refused |
|---|---|
| `docker run`, `exec`, `logs`, `cp`, `attach`, `stop`, `rm` | Privileged containers (`--privileged`) |
| `docker compose` (up, down, exec, logs, ...), but not Compose's `jobs` ([limits](#limits-and-gaps)) | Host networking, `--pid host`, `--ipc host`, devices |
| Named volumes, `tmpfs` | Bind mounts of folders outside the project |
| Bind mounts of folders inside the project | Writes to `.git` and `.marsh` through a bind mount, with exceptions (see [risks](security.md#services-risks-marsh-accepts)) |
| `-p` publishing (rewritten to loopback) | Docker's built-in `bridge` network (each workspace has its own default network) |
| Mounting the Docker socket | Devices (`--device`) |
| `docker pull` from `docker.io` and `ghcr.io` | Pulling from other registries, until Internet is on |
| `docker build` of a project folder | `docker push`, `docker login`, `docker load`, `docker import`, and a build from a remote context (a URL), on the Docker API |
| Testcontainers, with Ryuk | Plugins, swarm, Compose's `jobs` extension, and anything else not on the filter's list |
| Resource limits up to the caps below | Limits above the caps, and labels that start with `dev.marsh.` |

Examples of refusals:

<!-- services-example id=refused-privileged fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
docker run --rm --privileged busybox:1.37 true
```

<!-- services-example id=refused-host-network fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
docker run --rm --network host busybox:1.37 true
```

<!-- services-example id=refused-bridge-network fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
docker run --rm --network bridge busybox:1.37 true
```

<!-- services-example id=refused-device fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
docker run --rm --device /dev/null busybox:1.37 true
```

<!-- services-example id=refused-outside-bind fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
docker run --rm -v /etc:/host-etc busybox:1.37 ls /host-etc
```

In an ordinary git project a container cannot write the project's `.git`:

<!-- services-example id=refused-git-write fixture=stack-a run=job status=nonzero stderr="Read-only file system" -->
```sh
docker run --rm -v "$PWD:/p" busybox:1.37 sh -c 'echo x >> /p/.git/config'
```

<!-- services-example id=refused-reserved-label fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
docker run --rm --label dev.marsh.scope=other busybox:1.37 true
```

<!-- services-example id=refused-push fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
docker push docker.io/library/busybox:1.37
```

<!-- services-example id=refused-load fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
echo | docker load
```

<!-- services-example id=refused-build-remote fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
docker build https://example.com/context.tar.gz
```

Pulls come from two registries while Internet is off, and from anywhere once it
is on:

<!-- services-example id=docker-pull-allowed fixture=stack-a run=job after=services-internet-off stdout="pulled" -->
```sh
docker pull busybox:1.37 > /dev/null && echo pulled
```

<!-- services-example id=refused-registry fixture=stack-a run=job after=services-internet-off status=nonzero stderr="marsh:" -->
```sh
docker pull quay.io/prometheus/busybox:latest
```

A container with no limit gets 4 GiB of memory, 4 CPUs, and 4096 processes.
You can raise them, up to 64 GiB, 32 CPUs, and 65536 processes:

<!-- services-example id=resource-limit-default fixture=stack-a run=job stdout="4294967296" -->
```sh
docker run --rm busybox:1.37 cat /sys/fs/cgroup/memory.max
```

<!-- services-example id=resource-limit-over-cap fixture=stack-a run=job status=nonzero stderr="marsh:" -->
```sh
docker run --rm --memory 128g busybox:1.37 true
```

More detail:

- **Capabilities.** A container may add only `NET_BIND_SERVICE`, `NET_RAW`,
  `NET_ADMIN`, `SYS_NICE`, and `SYS_PTRACE`.
- **Names.** Container, network, and volume names that look like a full
  64-digit container ID are refused.
- **Docker API.** Version 1.44 or later.
- **One request at a time** per connection, with bounded sizes (32 KiB of
  headers, 1 MiB of JSON). Large builds and file copies are streamed.
- **Connections.** A job can have 64 open connections to its Docker,
  counting forwarded ports; a project 256.

## Builds and the cache

`docker build` works, and `RUN --mount=type=cache` works too. The filter checks
the Docker API requests of a build: it refuses a build from a remote context
(`docker build URL`), host networking in a build, and pushing through the image
API. But BuildKit, the builder behind `docker build`, talks to Docker over its
own stream (gRPC), and the filter relays that stream without reading it. So for
builds:

- **A remote context, a frontend, or a push that BuildKit makes inside its own
  stream is not seen by the filter.** A build can still fetch from a URL or push
  an image (`--output type=image,push=true`) that way. What limits it is the
  rest of the stack, not the filter: the Docker VM's Docker Sandboxes network
  policy applies, and so does the firewall that blocks the sandbox's credential
  gateway. The Internet-off gate does not cover these: it drops connections
  from containers, while a fetch or push BuildKit makes itself is made by
  Docker, which is also why pulls work. Do not treat Internet off as a promise
  that a build cannot reach out.
- **A build fetches from the Docker VM's own network, and so can reach what
  other workspaces of the project publish.** `ADD http://127.0.0.1:PORT/...`
  (and a git or HTTP source) is fetched by Docker inside the VM, where every
  workspace's published ports are on the one `127.0.0.1` (and a port published on
  another loopback address, such as `127.0.0.2`, is reachable from a build the same
  way, whether or not marsh forwards it into jobs). A lane's build can
  therefore read, with a GET, an HTTP service another lane or the base
  workspace published, and the answer becomes a layer of the lane's image. It
  is the same user and the same project, and a lane starts as a copy of the
  base, but it breaks the separation that two lanes' containers otherwise
  have. marsh does not block it: the services agent that forwards your ports
  dials `127.0.0.1` in that same network, so a rule against loopback would cut
  the ports too.
- **The registry allowlist does not cover what a build pulls.** The allowlist
  (Docker Hub and `ghcr.io` while Internet is off) is checked for `docker
  pull`, for the image of a container, and for a `BUILDKIT_SYNTAX` build
  argument, but that last check applies only to a build sent over Docker's REST
  API. A normal `docker build` or `docker buildx build` talks to BuildKit over its
  own stream, which the filter does not read, so it gets no such check. The
  allowlist is not checked for `FROM`, for a `# syntax=` line, for
  `--cache-from`, or for anything else inside the build context: the filter
  does not read the context, and a build with the older builder
  (`DOCKER_BUILDKIT=0`) pulls `FROM` images itself. A build can pull from any
  registry the Docker VM can reach.
- **BuildKit's whole control API is reachable, not only builds.** The stream
  BuildKit uses (`/grpc`; plain `docker build` and `docker buildx build` both use
  it) is relayed without being read, so the filter's refusal
  of `POST /build/prune` does not stop `docker buildx prune` or `docker buildx
  du`, which ask BuildKit directly and can empty or measure the cache shared by
  the project's lanes. marsh does not scope build history either, so BuildKit's
  history calls (`docker buildx history`) can see another lane's builds, with the
  build arguments and logs they recorded, and delete them. (These behaviors are
  BuildKit's own and have not been exercised end to end through marsh.) Keep secrets out of
  build arguments (use `--secret`, which stays in your job), and treat the build
  cache and history as shared by every lane of a project. This stays inside one
  project.
- **The build cache is shared across lanes of one project.** A cache written by
  one lane is readable by another lane of the same project. This is the same
  user and the same project, and lanes are copies of one tree, so marsh accepts
  it. It does not cross projects.
- **SSH and secret mounts** (`--ssh`, `--secret`) are served by the client in
  your job, never from your Mac or the Docker VM. What you pass into a build
  is visible to that build.
- **Extra permissions for a build** (`security.insecure`, `network.host`) are
  off in the builder, and cannot be turned on.

This example shows the shared cache. It puts both Dockerfiles in the project
folder first, so both lanes' copies have them, then builds in lane a and lane b:

<!-- services-example id=buildkit-cache-shared fixture=stack-a run=lanes stderr="secretA" -->
```sh
# In the project folder, before the lanes are made:
printf 'FROM busybox:1.37\nRUN --mount=type=cache,id=shared,target=/cache sh -c "echo secretA > /cache/note"\n' > write.Dockerfile
printf 'FROM busybox:1.37\nRUN --mount=type=cache,id=shared,target=/cache cat /cache/note\n' > read.Dockerfile
# In lane a:
docker build -q -f write.Dockerfile .
# In lane b:
docker build --progress=plain -f read.Dockerfile .
```

Lane b's build log prints `secretA`.

## Limits and gaps

In plain words:

- **Docker lives in a VM, not on your Mac.** Ports are on `localhost` inside
  jobs and the shell. A browser on your Mac cannot open them.
- **It costs memory and disk.** A Docker VM keeps a Docker disk (40 GiB by
  default) and runs while anything uses it, then idles out. The containers
  inside share what the VM has.
- **Images are shared by a project's lanes**, and so are the names of networks
  and volumes: a name another lane already uses is refused. Image tags are partly protected:
  `docker tag`, `docker commit`, `docker pull` and `docker rmi` refuse to move or
  remove a tag another workspace created (a pull also while another workspace is
  making the tag), `docker tag` and `docker commit` also refuse a pulled image's tag such as
  `postgres:16`, and `docker rmi` refuses to remove one (pulling it again is
  allowed). A build sent over Docker's REST API (the legacy
  builder, `DOCKER_BUILDKIT=0`) is checked the same way. **A normal `docker build -t` or `docker buildx
  build -t` is not prevented**: those talk to BuildKit over the stream described
  under [Builds and the cache](#builds-and-the-cache), which the filter does not
  read, so a lane can still build an image under a tag the base runs (`docker
  buildx build --load -t base-app:1 .` in a lane moved the base's `base-app:1`).
  What marsh does instead is **detect it and report it, and never undo it**: when
  a build's stream ends, the services agent looks again at the tags another workspace
  created (and the names on pulled images such as `postgres:16`) and reports each one
  whose image changed or went, in `marsh docker status` (one line) and `marsh docker status
  --json` (`image_tag_events`) and on the services agent's standard error (the latest 64
  are kept in its memory, and lost when the Docker VM restarts). The report names the
  tag, its owner, the image it had and the one it has, and says what to run to put it
  back: if the previous image still exists, `docker tag <previous image id> <name>` in
  the owner's workspace (otherwise rebuild it); for a pulled name, `docker pull <name>`
  fetches the registry's current image, not necessarily the previous one. With several
  builds at once the report names a stream that ran with the one that moved the tag, and
  says so. marsh cannot tell, without reading BuildKit's stream, whether the build moved the tag or its owner did, so it never changes a tag
  itself; when the owner had a `tag`, a pull or a build of its own meanwhile, the
  report says "this may be the owner's own change". **What stays open:** it is seen
  when the build's stream ends, not stopped: from the moment the build moves the tag
  until you put it back, the base's tag points at the lane's image, and a `docker run
  base-app:1` in that time runs the lane's image. A tag moved and moved back inside one
  stream is not seen. A tag a `docker build` in one lane creates is not recorded to that
  lane (only tags made over the REST API and pulled names are), so another lane's build
  can still take it without a trace. A build whose image is also a pulled image (a
  `FROM alpine` with no change) makes its tag count as a pulled name, and a later build
  of that tag in the same lane is reported. At most 256 protected tags are watched
  per stream, and a stream whose image list could not be read runs unwatched, with an
  "unable to check" report (so does an image name that cannot be watched: too long, or not a name the filter accepts). The report names up to three such
  names (cleaned of control characters, cut to 64 characters, then "+N more"). An untagged image or a digest reference such as
  `moby-dangling@sha256:...`, which the containerd image store lists, is not a tag and raises nothing. A pulled name stops
  being watched once a local image holds it, and a name created after a build began is
  not watched by that build. Each build costs two image lists, the first before the
  build starts, and can wait for another workspace's tag claim that is in the engine
  (up to 30 s for each claim ahead of it: they queue). Reports are kept in the agent's
  memory until it restarts: none expires,
  also not after you put the tag back, and a repeated "unable to check" with the same
  text for a workspace is shown once (other refused names are a new report). The BuildKit cache and history are as exposed as
  before. Where
  it applies, the workspace that creates a tag may move it; a name stays free until
  an image has it (another workspace can create it first), and it is free again
  after `docker rmi`. Two lanes cannot both build `-t app` over the REST API; give
  each its own name. Because a pulled image's tags are protected, a workspace
  cannot `docker rmi` a pulled tag such as `busybox:latest`, and `docker compose
  down --rmi all` (which removes the images of every service, pulled ones
  included) fails with a 403 for them; `--rmi local` and `docker rmi` of your own
  tag or alias work. `docker pull -a` (every tag of a repository, also `name:` with nothing after the
  colon) is refused: name a tag. Digest pulls (`name@sha256:...`, which the Docker
  client sends as `tag=sha256:...`) are not affected. A `docker rmi` of an image
  ID (short or full) or digest is refused if any tag of the image is another
  workspace's or a pulled image's, and it is never forced: `docker rmi -f <id>`
  acts as `docker rmi <id>`, so the engine refuses an image that has several tags
  ("referenced in multiple repositories", or in use by a stopped container). That
  includes the cleanup idiom `docker rmi -f $(docker images -q)` (it passes IDs): images with
  several tags, or used by a stopped container, fail with the engine's 409. Remove such an
  image by its tag names (`docker rmi name1 name2`), which keeps `-f`. A tag named like the
  start of an image's ID counts as an ID. `docker compose down --rmi` is not affected (it
  never forces, and removes tagged images by name). If the services VM cannot write its records at the moment a tag is
  made, the tag is left unowned and the next workspace to ask may take it. When a lane
  is closed, cancelled or removed, the tags it created are removed from the engine
  with its containers and volumes (untag only: an image another tag uses stays). If
  the engine does not confirm that, the tag stays the lane's and refused to others,
  and `marshd` repeats the cleanup every few seconds (it prints `dropping lane ...
  left image tag ...; will retry`). If the engine keeps refusing (a 409, for the last
  tag of an image another lane's container uses), `marshd` gives up after about a
  minute and says so; the tag then stays in the engine and, on a pulled image, only
  `marsh docker rm` removes it. Tags whose ownership records were lost (the ledger
  was set aside as corrupt) are not removed with a lane and, on a pulled image,
  cannot be moved or removed by any workspace; only `marsh docker rm` clears them.
  A tag longer than 255 characters cannot be claimed (the delete route could not name it).
  Because a normal `docker build -t` is not covered (above), a lane can build over a tag
  another lane created; closing the first lane then removes that tag from the engine
  (the second lane's image loses its name; nothing else is touched).
- **Compose's `jobs` (Compose 5.8 and later) is refused.** The `jobs:` element
  (one-shot and scheduled jobs, `docker compose run JOB`) reaches the engine
  over a gRPC channel the filter does not relay, and the engine's jobs
  extension is off by default anyway. Compose probes that channel in a few
  places even when a project has no jobs: `docker compose -p NAME down` (always,
  whether or not a compose file is found), and `--remove-orphans` on `down`,
  `up` and `create`. The probe is refused at once, so these commands print a
  warning such as `jobs of project NAME not removed: the engine's jobs extension
  is enabled but not ready: ...`, then carry on and exit 0; the warning can be
  ignored. A plain `docker compose down` or `down -v` (no `-p`) of a project that declares no `jobs:` does not probe,
  and neither do `up -d` or `ps` of a project without scheduled
  jobs. A project that declares scheduled jobs (`jobs:` with a `triggers.schedule`)
  fails on `docker compose up`, `create` and `start` before anything is
  created, with the same "jobs extension is enabled but not ready" text; remove
  the scheduled jobs from the file to run the project here. ([Troubleshooting](troubleshooting.md#docker-compose-prints-a-jobs-warning).)
- **Docker's built-in `bridge` network is refused.** Containers without a
  network join their workspace's own network, so a plain `docker run` in two
  lanes cannot reach each other. Containers in one stack reach each other by
  name as usual.
- **Not everything is restored into a lane.** See
  [Branch from here](#checkpoints-and-branch-from-here) for what comes back and
  what is reported as a gap.
- **marsh does not restart your containers for you.** If the Docker VM stops
  or Docker inside it is lost, the containers stop; a restart policy may bring
  some back. Start the rest again.
- **Shell objects are not checkpointed** when you use the shell's private
  Docker (`unset DOCKER_HOST`).
- **Dev-server detection** (pointing your dev server at a lane's ports
  automatically) is not part of this release.
- **Anyone in the shell VM can use a forwarded port.** The shell VM is shared
  by every project of the home, so another project's shell can connect to a
  port you forward there, as it can read your files. Use a separate
  `MARSH_HOME` for work that must stay apart.

See [the security model](security.md#docker-services) for what the filter
protects, what it does not, and the risks marsh accepts.

## Commands

| Command | What it does |
|---|---|
| `marsh config docker [on\|off\|ls]` | Show or set Docker for the current project |
| `marsh config services-disk-size [SIZE]` | Docker disk of newly created Docker VMs (default `40g`) |
| `marsh config services-idle-seconds [SECONDS]` | Idle time before the Docker VM stops (default 900) |
| `marsh docker status [--json]` | State of each project's Docker VM (`--json`: containers, ports, Internet) |
| `marsh docker start [--json]` | Create or start the Docker VM and wait until it is ready |
| `marsh docker stop [--json]` | Stop it, keeping its data |
| `marsh docker rm [--yes] [--json] [--project GONE_FOLDER]` | Remove it and all its data (`--project`: the old folder of a moved or deleted project) |
| `marsh docker internet on\|off [--json]` | Grant or revoke Internet |
| `marsh docker hold SECONDS`, `release HOLDER` | Keep the VM awake; for testing the idle timer |
| `marsh reset --include-services [--yes]` | Also delete Docker data, after listing the projects |

If Docker reports that project folders cannot be bound because the ownership
records were corrupt, marsh kept the damaged file and wrote `ledger.quarantine`
next to it in the Docker VM (`/pool/marsh/`). Check that your `.git` and `.marsh`
folders are where you expect, then remove that file; pins already marked failed
come back at the next Docker engine start.

`marsh docker` runs on your Mac. A job or the marsh shell cannot run it. The
same commands are in [marsh(1)](man/marsh.1.md).

Next: [Security](security.md), [The shell](shell.md), [Split and join](split.md),
[design: Docker](design/services.md).
