# Shell attachment, interruption, and cleanup

These are the implementation's shell transport boundaries. Linux socket/process
checks support them; release qualification still requires the matching Mac and
Linux artifacts on supported stock SBX. See the
[local product contract](../plan/local-product-contract.md).

## Streams and status

- Stdin EOF closes byte input, not the control connection. Signal and resize
  requests remain usable while the shell runs.
- A connected slow reader applies backpressure. Pausing a pager or stopping the
  controller does not, by itself, mean the controller disconnected. There is no
  five-second shell output-write deadline. Resume the reader to continue delivery.
- A PTY is selected only when stdin, stdout **and stderr** identify the same
  terminal device. Redirected or unconfirmed descriptors use separate pipes,
  including a redirect to a different TTY. For
  example, `marsh -c 'echo out; echo err >&2' 2>err.log` keeps guest stderr in
  `err.log`, not in host stdout.
- Complete, verified executions preserve their actual status, including ordinary
  `exit 1`. Shell daemon/transport failure, incomplete delivery, unknown exit,
  quarantine, and unverified cleanup return **125**. CLI usage errors remain **2**.
  This is shell-delivery error classification, not a blanket change to every
  marsh command's errors. Ordinary pipeline last-stage and `pipefail` rules still
  apply to the surrounding shell.

Input credits bound pending stdin; output is streamed in bounded chunks, not
accumulated into a full-output buffer. The control reader does not wait for output
credits or for a diagnostic to be printed. Repeated rejected-control diagnostics
may be coalesced under an output-blocked control flood; the control effects are
not queued behind those diagnostics.

## Controller ownership

An authenticated OpenShell request reserves one daemon-local session generation
before its acceptance frame. A duplicate open or public detach cannot retire an
active generation. If the acceptance write fails before backend admission, only
that request's registration is released. Once backend work is admitted, an
unfinished owner retains cleanup uncertainty rather than claiming guest absence.
Only the matching owner's completion can settle the generation. This is not
restart recovery or an authorization grant to another session.

An actual worker exit remains a separate receipt fact: incomplete required output
or uncertain cleanup reports public125 even when the actual worker exited7.
Complete, verified exit7 remains7. A frame and its public receipt must agree on
that public code; actual execution is not overwritten with a guessed failure.

## Ctrl-C during preparation

The host terminal stays in its normal mode until the backend has observed guest
containment readiness. Ctrl-C during preparation cancels the controller instead
of saving a `^C` byte or signal frame for a future shell. The backend checks loss
between preparation operations, rolls back session-owned resources, and cleans any
attachment already being started. A shared warm VM cache is not destroyed merely
because one controller cancels. The CLI waits for an independent cleanup-state
observation before returning the interruption status (130 for Ctrl-C).

An already-running stock operation may need to finish or reach its own bound
before rollback completes. Cancellation is not transactional undo of filesystem
changes made by user code whose start was already in flight. If rollback cannot
be confirmed, the result is 125 and recovery is required. After readiness,
Ctrl-C has ordinary attached-shell signal semantics.

The attachment control protocol accepts INT, TERM, KILL and HUP and their named
aliases. Unsupported names are rejected before a signal is sent. A guest
signal/resize helper rejection is reported as a nonterminal control error; it is
not proof of controller loss and does not replace the shell's actual later exit.

## What cleanup proves

Cleanup evidence is separate from output delivery and from local process reaping:

1. The guest session's admitted kernel containment must be observed empty.
2. Local attachment I/O is cancelled when necessary and its owned workers joined.
3. The trusted guest relay's exact PID/start identity must have exited, with the
   same observed guest boot, and its socket and token paths must be absent.
4. Mount/reference teardown must succeed, or retain authority and disclose
   uncertainty for operator recovery.

Revoking a relay token denies future authenticated requests; it does **not** prove
that the guest relay exited or removed its files. A local `sbx exec` proxy exit is
not that proof either. Failed inventory, changed identity, a live relay or retained
paths cannot be reported as verified cleanup. These checks do not create hostile
root isolation inside the selected-home trust domain.

## Recovering uncertainty

From the **host**, with the same selected home and control configuration:

```sh
marsh status --json
# Exit other shells and finish active jobs in this scope, then choose one:
marsh reset
marsh stop
```

Each prints what it removed on stderr (exact VM names from the daemon's
report) and names every VM it left with the next step; it exits 1 unless all
cleanup was verified. Add `--json` for the full report on stdout. Reset removes the owned
VM but leaves the daemon available; stop also stops the daemon when cleanup is
complete. A failed inventory is not evidence of absence. Active sessions, a
changed VM identity or failed removal keep recovery fenced. Restore stock SBX
health and retry the same operation; do not delete control state, guess a VM name
or kill the daemon to bypass quarantine.

Project files and persistent selected-home data are not discarded by this
recovery. VM-local package installs are. Keep any explicitly retained ephemeral
HOME until the corresponding cleanup is verified.
See [troubleshooting](../troubleshooting.md#removing-vms) and the
[stock adapter](stock-sbx-adapter.md).

## Measuring open and teardown

Measure shell open and teardown from the outside: wall time of `marsh -c true`
cold and warm (`tests/perf/warm.py`). Do not infer stock performance from a
Linux socket fixture or remove serial checks merely to lower these numbers.
Compare exact-candidate cold and warm runs on the supported host before making
a latency claim.
