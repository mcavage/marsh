# Publishing native Kits

`scripts/prepare-kit-inputs.py` and `scripts/publish-kits.py` prepare and publish
native Kit v3 sources. They do not create sandboxes or configure credentials.
Publication requires an already authorized Buildx builder and registry.

## Default and opt-in registries

`packaging/commands.json` selects the four public-source Kits. Other Kits
([bring your own agent Kit](../plan/custom-agent-kit.md)) use their own
registry file.

```sh
python3 -m venv "$HOME/.cache/marsh-publish-venv"
. "$HOME/.cache/marsh-publish-venv/bin/activate"
python3 -m pip install --require-hashes --no-deps --only-binary=:all: -r scripts/requirements-publish.txt
make kits
make kit-publish KIT_REPOSITORY_PREFIX=registry.example/team/marsh
```

Registries such as Docker Hub accept a namespace and a single repository
name. To give images distinct names within one namespace, use
`--repository-prefix REGISTRY/NAMESPACE --repository-name-prefix marsh-`
with `scripts/publish-kits.py`. For example, release CI publishes the fixture
as `docker.io/mcavage/marsh-fixture` (`.github/workflows/release.yml`). Create
private repositories before publication when the payload must stay private.

Preparation/publishing output directories must be **owner-private (0700)** and
outside every selected Kit source. New directories are created at 0700. For an
existing `target/kit-release`, review its contents and set its mode to 0700 first.
Files are written at 0600; a public or writable output directory is not silently
chmodded. `make kits` / `--validate-only` perform full native cache-only builds,
not pushes. They are **not pure or read-only validation**: Dockerfile build
instructions may execute, public/base images may be pulled and builder cache may
change. Every selected cache-only build must succeed before any push starts.

## One canonical input set

Every default Kit and the acceptance fixture receives:

- `packaging/dhi-notices/collected` as `dhi-notices/`;
- `packaging/image-repair` as `image-repair/`.

The four npm-based Kits receive `scripts/npm-notices.mjs` as
`collect-notices.mjs`. Provider legal texts are indexed in the canonical DHI
bundle. There are no independent Kit-local repair, collector, or Claude/Codex
notice copies to keep synchronized.

The publisher expands these sources once by their bundled source identity. It
reserves their **entire destination namespaces**, including case/Unicode variants:
source files and extra mappings cannot replace them or add new files underneath.
Canonical additions require a new preparation, just like modifications/deletions.
Custom Kits use their explicitly declared inputs. A fixture `--shell-image`
override requires **both** the exact original exported image file and an explicit
host-private full observed build receipt, not merely adjacent helper JSON:

```sh
python3 scripts/prepare-kit-inputs.py \
  --source-tree /canonical/current/candidate \
  --commands /canonical/current/candidate/tests/acceptance/fixture/commands.json \
  --shell-image /private/observed-export/shell-image \
  --shell-build-receipt /private/host-control/build.json \
  --output /private/kit-preparation/fixture-inputs.json
```

`--source-tree` defaults to this checkout and selects the candidate/Kit worktree.
Image, receipt and explicit source-tree paths must be canonical absolute paths,
not symlink aliases. The receipt must be outside source and both host/Linux
exports, in its private host directory. Strong prepared documents and publication
registries must also stay outside that observed source and its exports; neither
output may overwrite a consumed source/proof input (including an inode alias).
The shared
`image_observations.verified_shell_image` revalidates the observed build receipt,
exact source, host/Linux artifacts, licenses, producer image proof and original
export path. Copied/relabeled image files, stale artifacts with unchanged receipt
bytes, and helper-only JSON are rejected. Candidate/compiler/tool observations
come from the full host build receipt; the standalone shell image itself carries
no Rust candidate payload. This is trusted-host observation, not a signature or
proof against arbitrary host code forging receipts.

A distributable Kit `FROM` requires the observed explicit-registry publication.
A local-template proof or mutable alias is not a remotely resolvable base. The
final registry/config/runtime check remains a separate actual-host gate.

## Prepared document and publication receipt

Preparation writes **one atomic JSON document** (`marsh.prepared-kit-inputs/v2`),
not a map plus an independently replaceable `.sources.json`. Old v1 sidecars are
ignored and never evidence for v2; they are not silently deleted. It contains:

- `inputs`: selected source → descriptor arguments and explicit file mappings;
- `receipt.bindings`: SHA256, size and mode of preparer, publisher, shared
  `build_inputs.py`, shared `command_registry_rules.json`, command registry, and
  explicit configuration/proof files;
- `receipt.state.canonical`: canonical origin paths and actual file inventories
  (including directory membership), stored once rather than repeated for each Kit;
- `receipt.state.sources`: admitted Git source-tree hash, independently checked
  staged-tree hash, descriptor name/hash, effective arguments, count and byte size;
- `receipt.shell_images`: narrow strong verification requests linking a selected
  Kit's `shellImage` to exact candidate source, image-file and host build-receipt
  paths. Publisher re-runs the shared semantic verifier, not only a JSON hash.

