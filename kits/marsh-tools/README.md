# marsh-tools: work tools for agents, in one Kit

Work tools for agents running in [marsh](https://github.com/mcavage/marsh), as
one marsh Kit (`docker.io/mcavage/marsh-tools:stable`). An agent runs them like
any command (`gog gmail search …`), and each call becomes a short job in the
tools' own VM. Each tool's credential lives only in that VM's network proxy.
The agent never sees it, and can't use it any other way.

| Command | What it does |
|---|---|
| `gog` | Google Workspace through [gog](https://github.com/steipete/gogcli): read Gmail, Drive, Docs, Sheets, Slides, Calendar and more; create, edit and comment on documents, sheets, decks and forms it created, and create Gmail drafts and tasks. Everything else is read-only: it cannot send or notify anyone, share or publish, delete or trash files, or run code. |
| `bamboohr` | Read-only BambooHR directory: find people, reporting chains, org trees. |

Both commands come from the same image, so marsh runs them in one VM. The
image's entrypoint picks the tool from the name marsh records for the job
(`/run/marsh/job.json`). Outside marsh, name the tool first
(`docker run … marsh-tools gog gmail search …`).

gog is pinned to an exact version and checksum in the [Dockerfile](Dockerfile),
so upgrades are deliberate. CI builds the pin and only publishes it once the
offline smoke test passes.

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

The policy below allows more than these scopes do. To let agents also manage
tasks, add `tasks`. An allowed command whose scope you didn't grant fails at
Google, not in the Kit.

`drive.file` is the "created by the agent" boundary: Google lets the token
write only to files this app created, so the policy below can say "its own
files" and Google enforces it. It is **not privacy**. A file the agent created
is an ordinary file in your My Drive, and anything you later share, move into a
shared folder or open to others is open to them too. That is why new files land
in your My Drive root and nothing the agent can run shares, moves or publishes
them.

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

### The model

**Agents create, update and comment on the Google resources they created, and
read everything else.** Google's `drive.file` scope enforces "created by the
agent" (see the credential note above). Within that, the agent can do useful
things: formulas, Markdown and HTML images by URL, find and replace, copy and
paste in a sheet, deleting content, HTML drafts. The policy does not try to stop the agent from
sending data to a URL, because the agent can already reach arbitrary URLs
through its own web tools.

What it must never do with your Google credentials is **send or notify**
anyone, **share or publish** anything, **delete or trash** a file or anything
outside one, change your **account**, or **run code**.

### The policy

Every gog command path, hidden ones included, is either allowed
([`gog/allowed-commands`](gog/allowed-commands)) or refused
([`gog/refused-commands`](gog/refused-commands)).
[`gog/flag-rules`](gog/flag-rules) narrows the allowed commands that could
still break the model.

**Allow**

- Reads everywhere: get, list, search, export, metadata, raw, url, including
  settings, sharing and permissions.
- Creating new files, always in My Drive root: documents, spreadsheets, decks,
  forms, folders, uploads, copies, task lists. The wrapper rejects `--parent` on
  creates and pins copies to `--parent root`: a copy would otherwise land in its
  source's folder, which may be shared.
- Editing the content, tabs, sheets, slides, tables and formatting of those
  files: write, update, append, insert, replace, `sed`, find and replace, copy
  and paste, with formulas, Markdown and HTML images by URL, and Drive's own
  conversions. Any edit includes deleting content: text ranges, `clear`, rows,
  columns, tabs, slides, page elements, charts, named ranges, headers and
  footers, form questions (`docs delete`, `sheets delete-tab`,
  `slides delete-slide`, ...). Only the file itself stays out of reach.
- Raw Sheets `batchUpdate` requests (`sheets batch-request`): they edit inside
  the spreadsheet, and Google only lets the token edit one the agent created.
  No request type can share a file, delete the file (`deleteSheet` removes a
  tab, never the last one) or open another file. Two groups do reach beyond the
  sheet, so the wrapper reads the JSON (inline, `@file` or stdin, all
  decoded as Google reads them) and refuses any request type or field naming a
  data source (`addDataSource`, `refreshDataSource`, `dataSourceTable`, ...:
  external BigQuery data, billed to you) or a comment (`insertComment`,
  `addCommentReply`, `deleteComment`, ...: they can @-mention, and comments are
  handled by the comment commands).
- Comments on those files (Docs, Sheets, Slides and Drive): create, reply,
  resolve, reopen. Comment text containing `@` is refused, because an
  @-mention or `+email` makes Google email that person.
- Gmail drafts: create and update, including drafts of replies and forwards,
  HTML included. Nothing sends them; you do.
- Tasks: add, update, complete.

**Refuse**

- Anything that sends or notifies others: sending mail, sending a draft,
  replying, forwarding, invitations, RSVPs, `calendar propose-time`, Chat
  messages, Classroom, publishing a form.
- **Calendar writes** (create, update, update-calendar and the rest):
  an event can carry guests and invitations.
- Sharing, permissions, ownership, publishing, moving files (into a shared
  folder is sharing), shortcuts and folder sync.
- Choosing where a new file goes (`--parent`), which would inherit that
  folder's sharing.
- Uploading a *local* image into a document or slide (`docs insert-image`,
  `docs replace-image`, `slides insert-image`, `slides add-slide`,
  `slides replace-slide`): gog makes the file public so Docs or Slides can
  fetch it. Images by URL in Markdown or HTML are fine.
- Deleting or trashing a file (`drive delete`, with or without `--permanent`),
  and deleting anything outside a file: mail (`gmail trash`, `gmail batch
  delete`), calendar events, contacts, tasks, Keep notes, comments (editing or
  deleting one), data sources, watches. Deleting *content inside* a file the
  agent created is an ordinary edit and is allowed (below).
- Changing the account: Gmail settings, filters, forwarding, delegation,
  send-as, vacation, labels and mailbox state; contacts, Keep, Meet, YouTube
  and Photos changes.
- Admin, auth and token commands, gog's own config (including its no-send
  guard), backup, and request batches.
- Running code or talking to anything but Google's APIs: every program-running
  or browser flag (`--mmdc`, `--on-change`, `--on-new`, `--open`,
  `--browser`), hooks and webhooks, Apps Script `run`, the MCP server, the
  generic `api call`, Zoom, email tracking, gog's update check, and
  `slides create-from-markdown`.
- Any command not classified above. The default for a command nobody has
  reviewed is refused.

### Keeping it true

- **gog refuses everything not in `allowed-commands`**, using its own
  `--enable-commands-exact`. Aliases resolve first, so `gog gmail read` is
  `gmail thread get`. The top-level shortcuts (`gog send`, `gog upload`) are
  separate commands with their own lines.
- **The wrapper resolves the command before it applies the flag rules.** It
  asks gog, with nothing enabled, which command the arguments are, so
  `drive upload`, `upload` and any alias get the same rules. If gog can't name
  a command (a usage error, `--help`), the wrapper runs it with nothing
  enabled.
- **A new gog command or flag fails CI until someone decides.**
  [`tests/kits/tools-policy.py`](../../tests/kits/tools-policy.py) lists every
  command in the built image's gog (hidden ones too) and fails unless allowed
  plus refused equals that list exactly. It also checks that each rule names a
  real flag (every spelling of it), that the built wrapper enforces it, that
  gog has no program-running flag and no allowed command with a destination
  flag, comment text or raw request JSON that a rule doesn't cover, and that
  the image's gog is the version the Dockerfile pins.
- **Upgrading gog is a reviewed change.** Edit `GOG_VERSION` and `GOG_SHA256` in
  the Dockerfile (the checksum comes from the release's `checksums.txt`), then
  run the smoke test. It fails until every new command is allowed or refused and
  every new flag the policy cares about has a rule; list what is new since the
  last pin in the PR and classify it by the policy above (refuse when in doubt).
- **The wrapper's settings can't be changed.** The wrapper rejects
  `--enable-commands`, `--access-token`, `--home` and similar flags, and `--`.
  It runs gog with a clean environment and an empty config folder. If the
  allowlist or the rules are missing, it refuses to run, since an empty list
  would allow everything.

## Develop

```sh
docker buildx build --platform linux/arm64 --load -f kits/marsh-tools/kit.yaml -t marsh-tools:candidate kits/marsh-tools
tests/kits/tools-smoke.sh marsh-tools:candidate
```

To try a change in marsh before publishing, point both commands at the Kit
folder in `commands.json`. `marsh status` shows where that file is.
