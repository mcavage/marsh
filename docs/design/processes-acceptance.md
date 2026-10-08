# Nested processes acceptance scenarios

Black-box scenarios (P01-P38, P26 retired in revision 5; P32 is billed and runs only with `--live`) for `docs/processes.md` (cited as `sN`), written from the
spec before the implementation. Harness: `tests/acceptance/processes_uat.py`,
run by `make dev-processes DEV_KIT=<fixture>` against `~/.marsh-dev`
(`--prefix`). It reuses the `workspaces_uat.py` callers and run.py isolation:
own MARSH_HOME and control home, timeouts and process-group kills on every
subprocess, cleanup of only owned VMs by recorded identity, and a stock
`sbx ls` diff at the end. No unit tests or mocks.

**Callers.** Host bash, `marsh -c` sessions, the host CLI, and registered
Kit jobs acting as agents. The fixture's argv-only modes stand in for an agent
process: `pipeline | ARGV` runs `bash`, `/bin/bash`, `/bin/sh`, `env -i`, or
a registered name with no shell in between (Node `spawn('bash')`, a direct
`execvp`); `exec ARGV` replaces the entrypoint process, as a script's
`exec claude` does; `bench` times shells inside the job; `cap-flood` holds N
connections to `/run/marsh/cap.sock`. The harness adds `fixture-alt`, a second
registered name for the same fixture Kit, to its private `commands.json`. It
uses the packaged `shell` Kit as a second, different Kit.

**Observations.** stdout/stderr bytes and exit statuses, `jobs --json`,
`jobs show --json` (`lineage`, `mounts`, `vm_id`, `cleanup`), `jobs --tree`,
`status --json`, `marsh context`, the user's tree and `.git`, the selected
home, and stock `sbx exec <owned vm> docker inspect`, which proves each
container is deleted. A preflight checks VM-free `marsh --help` for `run` and
`context`, then `run --help` and `context`. If either is missing, every
scenario is `blocked` before any VM work. Use `--only`/`ONLY=`, `--list`,
`--fail-fast`, and `--no-preflight`. Evidence: `<evidence>/processes.json`.

