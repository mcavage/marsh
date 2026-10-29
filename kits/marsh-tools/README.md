# marsh-tools: work tools for agents, in one Kit

Work tools for agents running in [marsh](https://github.com/mcavage/marsh), as
one marsh Kit (`docker.io/mcavage/marsh-tools:stable`). An agent runs them like
any command (`gog gmail search …`), and each call becomes a short job in the
tools' own VM. Each tool's credential lives only in that VM's network proxy.
The agent never sees it, and can't use it any other way.

| Command | What it does |
|---|---|
| `gog` | Google Workspace through [gog](https://github.com/steipete/gogcli): read Gmail, Drive, Docs, Sheets, Slides and Calendar; create Gmail drafts; create documents, sheets and decks and edit the ones it created. It cannot send mail, share, or delete anything. |
| `bamboohr` | Read-only BambooHR directory: find people, reporting chains, org trees. |

Both commands come from the same image, so marsh runs them in one VM. The
image's entrypoint picks the tool from the name marsh records for the job
(`/run/marsh/job.json`). Outside marsh, name the tool first
(`docker run … marsh-tools gog gmail search …`).

CI rebuilds the Kit within six hours of each gog release, and only publishes
it once the offline smoke test passes.

## Install

```sh
marsh kit install gog docker.io/mcavage/marsh-tools:stable
marsh kit install bamboohr docker.io/mcavage/marsh-tools:stable
```

Register each command under its tool's name; any other name is refused.
Install only the tools you use: the Kit's credentials are optional, and a tool
without one fails its first call with a credential error.

### Moving from marsh-gog and marsh-bamboohr

The separate `marsh-gog` and `marsh-bamboohr` images are no longer built, so
commands installed from them stop updating. Reinstall from this Kit:

```sh
marsh kit uninstall gog
marsh kit uninstall bamboohr
marsh kit install gog docker.io/mcavage/marsh-tools:stable
marsh kit install bamboohr docker.io/mcavage/marsh-tools:stable
```

The `sbx secret` values stay as they are.

### Google credential

The Kit expects an SBX secret named `gog` holding a current Google access
token. Access tokens expire after an hour, so set it to a command that mints
one from your long-lived Google sign-in, and let SBX refresh it:

```sh
sbx secret set gog --command "/path/to/your-token-command" --refresh 40m
```

The command prints one access token. Sign in with these scopes, so Google
itself limits edits to files the tool created: `gmail.readonly`,
`gmail.compose`, `drive.readonly`, `drive.file`, `calendar.readonly`.

### BambooHR credential and company

```sh
printf '%s:x' "$BAMBOOHR_API_KEY" | base64 | sbx secret set bamboohr
```

The Kit mounts no home folder (see below), so the BambooHR company name comes
from the image. Build one for your organization and install that instead:

```dockerfile
FROM docker.io/mcavage/marsh-tools:stable
ENV BAMBOOHR_COMPANY_DOMAIN=yourcompany
```

Build it as a Kit (a `kit.yaml` like this one's, pointing at that Dockerfile)
so it keeps the network policy, credentials and home declaration.

## What a tool job can reach

- **Its own VM, not the agent's.** Docker Sandboxes applies network policy
  and credentials per VM, not per container. If a tool ran inside the
  agent's VM, the agent's VM would have to inject the tool's credential, and
  the agent could call the service directly, skipping every rule below. The
  Google grant includes `gmail.compose`, which Google also allows to send
  mail, so the allowlist is the only thing stopping a send, and it only holds
  because the token is somewhere the agent can't reach.
- **One VM for all the tools.** The tools share one network policy and one
  credential proxy. Neither tool is an agent: each binary talks only to its
  own service, with arguments the wrapper and the tool check.
- **The project, but not your home.** The Kit declares `org.marsh/home@1`
  with `mount: none` ([Kits](../../docs/kits.md#mounts)). marsh never mounts
  the guest home (other agents' settings and saved logins) into this Kit's VM,
  even when an agent that has the home starts the tool. `HOME` is the job's
  own `/tmp`. The project stays mounted, so gog's downloads, exports and file
  inputs work as usual. A job started from a folder inside the home is
  refused.

## How `gog` stays safe

- **gog refuses everything not listed** in
  [`gog/allowed-commands`](gog/allowed-commands), using its own
  `--enable-commands-exact`. Aliases resolve first, so `gog send` and
  `gmail drafts post` are refused too.
- **The wrapper's settings can't be changed.** The wrapper rejects
  `--enable-commands`, `--access-token`, `--home` and similar flags. It runs
  gog with a clean environment and an empty config folder. If the allowlist is
  missing, it refuses to run, since an empty list would allow everything.
- **Google limits edits.** With `drive.file`, edits work only on files this
  tool created.

## Develop

```sh
docker buildx build --platform linux/arm64 --load -f kits/marsh-tools/kit.yaml -t marsh-tools:candidate kits/marsh-tools
tests/kits/tools-smoke.sh marsh-tools:candidate
```

To try a change in marsh before publishing, point both commands at the Kit
folder in `commands.json`. `marsh status` shows where that file is.
