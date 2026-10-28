# Handoff: split timeouts / lost subagent work (2026-10-27)

Worktree: `/Users/mcavage/dev/marsh-split-detach`, branch
`split-detach-and-quarantine` (from `origin/main` 042c3c7, after v0.2.0).
No code changes yet. This file is untracked; delete it before the PR.

## State of the world

- v0.2.0 released (tag at ae27917, includes #179 cloud-detach UAT fixes and
  #181 shell VM replaced on image change). Tap formula is 0.2.0; the user's
  machine still runs Homebrew 0.1.12 (daemon pid 51907 on `~/.marsh`). Don't
  upgrade it for them; they'll `brew upgrade marsh`.
- #175 (GHA major runners: PyYAML/Python 3.15 on macOS 26, uutils `cat` in
  the jobs suite) merged.
- Clean: no test VMs/sandboxes, merged local branches deleted, my stash
  dropped. User-owned stashes (`fix/pr105-images`, `replay-pi`) and the
  uncommitted docs edits in `~/dev/marsh` (branch raise-job-limits) are theirs:
  leave them.
- Test scopes always isolated: `MARSH_HOME`/`MARSH_CONTROL_HOME` (dirs chmod
  700) + `MARSH_VM_PREFIX=marsh-xNNN-`, `unset DOCKER_SANDBOXES_APP_NAME
  XDG_CONFIG_HOME`, `M=~/.marsh-dev/bin/marsh`. NEVER run the dev marsh
  without these (I did once on 10-26 and put a dev daemon on the user's real
  `~/.marsh`). Tear down with `$M stop` (removes VMs; use `kill <marshd pid>`
  if you need the VMs to survive).
- Build: `make dev` (Docker Desktop must be running). Cheap model for agent
  runs: `claude -p --model haiku --dangerously-skip-permissions` (local
  claude Kit is signed in).

## The report

User: in another agent, `marsh split` jobs whose subagents did chunky tasks /
large refactors failed with "timeout errors". No error text available.

## Reproduced (dev build, isolated scope, `shell` Kit, no models)

1. **Client killed => whole split cancelled, forks deleted.** An agent's tool
   timeout kills `marsh split`.
   - SIGTERM: `split ID: cancelled; every branch ended; removed ...`, exit 130.
     (`split_cli.rs` `CancelOnSignal` sends INT on SIGINT/SIGTERM/SIGHUP.)
   - SIGKILL: same result ~10 s later (daemon `split.rs` ~887-904: reader
     thread cancels when the create stream drops before `finished`).
   - Message never says why. There is no detach mode (`-n` = no stdin).
2. **One job's failed setup kills every sibling on the Kit VM.** 8 split
   branches each writing ~1.5 GB into the container layer (`/tmp`) + CPU burn,
   while probe jobs (`$M -c 'shell -c "echo probe-ok"'`) start every 5 s.
   Reproduced 2 of 6 attempts (timing dependent; disk-full alone, CPU alone,
   or disk-full + one probe did NOT trigger).
   - Probe: `125 (setup_failed; cleanup_uncertain)` in ~1 s =>
     `container cleanup could not be verified; worker quarantined`.
   - All 8 branches: `unknown 125 (supervision_failed; transport_lost: worker
     transport ended: worker control I/O failed: failed to fill whole buffer
     (carrier exited 0); cleanup_uncertain)`.
   - Then every job on that Kit is refused until `marsh workers reset shell`.
   - Kit VM Docker disk (`/var/lib/docker`) is 9.8 GB; per-job writable limit
     is 10 GiB (`MARSH_JOB_WRITABLE_BYTES`), so N jobs share a disk smaller
     than one job's limit. A single job filling it gets a clean ENOSPC (exit 3).
   - Cause chain (code):
     - `marsh-worker/src/lib.rs` `prepare_container` (~492-516): ANY
       `docker create` error except `RuntimeError::ByteBridge` =>
       `CleanupOutcome::Uncertain` + quarantine, without trying to find or
       remove a partial container. Containers have no `--name`/`--label`
       (`marsh-runtime/src/lib.rs` `create_cancellable` ~1957), so a partial
       create can't be located.
     - `marsh-backend/src/lib.rs` `reconcile_worker_cleanup` (~2469) =>
       `sbx.stop_quarantined(vm)` (`marsh-sbx/src/lib.rs` ~4301) kills the
       VM's retained worker process => every running attempt on the VM dies.
     - Also: worker `delete()` caps rm + verify at 5 s
       (`marsh-worker/src/lib.rs` ~549) though the runtime allows 10 s + 10 s;
       under load an ordinary post-job cleanup can overrun => same quarantine.
3. Real agents (4 x haiku branches, 40-module refactor) work fine: 20 s, all
   exit 0, correct patches. Too short to hit either failure.

