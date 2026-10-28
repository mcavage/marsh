# Build inputs and third-party notices

Cargo builds use `Cargo.lock`, npm builds use `package-lock.json` with `npm ci`,
and container bases are pinned by immutable image digest. The Rust toolchain is
pinned in `rust-toolchain.toml`. Versioned native agent binaries are downloaded
from their publishers over HTTPS and smoke-tested after installation. Codex
archive members are streamed to fixed output paths rather than extracted into
the builder filesystem.

There is no separate release-checksum manifest, generated notice ledger,
license-family classifier, license-delta gate or dependency-record refresh.

## Keeping pins current

Renovate updates Cargo/npm dependencies, GitHub Actions, DHI image digests,
explicit DHI apt-package versions, and the agent versions declared by the Kits.
`.github/renovate.json5` describes the custom version locations and grouping;
`.github/workflows/renovate.yml` runs Renovate hourly and after successful CI.

Updates go through normal CI builds and tests and merge automatically when
those checks pass. To update a dependency manually, edit its version or lockfile
and run the relevant build/tests. No generated metadata needs to be committed.
The Rust toolchain, vendored Brush patch queue and pinned development-tool
source downloads still move through coordinated changes.

## Published images

Release CI builds and publishes the Kit images and shell VM template to
`docker.io/mcavage/marsh-*`. Kit images use the runtime DHI Debian/Node bases;
the shell template uses the Debian development variant. Their exact input
references are in the Dockerfiles and `packaging/shell-image`.

`packaging/image-base/assemble-packages.sh` assembles Docker Engine, shell tools
and each Kit's development tools from apt packages, retaining their dpkg records
and original copyright files. Runtime smoke stages check the installed commands,
agent versions, sandbox account and proxy-CA setup before the final image can be
built. They do not generate a separate notice-verification receipt.

The released registry pins the published image digests in `commands.json` and
`shell-image`. Users pull those self-contained images rather than the upstream
DHI inputs. The registry/repository destinations remain configurable through
`KIT_REPOSITORY_PREFIX` and `SHELL_IMAGE_REPOSITORY`.

## Pi's nested shrinkwrap

The Pi Kit installs its runtime and gateway dependencies with `npm ci` and
lifecycle scripts disabled. Its package lock carries integrity hashes, and its
declared Kit version must match the exact package version. The shell template
separately locks the Pi used by `pi-dev` under `packaging/shell/pi`.

Some earlier Pi releases shipped their own npm shrinkwrap with a vulnerable
`brace-expansion` (5.0.9). npm installed that nested copy despite a root
override and a changed outer lock, so checking the outer lock alone missed what
Pi actually used.

The Pi Kit locks `brace-expansion` 5.0.12. After installation, for
any Pi release, `patch-pi-dependencies.mjs` requires the fixed `brace-expansion`
(5.0.12 or later), fails if Pi bundles its own `brace-expansion` again, checks
that Pi's own `minimatch` resolves the fixed package, and exercises
representative brace/glob behavior. A Pi bump that reintroduces a nested copy
fails the image build.

The fixed release addresses the publisher's
[quadratic expansion](https://github.com/advisories/GHSA-q2hr-2g5m-vwhr),
[nested group recursion](https://github.com/advisories/GHSA-qhr7-859c-m2p7), and
[comma recursion](https://github.com/advisories/GHSA-6j4f-fj2g-mc7p)
denial-of-service advisories. Changes to these pins require a fresh installed
module inventory scan and CLI/ACP caller validation.

## Installed notices

Host and guest releases include the project's `LICENSE` and `NOTICE` and a
plain `THIRD-PARTY-NOTICES.txt` containing original dependency license files.
`scripts/third-party-notices.py` copies those texts from the Cargo package
sources and retained upstream files under `packaging/notice-overrides/`. It does
not require a version-indexed override, compare archive checksums or emit a
license inventory for dependency updates.

## Kit and provider notices

npm packages retain their original license files in the installed
`node_modules` trees copied into the images. Native Claude and Codex downloads
include their publisher's original license/notice files for the selected
release. The Node runtime's original LICENSE is copied using the version
installed in its DHI base.

Third-party software retains its own terms. In particular, Claude Code is
Anthropic's software; marsh's Apache license does not grant rights to it.

## Image notices

`packaging/dhi-notices/collected/` retains upstream notices and source materials
as ordinary image inputs. Debian copyright files remain with their installed
packages, npm license files remain with their packages, and the DHI base's own
license remains in the image. This archive is not a per-version inventory or an
assertion about an image's complete license coverage. Updating a dependency
does not regenerate it or require a license-delta approval.

The shell template also applies the existing focused package repairs in
`packaging/image-repair/repair.py`; those fix installed vulnerable code and
preserve the upstream notices. Their runtime checks remain part of CI.