| ID | Spec claim | Action | Pass criteria |
|---|---|---|---|
| P01 job-surface | s4 environment, nothing shadowed, read-only binds; s15 real bash | `/bin/sh` probe in a job | PATH starts `/run/marsh/bin:`; `SHELL` not under `/run/marsh`; `MARSH_JOB` = receipt id; PATH `bash` is the image's GNU bash (same `--version` as `/bin/bash`); no `bash` or `sh` link; links for `marsh`, `fixture`, `fixture-alt`, `shell`; `type fixture` = `fixture is /run/marsh/bin/fixture`; `context.md` names the job and its spawn set; artifact, `bin/`, and `job.json` not writable; `job.json` names the job and `fixture`; 1 job |
| P02 self-spawn-bash-c | s1, s5 child case, s9 lineage, s3 `ProcessShow` | job: `bash -c 'fixture identity; "$SHELL" -c "fixture identity"; marsh jobs --tree'` | 3 jobs; each child's parent is the job, same root, depth+1; `jobs show` lists the child ids; host `--tree` shows all 3; the in-job tree shows its own subtree and not an earlier unrelated job; all deleted |
| P03 direct-execve | s4 table row "registered name… direct execvp"; s1 any shell | `pipeline \| fixture streams 'a b'`; `/bin/sh -c` running the link path, the bare name, and `env fixture` | exact child bytes and status 23 relayed; 2 and 4 jobs with correct lineage |
| P04 entry-once | s5 entry case (`EntryOnce`, `NoEntryRecursion`) and both guards | `fixture exec fixture identity`; the same `exec` then a PATH/`MARSH_ENTRY` probe; then the image's `bash -c fixture`; control `exec env -u MARSH_ENTRY fixture` | 1 job (no recursion); marker unset and PATH still starts with the links after entry (GM 1); the image's bash still reaches the links (2 jobs); without the marker the own name is a child (2 jobs) |
| P05 image-tool-local | s5 image tool case | `fixture-alt identity`; in a `fixture-alt` job, `fixture` and `/usr/local/bin/marsh-fixture` | both run locally: 1 job each, output present |
| P06 cross-kit-default | s6 cross-Kit default | `fixture` job runs `shell -c …` and `fixture-alt` with no flags | both succeed; children `shell` and `fixture-alt` under the job; the shell child runs in a different VM with its own `MARSH_JOB` |
| P07 run-three-callers | s3 `ProcessRun`, s5 "`marsh run N` always child" | `printf in \| marsh run fixture streams x` from the host CLI, a session, and a job | the same stdout bytes and exit 23 in all three; the host and session runs are session roots; the in-job run is a child of the job |
| P08 fanout-in-job | s1 CLI forms in every shell; s2 shell-local concurrency; s15 | `printf input \| marsh fanout ::: copy cat ::: upper tr a-z A-Z \| marsh collect` via PATH `bash` and `/bin/bash`; `--json` with a `fixture identity` branch and a failing branch; the `fanout { }` sugar; `marsh fanout -b` | exact frame, `ps=0 0 0`, no extra jobs; labels in order, `ps=3 3`, one child job under the job; the image's bash rejects the sugar; `-b` exits 2 with the session-shell message and never runs; user tree unchanged |
| P09 cli-split-real-bash | s1 CLI in every shell; s7 split snapshots the job's view | job `/bin/bash -c 'marsh split ::: a fixture project-write … ::: k fixture project-write … \| marsh join -- cat'` | `ps=0 0`; rendering lists both files; each branch job's lineage names the creating job; user tree unchanged |
| P10 env-i-links | s4 links and the in-job CLI start from `job.json` alone | `env -i /run/marsh/bin/fixture identity; env -i /run/marsh/bin/marsh jobs --json` | both 0; child under the job |
| P11 env-forwarding | s6 environment for children; split filter; s13.8 `job.json` | job exports `JOB_EXPORTED` and secret-shaped names, sets an unexported variable, runs `shell -c env`, then changes the image var | the child sees `JOB_EXPORTED`, never `FIXTURE_IMAGE_ENV` (image ENV), the unexported value, any secret, or the parent's `MARSH_JOB`; it sees the changed image var; `job.json`'s digest of `FIXTURE_IMAGE_ENV` is not a plain SHA-256 of the value and no key is published |
| P12 depth-cap | s6 depth 4 (`LineageWF`) | a fixture/shell/fixture/… chain six levels deep | some level exits 125 with `depth limit 4`; one job per depth up to 4, none deeper (works for a root depth of 0 or 1) |
| P13 same-kit-chain | s6 same-Kit 2 (`SameKitBound`) | fixture → `bash -c fixture` → `bash -c fixture` | L3 = 125 with `fixture → fixture → fixture refused: same-Kit chain limit 2 (see /run/marsh/context.md)`; exactly 2 jobs |
| P14 fan-out-cap | s6 fan-out 4 (`FanOutBound`); fail fast | 4 gated live `shell` children, then a fifth | the fifth exits 125 within 10 s with `fan-out limit 4`; 4 children under the job |
| P15 pool-fail-fast | s6 pool 8 (`BudgetBound`); never queues | session: 7 held jobs, then a job whose child is the 9th | the child exits 125 with `capacity: 8 jobs`; its parent finishes within 30 s (no deadlock); 8 jobs, all deleted |
| P16 spawn-narrowing | s6 narrowing only (`SpawnSetAttenuates`) | `MARSH_SPAWN=fixture`: `shell` by name, `marsh run shell`, an in-job `MARSH_SPAWN=` widen, `--spawn fixture,shell`, own name; host `run --no-spawn` | the name fails (also in the grandchild, which still has every link, GM 5); `run` exits 125 naming spawn/shell; neither widen works; no `shell` job; lineage `spawn` never holds `shell`; own name works; `--no-spawn` keeps the socket (review L11) but no working name or `marsh run` |
| P17 branch-confinement | s7 view copied verbatim; capture waits for the subtree | host split branch `b`, whose `bash -c` children probe `<project>/README.md`, write outside, write `child.txt`, and run `identity` | `missing`, escape fails, and `A\tchild.txt` is in `files`; child cwd is in the fork; child `mounts` equal the branch's and exclude the project; user tree unchanged |
| P18 tty-child-refused | s6 no TTY children | under a real pty: `fixture pipeline \| fixture identity` | `interactive child jobs are not supported yet` and nonzero; 1 job |
| P19 ctrl-c-tree | s8 Ctrl-C cancels the whole tree | job runs `shell -c 'sleep 600' & fixture hold 600 & wait`; SIGINT to the root caller | nonzero within 25 s; 3 jobs across ≥ 2 VMs, all verified deleted |
| P20 parent-exit-cascade | s8 job end (`CancelPropagates`, `NoOrphanRunning`) | `pipeline \| --exit-after 15 shell -c 'sleep 600'` | parent exits 7; the shell child is deleted within the bound |
| P21 daemon-restart | s8 restart (`RestartUncertain`, `NoReplay`) | parent + child running; SIGKILL the verified owned daemon | client nonzero; both jobs uncertain/`daemon_restarted`; no job created after; `workers reset fixture` exits 0 |
| P22 cap-socket-cap | s3 capability socket fixes | in-job `cap-flood 24` (image binary, local), then `fixture identity` | ≥ 8 admitted; excess connections get `too many concurrent daemon requests in this job`; none silently dropped; a spawn works afterwards |
| P23 awareness-context | s9 awareness | host and session `marsh context`; in-job `marsh context` vs `/run/marsh/context.md` | mentions `marsh run` and `MARSH_SPAWN`; host = session; in-job print = file; project and home unchanged, no `CLAUDE.md`/`AGENTS.md`; `status` has `processes` |
| P24 real-bash-startup | s15 performance | `fixture bench 40` interleaving `bash -c true`, `/bin/bash -c true`, `/run/marsh/bin/marsh --help`, `/bin/true` | PATH bash p50 within 2 ms of `/bin/bash`; `marsh --help` p50 ≤ `/bin/true` + 20 ms; no failures |
| P25 child-after-first-split | s7 view copied verbatim (review H1) | a job in a fresh repo (no `.marsh`) runs `marsh split ::: k fixture identity \| marsh join`, then `fixture identity` | split and child exit 0; 3 jobs (job, branch, child) under one root; the child's mounts include the parent's; user tree unchanged |
| P27 no-shell-branch-from-job | s13.6 (B2): the daemon refuses shell branches from any job | in a job: `marsh split -b` and mixed `-b` + `:::` | each fails with the session-shell message naming `::: LABEL CMD`; the shell branch never ran; no new job, split record, or user-tree change |
| P28 env-whole-tree | s6 (B3) | root `export P28_ROOT=… API_TOKEN=…`; fixture → shell → shell, the middle re-exporting the same value and a new `P28_MID` | depth 1, 2, 3 all see `P28_ROOT`; depth 3 sees `P28_MID`; no depth sees `API_TOKEN` |
| P29 root-interrupt-cascade | s8 Ctrl-C (B4) | `marsh -c "shell -c \"shell -c 'sleep 300' </dev/null \| cat\""` and fixture → shell → shell, then SIGINT to the root caller | returns in < 15 s with 130; every job `cancelled`; deletion verified |
| P30 inspection-text | s9 text forms | a fixture job with one child | `jobs --tree` lines `SHORTID  Ns ago  finished  0  N.Ns  fixture pipeline "|" bash -c "...`, child drawn `└─ fixture identity`; `jobs show SHORTID` finds the job; `jobs show` text has `parent`/`depth` and `children`; `results` rows end with the parent's short id; status text has `processes:`; `marsh -c` sessions do not accumulate in status `shells`; a refused `marsh run` reports before the caller's next output |
| P31 agents-doc-examples | `docs/agents.md` | every `doc-test` example, agents replaced by fixture callers | status, stdout, stderr as marked |
| P32 live-claude-codex (`--live`, billed) | s4/s5 with a real agent | `claude -p "use your Bash tool to run: echo hi-from-bash; then run: codex exec 'reply with the number 7'"`, no `CLAUDE_CODE_SHELL` | the turn exits 0 with both outputs; codex is claude's child in the receipts and in `jobs --tree` |
| P33 jobs-listing-scope | s9 scoped listings | a job in one `marsh -c` session, then a job plus `jobs --tree`, `jobs`, `jobs --tree --all`, `jobs --tree --json` in a second | session text lists only its own job (header, short id, `Ns ago`, `fixture identity`); `--all` and `--json` have both; JSON nodes carry `args` and `session_id`; the host listing shows both (last hour) |
| P34 background-cold-notice | interactive job lines, cold start | `workers reset fixture` and `shell`; interactive `marsh` on a PTY; `shell -c true &`; then `fixture identity` in the foreground | `[1] PID` launch line and `[1]+  Done<pad>shell -c true` notification; no `worker VM` notice from the background job; the foreground cold start prints `[starting fixture worker VM…]` |
| P35 wall-inheritance | s6 wall time | daemon restarted with `MARSH_JOB_WALL_SECONDS=45`; a fixture job sleeps 12 s, then runs `fixture hold 600` | the child's lineage carries the parent's `deadline_unix_ms` with `parent_deadline: true`; it ends within 15 s of that deadline, well before its own 45 s; its `exit.cause` names `parent deadline`; both deleted; daemon restarted without the setting |
| P36 tree-kit-vm-cap | s6 Kit VMs per tree | daemon restarted with `MARSH_TREE_KIT_VMS=1`; a fixture job runs `fixture-alt identity` (same Kit), then `shell -c true` | the alias exits 0; `shell` exits 125 within 10 s with `shell refused: tree already uses 1 Kit VM (fixture) (MARSH_TREE_KIT_VMS)`; 2 jobs; a separate session's `shell -c true` (a new tree) exits 0 |
| P37 split-lineage-tree | s9 splits in the forest | one `marsh -c` session: `split { fix: fixture pipeline '\|' bash -c 'fixture identity'; review: fixture identity } \| join \| fixture identity`, then `marsh jobs --tree` | 4 jobs; the fix job's `lineage.parent` is `split:<id>/fix`, its bash child is its child; the review job's parent is `split:<id>/review`; the consumer's `lineage.consumes` is the split id; the session's tree shows `<id8> … joined 0 … split (fix, review)`, then `├─ fix: …` (`-`), the fix job, its child, `└─ review: …` (`-`), the review job, in that order, and the consumer row ends `(consumes split <id8>)` |
| P38 fanout-cli | s15 CLI fanout | host bash: `printf input \| marsh fanout ::: copy cat ::: upper tr a-z A-Z \| marsh collect`; `marsh fanout ::: a` (usage); `-b w='printf shared > p38-shared.txt'` + `::: id fixture identity` + a branch exiting 4, `collect --timing`; a session running the sugar, the same CLI, and `-b s='echo $((6*7))'` with `collect --json` | host frame exact, `ps=0 0 0`; usage 2; `ps=4 4`, branches in order, `Timing:`, `== bad stderr ==` after its header on stdout, a successful branch's stderr hidden (shown by `collect --stderr`), the shared file written in the user's project, one fixture job rooted in a session; session sugar frame == CLI frame; JSON branch `s` stdout `42` |

