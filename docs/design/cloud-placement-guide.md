# Run a pipeline in Docker Cloud (draft)

**Design draft.** The session-origin parts (sections 1 to 4) are now shipped
as a preview and documented in the manual: [Docker Cloud](../cloud.md). This
draft keeps the full intended experience, including nested placement, Kits in
cloud, and inspection/recovery flows that are **not implemented**. See the
[design spec](../design/cloud-placement.md) for release gates.

The intended result is a local pipeline whose remote stage returns output and
file edits without making you manage sandbox transfers yourself.

## Prerequisites

- A supported marsh build that implements cloud placement (not released yet).
- Cloud-compatible Kit artifacts. Live probes accepted Kit v2; local marsh uses
  v3, and v3 cloud support was not tested. Existing local v3 Kits must not be
  assumed to run unchanged in cloud. See the
  [probe findings and remaining gates](../plan/cloud-feasibility.md).
- An Apple Silicon Mac with a compatible stock Docker Sandboxes CLI, signed in
  with `sbx login` to the intended Docker account. The cloud minimum version is
  not established yet; the current local minimum is not a cloud guarantee.
- **A Docker Cloud subscription with sandbox access**, available quota, and
  permission to provision cloud resources. An agent/model subscription does
  not supply that compute. Check Docker's current terms and pricing.
- For agent calls, provider credentials configured through Docker's cloud-side
  credential mechanism. Local `sbx secret` setup is not proof of cloud setup.
- A project whose required inputs are included by marsh's snapshot rules and
  whose tools exist in the selected remote image.

**Cloud work uploads project data and costs money.** Ignore rules are not a
secret scanner. Your existing guest home and login files are not uploaded
implicitly. The exact subscription check, cloud provider-setup steps, resource
settings, and recovery commands must be verified before this guide is promoted
to the shipped manual. GPU availability is not promised.

## 1. Pass bytes through a cloud command

Proposed syntax:

```sh
printf 'pear\napple\n' | cloud { cat; } | sort
```

Expected result after implementation: stdout contains `apple` followed by
`pear`, each on its own line. The producer and sort run locally; cat runs
remotely. Startup messages go to stderr, not into sort's input. Output streams
while the remote program runs; stdin is not first collected into one file.

Likewise, a project-file processor can consume relative filenames:

```sh
find . -type f | cloud { process-files; } | sort
```

`process-files` is your tool in the cloud image, not a marsh builtin. Files
must be in the uploaded snapshot: an ignored file printed by local find does
not become available remotely merely because its name was sent on stdin.

## 2. Bring file edits back automatically

**This changes your current project. Cloud results may overwrite local edits
to the same paths. Use the one-branch split below if you want isolation.**

```sh
cloud { printf 'generated remotely\n' > cloud-output.txt; }
cat cloud-output.txt
```

Expected result: the local file contains `generated remotely` after the block
finishes. There is no `--write-back` option. Only the changes relative to the
starting snapshot return, not a replacement copy of the entire project.
Deletions and executable-bit changes return too. A command that exits nonzero
can still return edits, as a local command can edit a file and then fail.

There is no live synchronization. Concurrent edits to a returned path can be
overwritten; unchanged remote paths do not overwrite local changes. Imports
are not an atomic multi-file transaction. If apply fails partway, marsh must
report that and retain the result for inspection, not claim nothing changed.
The final inspection/recovery command spelling is still to be specified.
Unsupported destination filenames (including case/Unicode aliases on a Mac)
are rejected rather than silently renamed or allowed to overwrite a different
path. This is filesystem compatibility checking, not content conflict handling.

## 3. Keep edits private with a split of one

```sh
split {
  remote: cloud { printf 'generated remotely\n' > cloud-output.txt; }
} | join --keep
```

Expected result: join shows the branch result and patch. The cloud delta
returns to the branch's fork; your source tree remains unchanged. `--keep`
retains the artifacts for inspection and explicit patch application. Existing
[split limits](../split.md) still apply, including linked-worktree refusal.
On cancellation, confirmed split forks may be removed as today. Confirmed cloud
payloads must remain in separate recovery storage before that happens; they
must never be redirected into your original project because the fork is gone.

## 4. Choose where concurrency shares files

```sh
cloud {
  fanout {
    tests: make test
    lint: make lint
  } | collect
}
```

Expected result: both checks use one live remote workspace and collect reports
their statuses. This needs your project's test and lint targets and their
dependencies in the image. Shared writes can race.

Putting cloud inside each fanout branch instead creates independent remote
snapshots at each cloud entry, then returns their deltas to the shared caller:

```sh
fanout {
  a: cloud { printf 'a\n' > a.txt; }
  b: cloud { printf 'b\n' > b.txt; }
} | collect
```

Expected result: both files return. If the branches write the same path,
the last applied change wins. No content conflict prompt is promised. Use
split when branches must start from a common snapshot and remain isolated.

## 5. Inspect placement and finish work

The proposed integration keeps cloud blocks and child Kits in `marsh jobs
--tree`, with location and return/cleanup outcomes in structural receipts.
Nested cloud work can split/fanout, and an explicitly local Kit child runs
against a materialization of its parent's view, never a newly reopened original
checkout. Job-origin callers keep their existing narrower authority.
Nested crossings include generated ignored files already present in the
parent's admitted view; an explicit nested split still has split's ignore rules.
Cloud work starts with a private cloud home, not an upload of your laptop home.
Nested children share/return that lineage's home state, never your Mac logins.

Ctrl-C cancels the entire foreground tree, across locations. Confirmed partial
edits may return; an unverified live remote workspace is never imported.
Connection loss does not mean remote work stopped. Do not blindly retry:
inspect the uncertain receipt and owned resource IDs. Cleanup must verify
deletion; a successful cloud delete request alone is not proof of absence.
Pending child returns keep their parent workspace alive. Cancellation revokes
new execution, while the coordinator settles or retains already-started returns
from leaves upward. A timeout reports an incomplete result, not silent success.

Before release, replace this section with verified inspect/reset/stop examples,
including a subscription refusal, an apply failure, and an uncertain cleanup.
No transcripts or benchmarks have been collected for these examples yet.

## Next steps

- [Placement design and acceptance criteria](../design/cloud-placement.md)
- [Split and join](../split.md)
- [Fanout and collect](../fanout.md)

## Publication checklist

Do not remove the draft warning until the examples pass real-caller acceptance.
When shipping, update `install.md` and `faq.md` (Docker Cloud subscription,
compute/model costs), `configuration.md` (placement/shape/TTL/ceilings),
`shell.md`/`shells.md` (block and CLI forms), `agents.md` (inherited placement
and one lineage), `split.md`/`fanout.md` (return destination and visibility),
`security.md` (uploaded project/home/policy boundaries), `troubleshooting.md`
(login/quota/transfer/partial-apply/uncertain cleanup), and `man/marsh.1.md`.
Update the product contract, architecture, AGENTS scope, acceptance index, and
site/agent-readable mirrors only for implemented and verified behavior.
