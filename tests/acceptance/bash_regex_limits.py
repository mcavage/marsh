#!/usr/bin/env python3
"""Public shell regex resource, capture-retention, recovery and SIGINT callers.

Use a cohesive --candidate, never the matcher source driver. GNU5.3.20 supplies
finite semantic/signal expectations. The expensive GNU case is a retained
complexity control; only the candidate must return an explicit resource error.
Each invocation owns its PID/session; cleanup never signals shared processes.
"""
from __future__ import annotations
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import sys
import tempfile
import time


def session_members(session: int) -> list[int]:
    if Path("/proc").exists():
        pids = [int(path.name) for path in Path("/proc").iterdir() if path.name.isdigit()]
    else:
        pids = [int(row) for row in subprocess.check_output(["ps", "-axo", "pid="]).splitlines()]
    members = []
    for pid in pids:
        try:
            if os.getsid(pid) == session:
                members.append(pid)
        except (ProcessLookupError, PermissionError):
            pass
    return members


def record_cleanup(result: dict) -> dict:
    deadline = time.monotonic() + .5
    while True:
        members = session_members(result["pid"])
        if not members or time.monotonic() >= deadline:
            result["survivors"] = members
            return result
        time.sleep(.01)


def observe(exe: Path, script: bytes, operands: list[bytes], timeout: float,
            interrupt: bool = False, terminal: bool = False, group_interrupt: bool = False,
            prefix_args: tuple[bytes, ...] = (), interrupt_delay: float = 0) -> dict:
    with tempfile.TemporaryDirectory(prefix="regex-caller-home-") as home:
        return observe_in_home(exe, script, operands, timeout, interrupt, terminal,
                               group_interrupt, prefix_args, interrupt_delay, home)


def observe_in_home(exe: Path, script: bytes, operands: list[bytes], timeout: float,
                    interrupt: bool, terminal: bool, group_interrupt: bool,
                    prefix_args: tuple[bytes, ...], interrupt_delay: float, home: str) -> dict:
    argv = [os.fsencode(exe), *prefix_args, b"--noprofile", b"--norc", b"-c", script, b"regex-limit", *operands]
    env = {b"PATH": b"/usr/bin:/bin", b"LC_ALL": b"C", b"HOME": os.fsencode(home),
           b"HISTFILE": os.fsencode(Path(home)/"history")}
    start = time.monotonic()
    sent = None
    timed_out = False
    if terminal:
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(exe, argv, env)
        out = bytearray()
        status = None
        try:
            while status is None:
                if time.monotonic() - start > timeout:
                    timed_out = True
                    os.kill(pid, signal.SIGKILL)
                if select.select([fd], [], [], .01)[0]:
                    try:
                        out.extend(os.read(fd, 65536))
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                if interrupt and sent is None and b"READY\r\n" in out:
                    time.sleep(interrupt_delay)
                    sent = time.monotonic()
                    os.write(fd, b"\x03")
                waited, state = os.waitpid(pid, os.WNOHANG)
                if waited:
                    status = os.waitstatus_to_exitcode(state)
            while select.select([fd], [], [], 0)[0]:
                try:
                    tail = os.read(fd, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not tail:
                    break
                out.extend(tail)
            return record_cleanup(dict(pid=pid, status=status, stdout=bytes(out).hex(), stderr="",
                        elapsed=time.monotonic()-start, timeout=timed_out,
                        interrupt_elapsed=None if sent is None else time.monotonic()-sent))
        finally:
            if status is None:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.waitpid(pid, 0)
            os.close(fd)
    p = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, start_new_session=True, bufsize=0)
    prefix = b""
    try:
        if interrupt:
            assert select.select([p.stdout], [], [], timeout)[0], "missing READY marker"
            ready_deadline = time.monotonic() + timeout
            while b"READY\n" not in prefix:
                assert select.select([p.stdout], [], [], max(0, ready_deadline-time.monotonic()))[0], "incomplete READY marker"
                assert len(prefix) < 1024, "unexpected output before READY"
                chunk = os.read(p.stdout.fileno(), 64)
                assert chunk, "shell exited before READY"
                prefix += chunk
            time.sleep(interrupt_delay)
            sent = time.monotonic()
            if group_interrupt:
                os.killpg(p.pid, signal.SIGINT)  # exclusively this new owned session
            else:
                os.kill(p.pid, signal.SIGINT)
        out, err = p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        p.kill()
        out, err = p.communicate(timeout=2)
    finally:
        if p.poll() is None:
            p.kill()
            p.wait()
    return record_cleanup(dict(pid=p.pid, status=p.returncode, stdout=(prefix+out).hex(), stderr=err.hex(),
                elapsed=time.monotonic()-start, timeout=timed_out,
                interrupt_elapsed=None if sent is None else time.monotonic()-sent))


