# Contributing to marsh

Thanks for helping. This page covers building, testing, and sending changes.
The [design index](docs/design/README.md) explains how the parts fit together,
and [AGENTS.md](AGENTS.md) is the short version of the project rules.

For a large or cross-component change, open an issue first. Small fixes can go
straight to a pull request. If you have Kits or recipes to share rather than
code, post them in
[Show and tell](https://github.com/mcavage/marsh/discussions).

## Building on a Mac

You need an Apple Silicon Mac with Xcode Command Line Tools, Docker Desktop
(with Buildx), `sbx` 0.45.0 or newer, Rust 1.95.0 (`rust-toolchain.toml` picks
it), Python 3.10+, and `jq`.

```sh
brew install jq docker/tap/sbx
rustup toolchain install 1.95.0 --profile minimal --component rustfmt --component clippy
sbx login
```

`make dev` builds everything and installs a development copy into
`~/.marsh-dev`, outside the checkout:

```sh
make dev
~/.marsh-dev/bin/marsh                 # run it from any project
```

That install also enables `marsh --dev`, a shell whose `sbx` is a confined
broker, so you can build and test marsh from inside marsh
([design/self-development.md](docs/design/self-development.md)):

```sh
~/.marsh-dev/bin/marsh --dev -c pi-dev # Pi, in a dev shell, in this checkout
```

The acceptance smoke needs the fixture Kit, which release CI publishes:

```sh
make fixture-ref                       # writes target/fixture-ref
make dev-smoke                         # or: make dev-smoke DEV_KIT=REPO@sha256:...
```

To publish your own fixture instead:
`make kit-publish-fixture KIT_REPOSITORY_PREFIX=docker.io/YOU KIT_REPOSITORY_NAME_PREFIX=marsh-`.

## Testing

Everything runs against the installed dev product and real `sbx`.

| command | time | when |
|---|---|---|
| `make check` | ~7 s warm, ~30 s cold | after every change (`make dev && make check`; `make check-reset` stops its scope) |
| `make verify` | ~3 min | before you push: `check` plus the highest-signal acceptance scenarios |
| `make regress` | ~12 min | before a release: every sbx-backed suite, then the host suites |

`make verify` and `make regress` run `tests/regress.py`. Each suite gets its
own scope and evidence directory, at most 3 run at once (`REGRESS_JOBS=N`; it
backs off when load or disk is high), and a failure keeps its log in
`target/regress/` (`last.json` is the summary). Pick suites with
`ONLY=workspaces-1 SKIP=self-dev` (`python3 tests/regress.py --list`), or run
them one at a time with `make regress-serial`.

`make dev-acceptance`, `make dev-split`, and `make dev-processes` run single
areas (`ONLY=P02-...` picks one scenario). `make help` lists the rest.

### What CI covers, and what needs your Mac

Almost everything runs in CI on Linux arm64: the shell and its Bash
differential tests, the guest and worker crates, the ACP/MCP protocol callers,
the publisher, docs links, and the TLA+ model. CI's macOS job builds the host
side and runs its tests against a fake `sbx`.

What it cannot do is start a real microVM, so `make check`, `make verify`,
`make regress`, and `make acceptance` run on a Mac. The reasons:

- A microVM needs a hypervisor. GitHub's hosted macOS runners are themselves
  VMs on Apple Silicon without nested virtualization, and the hosted Linux
  arm64 runners have no KVM.
- Hosted Linux x64 runners have KVM, and stock `sbx` runs on Linux with it. But
  marsh's shell image and Kits are `linux/arm64`, and the host side admits a
  workspace with APFS file IDs and `clonefileat`
  (`crates/marsh-host-identity`), which exist only on macOS.

A Linux lane for the real stack would need a Linux workspace-identity backend
and amd64 guests, which is product work. Until then, the real gate is a Mac:
`make verify` before you push, `make regress` before a release.

Prefer tests that run the shipped binaries and check output, exit status,
receipts, and cleanup over unit tests that restate the implementation. Tests
never use real credentials.

## Building on Linux

A Linux arm64 machine can build and test the shell and protocols without a Mac,
Docker, `sbx`, or credentials. This is what CI's Linux job runs:

```sh
export CARGO_TARGET_DIR="/tmp/marsh-target-$(id -u)" CARGO_INCREMENTAL=0
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
make onboarding                        # real shell journeys through the shipped binaries
cargo test --locked -p marsh --test smoke --test bash_audit --test results_cli
make mcp-test
```

`bash_audit` compares marsh with GNU Bash from `PATH`; include `bash --version`
when you report results. On a small disk, set `CARGO_PROFILE_DEV_DEBUG=0` and
`CARGO_PROFILE_TEST_DEBUG=0`.

## Before you send a change

```sh
make verify-static
```

That runs `cargo fmt --check`, clippy, every Rust test (under `cargo-nextest`
if installed), and a quick TLA+ sweep, with no VMs. Add focused tests for the
crate you changed, and on a Mac run `make dev-smoke`. `make acceptance` builds
a fresh candidate and runs the full gate; it is the release run.

### Shell behavior

The shell is [Brush](https://github.com/reubeno/brush), vendored under
`vendor/brush/` with our changes kept as a small patch queue in
`docs/upstream/brush/`. marsh never runs Bash to implement the shell, and every
divergence from Bash is classified and tested.

To fix a shell bug, reproduce it through the shipped `marsh-local` entrypoint
and compare with Bash (see `crates/marsh/tests/bash_audit.rs`). Then fix it in
Brush as a small patch: add it to `docs/upstream/brush/` in order, apply it
under `vendor/brush/`, and describe it in the patch README. A general Bash fix
should be one upstream Brush could take; marsh-specific behavior stays here.

### VMs, workers, and the model

VMs come from stock `sbx` through its public CLI only, and marsh touches only
VMs in its ownership map. A job is never replayed after a lost connection: the
VM is quarantined instead.

If you change VM ownership, worker lifecycle, mounts, cancellation, or grants,
update the [TLA+ model](docs/model/README.md) in the same change: the
invariant, its negative control, and the code-location table.

```sh
docs/model/check.sh                    # Docker, or TLC_RUNNER=java with a local JDK
```

A model pass supports a review; it is not evidence about the Rust code.

### Docs

User docs live in `docs/` and become [runmar.sh](https://runmar.sh). Commands
shown must match `marsh --help`. `make site` builds the site and checks every
link; `make man` renders the manual page from `docs/man/marsh.1.md`. Examples
marked `<!-- doc-test: ... -->` in `docs/split.md` and `docs/agents.md` run in
the acceptance suite.

## Pull requests

Say what changed for users, what failure it fixes, exactly what you ran, and
what you did not test. Include platform, Rust, Bash, and `sbx` versions where
they matter. Don't paste credentials, guest-home contents, or agent
transcripts. Security problems go to [SECURITY.md](SECURITY.md), not an issue.

By contributing you agree that your work is licensed under the Apache License
2.0. Please follow the [code of conduct](CODE_OF_CONDUCT.md).

## Releases

Nobody edits a version number. CI stamps it into the build and never commits it.

- **Nightly.** Every commit on `main` whose CI run passes is published as
  `X.Y.(Z+1)-nightly.<UTC commit time>.g<sha>`: a GitHub prerelease (the
  newest 10 are kept), Docker Hub images tagged `:nightly`, and the
  `marsh-nightly` Homebrew formula. A separate workflow
  ([prune-nightlies.yml](.github/workflows/prune-nightlies.yml)) deletes older
  nightly image tags on Docker Hub; it needs a token with delete permission.
- **Stable.** Push a tag `vX.Y.Z` on a commit that is on `main`. The tag is the
  version. That publishes a GitHub Release, Docker Hub images tagged `:X.Y.Z`
  and `:latest`, and the `marsh` formula.

```sh
git tag v0.1.3 && git push origin v0.1.3
```

To rehearse a release, run the Release workflow by hand with a version: it
builds a draft and announces nothing. Without a version it publishes a nightly
from the branch you pick. [.github/workflows/release.yml](.github/workflows/release.yml)
documents the secrets and the stages.

## Thanks

marsh's shell is built on [Brush](https://github.com/reubeno/brush): upstream
revision `1389a8e` plus the patch queue in
[docs/upstream/brush](docs/upstream/brush/README.md). Most of the patches fix
Bash compatibility (job control, signals and traps, `wait`, aliases, namerefs,
subshells, globstar, redirections, startup files); the rest add marsh's
registered commands, `split`/`join`, and `fanout`/`collect`. Thank you to
Reuben Olinsky and the Brush contributors, and to the
[Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) team.

Contributors:

- [Eric Curtin](https://github.com/ericcurtin): made the flaky CI tests
  deterministic, and fixed the signal-handling race and the forked-shell hang
  they exposed in the shell (Brush patches 0050 and 0051).
