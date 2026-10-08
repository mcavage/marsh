# Install

marsh is Bash with `split`, `fanout`, `join`, and `collect` for running agents
in parallel, each command in its own sandbox.

```sh
brew install docker/tap/sbx mcavage/tap/marsh
sbx login
```

Then, from any project directory:

```console
$ cd ~/src/some-project
$ marsh --version                     # marsh VERSION
$ marsh -c 'pwd; uname -sm'
/Users/you/src/some-project
Linux aarch64
```

The command ran in a Linux VM at your project's Mac path. The first run
creates the shell VM, so it is slower. Later runs reuse it.

## Requirements

- A Mac with Apple Silicon. marsh does not run on Intel Macs or Linux hosts.
- Docker Sandboxes (`sbx`) 0.45.0 or newer, signed in. Check with
  `sbx version`. marsh is tested with 0.46.0. marsh uses `sbx` as installed and
  never changes it. Error messages for a missing or old `sbx` are in
[Troubleshooting](troubleshooting.md#missing-or-old-sbx).

The release binaries are not signed or notarized.

Docker Desktop is not needed to run a release. The shell and every Kit run in
VMs that `sbx` creates. Docker Desktop with Buildx is needed only to build
marsh from source or to run a Kit from a local source directory.

## Install marsh

### Homebrew

The commands at the top of this page are the Homebrew install. Run
`sbx login` once. The marsh formula installs:

- `marsh`, `msh`, `marshd`, and `marsh-mcp` on your `PATH`
- the Linux binaries for the VMs, under the formula's `libexec`
- the `marsh(1)` and `msh(1)` man pages

The formula does not install `sbx`, which ships only as the `docker/tap/sbx`
cask, and a formula cannot depend on a cask. If `sbx` is missing, the
formula's caveats and marsh print:

```
marsh needs Docker Sandboxes: brew install docker/tap/sbx; then `sbx login`
```

### curl

To install without Homebrew, first install and sign in to `sbx`:

```sh
brew install docker/tap/sbx && sbx login
curl -fsSL https://runmar.sh/install | sh
```

The script checks that this is an Apple Silicon Mac and that `sbx` is new
enough, downloads the latest release from GitHub, verifies its SHA-256
checksum, and installs into `~/.local`. It needs `curl`, `tar`, and `shasum`.
If `sbx` is missing it warns and installs anyway, but marsh will not start
without `sbx`.

The checksum comes from the same release as the download. It detects a
corrupt file, not a malicious release.

Installed files:

```
~/.local/bin/marsh, msh, marshd, marsh-mcp
~/.local/libexec/marsh/        Linux binaries for the VMs, command registry, shell image reference
~/.local/share/man/man1/marsh.1, msh.1
~/.local/share/licenses/marsh/
```

The script edits no shell profile. If `~/.local/bin` is not on your `PATH`,
it prints a line to add it, for example:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc && exec zsh
```

Options go after `sh -s --`. `MARSH_PREFIX` and `MARSH_VERSION` set the same
values as `--prefix` and `--version`.

```sh
curl -fsSL https://runmar.sh/install | sh -s -- --prefix /usr/local   # asks before sudo
curl -fsSL https://runmar.sh/install | sh -s -- --version 0.1.0
```

`--prefix` must be an absolute path. If the prefix is not writable, the script
asks before using `sudo`. With no terminal to ask on, it exits unless you pass
`--yes`.

To read the script before running it:

```sh
curl -fsSL https://runmar.sh/install -o install.sh
less install.sh
sh install.sh
```

### Release tarball

1. Download `marsh-VERSION-darwin-arm64.tar.gz` and its `.sha256` from
   [GitHub Releases](https://github.com/mcavage/marsh/releases).
2. Check it with `shasum -a 256 -c`.
3. Copy `bin/`, `libexec/` and `share/` into a prefix.

Keep `bin/` and `libexec/marsh/` under the same prefix. `marsh` looks for its
VM binaries in `../libexec/marsh`.

## Upgrade

With Homebrew:

```sh
brew upgrade mcavage/tap/marsh
```

With the curl installer, run it again. With a tarball, repeat the tarball
steps.

A running marsh daemon keeps the old binaries until it stops. When no jobs
are running, run `marsh stop`. The next `marsh` starts the new version.
`marsh stop` refuses while shells or jobs are open
([troubleshooting](troubleshooting.md#removing-vms)).

After you upgrade `sbx`, marsh restarts its idle daemon by itself. Rerun the
`marsh mcp install CLIENT` command in each project where you ran it, because
the registration records the `sbx` path.

## Uninstall

```sh
marsh stop            # removes the VMs marsh created and stops its daemon
```

With Homebrew:

```sh
brew uninstall marsh
```

Otherwise:

```sh
rm -f ~/.local/bin/{marsh,msh,marshd,marsh-mcp} ~/.local/share/man/man1/{marsh,msh}.1
rm -rf ~/.local/libexec/marsh ~/.local/share/licenses/marsh
```

Uninstalling does not remove your data. To remove it as well:

| Path | Contents |
| --- | --- |
| `~/.marsh` | guest home, including agent logins and history |
| `~/Library/Application Support/marsh` | host control directory |
| `~/Library/Application Support/marsh-mcp-scopes` | generated MCP server scopes |
| `~/Library/Caches/marsh` | Kit image cache |

## Docker Sandboxes policy

Release Kits and the shell image are published on Docker Hub as
`docker.io/mcavage/marsh-*`. If your Docker Sandboxes policy restricts Kit
sources, allow `docker.io/mcavage`.

## From source

You need Docker Desktop with Buildx, Rust 1.95, Python 3.10+, `jq`, and `sbx`
(see Requirements). See [CONTRIBUTING.md](../CONTRIBUTING.md) for details.

```sh
git clone https://github.com/mcavage/marsh && cd marsh
make install            # builds and installs into ~/.local
```

To build an installable tarball from your local build and install it like a
release tarball:

```sh
make dist-local         # -> target/dist/marsh-VERSION-local-darwin-arm64.tar.gz
mkdir -p ~/.local
tar -xzf target/dist/marsh-*-local-darwin-arm64.tar.gz -C /tmp
cp -R /tmp/marsh-*-local-darwin-arm64/{bin,libexec,share} ~/.local/
~/.local/bin/marsh --version    # marsh VERSION (local build: REV DATE; not a release)
```

A local tarball is marked: its name has `-local`, it contains
`LOCAL-BUILD.txt`, and `marsh --version` says `local build`. It differs from a
release in two ways:

- Its Kits are unpinned source directories, built on first use.
- Its shell image is a local image in this Mac's `sbx` image store.

Install a local tarball only on the Mac that built it. `make dist`, the
release packaging, refuses unpinned Kits and points you to `make dist-local`.

Next: [Quickstart](quickstart.md) runs an agent and a `split`.
