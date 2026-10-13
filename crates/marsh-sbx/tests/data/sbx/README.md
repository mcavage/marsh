# Stock `sbx` output corpus

Test data for the adapter's parsers, error classifiers and the stateful stock
model in `src/tests/stock_model.rs`. Loaded by `src/tests/corpus.rs` and the
`stock_inventory` tests.

## `v0.47.0/` — captured

Captured on 2026-10-11 from the installed stock CLI
(`sbx version: v0.47.0 0411f50ee4700fe7bd37e6e7e3aced563e850ca9`, macOS arm64)
with read-only commands only. All exited 0 except where noted.

| File | Command |
| --- | --- |
| `version.stdout` | `sbx version` |
| `*-help.stdout` | `sbx <verb> --help` for create, exec, rm, stop, cp, mount, umount, inspect, ls, `template ls`, `mcp load` |
| `ls-json.stdout` | `sbx ls --json` |
| `template-ls-json.stdout` | `sbx template ls --json` |
| `inspect-missing.stderr` | `sbx inspect --json marsh-nonexistent-zz9` (exit 1, empty stdout) |

Edits: in `ls-json.stdout` the host user's home directory was replaced by
`/Users/alice`; nothing else changed. `template-ls-json.stdout` keeps 6 of the
104 rows, verbatim.

The model takes each verb's accepted flags from these help texts, so a flag
stock does not accept fails in tests as it would for real.

## `v0.46.0/` — derived

No 0.46 binary was available. `ls-json.stdout` is the row a human captured
from 0.46 for an earlier fixture (object envelope, upper-case UUID,
`workspace_missing`). `template-ls-json.stdout` is the earlier fixture's object
envelope. `version.stdout` follows the 0.46 format with a placeholder commit.
Help texts are reused from v0.47.0.

## `v0.47-dev/` — derived

Dev builds after v0.47.0, as reported by a human: `sbx ls --json` and
`sbx template ls --json` print bare arrays, some inventory rows carry
`detached`, `sbx version` prints `sbx version vX` (no colon, with a leading
blank line), and `sbx create` no longer accepts `-q/--quiet`.
`version.stdout` is the string a human copied from such a build;
`create-help.stdout` is v0.47.0's with the `--quiet` line removed; the listings
reshape real rows.

## `unverified/` — not captured

Needs a mutating command against a real VM, which is out of bounds here:

- `mount-already-mounted.stderr`: what a stock that refuses to re-mount an
  existing target would print. v0.47.0 documents mounts as idempotent; the
  adapter still handles this wording (`stale_shared_mount_error`).
- `umount-not-exposed.stderr`, `umount-not-bound.stderr`: `sbx umount --help`
  says unmounting a target that is neither mounted nor saved fails. The two
  messages are the format strings found in the v0.47.0 binary
  (`strings $(readlink -f "$(command -v sbx)") | grep 'nothing to unmount'`):
  `nothing to unmount: %q is not exposed in the sandbox` and
  `nothing to unmount: %q is not bound at %q in the sandbox`. `HOST` and
  `TARGET` stand for the quoted paths, and the `Error: ` prefix is cobra's
  usual one; neither has been seen from a real run. The adapter relies only on
  the `nothing to unmount` phrase.

The missing-VM wording for verbs other than `inspect` (exec, cp, mount, rm,
stop) is assumed to match `inspect-missing.stderr`; capturing it needs a
command addressed at a VM name.
