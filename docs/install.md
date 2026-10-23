# Install

Install marsh and Docker Sandboxes on your Mac, then open your project in a
Linux shell. The [Quickstart](quickstart.md) covers provider credentials and
your first agent calls.

## Requirements

You need an **Apple Silicon Mac** and Docker Sandboxes (`sbx`) **0.47.0 or
newer**, signed in. Check the installed version with `sbx version`.
It does not run on Intel Macs or Linux hosts.

Docker Desktop is not required for a release install. You need Desktop with
Buildx only when building marsh from source or running a Kit from a local
source directory. Release binaries are unsigned and not notarized.

## Install marsh

### Homebrew

In your **Mac terminal**:

```sh
brew install docker/tap/sbx mcavage/tap/marsh
sbx login
```

This installs `sbx` as a cask and marsh as a formula. The marsh formula
cannot depend on that cask, so the command names both. If you install only
marsh, it will tell you to install `sbx` before starting.

The formula puts `marsh`, `msh`, `marshd`, and `marsh-mcp` on your `PATH`,
installs guest binaries under its `libexec`, and includes the `marsh(1)` and
`msh(1)` man pages.

### curl

To install marsh with the release script, first install and sign in to
Docker Sandboxes. These commands still use Homebrew for `sbx`:

```sh
brew install docker/tap/sbx
sbx login
curl -fsSL https://runmar.sh/install | sh
```

The script installs into `~/.local`. It checks the host architecture and
`sbx` version, downloads the latest GitHub release, and verifies its SHA-256
checksum. It requires `curl`, `tar`, and `shasum`. If `sbx` is absent, the
script warns but installs marsh; you must install `sbx` before marsh can run.

The checksum catches corrupt downloads, not a compromised release.

The install contains:

```text
~/.local/bin/marsh, msh, marshd, marsh-mcp
~/.local/libexec/marsh/        guest binaries, command registry, shell image reference
~/.local/share/man/man1/marsh.1, msh.1
~/.local/share/licenses/marsh/
```

The script does not edit your shell profile. If `~/.local/bin` is not on
your `PATH`, add it to the profile for your Mac shell. For zsh:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
exec zsh
```

To use another install prefix:

```sh
curl -fsSL https://runmar.sh/install | sh -s -- --prefix /usr/local
```

`--prefix` must be absolute. If the directory is not writable, the script
asks before using `sudo`. Without a terminal it exits unless you pass
`--yes`. Use `--version VERSION` to pin a release. `MARSH_PREFIX` and
`MARSH_VERSION` set the same values as those flags.

If you want to read the installer first:

```sh
curl -fsSL https://runmar.sh/install -o install.sh
less install.sh
sh install.sh
```

### Release tarball

Download `marsh-VERSION-darwin-arm64.tar.gz` and its `.sha256` file from
[GitHub Releases](https://github.com/mcavage/marsh/releases). Substitute your
chosen version and run
`shasum -a 256 -c marsh-VERSION-darwin-arm64.tar.gz.sha256` in the download
directory. Extract the archive and copy its `bin/`, `libexec/`, and `share/`
directories into your chosen prefix.

Keep `bin/` and `libexec/marsh/` under the same prefix. The host executable
looks for guest binaries in `../libexec/marsh`.

## Check the install

In your **Mac terminal**, change to an existing project directory, then run:

```sh
marsh --version
marsh -c 'pwd; uname -sm'
```

The first command prints the installed version. The second starts a
noninteractive guest shell and prints your project's absolute Mac path,
followed by `Linux aarch64`. It returns you to the Mac terminal when done.
The first launch can take a few minutes while Docker Sandboxes prepares the
shell VM. Later launches reuse that VM.

Agent calls need a provider credential as well as `sbx login`. Follow the
[Quickstart](quickstart.md) before making your first call.

## Upgrade

Upgrade marsh with Homebrew; Kits update themselves.

```sh
brew upgrade mcavage/tap/marsh
marsh stop                   # when nothing is running; the next launch uses the new marsh
```

The commands (`claude`, `codex`, `pi`, `shell`, `pix`, `hermes`, and Kits you
installed from a tag) move to newer images by themselves: marsh checks each
time its daemon starts and once a day, and switches a command when it has no
running jobs. The next time you use an updated command, marsh prints one line
such as `marsh: claude updated (2.1.305 → 2.1.307)`. `marsh status` lists
recent updates. To update right now, run `marsh stop` when nothing is running,
then use marsh again. To turn updates off, run `marsh config auto-update off`.
See [Updates](kits.md#updates).

A command that was updated keeps its newer image when marsh itself is
upgraded. `marsh kit uninstall NAME` returns a packaged command to the version
your marsh shipped.

For a script install, run the installer again instead of `brew upgrade`. For a
tarball install, repeat the extraction and copy steps with the new release.

A running daemon keeps the old marsh until `marsh stop`: finish its jobs,
`exit` every marsh shell, then run `marsh stop` from your **Mac terminal**.
Stop refuses while shells or jobs are active; see
[Removing VMs](troubleshooting.md#removing-vms).

After an `sbx` upgrade, marsh replaces its daemon automatically once no shell
or job is running. In projects where you registered an MCP client, rerun
`marsh mcp install CLIENT` with that client's name because the registration
stores the `sbx` path.

## Uninstall

Finish jobs and leave every marsh shell with `exit`. In your **Mac
terminal**, remove marsh's VMs and stop its daemon:

```sh
marsh stop
```

For Homebrew:

```sh
brew uninstall marsh
```

For a default `~/.local` script or tarball install, remove its installed
files. Adjust these paths if you used another prefix:

```sh
rm -f ~/.local/bin/{marsh,msh,marshd,marsh-mcp} ~/.local/share/man/man1/{marsh,msh}.1
rm -rf ~/.local/libexec/marsh ~/.local/share/licenses/marsh
```

These commands leave user data in place. Remove any of the following only
if you no longer need its contents:

| Path | Contents |
| --- | --- |
| `~/.marsh` | Guest home, including saved agent logins and history |
| `~/Library/Application Support/marsh` | Host control data and job records |
| `~/Library/Application Support/marsh-mcp-scopes` | Generated MCP server scopes |
| `~/Library/Caches/marsh` | Cached Kit images |

## Docker Sandboxes policy

The release shell image and Kits are published as
`docker.io/mcavage/marsh-*`, with version tags and `:latest`.
If your Docker Sandboxes policy restricts Kit sources, allow
`docker.io/mcavage`.

## From source

You need Docker Desktop with Buildx, Rust 1.95, Python 3.10 or newer, `jq`,
and the `sbx` version listed under [Requirements](#requirements). See
[CONTRIBUTING.md](../CONTRIBUTING.md) for the development loop.

```sh
git clone https://github.com/mcavage/marsh
cd marsh
make install
```

`make install` builds and installs into `~/.local`. To package a local
build as a tarball instead:

```sh
make dist-local
```

The archive is
`target/dist/marsh-VERSION-local-darwin-arm64.tar.gz`. Extract that one
archive into an empty directory and copy its `bin/`, `libexec/`, and `share/`
into your prefix, as for a release tarball.

A local archive includes `LOCAL-BUILD.txt`, and `marsh --version` identifies
it as a local build. Its Kits are unpinned source directories, built on first
use, and its shell image lives in that Mac's `sbx` image store. Install it
only on the Mac that built it. The release target, `make dist`, refuses
unpinned Kits and directs you to `make dist-local`.

Next, follow the [Quickstart](quickstart.md), or consult
[Troubleshooting](troubleshooting.md) if the install does not start.
