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

The current checksum sources are:

- [Claude Code 2.1.278 manifest](https://downloads.claude.ai/claude-code-releases/2.1.278/manifest.json).
- [Codex 0.155.1 release asset metadata](https://api.github.com/repos/openai/codex/releases/tags/rust-v0.155.1).

To update a native version, retrieve and review the publisher's asset digests,
update the descriptor and checksum file together, and build each supported
architecture. The digest binds the downloaded bytes; it does not establish
builder provenance or replace a provider redistribution review.

## Published images

Release CI (`.github/workflows/release.yml`, on a `v*` tag) builds every image
from the pinned inputs in this repository and publishes it to Docker Hub as a public repository under `docker.io/mcavage`. Kit images take their source
directory's name; the shell VM template gets its own:

| Image | Built from | Base |
|---|---|---|
| `docker.io/mcavage/marsh-shell-template` | `packaging/shell/Dockerfile` via `scripts/prepare-shell-image.py` | `dhi.io/sbx-templates:shell-docker` (`packaging/shell-image`) |
| `docker.io/mcavage/marsh-claude` | `kits/marsh-claude` | `dhi.io/sbx-templates:claude-code-docker` |
| `docker.io/mcavage/marsh-codex` | `kits/marsh-codex` | `dhi.io/sbx-templates:codex-docker`, `dhi.io/debian-base` |
| `docker.io/mcavage/marsh-pi` | `kits/marsh-pi` | `dhi.io/sbx-templates:claude-code-docker` |
| `docker.io/mcavage/marsh-shell` | `kits/marsh-shell` | `dhi.io/sbx-templates:shell-docker` |
| `docker.io/mcavage/marsh-fixture` | `tests/acceptance/fixture` (acceptance only) | `dhi.io/sbx-templates:shell-docker` |

The bases are Docker Hardened Images, pinned by digest. Pulling them needs a
Docker account with DHI access. CI logs in to both `docker.io` and `dhi.io`
with one Docker account (the `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN`
repository secrets) before building, and pushes with the same login. Users never
pull from `dhi.io`: the published images are self-contained, and installs pin
them by digest in `libexec/marsh/commands.json` and `libexec/marsh/shell-image`.
The release workflow writes those two files from the digests Buildx reports,
packs them into the tarball, and attaches the fixture reference as
`fixture-ref.txt` (read by `make fixture-ref`).

The public images therefore redistribute DHI-based builds. Each one carries the
notices collected in `packaging/dhi-notices/collected/` and the DHI license
text for the definitions, patches and build scripts it inherits; packaged
binaries keep their own terms. Two points need an owner's decision before the
first public release, and are recorded in
`packaging/dhi-notices/collected/qualification-boundaries.json`:

- whether DHI's terms permit public redistribution of images built on
  `sbx-templates`, and
- the Claude Code binary and a proprietary clipboard component in the
  `claude-code-docker` template are not open source; publishing an image that
  contains them is redistribution under their vendors' terms.

The registry and repository names are variables (`KIT_REPOSITORY_PREFIX`,
`SHELL_IMAGE_REPOSITORY` in the workflow). A new Docker Hub repository may be
created private; make each one public once in its settings.

## Pi's nested shrinkwrap

Both Pi Kits install their runtime and gateway dependencies with `npm ci` and
lifecycle scripts disabled. Their package locks carry integrity hashes. Native
Pi's declared Kit version must match its exact package version.

Pi 1.0.0 (like 0.86.1) embeds an npm shrinkwrap containing `brace-expansion` 5.0.9. npm can
install that version despite a root override and a changed outer lock entry.
Consequently, a clean audit of a manually changed outer lock is insufficient.

Each Pi Kit separately locks `brace-expansion` 5.0.12. After installation,
`patch-pi-dependencies.mjs` validates the reviewed versions, removes the nested
vulnerable package, and replaces it with a real copy of that fixed package
(not a symlink: the notice verifier rejects symlinked package ancestors). It checks the path that
Pi's own `minimatch` resolves and exercises representative brace/glob behavior.
The patch is idempotent and rejects unreviewed Pi or dependency versions.
The Kit input publisher stages the canonical repair into each build context
until an upstream Pi release removes this repair. The original nested 5.0.9 remains accurately recorded in the package
lock because it is downloaded before replacement; lock-only audit output must
be interpreted alongside the assembled module inventory.

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

Native Claude 2.1.278 carries the publisher's exact-version npm LICENSE and
README, retrieved with tarball integrity verification, alongside the unchanged
native binary. Native Codex 0.155.1 carries its exact-release LICENSE, NOTICE,
and bundled native notices. Codex ACP's separately locked npm release is 0.156.1
and receives that release's notices. The development shell also retains notices
for Claude 2.1.285 and Codex 0.159.2 copied from its maintained base stages.
Their exact public-image binary and package hashes, verified Claude npm package
integrity, and immutable Codex release commit are indexed in the common DHI
notice collection. Native Kit overlays retain both their configured-version
notices and the newer base-agent notices because both payloads remain installed.
Their final `--profile claude-kit --version 2.1.278` and
`--profile codex-kit --version 0.155.1` gates check exact selected native hashes,
required legal files and declared link/metadata changes after installation;
earlier observations are explicitly base-stage receipts. Unsupported versions
fail rather than silently upgrading. The template layers and their original
OS/package notices are preserved.

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

The supporting audit distinguishes collected text from unverified provider
build provenance and incomplete image/source-offer coverage. Public release
signing and notarization remain deferred under the product contract. A dependency
audit does not establish runtime isolation or readiness to release.

## Image dependency repairs

`packaging/image-repair/base-images.json` records reviewed official template
repins, exact ARM64 platform manifests, the signing-key hash and signed links to
their corresponding-source images. The development Dockerfile copies the
maintained Claude/Codex stage payloads described above; their OS layers are not
copied into the final development image.

`packaging/dhi-notices/collected` is a shared build input. Its offline verifier
selects the observed ARM64 or AMD64 inventory, checking 264 or 265 installed
package versions, twelve common runtime binary hashes, the exact clipboard
helper variant, and all indexed notice bytes. Six complete static filesystem
inventories bind the maintained agent additions, including native Codex tools
and libraries. Additional installed packages are reported separately; the receipt
names its `inventory_image` and does not infer the running image digest.
The source-attributed provider texts are stored in the canonical bundle.
Kit overrides use `$canonical_notice_files` mappings into its provider/text
paths; the thirteen actual duplicate input files (including two standard Apache
texts) were hash-compared, privately backed up, and removed. Installed notice
outputs remain derived distribution artifacts, not independent source inputs.
The MIT template retained for `proxy-agent-negotiate` is informational, not a
replacement for missing attribution or a redistribution grant.

The patched Zsh binary is now tied to `codex-zsh-v0.1.0`'s actual tag commit
`891f1f4c8584a082fc4658cabd48f1a8b01354e0`, its workflow's upstream
`77045ef899e53b9598bebc5a41db93a548a40ca6`, the exact LICENCE, patch,
manifest and both archive/member hashes. This records modified-source identity,
not a reproducible build. Both Codex 0.159.2 npm platform archives pass SHA512
integrity and all member hashes match the observed payload. Their decoded SLSA
statements identify `ff6aec96948b70d94983af2641a6b67c94faeff5`; **Sigstore
signatures/trust/transparency were not cryptographically verified**. Claude's
exact native checksum manifests are recorded separately from npm legal texts.

Available original Codex and ripgrep exact-release-lock Rust licence/NOTICE
texts are collected, with immutable `.cargo_vcs_info`-commit supplements for
omissions. The 1472/1485/1493-package locks for 0.159.2/0.155.1/0.156.1 are
conservative supersets, not evidence that every package is linked.
`primary-source-notices.json` supersedes the historical 23/22/23 gap counts:
eighteen unique gaps now retain actual archived declarations and copyright
headers with declared Apache-2.0 terms (or the explicitly permitted alternative).
Plain Apache declarations are not miscalled alternative elections; no copyright is
manufactured. Original difflib and fax upstream material is now supplied with its
version/copyright caveats: difflib's Kevin B. Knapp attribution is unchanged, and
fax's later clarification is not claimed to be in the old crate. Original upstream
full-text/rights disposition remains external for debugserver-types and icudata.
Npm and Rust now use one policy: informational standard permission text may
accompany a declaration, but is not an acquired upstream grant, invented copyright
or self-approved rights guarantee. Source-default feature analysis remains
supporting scope, not actual compiled linkage or a shipping-notice waiver.

Available native materials are now collected from actual pins: libcap2.75,
OpenSSL3.6.4, eleven voice source archives, original V8/rusty_v8/LLVM/ICU and
additional native dependency grants, producer-pinned musl1.2.6, and official
rustc1.95 release compiler legal documents. The V8 producer's own tag commit
`12b3e88028b983051913fb6bb95d7a11218bdceb`, Bazel overrides/registry hashes and
both consumer-pinned musl archive/binding manifests are retained. Its ICU input
is 77.1, not blindly the newer crate submodule; icudata identifies ICU77 data,
which is not generically relicensed MIT. Sandbox/default libc++/pointer-compression
selection is source-grounded. Release Cargo commands omit `--locked`, the main
helper's apt musl-tools version is unpinned, and the v8 crate records `dirty=true`.
Those limits do not become invented clean-source, signature or reproducible-build
claims. Release-compiler notices are not an inventory of the Rust toolchain copied into
the shell image.
Ripgrep elects MIT; both archive legal members and original PCRE2 10.46 licence
including JIT attribution and its precise binary-library exception are retained.

Required-notice mappings now bind agent/component identities independently of
the provider index. Dropping Codex legal files and their index entries fails.
Copied mode verifies the whole npm-global tree, uid/gid, permission bits and
link targets. Receipts hash their verifier and canonical bundle; actual image
identity and Dockerfile/tool/source/evidence hashes must come from the host.
The final development gate must follow every mutation, including any agent
`--version` smoke (which is execution, not collector proof).
The Codex Kit also puts `/usr/local/bin` first in its image PATH and invokes its
selected native binary by absolute path, with a built-image lookup/version guard.
Four npm recipes retain explicitly base-labelled early receipts and issue
final scoped receipts after additions. ACP0.156.1 and Claude SDK0.3.274 package
members and required retained texts are independently bound to SHA512-checked
public packages. Other npm coverage is metadata/notices only, not full-code approval.

A single notice vocabulary excludes whole source-code name matches; exact legal
comment excerpts are separately typed. Active indexes are reproducibly regenerated,
not merely annotated as historical noise. The authoritative README ships inside
`collected/`. Host proof now externally compares the bundle hash to the reviewed
source, retains raw index/platform digest correspondence, and has a no-bind baked
mode comparing fresh and baked scopes/receipts. Supporting bound public-base runs
are never promoted to baked final qualification.

The fixed-file public-image receipt and decoded bytes reconcile
Docker's image-level licence and clipboard SPDX. The former licenses DHI build
definitions/patches/scripts, not every included binary. SPDX confirms clipboard's
full commit and proprietary declaration. Its x/sys purl agrees with v0.46.0 and
BSD-3-Clause, but versionInfo/downloadLocation/SHA1 incorrectly repeat the main
repository/commit; original vendor bytes remain unchanged and the discrepancy is
recorded against observed Go build information and checksum-verified source.
The private licence's original acquisition and THIRD-PARTY-NOTICES cannot be
retrospectively asserted; computed blob identity/x/sys text do not invent them.
Authorized internal/user development with Docker-owned templates and external
redistribution by this repository are different permissions questions. General
external-redistribution uncertainty is not itself a technical runtime defect;
shipping notices and actual runtime evidence remain explicit separate gates.
ARM source-image links do not establish
complete source for differing custom versions/modified Moby; AMD64 source-image
links have not been established. Rust toolchain additions require a separate
exact notice/payload inventory. See
[`packaging/dhi-notices/README.md`](../../packaging/dhi-notices/README.md) for API,
source records, maintained real-public-npm CLI controls and the separate opt-in
host proof collector. The six public-base proofs are supporting only; actual baked
repaired-shell, final Claude/Codex and true shell-base development proofs, current
build receipts and distinct UAT remain required. These architecture-specific
inventory checks do not establish workload acceptance on either architecture.

The reviewed upstream npm and pip images bundle four dependencies requiring
security updates. Build-only `packaging/image-repair/repair.py` installs
checksum-pinned brace-expansion 5.0.12, undici 6.28.1, ip-address 10.7.1 and
urllib3 2.8.0. It accepts
only reviewed installed versions, validates archive paths, applies pip's import
namespace to urllib3, updates pip's installed vendor inventory and preserves
upstream licenses. It records actual installed tree hashes and adaptations under
`/usr/local/share/licenses/marsh-image-repair/repair-receipt.json`. Package scripts
are not run. The Kit input publisher stages this canonical directory into each independent
build context. Original package-manager lock metadata still describes its upstream
bundle; the repair receipt and final filesystem inventory describe the assembled
image.

The default shell publisher builds the repaired image and requires an explicit
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
