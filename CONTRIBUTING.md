# Contributing to marsh

Build a development install, test your change, and send a pull request. The
[design index](docs/design/README.md) explains how the parts fit together;
[AGENTS.md](AGENTS.md) lists the project rules.

Open an issue first for a large or cross-component change. Small fixes can go
straight to a pull request. A broken documentation example is a bug too.
To share a Kit, a pipeline, or an AGENTS.md recipe instead of code, post it in
[Show and tell](https://github.com/mcavage/marsh/discussions).

## Building on a Mac

You need an Apple Silicon Mac with Xcode Command Line Tools, Docker Desktop
(with Buildx), `sbx` 0.47.0 or newer, Rust 1.99.1 (`rust-toolchain.toml` picks
it), `cargo-nextest`, Python 3.10+, Node 24.11+ (24.x), GNU Bash 5.3 (the test oracle),
and `jq`.

```sh
brew install jq bash node cargo-nextest docker/tap/sbx
rustup toolchain install 1.99.1 --profile minimal --component rustfmt --component clippy
sbx login
```

`make dev` builds everything and installs a development copy into
`~/.marsh-dev`, outside the checkout:

```sh
make dev
~/.marsh-dev/bin/marsh                 # run it from any project
```

That install also enables `marsh --dev`. Its shell gets a scoped host `sbx`
broker so you can build and test marsh from inside marsh
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

Run `make test` without VMs or credentials. Use the Mac-only gates to test the
real `sbx` stack:

| command | runs | where |
|---|---|---|
| `make test` | `test-rust`, `test-harness`, `test-model` (below); no VMs, no credentials | your machine; CI on Linux arm64 and macOS |
| `make test-rust` | `make lint` (`cargo fmt --check`, clippy), `make test-unit` (every workspace test under `cargo-nextest`, one process per test, and doctests), `make test-callers` (the ignored binary callers and the typed ACP host caller) | CI on both |
| `make test-harness` | `make test-bash` (Linux: the Bash differential suites, `tests/acceptance/bash_suites.py`, GNU Bash 5.3.0), `make test-entrypoints` (onboarding journeys through the shipped binaries, MCP protocol callers), `make test-tools` (the Kit publisher, kit adapter scripts, self-tests of the acceptance/perf/release tooling) | CI on both (suites: Linux; tools: Linux, and macOS on `main`) |
| `make test-model` | quick TLA+ sweep (Docker, or `TLC_RUNNER=java`) | CI's model job runs every config |
| `make check` | real smoke test of the installed dev product | Mac, after `make dev` |
| `make verify` | `check` plus the highest-signal sbx suites | Mac, before you push |
| `make regress` | every sbx suite, then `make test` with the full model | Mac, by hand |
| `make acceptance` | builds a fresh candidate, runs the full gate with a receipt | Mac, by hand (the full acceptance run) |

`make test` needs `cargo-nextest`; it stops with an install hint rather than
falling back to `cargo test`, so it runs exactly what CI runs. CI uses the
nextest `ci` profile (`NEXTEST_PROFILE=ci`: no fail-fast). Nextest kills tests
that exceed their slow timeout (`.config/nextest.toml`) and reports them by
name, without retrying. Afterward, `tests/leaked_processes.py` finds surviving
processes by their test-build path or `MARSH_TEST_RUN` marker. It prints and
kills those processes, then fails the run.

If your test starts a child in a separate process group or session, use
`common::spawn_group` and `common::wait_for_exit`. They clean up the whole
group if the test fails or the child exceeds its time budget, even when
nextest's own group kill cannot reach it.

Tests must not pass your login environment to a shell: it can hold credentials.
Shells and the product start from an allowlist (`PATH`, `HOME`, `TERM`,
`LANG`, `LC_*`, `TMPDIR`, `USER`, `LOGNAME`, plus what each caller sets):
`common::command` in the Rust tests, `tests/acceptance/launch_env.py` in the
harnesses and `tests/regress.py`. `make test`, `make check` and `make regress`
export a credential-shaped canary (`MARSH_TEST_CANARY_API_KEY`), and the
checks fail if any credential-shaped name reaches a test shell.

`make verify` and `make regress` use `tests/regress.py` to schedule sbx suites.
Each suite gets its own scope and evidence directory. Up to 3 run at once;
set `REGRESS_JOBS=N` to change that, or `REGRESS_JOBS=1` to run them serially.
The scheduler reduces concurrency when load or disk use is high. Failures keep
their logs in `target/regress/`; `last.json` is the summary. `ONLY=` and `SKIP=`
take suite names, families, or scenario ids: `make regress ONLY=smoke`,
`make regress ONLY=workspaces`, `make regress ONLY=W01-plain-bash,P02-self-spawn-bash-c`
(list them with `python3 tests/regress.py --list --kit REF`). With `ONLY`,
`make regress` runs just those suites. `VERIFY_DEPS=` skips the `make dev`
rebuild.

`make check` keeps its scope warm between runs: a daemon and VMs behind
`target/check/`. Run `make check-reset` before you delete a checkout or
`git worktree remove` one; otherwise the scope's daemon and VMs outlive its
ownership map. Each warm scope is also registered outside the checkout (one
entry per scope under `~/Library/Application Support/marsh-dev/scopes`, holding
its home path and a copy of its VM ownership map that the daemon itself keeps
current), so if you forget, `make check-gc` (also run at the start of
`make check`, `make verify` and `make regress`) stops that scope's processes and
removes its VMs, only by the recorded VM name and UUID
(`tests/acceptance/scope_registry.py`).

### What CI covers, and what needs your Mac

CI splits `make test` into its pieces (`make lint`, `test-unit`,
`test-callers`, `test-bash`, `test-entrypoints`, `test-tools`, `test-model`)
and runs them as parallel jobs; `CI passed` is the one required check. A pull
request and a push to main run only the lanes their changed files need
(`scripts/ci-changes.sh`). Documentation runs the docs/link checks; image inputs
run image builds and tooling tests, and the Kits and command tables also run
the Rust lanes (the Rust tests read them); Rust
changes run the Linux and macOS code lanes. Linux runs the full workspace and Bash differential
tests (against GNU Bash 5.3). macOS PR jobs split the host tests between two
runners; selected macOS jobs on main also run the platform-neutral workspace
tests and tooling self-tests. Manual CI runs check everything, and so does the run release.yml
makes on the commit it is about to build (below). A push to `main` runs only
the lanes its diff selects, so "CI passed" on a `main` commit does not mean
`make test` passed on it. Stable, nightly and rehearsal builds re-run every
lane on the commit they build (a nightly still picks its commit from push CI);
per-commit Kit images follow the push run.

TLA+ checks run when specs, configs, their runner or shared build/CI inputs
change. Normal CI excludes the huge configs; those run weekly and in
a manual `make regress` ([model.yml](.github/workflows/model.yml)); no
release waits for them. Per-commit image publishing runs only when the
passing CI run built images.
The macOS release build runs in the nightly and release
workflows, not in CI. Rust jobs use content-addressed `sccache` compiler
results across source changes. The isolated Python harness tests run
concurrently (`HARNESS_JOBS=3`; set it to 1 to serialize). Host-side tests use
a fake `sbx` and temporary homes. Nothing in CI starts a microVM, so `make check`, `make verify`, `make regress`, and
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
`make verify` before you push, and `make regress` when you change
something only the real stack exercises. No release runs either for you.

Prefer tests that run the shipped binaries and check output, exit status,
receipts, and cleanup over unit tests that restate the implementation. Never
use real credentials in unit tests or VM-free harnesses. The sbx acceptance
suites use fixture Kits, not provider accounts.

## Building on Linux

A Linux arm64 machine runs `make test-rust` and `make test-harness` without a
Mac, Docker, `sbx`, or credentials; that is CI's Linux job. You need Rust
1.99.1, `cargo-nextest`, Python 3.10+, Node 24.11+ (24.x), and GNU Bash 5.3 first on
`PATH` (`bash_audit` and the other differential tests compare marsh with the
`bash` on `PATH`; include `bash --version` when you report results). On a small
disk, set `CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_TEST_DEBUG=0`.

## Before you send a change

Run `make test`. Add focused tests for the crate you changed, and on a Mac run
`make verify` (`make dev` first). `make acceptance` builds a fresh candidate
and runs the full acceptance gate, by hand.

### Shell behavior

The shell is [Brush](https://github.com/reubeno/brush), vendored under
`vendor/brush/` with our changes kept as a small patch queue in
`docs/upstream/brush/`. marsh never runs Bash to implement the shell, and every
divergence from Bash is classified and tested.

To fix a shell bug, reproduce it through the shipped `marsh-local` entrypoint
and compare with Bash (see `crates/marsh/tests/bash_audit.rs`). Then fix it in
Brush: change `vendor/brush/` and fold the change into the patch for its area in
`docs/upstream/brush/` (see `vendor/brush/MARSH-VENDORING.md`). A general Bash fix
should be one upstream Brush could take; marsh-specific behavior stays here.
A suite mismatch you do not fix gets an entry in
`tests/acceptance/bash_divergences.json` and `docs/design/bash-compatibility.md`.

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

TLC checks the model. Review and test the corresponding Rust code too.

### Dependency updates

Managed inputs use explicit versions, package lockfiles and immutable image
digests. Update those inputs and run the relevant builds/tests. Renovate sends
updates through normal CI and merges passing PRs automatically. A new release
waits 3 days (6 hours for the Kits' ACP adapters and Pi) before Renovate
proposes it, except from the trusted vendors Docker, DHI, Anthropic and OpenAI,
whose releases are taken at once (the list is in
[build inputs](docs/design/supply-chain.md#keeping-pins-current)). A new Docker
Compose version is the exception: Renovate moves the services engine's
`docker-compose-plugin` pin in its own PR, which fails
`tests/release/test_compose_pins.py` until you move the SHA-256-pinned copies
the Kits fetch (the PR's note lists them). There is no
checksum/notice-record refresh or generated license-delta approval step.
See [build inputs](docs/design/supply-chain.md).

### Docs

User docs live in `docs/` and become [runmar.sh](https://runmar.sh). Check
commands against the CLI and its subcommand help. `make site` builds the site
and checks internal links; `make site-check` also checks relative links in
Markdown. `make man` renders `docs/man/marsh.1.md`.

The workspace suite's `W29-doc-examples` runs examples marked
`<!-- doc-test: ... -->` across the public docs listed by
`tests/acceptance/doc_examples.py`, including the README, guides, manual, and
agent primer. `P31-agents-doc-examples` runs the examples in `docs/agents.md`.
`replace=` substitutes fixture commands for model calls; these runs check the
surrounding shell composition, not model answers. They do not cover unmarked
transcripts or the full journey from a fresh install to a working agent.
Label fixture results as such.

Write task guides around what the reader needs to do. Keep schemas and exact
limits in reference pages, and implementation rationale in design documents.
Describe current behavior in one place rather than retaining review rounds
alongside it. Preserve useful warnings where an example grants access.

Keep agent handoffs, private machine paths, and session transcripts out of the
repository. Add new user pages to the site navigation and descriptions so they
also appear in the full manual. Check the deployed page after publishing.

## Pull requests

Say what changed for users, what failure it fixes, exactly what you ran, and
what you did not test. Include platform, Rust, Bash, and `sbx` versions where
they matter. Don't paste credentials, guest-home contents, or agent
transcripts. Security problems go to [SECURITY.md](SECURITY.md), not an issue.

By contributing you agree that your work is licensed under the Apache License
2.0. Please follow the [code of conduct](CODE_OF_CONDUCT.md).

## Releases

CI stamps the release version into the build without committing it. Do not
edit a version number in the source.

Before a stable, nightly or rehearsal build builds anything, release.yml runs
every CI lane (the jobs `make test` is split into, as `ci.yml` with `all`) on
the exact commit it is about to build, and stops there if any lane fails.
Nothing in a release starts a microVM: the stock-SBX suites (`make regress`,
`make acceptance`) and the huge TLA+ configs are manual qualification on a
Mac, not a release gate.

- **Nightly.** Once a day (08:00 UTC), the newest commit on `main` whose CI run
  passed is published as `X.Y.(Z+1)-nightly.<UTC commit time>.g<sha>`, unless
  `main` has not moved since the last nightly: a GitHub prerelease (the newest
  10 are kept), Docker Hub images tagged `:nightly`, and the `marsh-nightly`
  Homebrew formula. A separate workflow
  ([prune-nightlies.yml](.github/workflows/prune-nightlies.yml)) deletes older
  nightly image tags on Docker Hub; it needs a token with delete permission.
- **Kit images, per commit.** Every commit on `main` whose CI run passes also
  builds and publishes the Kit images (`:nightly`, and the stable `:latest`
  when the rule at the top of `release.yml` allows), so agent updates reach
  users the same day. That run follows the push CI run's image job instead of
  a full CI run. It does not build marsh, publish a prerelease, or touch
  Homebrew.
- **Stable.** Push a tag `vX.Y.Z` on a commit on `main`. The tag is the
  version. The Release workflow refuses a tag that is not on `main` or not
  `X.Y.Z`, runs every CI lane on the commit, and only then publishes a GitHub
  Release, Docker Hub images tagged `:X.Y.Z` and `:latest`, and the `marsh`
  formula. Stable releases run one at a time. A release of a version older
  than one already tagged, whenever it runs (out of order, or a re-run of
  failed jobs), publishes only its version: the GitHub "Latest" release,
  `:latest` and the formula stay on the newest, and the tap never moves back.

```sh
git switch main && git pull --ff-only
git tag v0.1.3 && git push origin v0.1.3
```

Anyone who can create a `v*` tag can start a stable release. Restrict that
with a tag ruleset in the repository settings.

**Withdrawing a stable release.** Nothing moves a channel back on its own:
each publishing job checks the version tags when it runs, and the tap job
never writes a lower version than the tap has. Prefer releasing a fixed,
higher version, which moves everything forward. To withdraw `vX.Y.Z` in
favor of an earlier `vW.V.U` by hand:

1. Cancel every queued or running Release run of `vX.Y.Z` (`gh run list
   --workflow release.yml`, then `gh run cancel ID`). A job already past its
   tag check still pushes what it decided, so wait for cancelled runs to stop.
2. Delete the release and its tag: `gh release delete vX.Y.Z --cleanup-tag`.
   The `v*` tag ruleset blocks deleting the tag unless you are on its bypass
   list. From then on a run of `vX.Y.Z` fails its tag check and moves nothing.
3. Mark the earlier release Latest: `gh release edit vW.V.U --latest`.
4. Point the moving image tags back. For each image in `vW.V.U`'s run summary
   ("Published images"): `docker buildx imagetools create --tag REPO:latest
   REPO:W.V.U`, and the same for `:release` (Kits and the fixture) or
   `:repaired` (the shell template).
5. Edit `Formula/marsh.rb` in mcavage/homebrew-tap back to `vW.V.U`'s
   formula (the `marsh.rb` asset of its release). No release run will do
   this: the tap never moves back.
6. The nightly formula stops moving too. Nightlies are numbered from the
   newest stable tag (`W.V.(U+1)-nightly.…` now), which is lower than the
   `X.Y.(Z+1)-nightly.…` already in the tap, so the tap job refuses them
   until a stable release at or above `vX.Y.Z` is tagged. To resume it
   sooner, after the next nightly is published copy its `marsh-nightly.rb`
   release asset to `Formula/marsh-nightly.rb` in the tap.

Until these are done the channels disagree. Once the tag is gone, a re-run of
a lower version's run moves the image tags (its images job counts it as the
newest) but not the "Latest" mark (its release step refuses a release that
already exists); a new tag for a lower version would move both. Either way
the formula keeps naming `vX.Y.Z` until step 5.

To rehearse a release, run the Release workflow by hand with a version: it
runs the same CI, builds a draft and announces nothing. Its images get only
the moving tag `:draft`, so no tag a user reads moves, and it replaces any
earlier draft release of the same version. Without a version it publishes a
nightly from the branch you pick, any time.
[.github/workflows/release.yml](.github/workflows/release.yml) documents the
secrets and the stages.

## Thanks

marsh's shell is built on [Brush](https://github.com/reubeno/brush), with its
vendored revision and patch queue recorded in
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