def helper_children(owner: int) -> list[dict]:
    """Observe only this paused caller's direct private-helper children."""
    if Path("/proc").exists():
        children = Path(f"/proc/{owner}/task/{owner}/children").read_text().split()
        result = []
        for child in children:
            pid = int(child)
            stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            assert os.getsid(pid) == owner and os.getpgid(pid) == pid
            assert b"--brush-internal-regex-v1" in Path(f"/proc/{pid}/cmdline").read_bytes()
            result.append(dict(pid=pid, start_ticks=stat[19]))
        return result
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid=,pgid=,command="]).splitlines()
    result = []
    for row in rows:
        pid, parent, group, command = row.strip().split(None, 3)
        if int(parent) == owner:
            assert int(group) == int(pid) and b"--brush-internal-regex-v1" in command
            result.append(dict(pid=int(pid)))
    return result


def observe_generations(exe: Path, pattern: Path, value: Path,
                        prefix_args: tuple[bytes, ...] = ()) -> dict:
    """Real [[ =~ ]] caller, paused only by shell read at observable boundaries.

    No fixture peer, injected helper, or out-of-band regex implementation. File
    stderr avoids blocking the maximum diagnostic; stdin releases each pause.
    """
    code = (b'IFS= read -r -d "" p < "$1"; IFS= read -r -d "" v < "$2"; '
            b'[[ aa =~ (a+) ]]; printf "BEFORE\\n"; read -r go; '
            b'[[ $v =~ $p ]]; printf "RESULT:%s:%s\\n" "$?" "${BASH_REMATCH[*]}"; read -r go; '
            b'[[ ab =~ (a)(b) ]]; printf "RECOVER:%s:%s\\n" "$?" "${BASH_REMATCH[*]}"; read -r go')
    start = time.monotonic()
    phases, out = [], bytearray()
    with tempfile.TemporaryFile() as errors, tempfile.TemporaryDirectory(prefix="regex-generation-home-") as home:
        p = subprocess.Popen([os.fsencode(exe), *prefix_args, b"--noprofile", b"--norc", b"-c",
                              code, b"regex-generation", os.fsencode(pattern), os.fsencode(value)],
                             env={"PATH": "/usr/bin:/bin", "LC_ALL": "C", "HOME": home,
                                  "HISTFILE": str(Path(home)/"history")},
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors,
                             start_new_session=True, bufsize=0)
        timed_out = False
        try:
            for _ in range(3):
                line = bytearray()
                deadline = time.monotonic() + 15
                while not line.endswith(b"\n"):
                    if not select.select([p.stdout], [], [], max(0, deadline-time.monotonic()))[0]:
                        raise subprocess.TimeoutExpired(p.args, 15)
                    part = os.read(p.stdout.fileno(), 1)
                    if not part:
                        raise RuntimeError("caller exited before generation boundary")
                    line.extend(part)
                    assert len(line) < 1024
                out.extend(line)
                phases.append(helper_children(p.pid))
                p.stdin.write(b"go\n")
            tail, _ = p.communicate(timeout=3)
            out.extend(tail)
        except subprocess.TimeoutExpired:
            timed_out = True
            p.kill()
            tail, _ = p.communicate(timeout=3)
            out.extend(tail)
        finally:
            if p.poll() is None:
                p.kill()
                p.wait()
        errors.seek(0)
        err = errors.read()
    return record_cleanup(dict(pid=p.pid, status=p.returncode, stdout=bytes(out).hex(),
                               stderr=err.hex(), generations=phases, timeout=timed_out,
                               elapsed=time.monotonic()-start))