Also from code (not run): split hard deadline 1 h, not configurable
(`split.rs:33 DEADLINE`, thread ~906-920), looks identical to (1); combined
branch output cap 16 MiB (`split.rs:31 OUTPUT_LIMIT`) cancels the same way.
Full limit inventory from a subagent: see the table in this conversation
(session "Rebase onto main and UAT"); key ones are above.

## Agreed plan (user said yes to A, B, C; D needs discussion)

### A. `marsh split -d` (detached split) + honest cancel reasons

- Contract: add `#[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub detach: bool` to `SplitCreateSpec` (`split.rs:66`, which is
  `deny_unknown_fields`; skip-if-false keeps a new CLI working with an older
  daemon for the non-detached case).
- Daemon `create` (`split.rs:835`): when `detach`, the stream-reader thread
  must not cancel on stream loss (an explicit `Signal` frame can still cancel
  if one arrives before the client goes). Everything else already runs in the
  daemon; the final `SplitFinished` write is `let _ =`. After finish the
  split is `Await`, becomes kept after `LEASE` (60 s) unheld; `join` works on
  `Await | Kept` only (`split.rs:773`).
- CLI (`split_cli.rs`): parse `-d`; after `SplitStarted { id }` print the
  handle line `{"marsh_split":1,"id":"ID"}` and exit 0 without
  `CancelOnSignal`. Do NOT send `DetachShell` for a host split in `-d` mode
  (`split_cli.rs:158`); the daemon's `Detach` guard (`split.rs:843-853`)
  detaches the host session when create finishes. VERIFY live that shell
  (`-b`) branches survive the CLI exiting (the host session's lifetime may be
  tied to the client connection). If they don't, either keep the session
  alive in the daemon or refuse `-d` with `-b` for now and say so.
- `join` on a still-running split currently errors (`split ID is Run;
  nothing to join`). For `-d` to be usable: either make `join` wait for
  `Run` to finish (preferred; respect Ctrl-C = stop waiting, not cancel) or
  document polling `marsh splits ID`. Decide; waiting is friendlier.
- Job callers (`Creator::Job`, in-job `marsh split` via cap.sock): check what
  happens to a detached split when its creator job ends (branches are
  children; lineage/admission in `docs/design/processes.md`). If unclear,
  allow `-d` for host/session callers first and refuse in jobs with a clear
  message.
- Cancel reasons in the message/record: `cancelled: the split's client went
  away (signal TERM | connection lost); use marsh split -d for long splits`,
  `cancelled: split time limit (1h) reached`, `cancelled: branch output
  exceeded 16 MiB`. Set a reason on `Runtime` before `cancel()` and use it in
  the message at ~1006-1024. Shell `split { } | join` path prints nothing
  today on 130 (`split_cli.rs` ~691-700, brush `split_interp.rs:242`); at
  least print the reason to stderr.
- Docs: `docs/split.md` "Long splits from an agent's tool" -> recommend
  `marsh split -d` then `marsh join` (or `marsh splits ID`); update
  `SPLIT_HELP`; `docs/design/workspaces.md` s4 (deadline, cancel on stream
  loss); man page `docs/man/marsh.1.md`.
- Model: workspace/split lifecycle is in `docs/model/Workspace.tla` (check
  for "stream loss cancels"/held/lease). A detached create = never held after
  start; update invariant + negative control + code-location table if the
  model encodes cancel-on-drop.

### B. Don't let one job's failed create quarantine the VM

- Runtime: add `--label marsh.attempt=<attempt id>` (or job id; find what
  `JobSpec` carries) to `docker create`.
- Worker: on create error (not ByteBridge), `docker container ls --all
  --filter label=marsh.attempt=ID` -> `rm --force` each -> verify none left
  => `CleanupOutcome::Verified`, `quarantine: false`, outcome stays
  `SetupFailed { Create }`. Only if listing/removal can't be verified, keep
  today's Uncertain + quarantine.
- Raise worker `delete()` cap from 5 s to cover the runtime's own 10 s rm +
  verify attempts (e.g. 30 s), and check host-side
  `WORKER_TERMINAL_GRACE`/transport idle (30 s) still exceed it.
- Tests: focused worker/runtime tests (fake runner) for "create fails, a
  labelled partial container exists -> removed, verified, no quarantine" and
  "create fails, listing fails -> uncertain + quarantine" (negative control).
  Model: `docs/model/Worker.tla` covers create/cleanup/quarantine; update the
  create-failure cleanup step + table row (README line ~195-199).
- Live: rerun repro 2 (below) several times; expect probe = `setup_failed`
  with cleanup verified, siblings unaffected, no quarantine.

### C. Configurable split deadline

- `split.rs:33` const -> daemon-start setting, e.g. `MARSH_SPLIT_WALL_SECONDS`
  (pattern: `MARSH_JOB_WALL_SECONDS` in `marsh-daemon/src/daemon_startup.rs`
  ~38/96). Default: proposed = the job wall limit (24 h) rather than 1 h;
  confirm with user if unsure. Show it in `marsh status` "Per-job defaults"
  if that's where limits are listed. Docs: split.md:359, workspaces.md:146.

