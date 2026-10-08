# Workspaces acceptance scenarios

Black-box scenarios for `docs/workspaces.md` (cited as `sN`), written before
the implementation. Harness: `tests/acceptance/workspaces_uat.py`, run by
`make dev-workspaces DEV_KIT=<fixture>` against `~/.marsh-dev` (`--prefix`).
There are no unit tests or mocks. Callers are host bash, `marsh -c` sessions,
the host CLI, and registered fixture Kit commands. Inside Kit jobs the caller is
the shipped `marsh` shim, driven by the fixture's argv-only `pipeline SEP ARGV
[SEP ARGV]...` mode. That mode turns `%%%` into `:::` one level at a time,
because a job argument cannot be the literal `:::`. The OCI fixture published
before this change lacks `pipeline`. With it, W17-W21 fail and say so; pass
`--kit tests/acceptance/fixture` or republish.

The harness observes only stdout/stderr bytes, exit statuses, `PIPESTATUS`,
the user's tree, and the user's `.git`: index bytes and mtime, HEAD, refs,
packed-refs, object list, config, and hooks. It also reads `out/`
artifacts, public JSON (`splits`, `status`, `jobs show`), and stock
`sbx exec <owned vm> docker inspect`. Each run is isolated in its own
MARSH_HOME and control home. Cleanup removes only VMs in its own ownership map
whose stable IDs it has seen (run.py `IsolatedScopeCleanup`). Every subprocess
has a timeout and is killed by process group. A preflight checks
`marsh --help`, which needs no VM, then `splits --json`, `split --help`, and
`join --help`. If the CLI is missing, it marks every scenario blocked before
any VM work. Without the preflight, an unknown `marsh split` runs as a Brush
script and boots a shell VM. Use `--no-preflight` to skip it,
`--only ID` (repeatable) or `ONLY=` to select scenarios, and `--fail-fast` to
stop at the first failure. Evidence goes to `<evidence>/workspaces.json`.

| ID | Spec claim | Setup / action | Pass criteria |
|---|---|---|---|
| W01 plain-bash | s2 primary CLI; spool; argv verbatim; `SPLIT_*`; release | host bash: ``printf 'in\0put\n' \| marsh split -b sh=cat ::: k fixture streams 'a b' '$x' '' '*' \| marsh join -- sh -c 'copy out/'`` | `PIPESTATUS` `0 23 0`; exact `out/{sh,k}/{stdout,stderr,status}` bytes; `SPLIT_ID/DIR/MANIFEST/OBJECTS`, with `SPLIT_DIR=<root>/.marsh/split/<id>/out`; manifest v2 with placement `shell-vm` and `kit:*`; split removed (CMD 0); user state unchanged; Kit container verified deleted |
| W02 lease-kept-joinable | s6 lease; s2 two-step | session: `h=$(marsh split ... </dev/null)`; sleep 68 s; `marsh join -- CMD <<<"$h"` | `splits --json` state `kept` after the lease; join works and CMD sees `A\ta.txt`; consumed(0) removes |
| W03 snapshot-fidelity | s1 snapshot S/H/I; s8 gitfile; ignores | main repo with staged, unstaged, deleted, untracked, and ignored edits; `-b g='git status; git diff --cached; rev-parse; ls -A'` | fork status, cached diff, and HEAD equal the user's; ignored `secret.log` and `.marsh` absent; `.git` is a file; user index bytes and mtime, refs, objects, and config untouched |
| W04 snapshot-consistency | s8 M2 frozen base | branch waits on a gate; the user then edits `u.txt`, adds `late.txt`, and stages | branch reads `one` and no `late.txt`; no `diff.patch` or `files`; the user's edits and index survive |
| W05 patch-roundtrip | s5 layout; s8 patch writer (literal binary, renames as D+A) | shell branch: binary append, chmod +x, symlink retarget, file to symlink, delete, add binary, add in new dir, no-EOL, non-ASCII path with space; Kit chmod and Kit symlink; `join -- git apply` each `diff.patch` | user tree equals the expected bytes, exec bits, and link targets; `files` sets exact (`M`, `T`, `D`, `A`); user `.git/index` unchanged; the rendering shows `binary file changed: PATH (N bytes)`, never `GIT binary patch` |
| W06 non-git | s1 (S without H/I); s8 confinement | plain dir with shell edits, a Kit write, a Kit absolute write; `join -- git apply` | patches apply, including binary; escape file absent; no `.git` created; no `.marsh/.gitignore` |
| W07 status-pipefail | s2 declaration order; pipefail | `-b a='sleep 2; exit 3' -b b='exit 5'` with `join -- true` / `join -- exit 4`, with and without pipefail | `p1=3 3 0`, `p2=4 3 4`, `p3=0 3 0`; `join -- true` tolerates CMD not reading stdin |
| W08 join-after-failure | s2 CMD runs after failure; s2 sugar | `-b bad='...; exit 4'` then `join -- CMD`; Brush `printf in \| split { a: cat; b: exit 3 } \| join \| {...}` | CMD gets the rendering with statuses and `out/bad/{status,stderr}`; CMD 0 removes the split; sugar `PIPESTATUS` `0 3 N 0`; `$SPLIT_DIR/a/stdout`; `SPLIT_ID` unset afterwards |
| W09 retention | s2 release; s6; s5 status counts | four splits: bare/ok, bare/failed, `--keep`, `-- false` | first one removed; the other three are `kept` in `splits --json`; `status --json` kept count ≥ 3; `splits rm` removes each |
| W10 environment | s2 env; s2 no tty | host: credential-shaped names (`*_KEY*`, `*TOKEN`, `*PASSWORD`, `*_PAT`, `*SECRET`, `*CREDENTIALS`, `AWS_*`) never reach a branch; exported, unexported, `MARSH_*`, `DOCKER_HOST`, `SBX_*`; session: function, unexported var, `set -f` | only the exported value arrives; `/dev/tty` fails; no function or `set` option inherited |
| W11 kit-confinement | s8 mounts (`NoWriteToUserTree`, `SnapshotImmutable`, `AdminReadOnly`); s4 no sockets; s5 lineage | Kit branches write absolute `<proj>/x`, `../../../../x`, `.git/x`, `.git/objects/x`, `../store.git/x`; plus `ok`, `authority`, `identity` | escapes fail and nothing appears; `ok` = `A\tinside.txt`; no sockets or authority env; cwd = fork; each receipt's mounts ⊇ the s8 table (admin dir read-write, its config/HEAD/info/packed-refs and the gitfile read-only) and no other user-tree path; store and user objects read-only; receipts carry `lineage`; containers deleted |
| W12 admin-tamper | s8 capture verification; s11 bullet 4; review M6 | Kit writes `../.admin/lk/COMMIT_EDITMSG` (must succeed: Git state stays writable) and writes `../.admin/L/{config,HEAD,info/exclude}` (fsmonitor and hooksPath to real host scripts) and replaces the `.git` gitfile; `join --keep`; host `git -C <fork> status` and `commit` | each branch is `rejected:` or failed with EROFS/EBUSY/EACCES (not ENOENT); fork config has no `core.fsmonitor/hooksPath`; marker files absent. A control repo first proves host git runs fsmonitor |
| W13 nested-dotgit | s8 no nested `.git` at any depth (case-insensitive APFS) | Kit writes `src/.git` and `src/.GiT` gitfiles that point at a repo with an fsmonitor marker | both `rejected:` and fork removed; sibling branch `exited 0`; marker absent |
| W14 hostile-user-repo | s8 isolated gix; s11 bullet 5 | user repo config: fsmonitor, required clean/smudge filter, `diff.external`, hooksPath, `include.path`; dirty file | split and join succeed; no marker from any of them |
| W15 label-and-argv-refusal | s2 labels; s2 registered CMD; s7 all-or-nothing | `../x`, `a/b`, `A`+`a`, duplicate, `out`, `Base`, `.admin`, `::: ../y`, `::: k /bin/sh -c`, `::: k bash -c` | each exits 2 with a diagnostic; marker absent; no new split dir; no Kit job |
| W16 caps-and-pool | s4 fan-out cap 16; pool of 8 (`BudgetBound`) | 17 branches; then 9 × `sleep 6` with `join -- CMD` | 17 gives 2 and nothing runs; 9 gives exactly one `failed: capacity` and 8 `exited 0`, both visible in the rendering, and CMD runs |
| W17 nested-from-kit | s2 shim; s3 creator-only join; s1 sibling forks; s5 lineage | session: `::: p fixture pipeline \| marsh split %%% c ... \| tee /dev/stderr \| marsh join --keep`; then the session joins the child handle | child created and kept; the ancestor's join exits nonzero and its CMD never runs; child lineage names the parent; child `out/` is a sibling under `.marsh/split/`; `c.txt` absent from the user tree; receipts verified deleted |
| W18 depth-cap | s4 depth 3 | `d1 -> d2 -> d3` nested through `pipeline`; d3 attempts the depth-4 split | root exits 2 and `out/d1/status` = `exited 2`; "depth" visible; exactly 3 Kit jobs (d4 never runs) |
| W19 interrupt-subtree | s2 signals; s7 cancel | host split: shell `sleep 15; touch late` + Kit `hold` + nested Kit `hold`; SIGINT to the process group | exits 130 within 25 s with no handle; a later join refuses and its CMD does not run; `late` never appears; all 3 containers verified deleted; the confirmed-cancelled split directories are removed |
| W20 creator-exit-cascade | s4 revoke on exit; s7 | `p`: `pipeline \| --exit-after 20 marsh split %%% c fixture hold 600` | `p` = `exited 7`; child split cancelled or done; both containers deleted; finishes in under 120 s |
| W21 daemon-restart | s6 restart; `NoReplay`; s7 quarantine | shell + Kit `hold` running; authenticated daemon shutdown; next command restarts the daemon | client exits nonzero; split `uncertain`, forks flagged `untrusted` and retained; no Kit job is created after the restart; `workers reset fixture` succeeds |
| W22 git-clean-replaced | s6 `(dev, ino)` check; fd-based removal | branch gated; user runs `git clean -xfd`; recreates `<id>/w/sentinel` | split nonzero; `workspace replaced` reported; sentinel survives join and `splits rm` |
| W23 non-creator-join | s2/s3 creator only | session A creates; session B runs `join -- touch m` | B nonzero; marker absent; split not released |
| W25 planted-symlinks | review C1: descriptor-only host I/O | split running; an ordinary Kit job tries `project-symlink out/a/stdout -> <host file>`; the host plants `out/a/{stdout,status}` symlinks | the Kit job fails (`.marsh` is read-only to it); the host file is unchanged after capture and `splits rm`; `tampered` is recorded |
| W26 nested-repository | review H2: nested `.git` pruned at snapshot | user tree holds a nested repository `vendor/lib` | split exits 0; the fork lists no `.git` there; `files` = `A\ta.txt`; the user's nested repository is intact |
| W27 sugar-interrupt | s2 signals for the Brush sugar | `split { a: ...; sleep 30; touch late } \| join \| touch joined`, SIGINT | exits within 25 s with 130; neither `late` nor `joined` appears; the split directory is removed |
| W28 quarantine-retire | s7 quarantine; operator retire by recorded UUID | (1) stop the Kit VM under `fixture hold`; `workers reset fixture`. (2) a shell that ran `fixture` loses its VM; `workers reset all`, then `reset`. (3) Kit quarantine; `reset`. (4) shell loss; `stop` | every command exits 0, prints what it removed (or the cause), and the VMs are gone from `sbx ls` |
| W29 doc-examples | `docs/split.md` as written | every marked example, from host bash and/or `marsh -c`, in a fresh repo or plain dir; the `--3way` recipe after the other side last wrote the index, plus a control run without the refresh | statuses and output as marked; the control fails; `status` text shows attached shells and split counts; `--help` lists no `flight` |
| W30 capture-ignored | s8 capture of ignored output, rendering, phase times | Git repo with `.gitignore` = `__pycache__/`, `*.log`; a shell branch runs `python3 -c 'import x'`, writes `build/__pycache__/y.pyc` and `run.log`, adds `notes.txt` to `.gitignore`, edits `notes.txt`, appends to `blob.bin`; `join --timing -- CMD` | the branch made a `.pyc`; `files` = `M .gitignore`, `M notes.txt`, `M blob.bin`; no `diff --git` names a `.pyc` or `run.log`; the rendering shows `binary file changed: blob.bin (5 bytes; full patch in $SPLIT_DIR/py/diff.patch)`; stderr has `timing: snapshot …; forks …; run …; capture py …; consumer …` |
| W24 performance | proposed budget (spec states none) | 10k-file committed repo, warm; median of 3 for `split -b a=true \| join` vs `marsh -c true` | overhead ≤ 1.5 s |

## Open questions found while writing these

1. Host CLI creator identity. s2 shows `marsh split … | marsh join` and the
   two-step `h=$(…); marsh join <<<"$h"` from a plain terminal. In that case
   each process opens its own ephemeral session. W01, W04, and W05 assume the
   host user is one creator. W23 uses two `marsh -c` sessions.
2. JSON shapes are not specified for `splits --json`, `status --json` split
   counts, manifest branches, or receipt `lineage`. The harness only searches
   for records with `id`/`state`/`label`/`placement` and for `kept` counts.
3. Depth numbering. The harness takes the root split as depth 1.
4. The state string after a cancel (`cancel`, `cancelled`, or `done`).
