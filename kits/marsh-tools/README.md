# marsh-tools: work tools for agents, in one Kit

Work tools for agents running in [marsh](https://github.com/mcavage/marsh), as
one marsh Kit (`docker.io/mcavage/marsh-tools:stable`). An agent runs them like
any command (`gog gmail search …`), and each call becomes a short job in the
tools' own VM. Each tool's credential lives only in that VM's network proxy.
The agent never sees it, and can't use it any other way.

| Command | What it does |
|---|---|
| `gog` | Google Workspace through [gog](https://github.com/steipete/gogcli): read Gmail, Drive, Docs, Sheets, Slides, Calendar and more; create Gmail drafts, calendar events (without guests), tasks, documents, sheets, decks and forms, and edit the files it created. It cannot send or notify anyone, share, or delete anything. |
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

The policy below allows more than these scopes do. To let agents also create
and update calendar events or manage tasks, add `calendar.events` or `tasks`.
Google doesn't limit those to tool-created items: `calendar.events` can edit
any event, so the policy (no guests, no invitations) is the only bound there.
An allowed command whose scope you didn't grant fails at Google, not in the Kit.

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

### The policy

Every gog command path, hidden ones included, is either allowed
([`gog/allowed-commands`](gog/allowed-commands)) or refused
([`gog/refused-commands`](gog/refused-commands)), by one rule set:

**Allow**

- Reads everywhere: get, list, search, export, metadata, raw, and url.
- Creating new files: documents, spreadsheets, decks, forms, folders,
  uploads, copies, task lists.
- Editing content: write, update, append, insert, replace, formatting, and
  adding, renaming, duplicating and reordering tabs, sheets and slides. Google
  limits these to tool-created files when the token has `drive.file`.
- Gmail drafts: create and update, including drafts of replies and forwards.
  Nothing sends them; you do.
- Calendar events: create and update, with no guests and no invitations (the
  wrapper rejects `--attendees`, `--add-attendee` and `--send-updates`).
- Tasks: add, update, complete.

**Refuse**

- Anything that sends or notifies others: sending mail, sending a draft,
  replying, forwarding, RSVPs, Chat messages, Drive comments, calendar
  auto-declines (focus time, out of office), Classroom, publishing a form.
- Sharing, permissions, ownership, and bulk permission changes. This
  includes commands that briefly make a file public so Docs or Slides can
  fetch it: inserting or replacing a *local* image (`docs insert-image`,
  `docs replace-image`, `slides insert-image`, `slides add-slide`,
  `slides replace-slide`).
- Deleting, trashing, clearing or removing anything, including text ranges,
  rows, columns, tabs, slides, and elements. To remove content from a file
  the tool made, overwrite it with `write`, `update` or `edit`.
- Changing the account: Gmail settings, filters, forwarding, delegation,
  send-as, vacation, labels and mailbox state; contacts, Keep, Meet, YouTube
  and Photos changes; calendar lists and aliases.
- Admin, auth and token commands, gog's own config (including its no-send
  guard), backup, and request batches.
- Anything that runs code or talks to something other than Google's own APIs:
  hooks and webhooks (`--on-change`, `--on-new`, Pub/Sub watches, `drive
  changes serve`), Apps Script `run`, the MCP server, the generic `api call`,
  Zoom, email tracking, and gog's update check.
- Any command not classified above. The default for a command nobody has
  reviewed is refused.

Limits of the policy: `slides create-from-markdown` briefly makes generated
icon images (never your files) public while it builds the deck. `calendar
update` can change an existing event that already has guests; Google sends no
notification by default, but their calendars change.

### Keeping it true

- **gog refuses everything not in `allowed-commands`**, using its own
  `--enable-commands-exact`. Aliases resolve first, so `gog gmail read` is
  `gmail thread get`. The top-level shortcuts (`gog send`, `gog ls`) are
  separate commands with their own lines.
- **A new gog command fails CI until someone decides.**
  [`tests/kits/tools-policy.py`](../../tests/kits/tools-policy.py) lists every
  command in the built image's gog (hidden ones too) and fails unless
  allowed plus refused equals that list exactly, with no overlap and no stale
  lines. The scheduled build therefore stops publishing `:stable` after a gog
  release that adds commands, until both lists are updated.
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
