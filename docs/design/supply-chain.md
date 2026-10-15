# Build inputs and third-party notices

The Rust toolchain is pinned in `rust-toolchain.toml`. The default Linux builder
in `Makefile` and `packaging/linux-arm64/Dockerfile` also uses an immutable image
digest. Setting `RUST_IMAGE` is an explicit authoring override and changes the
candidate's build inputs. Cargo builds use the checked-in lockfile.

Native Claude and Codex downloads are checked against the Kit's
`release-checksums.txt` before installation or archive extraction. The checksum
selector requires exactly one entry for the requested version and architecture.
An unlisted version fails before downloading it. Codex archive members are read
to stdout into fixed output files, without extracting archive paths into the
builder filesystem.

The shell image installs Vim with apt and builds terminal Emacs from
checksum-pinned source in `packaging/shell/Dockerfile`. Emacs is built
without native compilation to avoid replacing the DHI GCC libraries.

The current checksum sources are:

- [Claude Code 2.1.296 manifest](https://downloads.claude.ai/claude-code-releases/2.1.296/manifest.json).
- [Codex 0.162.1 release asset metadata](https://api.github.com/repos/openai/codex/releases/tags/rust-v0.162.1).

To update a native version, change the descriptor and run `make refresh-pins`
(below): it rewrites each Kit's `release-checksums.txt` from the publisher's own
metadata (the two sources above, `scripts/refresh-agent-checksums.py`), in the
committed format, and records the release's notices. Review the resulting diff
against the publisher. The digest binds the downloaded bytes; it does not
establish builder provenance or replace a provider redistribution review.

## Keeping pins current

Builds keep these pinned inputs; Renovate proposes updates for review.
Renovate
(`.github/renovate.json5`, run on a schedule by `.github/workflows/renovate.yml`)
opens a PR when any of these moves:

| Pin | Where | How it updates |
|---|---|---|
| DHI base images (`debian-base`, `node`) | `packaging/shell-image`, `kits/*/*.dockerfile`, fixtures | digest, one grouped PR |
| DHI-built apt packages (Docker Engine; the shell template's Node.js, npm, Python) | `packaging/image-base/*.packages`, `packaging/shell/Dockerfile` | version, from DHI's apt index (`deb` datasource), one grouped PR |
| Rust builder image | `packaging/linux-arm64/Dockerfile`, `packaging/shell/Dockerfile`, `Makefile` | digest only; the toolchain moves by hand |
| Claude Code, Codex, Pi | `kits/*/*.yaml` (`args.version.default`) | version |
| ACP adapters, other npm packages | `kits/*/package.json`, lock files, `provides` | version, together with the descriptor |
| Rust crates, GitHub Actions, PyYAML | `Cargo.lock`, workflows, `requirements-publish.txt` | weekly, grouped |

Renovate only edits pins. Everything the image gate checks is derived from them,
and `make refresh-pins` (`scripts/refresh-pins.py`) regenerates all of it:

1. `kits/*/release-checksums.txt` from the publishers (`refresh-agent-checksums.py`);
2. the agent release records: the Claude Kit's release (`add-claude-release.py`)
   and the Codex Kit's release and the one codex-acp locks (`add-codex-release.py`:
   exact-lock Rust notices, provider legal files, the Kit profile). A release
   already recorded is not re-acquired; records of releases no longer shipped are
   retired;
3. `kits/*/notices/overrides.json`: an exact-version npm notice override follows
   a bumped package to its new version's source commit (its npm `gitHead`, or the
   agent release record); a standard text selected by a package's declaration is
   kept only while the declaration is unchanged;
4. `adapter-payloads.json` (`collect-adapter-payloads.py`);
5. the image records: it builds each image's `image` stage and runs `inventory.py`
   (installed packages and their license families, the notices of DHI-built
   packages, Go binaries, compiled-in components) and, when the Go modules moved,
   `collect-go.py`. A new version of a DHI-built package gets its upstream legal
   texts from the release tag's commit (`packaging/dhi-notices/upstream-sources.json`);
   runc follows the commit its binary embeds;
6. unreferenced texts are deleted;
7. the license delta against `REFRESH_BASE` (default `origin/main`,
   `scripts/license-delta.py`), written to `target/refresh-pins/license-delta.{md,json}`.

A run on a tree whose records are current changes nothing. Its exit status is
the delta's verdict: 0 **OK** when the change is only new versions of reviewed
components and new components under license families the tree already has; 2
**NEEDS-OWNER** for a new license family, a changed license text of a
proprietary component (`proprietary_components` in
`qualification-boundaries.json`: Claude Code and the Claude Agent SDK), or a new
proprietary or binary-only component; 1 when a record could not be regenerated
(for example a new npm dependency that ships no notice and has no override, or a
DHI-built package with no entry in `upstream-sources.json`). Families come from
`license_families.py`: fingerprints of license wording, npm and SPDX declarations,
and Debian DEP-5 `License:` names. They label evidence for a reviewer; they are
not a license determination.

`.github/workflows/renovate-refresh.yml` runs `make refresh-pins` on every push
to a `renovate/**` branch (an arm64 runner with dhi.io access), commits the
regenerated records back as `refresh-pins@users.noreply.github.com` (an author
Renovate ignores, so it keeps managing the branch), posts the delta as one PR
comment that later runs update, and sets the **License review** commit status:
red only for NEEDS-OWNER or a failed refresh. CI, including the `images` job that
builds and verifies every image, then runs on the completed head. Nothing merges
itself.

Not covered by Renovate, because each needs a coordinated change: the Rust
toolchain (`rust-toolchain.toml`, the workflows, the builder image tag), the
pinned downloads in `ci.yml` (nextest, TLA+ tools, the Bash 5.3 source), the Node
and Java versions in CI, the vendored Brush revision, and the Emacs source
pin in the shell Dockerfile. Other Debian packages
are not pinned; each carries its own copyright file, and the license delta shows
when one moved. Vendored files (`vendor/`) are never touched.

A new Kit that follows the same layout (a descriptor with `args.version.default`
and a `package.json`) needs a `customManagers` entry for its CLI in
`renovate.json5`. Third-party Kits live outside this repository.

## Published images

Release CI (`.github/workflows/release.yml`, on a `v*` tag) builds every image
from the pinned inputs in this repository and publishes it to Docker Hub as a public repository under `docker.io/mcavage`. Kit images take their source
directory's name; the shell VM template gets its own:

| Image | Built from | Base |
|---|---|---|
| `docker.io/mcavage/marsh-shell-template` | `packaging/shell/Dockerfile` via `scripts/prepare-shell-image.py` | `dhi.io/debian-base:trixie-dev` (`packaging/shell-image`) |
| `docker.io/mcavage/marsh-claude` | `kits/marsh-claude` | `dhi.io/node:24-debian13` |
| `docker.io/mcavage/marsh-codex` | `kits/marsh-codex` | `dhi.io/node:24-debian13` |
| `docker.io/mcavage/marsh-pi` | `kits/marsh-pi` | `dhi.io/node:24-debian13` |
| `docker.io/mcavage/marsh-shell` | `kits/marsh-shell` | `dhi.io/debian-base:trixie` |
| `docker.io/mcavage/marsh-fixture` | `tests/acceptance/fixture` (acceptance only) | `dhi.io/debian-base:trixie` |

The bases are Docker Hardened Images, pinned by digest: the runtime variants,
which have no shell tooling or package manager, except the shell VM template,
whose user has `sudo` and `apt` for their own tools. A Kit image is also its
Kit VM's root filesystem (`sbx create oci://KIT`), and the Kit's jobs run in
that VM's Docker Engine from the same image. So every Kit image adds Docker
Engine (the DHI-built `docker-ce`, `docker-ce-cli` and `containerd.io`), the
sandbox `agent` user (uid 1000) in the `docker` group, `bash`, `tar` (for
`sbx cp`), the tools `update-ca-certificates` needs to trust the sandbox
proxy's CA at VM start, `git`, `curl`, and the
`com.docker.sandboxes.start-docker=true` label that has Docker Sandboxes start
`dockerd`. `packaging/image-base/assemble-packages.sh` installs these from the
Debian and DHI apt repositories in a `debian-base` `-dev` build stage and copies
each package, with its dpkg records and copyright file, onto the runtime base;
`kit-vm.packages` and `userland.packages` list them. The agent Kits add the
toolchain an ordinary project gate runs in their containers
(`devtools.packages`: make, gcc, libc6-dev, python3) and ripgrep; Go and Java
are not bundled;
the shell Kit and the fixture add `python3`. npm runs only in a `dhi.io/node`
`-dev` build stage, and the Claude and Codex downloads in a `debian-base` `-dev`
stage. Each recipe smoke-tests the finished image in a throwaway stage and
verifies its notices from a build stage (the runtime images have no Python). Pulling them needs a
Docker account with DHI access. CI logs in to both `docker.io` and `dhi.io`
with one Docker account (the `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN`
repository secrets) before building, and pushes with the same login. Users never
pull from `dhi.io`: the published images are self-contained, and installs pin
them by digest in `libexec/marsh/commands.json` and `libexec/marsh/shell-image`.
The release workflow writes those two files from the digests Buildx reports,
packs them into the tarball, and attaches the fixture reference as
`fixture-ref.txt` (read by `make fixture-ref`).

The public images therefore redistribute DHI-based builds. Each one carries
every Debian package's own copyright file, the notices collected in
`packaging/dhi-notices/collected/` for the rest, and the DHI license text for
the definitions, patches and build scripts it inherits; packaged binaries keep
their own terms. The images do not derive from the Docker Sandboxes
templates, so neither the templates' redistribution terms nor their
preinstalled agents and proprietary clipboard component apply. One
redistribution question remains open, recorded in
`packaging/dhi-notices/collected/qualification-boundaries.json`: the Claude Kit
redistributes Claude Code (the native binary and the one inside the Claude
Agent SDK), which is proprietary to Anthropic and distributed under its terms.

The registry and repository names are variables (`KIT_REPOSITORY_PREFIX`,
`SHELL_IMAGE_REPOSITORY` in the workflow). A new Docker Hub repository may be
created private; make each one public once in its settings.

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

`make install` generates `THIRD-PARTY-NOTICES.txt` and
`rust-package-notices.json`, then installs them with the project license under
`PREFIX/share/licenses/marsh`. The generator traverses the normal and build
dependency graph for macOS ARM64 and Linux ARM64 and records the lockfile hash.
It preserves package license texts, copyright notices, authors, and source URLs.
It fails if a dependency lacks notice text or a checked-in override changes.

Some crates omit license files from their published packages. The files in
`packaging/notice-overrides` come from the exact commits named by those crates'
`.cargo_vcs_info.json`, with source URLs and hashes in `sources.json`.
`utf8-chars` declares `MIT OR Apache-2.0` but supplies no license text; its
Apache-2.0 alternative is selected and the standard Apache text is included.

The generator also binds `libduckdb-sys`'s bundled source archive hash to
DuckDB commit `d8cdaa33fda8df955cc76ef58a280f68f4cd43fa`, the exact gitlink in the
locked Rust crate's upstream commit. Its 26 embedded C/C++ component directories
have 28 preserved license/notice files, including DuckDB's own license and the
additional tdigest notices. A changed archive or component set fails generation.
`embedded-native-notices.json` records this mapping. Linux binary exports carry
the same notice bundle under `licenses/marsh`.

## Kit and provider notices

Each npm Kit runs `collect-notices.mjs` after installing and repairing its actual
modules. It preserves every installed package's identity, authors, declared
license, manifest hash and notice texts under `/usr/local/share/licenses/marsh-*`.
It walks installed nested modules, including bundled dependencies, and rejects
missing notice text. The Kit input publisher stages the canonical `scripts/npm-notices.mjs` collector
into each build context. Original notices remain with the installed packages.

Exact-version overrides restore notices omitted by upstream npm packaging.
Pi's notices come from its published `gitHead`; esbuild and Standard Webhooks
likewise use exact commits. The selected AWS packages declare Apache-2.0 but omit
the text, so the complete standard license accompanies their original author
metadata. `proxy-agent-negotiate` 1.1.0 declares MIT but supplies no copyright
notice in its package or recorded source commit. Its inventory explicitly
preserves that limitation and supplies the unmodified SPDX MIT template without
inventing a copyright holder or year. Retrieved npm provenance payloads identify
source commits; this work does not claim cryptographic verification of those
provenance signatures.

Native Claude 2.1.296 carries the publisher's exact-version npm LICENSE and
README, retrieved with tarball integrity verification, alongside the unchanged
native binary. Native Codex 0.162.1 carries its exact-release LICENSE, NOTICE,
and bundled native notices. Codex ACP's separately locked npm release is 0.160.1
and receives that release's notices. No other agent is installed. The final
`--profile claude-kit --version 2.1.296` and
`--profile codex-kit --version 0.162.1` gates check the exact selected native
files, owners, modes and links and the required legal files after
installation. Unsupported versions fail rather than silently upgrading.

Claude's [current licensing documentation](https://code.claude.com/docs/en/legal-and-compliance)
permits preinstallation subject to its commercial terms and specified conditions:
preserve the unmodified binary and its authentication methods, and use each end
user's own authorized authentication and billing. A copied notice does not accept
an agreement or establish that a particular hosting/account arrangement meets
those conditions.

Base-image license declarations are separate from the licenses of their bundled
components. Retain the exact platform manifest, installed OS/package inventory,
upstream SBOM/provenance and original notices for each distributed candidate.
[Docker documents signed DHI SBOM verification](https://docs.docker.com/dhi/explore/security-concepts/sbom/).
Where GPL/LGPL components are redistributed, the corresponding-source and
relinking requirements must be met for those exact binaries; a generic image
license, SBOM or link to an unrelated current source tree is insufficient.

The notice records distinguish collected text from provider build provenance,
which is not verified, and from image/source-offer coverage, which is
incomplete. Public release signing and notarization are deferred under the
product contract.

## Image notices

`packaging/dhi-notices/collected` is a shared build input. Its verifier runs
against each finished image (`--root`) and requires, for every installed
package, the package's own Debian copyright file or an indexed row for its exact
version. The DHI-built Engine, Node.js, npm and Python packages ship no
copyright file; `installed-package-notices.json` indexes their in-image legal
files (including those inside the pip and setuptools wheels) and the upstream
LICENSE/NOTICE texts of their exact source revisions. Every Go binary the OS
packages install must be indexed with its hash in `go-module-notices.json`,
whose 426 dependency modules were collected through the Go checksum database
from those binaries' own build information. The DHI license file at
`/opt/docker/.license.txt` is checked in place. `packaging/dhi-notices/inventory.py`
regenerates these indexes from locally built image stages without running image
code. The source-attributed provider texts are stored in the canonical bundle.
Kit overrides use `$canonical_notice_files` mappings into its provider/text
paths. Installed notice outputs remain derived distribution artifacts, not
independent source inputs. The MIT template retained for `proxy-agent-negotiate`
is informational, not a replacement for missing attribution or a redistribution
grant.

The patched Zsh binary is tied to `codex-zsh-v0.1.0`'s actual tag commit
`891f1f4c8584a082fc4658cabd48f1a8b01354e0`, its workflow's upstream
`77045ef899e53b9598bebc5a41db93a548a40ca6`, the exact LICENCE, patch,
manifest and both archive/member hashes. This records modified-source identity,
not a reproducible build. The same zsh binary ships in codex-acp's Codex 0.160.1
platform package, whose members pass SHA512 integrity checks
(`adapter-payloads.json`); **Sigstore signatures/trust/transparency were not
cryptographically verified**. Claude's exact native checksum manifests are
recorded separately from npm legal texts.

Available original Codex and ripgrep exact-release-lock Rust licence/NOTICE
texts are collected, with immutable `.cargo_vcs_info`-commit supplements for
omissions. The locks of the shipped releases, 0.162.1 (native) and 0.160.1
(codex-acp), are conservative supersets, not evidence that every package is
linked.
`packaging/dhi-notices/add-codex-release.py --version V` records a further
Codex release (exact-tag lock and source, the canonical collectors, provider
legal files); `add-claude-release.py --version V` records a Claude Code
release's npm legal files and native manifest and refuses a changed LICENSE.md.
`primary-source-notices.json` records notice gaps and the archived declarations,
copyright headers, or later clarifications collected for each. Original upstream
full text and rights disposition remain external for debugserver-types and
icudata. Informational standard license text does not supply a missing upstream
grant. See [notice collection](../../packaging/dhi-notices/README.md) for the
per-component records and collection procedure.

Collected native materials follow the actual pins: libcap 2.75,
OpenSSL 3.6.4, eleven voice source archives, V8/rusty_v8/LLVM/ICU and
additional native dependency grants, producer-pinned musl 1.2.6, and official
rustc 1.95 release compiler legal documents. The V8 producer's tag commit
`12b3e88028b983051913fb6bb95d7a11218bdceb`, Bazel overrides/registry hashes and
both consumer-pinned musl archive/binding manifests are retained. The producer
used ICU 77.1 rather than the newer crate submodule; icudata identifies ICU 77
data, whose terms remain separate from the crate's MIT declaration. The source
configuration records sandbox, default libc++, and pointer-compression choices.

There are limits to the upstream build evidence: the Codex release workflow's
Cargo commands omit `--locked`, its musl build helper installs an unpinned apt
`musl-tools`, and the v8 crate records `dirty=true`. marsh's own Cargo builds
use the checked-in lockfile. Upstream build reproducibility and signatures are
not verified. The release-compiler notices also do not inventory the
Rust toolchain copied into the shell image.
Ripgrep elects MIT; both archive legal members and original PCRE2 10.46 licence
including JIT attribution and its precise binary-library exception are retained.

Required-notice mappings bind agent/component identities independently of
the provider index. Dropping Codex legal files and their index entries fails.
Receipts hash their verifier and canonical bundle; actual image identity and
Dockerfile/tool/source/evidence hashes must come from the host. Each Kit's
verification reads its finished image stage, so it follows every mutation; the
agent `--version` smoke runs in a separate throwaway stage (it is execution,
not collector proof). The Codex Kit also puts `/usr/local/bin` first in its
image PATH and invokes its selected native binary by absolute path, with a
built-image lookup/version guard. ACP 0.160.1 and Claude SDK 0.3.293 package
members and required retained texts are independently bound to SHA512-checked
public packages. Other npm coverage is metadata/notices only, not full-code approval.

The notice inventory distinguishes exact legal-comment excerpts from full
notice texts and excludes whole-source-code name matches. Its authoritative
README ships inside `collected/`. Corresponding source for the DHI-built packages and DHI's Debian
package patches is not linked or bundled. Rust toolchain additions to the shell
template require a separate exact notice/payload inventory. See
[`packaging/dhi-notices/README.md`](../../packaging/dhi-notices/README.md) for the
tools. These checks do not establish workload acceptance.

The npm and pip the shell VM template installs from DHI's packages bundle four
dependencies requiring security updates. Build-only
`packaging/image-repair/repair.py` installs
checksum-pinned brace-expansion 5.0.12, undici 6.28.1, ip-address 10.7.1 and
urllib3 2.8.0. It accepts
only reviewed installed versions, validates archive paths, applies pip's import
namespace to urllib3, updates pip's installed vendor inventory and preserves
upstream licenses. It records actual installed tree hashes and adaptations under
`/usr/local/share/licenses/marsh-image-repair/repair-receipt.json`. Package scripts
are not run. Kit images ship no npm or pip (npm runs only in their build
stages), so only the shell template is repaired. Original package-manager lock
metadata still describes its upstream bundle; the repair receipt describes the
assembled image.

The default shell publisher builds the shell template and requires an explicit
registry destination. The installed `shell-image` must identify that immutable
output; the upstream digest in `packaging/shell-image` is a build input. A local
test registry reference is not a public release pin.

Validation exercises installed undici against an unsolicited WebSocket protocol
response and the actual pip CLI against an oversized chunk header. The repaired
callers reject both inputs without an uncaught Node exception or an unbounded
chunk-header read. Ordinary brace expansion and package-manager startup are also
checked. This is dependency-boundary evidence, not complete image qualification.
The installed ip-address callers also reject mixed-family subnet matches and
oversized parse diagnostics, and recognize link-local and private NAT64 ranges.

Image CVE reports retain both raw matches and publisher VEX triage. Signed VEX
explains many matches against backported or absent code; a scan count alone is
not a finding of exploitability. Retain the final assembled inventory, verified
VEX and remaining advisory dispositions for the exact shipped image.
