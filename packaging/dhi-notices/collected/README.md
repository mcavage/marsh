# DHI base and agent license notices

This directory is copied into every marsh image at
`/usr/local/share/licenses/marsh-dhi`. It holds the license texts and notices
for the parts of an image that do not carry their own: the DHI-built packages,
the Go modules compiled into Docker Engine, the agents and ACP adapters the
Kits add, and the machine-readable inventories that `verify.py` checks them
against. The preparation tools one level up produce it; this is the only README
for it.

`verify.py` checks bytes, metadata, package versions and the presence of every
required notice. A passing run is not a legal opinion, a build signature, or a
grant of redistribution rights.

## What each image is

Each image starts from a pinned Docker Hardened Image: `dhi.io/debian-base`
(the shell Kit, the acceptance fixture and, in its `-dev` variant, the shell VM
template) or `dhi.io/node` (the Pi, Claude and Codex Kits and the ACP
fixture). Kit images are also their Kit VM's root filesystem, so each one adds
Docker Engine and the tools a Kit VM needs. The runtime bases have no package
manager: `packaging/image-base/assemble-packages.sh` installs the packages in a
`debian-base` `-dev` build stage and copies each one's files, file list,
md5sums, copyright file and dpkg status entry onto the runtime base.

## Layout

| Path | Contents |
|---|---|
| `texts/` | License and notice texts, named by SHA-256 |
| `provider-notices/` | Agent and adapter legal files, named by component and version |
| `source-records/` | Upstream source identities, locks and acquisition records the indexes cite |
| `installed-package-notices.json` | Installed packages that ship no Debian copyright file (DHI-built Engine, Node.js, npm, Python), by exact version |
| `go-module-notices.json` | Go dependency modules of the Engine's Go binaries, with each binary's hash |
| `image-primary-notices.json` | The DHI license text every base carries at `/opt/docker/.license.txt` |
| `main-component-notices.json`, `public-component-notices.json`, `native-build-notices.json` | Components inside packages and agent payloads (runc; Codex's bundled ripgrep and zsh; Codex native build inputs) |
| `rust-notices.json`, `selected-rust-notices.json`, `primary-source-notices.json` | Rust and native source notices for the shipped Codex releases and ripgrep |
| `adapter-payloads.json`, `derived-profiles.json` | Expected payload files, hashes and the notices each one requires |
| `agent-notices.json` | Provider legal files of the shipped agent and adapter releases |
| `notice-rules.json` | Shared vocabulary for what counts as a legal document |
| `qualification-boundaries.json` | What the verifier does and does not establish, and open source-delivery obligations |
| `verify.py` | The verifier |

## Running the verifier

Runtime images have no Python, so each Kit and fixture recipe verifies its
finished `image` stage read-only from a build stage and copies only the receipt
into the final image:

```sh
python3 -I -S /image/usr/local/share/licenses/marsh-dhi/verify.py --root /image --receipt-dir /receipt
python3 -I -S /image/usr/local/share/licenses/marsh-dhi/verify.py --root /image --adapter pi --receipt-dir /receipt
python3 -I -S /image/usr/local/share/licenses/marsh-dhi/verify.py --root /image --profile claude-kit --version 2.1.296 --adapter claude-acp --receipt-dir /receipt
python3 -I -S /image/usr/local/share/licenses/marsh-dhi/verify.py --root /image --profile codex-kit --version 0.162.1 --adapter codex-acp --receipt-dir /receipt
```

The shell VM template has Python and runs the verifier in place (`--root /`)
after its last mutation. The receipt is `image-verification.json` in the
bundle directory.

What a run checks:

- **Every installed package** (dpkg `status` and DHI `status.d`): its own
  `/usr/share/doc/PACKAGE/copyright`, or an indexed row for its exact version
  whose in-image legal files (and the legal members of the wheels it installs)
  still match. A package with neither fails the build.
- **Go binaries** under `/usr/bin`, `/usr/sbin`, `/usr/libexec` and
  `/usr/lib/docker` (found by their Go build information): each must be indexed
  with its hash, so its module notices apply.
- **The DHI license file** at `/opt/docker/.license.txt`.
- **Native Kits:** the selected Claude 2.1.296 and Codex 0.162.1 files, modes,
  owners and links, and the selected `versions/` tree.
- **ACP adapters:** SHA-512-checked public package members for codex-acp's
  Codex 0.160.1 and the Claude Agent SDK 0.3.293, with tree, mode, owner, link
  and required-text checks. The SDK's own legal notice is retained as published.
- **Other npm packages:** lock identity, installed package metadata and
  collected notice hashes. This does not cover every executable byte.
- **The bundle itself:** every text is indexed and every indexed text is
  present, byte for byte.

Receipts record the verifier hash and a hash of the whole bundle tree (path,
SHA-256, size and mode of every file except the receipt).

## Regenerating

`make refresh-pins` regenerates every record here from the pins in the tree and
prints the license delta (docs/design/supply-chain.md, "Keeping pins
current"). Its steps are these tools:

- `inventory.py` streams `docker export` of each image's `image` stage (it runs
  no image code) and rewrites `installed-package-notices.json`,
  `../go-input.json`, `../image-packages.json` and
  `source-records/dhi-base-image-legal.json`. Rows are reused for the identical
  package version; packages built from one source share its legal files; a new
  version of a package in `../upstream-sources.json` gets the upstream texts at
  its release tag's commit; runc follows the commit its binary embeds.
- `collect-go.py --go GO --output DIR` acquires the Go module notices through
  the Go checksum database.
- `add-claude-release.py` and `add-codex-release.py` record agent releases;
  `collect-adapter-payloads.py` records the ACP adapters' native payloads.

The collectors (`collect-*.py`, `complete-rust-notices.py`,
`supplement-rust-notices.py`, and `scripts/npm-notices.mjs` for npm) share
`notice-rules.json` and `notice_rules.py`:

- a file is a legal document by name (`LICENSE`, `COPYING`, `NOTICE`, ...) or
  by location (`licenses/`);
- source code, test fixtures (`testdata`, `testcases`, `tests/files`) and data
  files (`.dict`, `.dat`) are not, though a leading license comment in source is
  kept as an exact byte-range excerpt;
- standard license text added next to a publisher's declaration is
  informational and is not treated as an upstream grant.

## Notes on specific components

- **Codex:** native and ACP packages pass SHA-512 and member checks. Their
  SLSA provenance statements are recorded but Sigstore signatures are not
  verified. The bundled zsh is tied to tag `codex-zsh-v0.1.0` (commit
  `891f1f4c8584a082fc4658cabd48f1a8b01354e0`, upstream zsh
  `77045ef899e53b9598bebc5a41db93a548a40ca6`) and its patch.
- **Rust dependencies of Codex:** notices are a superset collected from each
  shipped release's `Cargo.lock` (0.162.1 native, 0.160.1 in codex-acp), the V8
  producer inputs, LLVM, ICU, the voice libraries, libcap, OpenSSL, musl and the
  Rust 1.95 compiler distribution. This is not a record of what is actually
  linked.
- **Missing upstream full texts:** `debugserver-types` 0.5.0 and
  `deno_core_icudata` 0.77.0 declare MIT but ship no license file; the
  declaration is kept with the standard MIT text for information. ICU 77 data
  carries the Unicode/ICU terms. `difflib` and `fax`/`fax_derive` license files
  come from upstream with version caveats recorded in the index.
- **Claude Code and the Claude Agent SDK** are proprietary to Anthropic and
  are distributed under Anthropic's terms, not an open-source license.
- **DHI:** the DHI license text covers DHI definitions, patches and build
  scripts (Apache-2.0); included packages and binaries keep their own terms.
- **LGPL components:** the Codex voice libraries (GLib, GStreamer,
  proxy-libintl) and bubblewrap are under the GNU LGPL / Library GPL. The
  upstream source archives and license texts are identified in
  `qualification-boundaries.json` (`lgpl_corresponding_source`); a
  distributor must meet those licenses' source-delivery terms.
