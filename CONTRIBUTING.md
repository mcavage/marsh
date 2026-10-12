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
it), `cargo-nextest`, Python 3.10+, Node 22, GNU Bash 5.3 (the test oracle),
and `jq`.

```sh
brew install jq bash node cargo-nextest docker/tap/sbx
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

The sbx-backed tests need the fixture Kit, which release CI publishes:

```sh
make fixture-ref                       # writes target/fixture-ref; or pass DEV_KIT=REPO@sha256:...
```

To publish your own fixture instead:
`make kit-publish-fixture KIT_REPOSITORY_PREFIX=docker.io/YOU KIT_REPOSITORY_NAME_PREFIX=marsh-`.

## Testing

There are two kinds of tests: VM-free ones (`make test`, which CI runs) and
ones that drive real `sbx` microVMs (only on a Mac, by hand).

| command | runs | where |
|---|---|---|
| `make test` | `test-rust`, `test-harness`, `test-model` (below); no VMs, no credentials | your machine; CI on Linux arm64 and macOS |
| `make test-rust` | `cargo fmt --check`, clippy, every workspace test under `cargo-nextest` (one process per test), doctests, the `#[ignore]`d MCP load callers against the built binaries | CI on both |
| `make test-harness` | onboarding journeys through the shipped binaries, MCP protocol callers, the Kit publisher, kit adapter scripts, self-tests of the acceptance/perf/release tooling | CI on both |
| `make test-model` | quick TLA+ sweep (Docker, or `TLC_RUNNER=java`) | CI's model job runs every config |
| `make check` | fast real smoke of the installed dev product (~7 s warm) | Mac, after `make dev` |
| `make verify` | `check` plus the highest-signal sbx suites (~3 min) | Mac, before you push |
| `make regress` | every sbx suite (~12 min), then `make test` with the full model | Mac, before a release |
| `make acceptance` | builds a fresh candidate, runs the full gate with a receipt | Mac, the release run |

`make test` needs `cargo-nextest`; it stops with an install hint rather than
falling back to `cargo test`, so it runs exactly what CI runs. CI uses the
nextest `ci` profile (`NEXTEST_PROFILE=ci`: no fail-fast). A test that runs
longer than its slow timeout (`.config/nextest.toml`) is killed and reported
by name. There are no retries.

`make verify` and `make regress` run `tests/regress.py`, the one scheduler for
sbx suites. Each suite gets its own scope and evidence directory, at most 3 run
at once (`REGRESS_JOBS=N`; `REGRESS_JOBS=1` runs them one at a time; it backs
off when load or disk is high), and a failure keeps its log in
`target/regress/` (`last.json` is the summary). `ONLY=` and `SKIP=` take suite
names, families, or scenario ids: `make regress ONLY=smoke`,
`make regress ONLY=workspaces`, `make regress ONLY=W01-plain-bash,P02-self-spawn-bash-c`
(list them with `python3 tests/regress.py --list --kit REF`). With `ONLY`,
`make regress` runs just those suites. `VERIFY_DEPS=` skips the `make dev`
rebuild.

### What CI covers, and what needs your Mac

CI runs `make test-rust` and `make test-harness` on Linux arm64 and on macOS,
the full TLA+ sweep, and the docs and link checks. On Linux that includes the
shell and its Bash differential tests (against GNU Bash 5.3) and every other
workspace crate; host-side tests use a fake `sbx` and temporary homes. Nothing in CI
starts a microVM, so `make check`, `make verify`, `make regress`, and
`make acceptance` run only on a Mac. The reasons:

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

A Linux arm64 machine runs `make test-rust` and `make test-harness` without a
Mac, Docker, `sbx`, or credentials; that is CI's Linux job. You need Rust
1.95.0, `cargo-nextest`, Python 3.10+, Node 22, and GNU Bash 5.3 first on
`PATH` (`bash_audit` and the other differential tests compare marsh with the
`bash` on `PATH`; include `bash --version` when you report results). On a small
disk, set `CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_TEST_DEBUG=0`.

## Before you send a change

Run `make test`. Add focused tests for the crate you changed, and on a Mac run
`make verify` (`make dev` first). `make acceptance` builds a fresh candidate
and runs the full gate; it is the release run.

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
make test-model                        # quick sweep; MODEL_FLAGS= runs every config
docs/model/check.sh                    # every config (Docker, or TLC_RUNNER=java with a local JDK)
```

A model pass supports a review; it is not evidence about the Rust code.

### Docs

User docs live in `docs/` and become [runmar.sh](https://runmar.sh). Commands
shown must match `marsh --help`. `make site` builds the site and checks every
link; `make man` renders the manual page from `docs/man/marsh.1.md`. Examples
marked `<!-- doc-test: ... -->` in `docs/split.md`, `docs/agents.md`, and
`docs/guides/` run in the acceptance suite (a guide swaps its model calls for
fixture commands with `replace=`).

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