Tree hashes are SHA256 of sorted, indented JSON plus a final newline. Each file
record contains `sha256` (with prefix), `size`, and `mode`. Staged file records use
normalized modes. The publisher measures current source and copied bytes itself;
it does not accept the receipt's hashes as assertions of what it staged.

Bundled Kits require this prepared document even for `--validate-only`. A stale
script, canonical input, descriptor, Kit file, added/deleted admitted file or
configuration fails **before any Docker invocation**. Rechecks after all native
cache-only builds prevent a changed source from starting a push.
Private context changes are also refused. Strong shell proof verification is
repeated before Docker, after cache-only builds, before each push, and after
publication; unchanged receipt bytes cannot hide changed candidate/artifact bytes.
After publication, all inputs and contexts are checked again; a failure then
explicitly says **publication may
have occurred**, never that there were zero remote effects.

Successful publication writes a durable content-addressed receipt named
`<registry>.publication-<sha256>.json` before atomically replacing the ordinary
`command → repository@sha256:digest` registry. The receipt binds the exact prepared
file bytes, source/script bindings, effective arguments and staged hashes to each
reported immutable digest and the exact registry hash. Existing receipts are not
overwritten. Consumers can verify the receipt filename and `registry_sha256`.
A failure before registry commit preserves the previous registry and receipts;
a crash can leave an unreferenced new receipt. These two local files are not
claimed to be a filesystem-wide transaction. Remote pushes are not transactional:
partial runs can leave images and move mutable `:release` tags.

This is byte provenance, **not a signature, attestation of trustworthy source,
legal completeness proof, or runtime/image qualification**. Do not modify source
concurrently with preparation/publication. Before/after checks detect observed
changes; they are not an atomic snapshot of a hostile same-user editor.

## Explicit authoring inputs and native argument rules

`--commands FILE` maps command names to directories beneath `--source-root`.
Sources must be in a Git worktree and contain one included `.yaml` workload
descriptor. Command names/count/document bytes use the same declarative rules as
Rust, `crates/marsh-contracts/src/command_registry_rules.json` (currently 251 names,
128 ASCII bytes per name, 1 MiB registry). Reserved names and dots are refused;
numeric-leading names are allowed. That rules file is a bound preparation input.
`KIT_BUILD_INPUTS` supplies extra args/files to the preparer:

```json
{
  "kits/my-kit": {
    "args": {"version": "1.2.3"},
    "files": {"artifacts/tool": "/absolute/owner-controlled/tool"}
  }
}
```

Custom authoring Kits may also supply this legacy explicit mapping directly to
`--build-inputs`. It receives the same staging/authority/native checks and a
publication receipt, but no claim of a separate prepared-source receipt. To seal
custom inputs, the shared Python API is
`seal_prepared(root, commands_path, inputs, bindings=(), *, shell_images=())`;
bind all helper/proof files. For fixture or custom `shellImage` overrides, supply:

```python
shell_images=[{"source": "kits/my-kit", "image_file": str(original_image_file),
               "source_tree": str(root), "build_receipt": str(host_build_receipt)}]
```

`source` is the selected Kit path; the argument is fixed to `shellImage`.
The publisher automatically binds the returned proof paths and shared verifier
modules. The fixture requires sealed v2 inputs; raw maps and requests without the
strong proof are refused. The public wrapper remains
`verified_shell_image(path, *, source_tree=ROOT, build_receipt=None) → (reference,
bindings)`, but the formerly optional receipt now fails closed when absent.
`prepare-kit-inputs.map_tree(source, destination) → (files, inventory)` remains
compatible. Explicit file origins are absolute; destinations cannot
traverse, overwrite source files, or add Docker ignore controls.

Only descriptor argument names are accepted, not raw Docker `buildArg` names.
Missing values use declared defaults, otherwise they are required. Strings,
argument counts, patterns and YAML nesting are bounded; aliases and duplicate
keys are refused. **Python regex matching is not used.** All selected private
contexts undergo full native `--output type=cacheonly` builds before any push.
Go's native regex parser is the pattern authority, including `\z`, POSIX classes
and rejection of backreferences. It checks the effective value: a default that
does not match its pattern can be replaced by a valid supplied value. Each
cache-only build/push has a finite 30-minute deadline. Any failed cache-only build
prevents every push. The pinned frontend **does not support `--call=validate`**;
the publisher uses no frontend subrequest flag. Native compatibility must be
tested against actual Buildx/frontend, never inferred from a recording executable.

Canonical aliases are compared by actual device/inode, including case aliases
on case-insensitive macOS volumes. One source gets one build/input configuration;
conflicting alias configurations are refused. Distinct sources must have distinct
lowercase directory names because those names become repository suffixes.

## Filesystem boundaries

- Command registries are at most the shared 1 MiB bound; other JSON at most
  4 MiB; each file at most 128 MiB; all staged contexts together
  at most 512 MiB. A Kit admits at most 10,000 files. Canonical/shared tree walks
  also bound directory entries and depth (64), not just file bytes.
