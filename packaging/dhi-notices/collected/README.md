# DHI base and agent license notices

This directory is copied into every marsh image at
`/usr/local/share/licenses/marsh-dhi`. It holds the license texts and notices
for the Docker Hardened Image (DHI) base, the agents and ACP adapters the Kits
add, and the machine-readable inventories that `verify.py` checks them against.
The preparation tools one level up produce it; this is the only README for it.

`verify.py` checks bytes, metadata, package versions and the presence of every
required notice. A passing run is not a legal opinion, a build signature, or a
grant of redistribution rights.

## Layout

| Path | Contents |
|---|---|
| `texts/` | License and notice texts, named by SHA-256 |
| `provider-notices/` | Agent and adapter legal files, named by component and version |
| `source-records/` | Upstream source identities, locks and acquisition records the indexes cite |
| `*-notices.json` | Notice indexes: installed OS packages, Go modules, Rust crates, npm packages, native sources |
| `adapter-payloads.json`, `agent-payload-deltas.json`, `derived-profiles.json`, `payload-obligations.json` | Expected payload files, hashes and the notices each one requires |
| `notice-rules.json` | Shared vocabulary for what counts as a legal document |
| `notice-regeneration.json` | Changes made by `rebuild-notice-indexes.py` |
| `qualification-boundaries.json` | What the verifier does and does not establish, and open source-delivery obligations |
| `verify.py` | The in-image verifier |

## Running the verifier

A bare `verify.py --receipt` checks base scope and writes
`base-image-verification.json`. Recipes pass `--stage base` explicitly.
`--expect-agent none|claude|codex` makes a missing expected agent fail.

Final scopes run after every payload addition:

```sh
python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --copied-agents --receipt
python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --profile claude-kit --version 2.1.278 --adapter claude-acp --receipt
python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --profile codex-kit --version 0.155.1 --adapter codex-acp --receipt
python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --adapter pi --stage final --receipt
```

There is one Kit per agent. The claude and codex Kits ship the native CLI and
the agent's ACP adapter, so their final gate combines the Kit profile with that
adapter's scope (`scope_kind` `native-kit+adapter`); a profile accepts only its
own agent's adapter. The pi Kit's single lock carries Pi and pi-acp
(`--adapter pi`). Each recipe writes a base receipt first and runs the final
gate after installation, repair and the last `COPY`.

`stage` names the caller's verification point; `scope_kind` and
`adapter_scope` describe what was covered. `check-receipt-order.py` checks the
order of these steps in the known recipes, using the bounded Dockerfile reader
in `recipe_instructions.py` (backslash continuations only; other parser
directives, heredocs and repeated trailing escapes are refused).

What each scope checks:

- **Base:** installed package versions, common binary identities, the exact
  clipboard and Corepack variants, and maintained agent additions when present.
  Extra packages are reported, not counted as covered.
- **Copied development agents:** Claude 2.1.285 at `/usr/local/bin/claude` and
  the Codex 0.159.2 npm-global tree: bytes, type, mode, owner, link targets and
  tree equality.
- **Native Kits:** the selected Claude 2.1.278 and Codex 0.155.1 hashes and
  their allowed mutations. The Codex recipe also checks `command -v codex` and
  `codex --version` before the final receipt.
- **ACP adapters:** SHA-512-checked public package members for codex-acp
  0.156.1 and the Claude Agent SDK 0.3.274, with tree, mode, owner, link and
  required-text checks. The SDK's own legal notice is retained as published.
- **Other npm packages:** lock identity, installed package metadata and
  collected notice hashes. This does not cover every executable byte.

Receipts record the verifier hash and a hash of the whole bundle tree (path,
SHA-256, size and mode of every file except the two receipt files).

## Host proof

`host-proof.py` runs on the host. It compares the bundle against an external
`--reviewed-source` manifest, so a consistent edit to both an index and its
texts is still caught.

- `--supporting-base-bind` binds the bundle read-only onto a public base image.
- `--baked` runs the image's own verifier with no bind, compares its tree and
  verifier hashes with the manifest, and checks the baked receipt and the
  image-repair inputs.

`capture-index.py` captures the raw OCI index of each pinned `FROM` image with
`docker buildx imagetools inspect --raw`; `host-proof.py` checks those bytes
against the pinned digests.

## Regenerating

`rebuild-notice-indexes.py` rebuilds the active indexes from a retained seed
and exact source acquisitions and records its changes in
`notice-regeneration.json`. The collectors (`collect-*.py`,
`complete-rust-notices.py`, `supplement-rust-notices.py`, and
`scripts/npm-notices.mjs` for npm) share `notice-rules.json` and
`notice_rules.py`:

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
  release's `Cargo.lock`, the V8 producer inputs, LLVM, ICU, the voice
  libraries, libcap, OpenSSL, musl and the Rust 1.95 compiler distribution.
  This is not a record of what is actually linked.
- **Missing upstream full texts:** `debugserver-types` 0.5.0 and
  `deno_core_icudata` 0.77.0 declare MIT but ship no license file; the
  declaration is kept with the standard MIT text for information. ICU 77 data
  carries the Unicode/ICU terms. `difflib` and `fax`/`fax_derive` license files
  come from upstream with version caveats recorded in the index.
- **Claude Code and the Claude Agent SDK** are proprietary to Anthropic and
  are distributed under Anthropic's terms, not an open-source license.
- **Docker clipboard-bridge** (in the DHI base) is proprietary to Docker; its
  SPDX document is kept unchanged and reconciled in
  `source-records/clipboard-spdx-reconciliation.json`. The DHI license text
  covers DHI definitions, patches and build scripts (Apache-2.0); included
  binaries keep their own terms.
- **LGPL components:** the Codex voice libraries (GLib, GStreamer,
  proxy-libintl) and bubblewrap are under the GNU LGPL / Library GPL. The
  upstream source archives and license texts are identified in
  `qualification-boundaries.json` (`lgpl_corresponding_source`); a
  distributor must meet those licenses' source-delivery terms.
