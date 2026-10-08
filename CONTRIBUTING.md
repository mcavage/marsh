# Contributing to marsh

This file covers building, testing, and sending changes. The
[design index](docs/design/README.md) explains how the parts fit.
[AGENTS.md](AGENTS.md) is the shortest summary of the project rules; it is
written for coding agents, but people can read it too.

Not ready to send code? Share your Kits and recipes in
[Show and tell](https://github.com/mcavage/marsh/discussions).

## Ground rules

- The shell is a patched Brush: upstream source vendored under
  `vendor/brush/`, with the patch queue in `docs/upstream/brush/`. marsh must
  never run Bash to implement the shell. Every divergence from Bash is
  classified and tested.
- VMs come from stock `sbx`, through its public CLI only.
- marsh touches only VMs recorded in its ownership map.
- A job is never replayed after a lost connection; the VM is quarantined.
- Tests use no real credentials.

Open an issue before a large or cross-component change. Small fixes can go
straight to a pull request.

## On a Mac: the dev loop

Prerequisites: Apple Silicon, Xcode Command Line Tools, Docker Desktop with
Buildx, `sbx` 0.45.0 or newer signed in, Rust 1.95.0 with rustfmt and Clippy
(`rust-toolchain.toml` selects it), Python 3.10+, and `jq`.

```sh
brew install jq docker/tap/sbx
rustup toolchain install 1.95.0 --profile minimal --component rustfmt --component clippy
sbx login
```

Build and install a development copy into `~/.marsh-dev`, outside the checkout:

```sh
make dev
~/.marsh-dev/bin/marsh                 # run it from any project
```

`make dev` builds the host binaries, the Linux guest binaries (in Docker), the
shell image, and the packaged Kits, incrementally, and installs them. A
`make dev` install also enables `marsh --dev`, which opens a shell whose `sbx`
is a confined broker, so you can build and test marsh from inside marsh
([design/self-development.md](docs/design/self-development.md)):

```sh
~/.marsh-dev/bin/marsh --dev -c pi-dev # Pi, in a dev shell, in this checkout
```

The acceptance smoke needs the fixture Kit, published by release CI:

```sh
make fixture-ref                       # writes target/fixture-ref
make dev-smoke                         # or: make dev-smoke DEV_KIT=REPO@sha256:...
```

The everyday loop is `make dev && make check` (about 30 s cold, under 10 s
warm; `make check-reset` stops its scope). Run `make regress` before a release.

To publish your own fixture instead:
`make kit-publish-fixture KIT_REPOSITORY_PREFIX=docker.io/YOU KIT_REPOSITORY_NAME_PREFIX=marsh-`.

Other dev targets: `make dev-acceptance`, `make dev-split`, `make dev-processes`
(`ONLY=P02-...` selects one scenario). `make help` lists everything.

## On Linux: contributor checks

A Linux arm64 machine can build and test the shell and protocols without a
Mac, Docker, `sbx`, or credentials. This is what CI runs on Linux.

```sh
export CARGO_TARGET_DIR="/tmp/marsh-target-$(id -u)" CARGO_INCREMENTAL=0
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
make onboarding                        # real shell journeys through the shipped binaries
cargo test --locked -p marsh --test smoke --test bash_audit --test results_cli
make mcp-test
```

`bash_audit` compares marsh with GNU Bash from `PATH`; record `bash --version`
with results. On a small disk, set `CARGO_PROFILE_DEV_DEBUG=0` and
`CARGO_PROFILE_TEST_DEBUG=0`.

## Before you send a change

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

plus focused tests for the crate you changed. Prefer real-caller tests that run
the shipped binaries and check output, exit status, receipts, and cleanup over
unit tests that restate the implementation. On a Mac, run `make dev-smoke`.
`make acceptance` builds a fresh candidate and runs the full gate; it is the
release run.

### Shell behavior

Reproduce the problem through the shipped `marsh-local` entrypoint and compare
with Bash (see `crates/marsh/tests/bash_audit.rs`). Fix it in Brush as a small
patch: add it to `docs/upstream/brush/` in order, apply it under
`vendor/brush/`, and describe it in the patch README. A general Bash fix
should be one upstream Brush could take; marsh-specific behavior stays here.

### Cross-component state

If you change VM ownership, worker lifecycle, mounts, cancellation, or grants,
update the [TLA+ model](docs/model/README.md): the invariant, its negative
control, and the code-location table, together. Then:

```sh
docs/model/check.sh                    # Docker, or TLC_RUNNER=java with a local JDK
```

A model pass supports a review; it is not evidence about the Rust code.

### Docs

User docs are in `docs/` and become [runmar.sh](https://runmar.sh). Commands
shown must match `marsh --help`. `make site` builds the site and checks every
link; `make man` renders the manual page from `docs/man/marsh.1.md`.
Examples marked `<!-- doc-test: ... -->` in `docs/split.md` and
`docs/agents.md` run in the acceptance suite.

## Pull requests

Say what changed for users, what failure it fixes, exactly what you ran, and
what you did not test. Include platform, Rust, Bash, and `sbx` versions where
they matter. Do not paste credentials, guest-home contents, or agent
transcripts. Security problems go to [SECURITY.md](SECURITY.md), not an issue.

By contributing you agree that your contributions are licensed under the
Apache License 2.0. Please follow the [code of conduct](CODE_OF_CONDUCT.md).