### D. (Discuss first) Quarantine drains running siblings instead of killing

Today quarantine stops the VM worker immediately (fail-closed; Worker.tla).
Draining would refuse new attempts but let running ones finish. Bigger
lifecycle/model change; not started.

## Repro scripts (paste into a shell)

Scope setup (used `$TMPDIR/opencode/splitrepro`):
```sh
R=/private/var/folders/zr/1nbpv_812d17xnm1kdwwvq1m0000gn/T/opencode/splitrepro
mkdir -p -m 700 $R/home $R/control; mkdir -p $R/project
export MARSH_HOME=$R/home MARSH_CONTROL_HOME=$R/control MARSH_VM_PREFIX=marsh-xspl01-
unset DOCKER_SANDBOXES_APP_NAME XDG_CONFIG_HOME; M=$HOME/.marsh-dev/bin/marsh; cd $R/project
git init -q; for i in $(seq 1 40); do printf 'def f%s(x):\n    return x + %s\n' $i $i > mod$i.py; done
git -c user.email=a@b -c user.name=t add -A; git -c user.email=a@b -c user.name=t commit -qm init
```
Repro 1 (client kill): run `$M split -n ::: a shell -c 'sleep 90' ::: b shell -c 'sleep 90'`
from python `subprocess.Popen`, `send_signal(SIGTERM or SIGKILL)` after 25 s,
then `$M splits` (expect cancelled/removed today; with `-d` expect running,
then joinable).

Repro 2 (sibling kill; run 3-5 times, `$M workers reset shell` between):
```sh
HEAVY='cd /tmp; for i in $(seq 1 1500); do head -c 1048576 /dev/urandom > f$i; done; python3 -c "
import time
t=time.time()
while time.time()-t<60: sum(i*i for i in range(100000))
"; echo done > out.txt'
args=(); for b in a b c d e f g h; do args+=(":::" "$b" shell -c "$HEAVY"); done
( for i in $(seq 1 8); do sleep 5; $M -c 'shell -c "echo probe-ok"' 2>&1 | tail -1; done ) &
$M split -n "${args[@]}"; wait; $M jobs | head -12
```

Gates before PR: `cargo nextest run -p marsh-daemon -p marsh-worker
-p marsh-runtime -p marsh-backend -p marsh-sbx -p marsh`, clippy, fmt,
`make test-model MODEL_FLAGS=` if `.tla` files change, live repros above,
then `make verify` if time allows. One PR per fix (A, B, C) is fine.

## After A, B, C (and any other open fixes) are merged: publish 0.2.1

The user wants 0.2.1 to ship these fixes, not before them. Cut from a clean
`~/dev/marsh` on `main`: `make release VERSION=0.2.1` (scripts/cut-release.py:
checks main, runs the full `make regress` on this Mac one suite at a time,
writes the result into the tag message, pushes; `DRY_RUN=1` to rehearse).
v0.2.0 recorded a known skip of acceptance (issue #176) and an intermittent
P35 (issue #178); same `SKIP=acceptance SKIP_REASON=...` likely applies.
Needs Docker Desktop running and a few hours.

## Progress (2026-10-27)

- PR #191 (branch split-detach-and-quarantine): A (split -d, join waits,
  cancel reasons, real signal forwarded) + C (split limit = job wall limit)
  + B (labelled creates; refused create cleaned + verified => no quarantine;
  worker delete cap 30 s; host start-progress 60 s) + URL-open test flake.
  Live: W32/W19/W27/W02 passed; repro 2 x4 clean (32/32 branches).
- D (quarantine drains instead of kills) NOT started; needs discussion.
- New bug found: failed `kit install` (VM create timeout) leaves a
  quarantined VM unreachable by `marsh workers reset` (command never
  registered); only `marsh stop` clears it. Not fixed.
- (Resolved: this was airplane Wi-Fi.) On 10-27 apt inside any Docker container
  failed ("Connection failed") while wget/raw TCP work => `make dev` can't
  rebuild the shell image or local Kits (worked around: `touch
  target/libexec/marsh/shell-image`, use published Kits by another name,
  e.g. `marsh kit install psh docker.io/mcavage/marsh-shell@sha256:6b8d...`).
  sbx self-updated to 0.50.0; `sbx ls` intermittently 10-30 s, Kit VM
  creates time out at 300 s. Don't "fix" by restarting Docker/sbx without
  asking (user's sessions run there).
- 0.2.1: after #191 merges (and the user agrees nothing else is pending).

## 2026-10-28 redrive

- #191 merged; #193 (Kit package layer cache key) merged; kit-install
  quarantine bug filed as an issue.
- Real agents: `split -d` + `join` with 2 haiku branches passed.
- Release worktree: ~/dev/marsh-release on main (~/dev/marsh is another
  agent's, on fix/stale-daemon-after-sbx-upgrade). Pre-run `make regress`
  started there; then `make release VERSION=0.2.1` once main CI passes.
