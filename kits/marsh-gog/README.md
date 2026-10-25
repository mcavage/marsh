# marsh-tools

Work tools for agents running in [marsh](https://github.com/mcavage/marsh), as
marsh Kits. An agent runs them like any command (`gog gmail search …`), and
each call becomes a short job in its own sandbox. The tool's credential lives
only in that sandbox's network proxy. The agent never sees it, and can't use
it any other way.

| Kit | Image | What it does |
|---|---|---|
| `gog` | `docker.io/mcavage/marsh-gog:stable` | Google Workspace through [gog](https://github.com/steipete/gogcli): read Gmail, Drive, Docs, Sheets, Slides and Calendar; create Gmail drafts; create documents, sheets and decks and edit the ones it created. It cannot send mail, share, or delete anything. |
| `bamboohr` | `docker.io/mcavage/marsh-bamboohr:stable` | Read-only BambooHR directory: find people, reporting chains, org trees. |

CI rebuilds `gog` within six hours of each gog release, and only publishes it
once the offline smoke test passes.

## Install

```sh
marsh kit install gog docker.io/mcavage/marsh-gog:stable
marsh kit install bamboohr docker.io/mcavage/marsh-bamboohr:stable
```

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

### BambooHR credential

```sh
printf '%s:x' "$BAMBOOHR_API_KEY" | base64 | sbx secret set bamboohr
mkdir -p ~/.marsh/home/.config/bamboohr       # marsh's shared guest home
echo yourcompany > ~/.marsh/home/.config/bamboohr/company
```

## How `gog` stays safe

- **gog refuses everything not listed** in
  [`kits/gog/allowed-commands`](kits/gog/allowed-commands), using its own
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
docker buildx build --platform linux/arm64 --load -f kits/gog/kit.yaml -t marsh-gog:candidate kits/gog
docker buildx build --platform linux/arm64 --load -f kits/bamboohr/kit.yaml -t marsh-bamboohr:candidate kits/bamboohr
tests/smoke.sh marsh-gog:candidate marsh-bamboohr:candidate
```

To try a change in marsh before publishing, point the command at the Kit
folder in `commands.json`. `marsh status` shows where that file is.