- Inputs must be caller-owned regular files with **one hard link**, no group/world
  writes. Reads are nonblocking and no-follow through every parent component.
  Canonical tree directories must be caller-owned without other writers. Trusted
  system ancestors are permitted, including root-owned sticky temporary roots.
- Kit source contexts include tracked and nonignored untracked authoring files,
  not only committed HEAD. Tracked deletions are not resurrected. Repository `.gitignore`
  rules apply; personal excludes and `.git/info/exclude` do not change inputs.
  **Every nonignored untracked input path (including canonical origins) is
  printed once, JSON-escaped, to stderr before the first remote builder call.**
  Explicit origins are also listed; contents and argument values are not printed.
  Review the list and use repository ignore rules for Kit scratch files; inclusion
  does not mean a file is secret-free. Canonical trees are explicit whole-tree
  inputs, not Git snapshots: keep scratch/secret files out of them even if ignored.
  New admitted files after preparation require re-preparation.
- Source symlinks and admitted special files are refused. Git can omit untracked
  special files; omitted/ignored paths are not part of the admitted context.
- Temporary context parents are 0700. Context directories are 0755; regular
  files are 0644 plus original execute bits so image users can read notices even
  from a umask-077 checkout. Input modes/ownership are not changed.
- Output parents are no-follow, owner-private, and outside selected source
  identities. The publisher retains and verifies the directory fd across Docker
  calls, then uses it for atomic replacement and fsync; pathname substitution
  cannot redirect a write.

These controls do not make an untrusted Dockerfile safe. Review build sources
and nonsecret inputs before granting the builder authority.

## Credential-free caller checks

```sh
python3 -W error tests/kits/test_publisher_cli.py -v
```

The suite calls the real CLIs on copied default-seven and fixture source, with a
recording Docker executable. It independently hashes canonical/staged bytes and
checks stale additions/deletions/modifications, single-link/owner/no-follow/size
refusals, namespace reservation, long paths, output boundaries, source changes
during builds, publication receipt binding and preservation. It also checks
cache-only-before-push call order, **not native regex semantics**; the recorder
explicitly refuses frontend subrequest flags. The APFS alias case is skipped on
case-sensitive test volumes.
Actual registry/nonroot image readability and stock-SBX remain separate host gates.

Strong-provenance CLI tests additionally use a fresh tiny
**official Rust 1.95** / real Cargo / `build-candidate.py` observation fixture,
with a controlled image peer. They do not patch the verifier or adopt an old
receipt. This proves consumer/protocol behavior, **not** that the peer's simulated
image digest or ARM-named executable copies qualify a real image. The fixture
uses an isolated offline dependency-free build, not workspace Cargo churn.

When the actual current host image/build receipt is ready, run the maintained
no-Docker proof/preparation caller (no historical or synthetic fallback):

```sh
python3 tests/kits/strong_provenance_uat.py \
  --source-tree /canonical/current/candidate \
  --shell-image /private/observed-export/shell-image \
  --shell-build-receipt /private/host-control/build.json \
  --evidence /private/new-strong-proof-evidence
```

It calls the real preparer, verifies the positive exact-path document, tests
copied image/helper-only/symlink refusals without changing original artifacts or
old output, then revalidates the original observed package. It invokes no Docker,
registry, compiler, or SBX effects. A current final receipt is required;
its absence is not permission to bless a historical image.

## Opt-in actual frontend regression

On the authorized host with a local Docker Engine, run the maintained live test:

```sh
python3 tests/kits/native_frontend_uat.py --run-native \
  --docker /absolute/path/to/docker \
  --buildx-plugin /absolute/path/to/docker-buildx \
  --docker-host unix:///absolute/path/to/docker.sock \
  --evidence /canonical/private/parent/new-native-evidence
```

This is opt-in and is **not run by the recorder suite**. It selects an explicit
local Unix Docker endpoint and a fresh credential-free Docker configuration with
the supplied Buildx plugin, records tool versions/SHA256 and source/context fences,
and runs the exact five pattern cases: end-anchor `\z`, POSIX digits, invalid
backreference, invalid default with valid override, and nested quantifier mismatch
with 20,001 bytes. Frontend and conformant DHI base are pinned by digest. The fixed
Dockerfile has `USER agent`, inert `ENTRYPOINT`, and **no RUN instructions**.
The same cases also call the real publisher's cache-only route; its retained
4,096-byte input bound refuses the oversized nested case before Docker. The
20,001-byte direct-native case separately proves native finite mismatch behavior.

Each live command has a 120-second default deadline (bounded override to 600).
Negative cases require native pattern diagnostics; a generic setup/unsupported
frontend error is not a passing refusal. No push, image load, workload/agent
execution or SBX command exists in this harness. Public image resolves and
builder-cache writes are possible; do not describe this as zero build effects.
A fresh host run is still required for the exact current publisher.