def residual_cases(candidate: Path, oracle: Path, prefix: tuple[bytes, ...],
                   selected: list[str] | None = None) -> list[dict]:
    rows = []
    # 513 full-length captures total 32 MiB + 64 KiB. A smaller operand keeps
    # shell input setup out of the 15-second observation deadline on debug
    # product builds while crossing the same aggregate capture-byte boundary.
    capture_source = b"("*512 + b"a*" + b")"*512
    cases = [("healthy-invalid-generation", b"[", b"a", "healthy"),
             ("capture-byte-limit-generation", capture_source, b"a"*65536, "capture")]
    if sys.platform == "linux":
        cases += [("maximum-source-diagnostic", b"[" + b"a"*(1024*1024-1), b"a", "healthy")]
        # N=30 reaches pressure on glibc 2.41 but fits on bookworm's 2.36.
        cases += [("glibc-enomem-terminal-generation", b"(a{255}){255}{255}", b"a", "memory"),
                  ("glibc-enomem-corruption-generation", b"(a{255}){255}{45}", b"a", "poison")]
    elif sys.platform == "darwin":
        # Both '(' and '[' followed by 1 MiB of atoms return REG_ESPACE on
        # Darwin, even in uncapped GNU. This is a terminal-memory caller there;
        # the full healthy diagnostic bound is tested on Linux and by the
        # source-bound synthetic reply peer, not mislabelled as Mac syntax.
        cases += [("darwin-enomem-terminal-generation", b"(" + b"a"*(1024*1024-1), b"a", "memory")]
    if selected:
        unknown = set(selected) - {case[0] for case in cases}
        if unknown:
            raise ValueError(f"unavailable residual cases: {sorted(unknown)}")
        cases = [case for case in cases if case[0] in selected]
    with tempfile.TemporaryDirectory(prefix="regex-residual-") as directory:
        pattern, value = Path(directory)/"pattern", Path(directory)/"value"
        for name, source, operand, policy in cases:
            pattern.write_bytes(source)
            value.write_bytes(operand)
            after = observe_generations(candidate, pattern, value, prefix)
            expected = b"BEFORE\nRESULT:2:aa aa\nRECOVER:0:ab a b\n"
            diagnostic = bytes.fromhex(after["stderr"])
            generations = after["generations"]
            passed = (after["status"] == 0 and not after["timeout"] and not after["survivors"]
                      and after["stdout"] == expected.hex() and len(generations) == 3
                      and len(generations[0]) == len(generations[2]) == 1)
            if policy in ("healthy", "capture"):
                passed = passed and generations[0] == generations[1] == generations[2]
            else:
                passed = passed and not generations[1] and generations[0] != generations[2]
            row = dict(case=name, candidate=after, policy=policy)
            if policy == "poison" and not diagnostic:
                # The glibc 2.41 corruption regression fits below the limit on
                # glibc 2.36. Check its genuine no-match against GNU, recording
                # that this run did not exercise the pre-return crash path.
                code = (b'IFS= read -r -d "" p < "$1"; IFS= read -r -d "" v < "$2"; '
                        b'[[ aa =~ (a+) ]]; printf "BEFORE\\n"; '
                        b'[[ $v =~ $p ]]; printf "RESULT:%s:%s\\n" "$?" "${BASH_REMATCH[*]}"; '
                        b'[[ ab =~ (a)(b) ]]; printf "RECOVER:%s:%s\\n" "$?" "${BASH_REMATCH[*]}"')
                before = observe(oracle, code, [os.fsencode(pattern), os.fsencode(value)], 15)
                row["oracle"] = before
                row["classification"] = "no-pressure-on-this-libc"
                row["passed"] = (after["status"] == before["status"] == 0
                                 and not after["timeout"] and not before["timeout"]
                                 and not after["survivors"] and not before["survivors"]
                                 and not before["stderr"] and after["stdout"] == before["stdout"]
                                 == b"BEFORE\nRESULT:1:\nRECOVER:0:ab a b\n".hex()
                                 and len(generations) == 3 and len(generations[0]) == 1
                                 and generations[0] == generations[1] == generations[2])
                rows.append(row)
                continue
            if policy == "healthy":
                # GNU has no helper children; ordinary observe still checks the
                # exact full diagnostic and raw prior-capture/recovery output.
                code = (b'IFS= read -r -d "" p < "$1"; [[ aa =~ (a+) ]]; printf "BEFORE\\n"; '
                        b'[[ a =~ $p ]]; printf "RESULT:%s:%s\\n" "$?" "${BASH_REMATCH[*]}"; '
                        b'[[ ab =~ (a)(b) ]]; printf "RECOVER:%s:%s\\n" "$?" "${BASH_REMATCH[*]}"')
                # Identical $0 for byte-exact diagnostic prefixes.
                before = observe(oracle, code, [os.fsencode(pattern)], 15)
                before_err = bytes.fromhex(before["stderr"]).replace(b"regex-limit:", b"regex-generation:", 1)
                row["oracle"] = before
                passed = (passed and before["status"] == 0 and not before["timeout"]
                          and not before["survivors"] and before["stdout"] == expected.hex()
                          and diagnostic == before_err)
            elif policy == "capture":
                passed = passed and diagnostic.endswith(b"regular expression capture byte limit exceeded\n")
            else:
                clean = diagnostic.endswith(b"regular expression memory limit exceeded\n")
                signalled = any(diagnostic.endswith(f"regular expression helper terminated by signal {s}\n".encode())
                                for s in (signal.SIGABRT, signal.SIGSEGV))
                row["classification"] = "terminal-memory" if clean else "actual-libc-signal" if signalled else "unexpected"
                passed = passed and (clean if policy == "memory" else clean or signalled)
            row["passed"] = passed
            rows.append(row)
    return rows


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--candidate", type=Path, required=True)
    ap.add_argument("--oracle", type=Path, required=True)
    ap.add_argument("--evidence", type=Path, required=True)
    ap.add_argument("--signals", action="store_true")
    ap.add_argument("--residual-only", action="store_true", help="only N2/N4/N5 public generation callers")
    ap.add_argument("--residual-case", action="append", help="select a named case with --residual-only")
    ap.add_argument("--candidate-arg", action="append", default=[], help="argument before shell options, e.g. --candidate-arg=--marsh-guest")
    args = ap.parse_args()
    if args.residual_case and not args.residual_only:
        ap.error("--residual-case requires --residual-only")
    candidate, oracle = args.candidate.resolve(), args.oracle.resolve()
    candidate_prefix = tuple(os.fsencode(arg) for arg in args.candidate_arg)
    version = subprocess.check_output([oracle, "--version"])
    assert b"version 5.3.20(" in version
    args.evidence.mkdir(parents=True, exist_ok=True)
    identity = dict(candidate=dict(path=str(candidate), arguments=args.candidate_arg, sha256=hashlib.sha256(candidate.read_bytes()).hexdigest()),
                    oracle=dict(path=str(oracle), sha256=hashlib.sha256(oracle.read_bytes()).hexdigest(), version=version.hex()),
                    harness_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), scope="public cohesive shell callers")
    (args.evidence/"identity.json").write_text(json.dumps(identity, indent=2)+"\n")
    if args.residual_only:
        rows = residual_cases(candidate, oracle, candidate_prefix, args.residual_case)
        (args.evidence/"observations.json").write_text(json.dumps(rows, indent=2)+"\n")
        summary = dict(total=len(rows), passed=sum(row["passed"] for row in rows))
        (args.evidence/"summary.json").write_text(json.dumps(summary, indent=2)+"\n")
        print(json.dumps(summary))
        return int(summary["passed"] != summary["total"])
    script = (b"[[ aa =~ (a+) ]]; [[ $2 =~ $1 ]]; s=$?; "
              b"printf '%s\\0' \"$s\" \"${BASH_REMATCH[@]}\"; "
              b"[[ ab =~ (a|ab) ]]; printf '%s\\0' \"$?\" \"${BASH_REMATCH[@]}\"")
    rows = []
    alternating = (b'for ((i=0;i<100;i++)); do '
                   b'[[ xxababz =~ (ab|a)+ ]]; printf \'%s\\0\' "$?" "${BASH_REMATCH[@]}"; '
                   b'[[ foo17 =~ ([[:alpha:]]+)([[:digit:]]+) ]]; '
                   b'printf \'%s\\0\' "$?" "${BASH_REMATCH[@]}"; done')
    before = observe(oracle, alternating, [], 7)
    after = observe(candidate, alternating, [], 7, prefix_args=candidate_prefix)
    rows.append(dict(case="alternating-compiled-expressions", oracle=before, candidate=after,
                     passed=all(before[key] == after[key] for key in ("status", "stdout", "stderr"))
                     and not before["timeout"] and not after["timeout"]))
    for name, pattern, value in [("invalid-retention", b"[", b"a"),
                                  ("backreference-budget", b"^(a|aa)*\\1b$", b"a"*10000),
                                  ("counted-compile-control", b"(a{255}){255}", b"a"*100000+b"c")]:
        # Counted repetition can complete normally near the old 400ms oracle
        # cutoff. Give that finite control the candidate's full observation
        # window; retain the short timeout for the hard backreference control.
        before = observe(oracle, script, [pattern, value], .4 if name == "backreference-budget" else 7)
        after = observe(candidate, script, [pattern, value], 7, prefix_args=candidate_prefix)
        same = all(before[key] == after[key] for key in ("status", "stdout", "stderr")) and not before["timeout"] and not after["timeout"]
        resource = (after["status"] == 0 and after["stdout"] == (b"2\0aa\0aa\0" + b"0\0ab\0ab\0").hex()
                    and b"regular expression" in bytes.fromhex(after["stderr"]) and not after["timeout"])
        passed = same if name == "invalid-retention" else same or resource
        rows.append(dict(case=name, oracle=before, candidate=after, passed=passed))
    for name, expression, value in [
        ("quoted-close-bracket", b'\"]\"', b"]"),
        ("quoted-close-brace", b'\"}\"', b"}"),
        ("quoted-repeat", b'\"a{1}\"', b"a{1}"),
        ("quoted-bracket-piece", b'x\"[.]\"y', b"x[.]y"),
    ]:
        code = (b'set -x; [[ $1 =~ ' + expression
                + b' ]]; s=$?; set +x; printf \'%s\\0\' "$s" "${BASH_REMATCH[@]}"')
        before = observe(oracle, code, [value], 7)
        after = observe(candidate, code, [value], 7, prefix_args=candidate_prefix)
        passed = all(before[key] == after[key] for key in ("status", "stdout", "stderr")) and not after["timeout"]
        rows.append(dict(case=f"trace-{name}", oracle=before, candidate=after, passed=passed))
    no_children = (b'import os\ntry:\n p,s=os.waitpid(-1,os.WNOHANG)\n print("UNEXPECTED_CHILD",p,s)\n'
                   b'except ChildProcessError:\n print("NO_CHILDREN")\n')
    with tempfile.TemporaryDirectory(prefix="regex-exec-") as directory:
        fallback = Path(directory)/"no-shebang"
        fallback.write_bytes(b'exec "$1" -c "$2"\n')
        fallback.chmod(0o700)
        for name, code, operands in [
            ("external", b'[[ a =~ (a) ]]; exec "$1" -c "$2"', [os.fsencode(sys.executable), no_children]),
            ("script-fallback", b'[[ a =~ (a) ]]; exec "$1" "$2" "$3"', [os.fsencode(fallback), os.fsencode(sys.executable), no_children]),
        ]:
            before = observe(oracle, code, operands, 7)
            after = observe(candidate, code, operands, 7, prefix_args=candidate_prefix)
            passed = (all(before[key] == after[key] for key in ("status", "stdout", "stderr"))
                      and after["stdout"] == b"NO_CHILDREN\n".hex() and not after["timeout"])
            rows.append(dict(case=f"exec-cleanup-{name}", oracle=before, candidate=after, passed=passed))
    if os.uname().sysname == "Linux":
        for filled in (False, True):
            code = (b"x=; for ((i=0;i<63;i++)); do x+=x; p='(a{255}){255}'$x; [[ a =~ $p ]]; done; " if filled else b"")
            code += b"p='(a{255}){1200}TARGET'; [[ aaaaaaaaaa =~ $p ]]; printf 'FIRST:%s\\n' \"$?\"; [[ aaaaaaaaaa =~ $p ]]; printf 'AGAIN:%s\\n' \"$?\""
            before = observe(oracle, code, [], 20)
            after = observe(candidate, code, [], 20, prefix_args=candidate_prefix)
            passed = (all(before[key] == after[key] for key in ("status", "stdout", "stderr"))
                      and after["stdout"] == b"FIRST:1\nAGAIN:1\n".hex() and not after["timeout"])
            rows.append(dict(case=f"cache-history-{'filled' if filled else 'fresh'}", oracle=before, candidate=after, passed=passed))
    for name, operation in [
        ("valid", b"[[ a =~ (a) ]]"),
        ("invalid", b"r='['; [[ a =~ $r ]]"),
        ("user-child", b"[[ a =~ (a) ]]; /usr/bin/true"),
        ("user-function-child", b"[[ a =~ (a) ]]; f() { :; }; f & wait"),
    ]:
        code = b'trap \'printf "CHLD:%s\\n" "$?"\' CHLD; ' + operation + b'; printf "DONE:%s\\n" "$?"'
        before = observe(oracle, code, [], 7)
        after = observe(candidate, code, [], 7, prefix_args=candidate_prefix)
        passed = (all(before[key] == after[key] for key in ("status", "stdout", "stderr"))
                  and not before["timeout"] and not after["timeout"])
        rows.append(dict(case=f"chld-{name}", oracle=before, candidate=after, passed=passed))
    budget_pattern, budget_value = ((b"(a{255}){255}", b"a"*100000+b"c") if os.uname().sysname == "Darwin"
                                    else (b"^(a|aa)*\\1b$", b"a"*10000))
    code = (b'trap \'printf "CHLD:%s\\n" "$?"\' CHLD; [[ $2 =~ $1 ]]; '
            b'printf "LIMIT:%s\\n" "$?"; [[ a =~ (a) ]]; printf "RECOVER:%s\\n" "$?"')
    before = observe(oracle, code, [budget_pattern, budget_value], .4)
    after = observe(candidate, code, [budget_pattern, budget_value], 7, prefix_args=candidate_prefix)
    passed = (after["status"] == 0 and after["stdout"] == b"LIMIT:2\nRECOVER:0\n".hex()
              and b"regular expression" in bytes.fromhex(after["stderr"]) and not after["timeout"])
    rows.append(dict(case="chld-resource-cleanup", oracle=before, candidate=after, passed=passed))
    if args.signals:
        with tempfile.TemporaryDirectory(prefix="regex-signal-") as directory:
            operand = Path(directory)/"operand"
            operand.write_bytes(b"a"*8_000_000)
            for terminal in (False, True):
                for name, trap in [("default", b""), ("trap", b"trap 'printf \"TRAPPED:%s\\n\" \"$?\"' INT; "),
                                   ("ignore", b"trap '' INT; "),
                                   ("exit-prior-status", b"trap 'printf \"EXIT:%s\\n\" \"$?\"' EXIT; "),
                                   ("exit-cannot-mask-signal", b"trap 'printf \"EXIT:%s\\n\" \"$?\"; exit 0' EXIT; ")]:
                    code = trap + b'v=$(cat "$1"); printf "READY\\n"; [[ $v =~ ^a*$ ]]; printf "AFTER:%s\\n" "$?"'
                    before = observe(oracle, code, [os.fsencode(operand)], 10, True, terminal)
                    after = observe(candidate, code, [os.fsencode(operand)], 10, True, terminal, prefix_args=candidate_prefix)
                    passed = all(before[key] == after[key] for key in ("status", "stdout", "stderr")) and not after["timeout"]
                    rows.append(dict(case=f"signal-{name}-{'pty' if terminal else 'stdio'}", oracle=before, candidate=after, passed=passed))
                for boundary in ("subshell", "command-substitution"):
                    operation = b'printf "READY\\n" >&3; [[ $v =~ ^a*$ ]]; printf "CHILD_AFTER:%s\\n" "$?"'
                    code = b'v=$(cat "$1"); exec 3>&1; '
                    if boundary == "subshell":
                        code += b'( ' + operation + b' ); printf "PARENT_AFTER:%s\\n" "$?"'
                    else:
                        code += b'result=$( ' + operation + b' ); printf "PARENT_AFTER:%s:%s\\n" "$?" "$result"'
                    before = observe(oracle, code, [os.fsencode(operand)], 10, True, terminal, True)
                    after = observe(candidate, code, [os.fsencode(operand)], 10, True, terminal, True, candidate_prefix)
                    passed = all(before[key] == after[key] for key in ("status", "stdout", "stderr")) and not after["timeout"]
                    rows.append(dict(case=f"signal-{boundary}-{'pty' if terminal else 'stdio'}", oracle=before, candidate=after, passed=passed))
                pattern, value = ((b"(a{255}){255}", b"a"*100000+b"c") if os.uname().sysname == "Darwin"
                                  else (b"^(a|aa)*\\1b$", b"a"*10000))
                code = b'printf "READY\\n"; [[ $2 =~ $1 ]]; printf "AFTER:%s\\n" "$?"'
                before = observe(oracle, code, [pattern, value], 1, True, terminal, interrupt_delay=.1)
                after = observe(candidate, code, [pattern, value], 7, True, terminal, prefix_args=candidate_prefix, interrupt_delay=.1)
                passed = (after["status"] == -signal.SIGINT and b"AFTER" not in bytes.fromhex(after["stdout"])
                          and not after["timeout"] and after["interrupt_elapsed"] is not None and after["interrupt_elapsed"] < .05)
                rows.append(dict(case=f"signal-busy-helper-{'pty' if terminal else 'stdio'}", oracle=before, candidate=after, passed=passed))
    rows.extend(residual_cases(candidate, oracle, candidate_prefix))
    for row in rows:
        row["passed"] = (row["passed"] and not row["candidate"]["survivors"]
                         and not row.get("oracle", {}).get("survivors", []))
    (args.evidence/"observations.json").write_text(json.dumps(rows, indent=2)+"\n")
    summary = dict(total=len(rows), passed=sum(row["passed"] for row in rows))
    (args.evidence/"summary.json").write_text(json.dumps(summary, indent=2)+"\n")
    print(json.dumps(summary))
    return int(summary["passed"] != summary["total"])


if __name__ == "__main__":
    raise SystemExit(main())
