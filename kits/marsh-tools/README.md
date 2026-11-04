# marsh-tools: work tools for agents, in one Kit

Work tools for agents running in [marsh](https://github.com/mcavage/marsh), as
one marsh Kit (`docker.io/mcavage/marsh-tools:stable`). An agent runs them like
any command (`gog gmail search …`), and each call becomes a short job in the
tools' own VM. Each tool's credential lives only in that VM's network proxy.
The agent never sees it, and can't use it any other way.

| Command | What it does |
|---|---|
| `gog` | Google Workspace through [gog](https://github.com/steipete/gogcli): read Gmail, Drive, Docs, Sheets, Slides, Calendar and more; create Gmail drafts, tasks, and private documents, sheets, decks and forms, and edit the ones it created. It cannot send or notify anyone, share or publish, delete, or run code. |
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

The policy below allows more than these scopes do. To let agents also manage
tasks, add `tasks`. An allowed command whose scope you didn't grant fails at
Google, not in the Kit.

`drive.file` limits which files the token can edit. It does **not** make them
private: a file the tool created is an ordinary file in your My Drive, and
anything you later share, move into a shared folder or open to others is open
to them too. Privacy comes from the policy below: new files land in your My
Drive root, and nothing the agent can run shares, moves or publishes them.

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

### Threat model

The agent is not trusted. With your Google credentials it must never:

- **send or notify** anyone (mail, invitations, comments, chat, RSVPs);
- **share or publish** anything, or leave it where others can see it;
- **delete** anything;
- **run code**, or make Google fetch a URL the agent chose (a way to carry
  your data out).

It may read what the token reads, and create and edit its own private files.

### The policy

Every gog command path, hidden ones included, is either allowed
([`gog/allowed-commands`](gog/allowed-commands)) or refused
([`gog/refused-commands`](gog/refused-commands)). [`gog/flag-rules`](gog/flag-rules)
narrows the allowed commands that could still break the model.

**Allow**

- Reads everywhere: get, list, search, export, metadata, raw, and url.
- Creating new files, always in My Drive root: documents, spreadsheets,
  decks, forms, folders, uploads, copies, task lists. The wrapper rejects
  `--parent` on creates and pins copies to `--parent root` (a copy would
  otherwise land in its source's folder, which may be shared). It also rejects
  conversions (Drive's conversion evaluates formulas and fetches images).
- Editing content: write, update, append, insert, replace, formatting, and
  adding, renaming, duplicating and reordering tabs, sheets and slides.
  Sheets values are always stored as typed (`--input RAW` is forced), so
  `=IMAGE(...)` and `=IMPORTDATA(...)` are never evaluated. Docs content is
  refused if it holds image markup (`![`, `<img`), in an argument or a file.
- Gmail drafts: create and update, including drafts of replies and forwards,
  as plain text. Nothing sends them; you do.
- Tasks: add, update, complete.

**Refuse**

- Anything that sends or notifies others: sending mail, sending a draft,
  replying, forwarding, RSVPs, Chat messages, Drive comments, Classroom,
  publishing a form.
- **Calendar writes.** Creating or updating an event can carry guests,
  conferencing, visibility or a shared calendar through flags and a
  positional calendar ID that the wrapper can't constrain safely, and
  updating can change an existing event that has guests.
- Sharing, permissions, ownership, moving files (into a shared folder is
  sharing), shortcuts, folder sync, and commands that briefly make a file
  public so Docs or Slides can fetch it (inserting or replacing a *local*
  image).
- Deleting, trashing, clearing or removing anything, including text ranges,
  rows, columns, tabs, slides and elements. To remove content from a file
  the tool made, overwrite it with `write`, `update` or `edit`.
- Changing the account: Gmail settings, filters, forwarding, delegation,
  send-as, vacation, labels and mailbox state; contacts, Keep, Meet, YouTube
  and Photos changes.
- Admin, auth and token commands, gog's own config (including its no-send
  guard), backup, and request batches.
- Running code or talking to anything but Google's APIs: hooks and webhooks,
  Apps Script `run`, the MCP server, the generic `api call`, Zoom, email
  tracking, gog's update check, and `slides create-from-markdown` (its
  `--mmdc` runs any program).
- Commands whose side effects the wrapper can't bound: `docs sed`,
  `sheets find-replace` and `sheets copy-paste` (they can bring in or propagate
  formulas or images).
- Any command not classified above. The default for a command nobody has
  reviewed is refused.

Reads are allowed everywhere, including reads of settings, sharing and
permissions. The data the agent reads is the data your token can read.

### Keeping it true

- **gog refuses everything not in `allowed-commands`**, using its own
  `--enable-commands-exact`. Aliases resolve first, so `gog gmail read` is
  `gmail thread get`. The top-level shortcuts (`gog send`, `gog upload`) are
  separate commands with their own lines.
- **The wrapper resolves the command before it applies the flag rules.** It
  asks gog, with nothing enabled, which command the arguments are, so
  `drive upload`, `upload` and any alias get the same rules. If gog can't name
  a command (a usage error, `--help`), the wrapper runs it with nothing
  enabled. Docs files are copied into the job's own `/tmp` and scanned, and the
  copy is what runs, so the agent can't change a file after the check. In docs commands stdin
  is reachable only as `--file -`, which is scanned; gog never reads it directly.
- **A new gog command or flag fails CI until someone decides.**
  [`tests/kits/tools-policy.py`](../../tests/kits/tools-policy.py) lists every
  command in the built image's gog (hidden ones too) and fails unless allowed
  plus refused equals that list exactly. It also checks that each rule names a
  real flag (every spelling of it), that the built wrapper enforces it, and
  that no allowed command has a flag that runs a program, picks a destination,
  converts a file, takes HTML, or evaluates formulas without a rule. The
  scheduled build therefore stops publishing `:stable` after a gog release that
  adds commands or such flags, until the lists and rules are updated.
- **The wrapper's settings can't be changed.** The wrapper rejects
  `--enable-commands`, `--access-token`, `--home` and similar flags, and `--`.
  It runs gog with a clean environment and an empty config folder. If the
  allowlist or the rules are missing, it refuses to run, since an empty list
  would allow everything.
- **Google limits edits, not visibility.** With `drive.file`, edits work only on
  files this tool created (see the note under the credential).

Known limit: plain text can still carry data to a place you later look at it
(a draft, a document). The policy stops the agent from sending it anywhere;
it doesn't stop it from writing it somewhere you can read.

## Develop

```sh
docker buildx build --platform linux/arm64 --load -f kits/marsh-tools/kit.yaml -t marsh-tools:candidate kits/marsh-tools
tests/kits/tools-smoke.sh marsh-tools:candidate
```

To try a change in marsh before publishing, point both commands at the Kit
folder in `commands.json`. `marsh status` shows where that file is.