## Fixture changes (`tests/acceptance/fixture`)

- New modes: `exec ARGV`, `bench N SEP ARGV…`, and `cap-flood COUNT [PATH]`.
  The ELF entrypoint is unchanged, so existing harnesses behave the same.
- The image now ships `/usr/local/bin/fixture`, a symlink to `marsh-fixture`.
  The entry case and the image-tool case need it.
- New `ENV FIXTURE_IMAGE_ENV=image-config`, which P11 needs to observe forwarding.
- A fixture published before this change lacks these. The scenarios then fail
  and say so. Pass `--kit tests/acceptance/fixture` or republish.

## Spec gaps found while writing these

1. **Entry case vs PATH routing.** After a script entry, s5 removes
   `/run/marsh/bin` from PATH for the image binary. It and its descendants
   then reach the image's `bash`, and their registered names fail or stay
   local. Only `$SHELL` re-adds the links. That contradicts the s4 rows for
   Codex `bash -lc` and Node `spawn('bash')` under a script entrypoint.
   P04 asserts the literal s5 text.
2. **Same-Kit chain definition.** It may count consecutive parent→child
   steps or occurrences in the ancestry. "Kit" may mean the registry name or
   the Kit image (`fixture-alt` shares the fixture's source). P12 assumes
   consecutive steps.
3. **Depth base.** It is unstated whether the root job is depth 0 or 1. P12
   works for either.
4. **Refused admissions.** It is unstated whether a refusal leaves a receipt.
   The harness assumes none, because admission precedes the record (s3).
5. **Spawn-set refusals.** No refusal text is specified. It is also unclear
   whether `--spawn` beyond the parent's set is refused or clamped. A
   non-spawnable name has no link, so it gives 127 rather than the s11
   "refused, with the message".
6. **Split shell branches from a job.** Resolved (s13.6, B2): refused by the
   daemon for every job creator; P27.
7. **Pool scope.** Each host CLI call opens its own ephemeral session, so
   the "session pool" scope is unclear. P15 uses one session.
8. **Unspecified formats.** No JSON shape is given for the lineage `parent`
   (`job:<id>`, `split:<id>/<label>`), for `jobs --tree`, for `status.nested`,
   or for `job.json`. The harness only searches for ids.
9. **Restart exit code.** It is unspecified which client "exits 125 with
   `job uncertain`": the root session, or links inside dead jobs. P21
   requires only a nonzero exit.
10. **Connection cap counting.** It is unclear whether idle connections count
    against the cap, so whether the refusal comes at accept or at the first
    request. P22 holds idle connections.
11. **TTY test.** A child with either stdin or stdout on a terminal is
    refused; the message suggests `</dev/null | cat` (P18).
12. **Not covered.** The total cap of 64 (too costly), the default Kit VM
    cap of 4 (P36 checks the cap at 1; four distinct Kits need billed agent
    Kits), the stdin credit window, and `bash -lc` in the real claude
    image with a selected home (P24 uses the fixture image).
