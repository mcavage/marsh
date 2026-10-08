#!/usr/bin/env python3
"""Byte-exact public CLI/real PTY readline differential, never a Bash implementation.

First --shell is GNU oracle (default required version 5.3.20). Linux 5.3.0
--oracle-version override is supporting investigation ONLY. All observed bytes,
PTY control traffic, deadlines, statuses, effects and cleanup are retained.
PTY paints differ between editors: compare command streams/buffer observations
exactly, assert prompt payload/marker invariants separately, retain paints raw.
"""
from __future__ import annotations
import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import pty
import select
import signal
import subprocess
import tempfile
import termios
import time

PROMPT = b"__RL_P1__> "
SECONDARY = b"__RL_P2__> "


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def processes():
    # Inspection only. Never signal by name or probe an unowned process/group.
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,pgid="], capture_output=True,
                         check=True, timeout=3).stdout
    return [tuple(map(int, line.split())) for line in out.splitlines() if line.strip()]


def terminal():
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


class Owned:
    def __init__(self, binary, args, cwd, env, tty):
        env = dict(env)
        if b"HISTFILE" not in env and "HISTFILE" not in env:
            # Each caller owns cwd; never fall back to the real user's history.
            fd, history = tempfile.mkstemp(prefix=".readline-history-", dir=cwd)
            os.close(fd)
            env[b"HISTFILE"] = os.fsencode(history)
        self.master = None
        self.raw = bytearray()
        self.steps = []
        self.groups = set()
        self.pids = set()
        self.timed_out = False
        self.forced = False
        self.deadline = time.monotonic() + 12
        slave = None
        if tty:
            self.master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, b"\x18\x00\x50\x00\x00\x00\x00\x00")
        self.child = subprocess.Popen([b"readline-test", *args], executable=os.fsencode(binary),
            cwd=cwd, env=env, stdin=slave if tty else subprocess.PIPE,
            stdout=slave if tty else subprocess.PIPE, stderr=slave if tty else subprocess.PIPE,
            start_new_session=True, preexec_fn=terminal if tty else None)
        self.pids.add(self.child.pid)
        self.groups.add(self.child.pid)
        if slave is not None:
            os.close(slave)
            os.set_blocking(self.master, False)
        self.track()

    def track(self):
        rows = processes()
        changed = True
        while changed:
            changed = False
            for pid, ppid, pgid in rows:
                if (ppid in self.pids or pgid in self.groups) and pid not in self.pids:
                    self.pids.add(pid)
                    self.groups.add(pgid)
                    changed = True
        return [row for row in rows if row[0] in self.pids or row[2] in self.groups]

    def pump(self, duration=0.04):
        if time.monotonic() > self.deadline:
            self.timed_out = True
            raise TimeoutError("owned PTY deadline")
        if select.select([self.master], [], [], duration)[0]:
            try:
                data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno not in (errno.EIO, errno.EAGAIN):
                    raise
                data = b""
            start = max(0, len(self.raw) - 3)
            self.raw.extend(data)
            if len(self.raw) > 4 * 1024 * 1024:
                raise RuntimeError("PTY capture exceeded 4 MiB bound")
            # Answer actual terminal cursor requests, retain both directions.
            for _ in range(bytes(self.raw[start:]).count(b"\x1b[6n")):
                self.send(b"\x1b[1;1R", "cursor-report")
        self.track()

    def send(self, data, reason="input"):
        os.write(self.master, data)
        self.steps.append({"sent": data.hex(), "reason": reason, "pty_offset": len(self.raw)})

    def wait(self, predicate):
        while not predicate():
            self.pump()
            if self.child.poll() is not None:
                raise RuntimeError("shell exited before handshake")

    def settle(self):
        end = time.monotonic() + .15
        while time.monotonic() < end:
            self.pump(.02)

    def finish(self):
        while self.child.poll() is None:
            self.pump()
        self.child.wait(timeout=1)
        self.settle()

    def close(self):
        rows = self.track()
        if self.child.poll() is None or rows:
            self.forced = True
            # Each group was captured via this owned session/descendant tree.
            # Refuse to signal the harness's own group under any circumstance.
            for pgid in self.groups:
                if pgid <= 1 or pgid == os.getpgrp():
                    raise RuntimeError("unsafe cleanup group")
                if any(row[2] == pgid for row in rows):
                    try:
                        os.killpg(pgid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
        self.child.wait(timeout=3)
        end = time.monotonic() + 2
        remaining = self.track()
        while remaining and time.monotonic() < end:
            time.sleep(.02)
            remaining = self.track()
        if self.master is not None:
            os.close(self.master)
        return {"pid": self.child.pid, "pgids": sorted(self.groups), "pids": sorted(self.pids),
                "remaining": remaining, "forced": self.forced, "clean": not remaining and not self.forced}


def cases():
    result = []
    for name, line in (("ascii", b"abZ"), ("utf8", b"a\xc3\xa9Z"),
                       ("invalid", b"a\xff\xfeZ"), ("mixed", b"a\xc3\xa9\xffZ"),
                       ("unicode-not-raw", "a\ufffd\ue000Z".encode())):
        for point in (0, 1, 2, 3, 99, -1):
            result.append(dict(name=f"point-{name}-{point}", line=line, point=str(point).encode(), keys=b""))
        for op, keys in (("backspace", b"\x7f"), ("left-insert", b"\x02Q"),
                         ("left-backspace", b"\x02\x7f"), ("right-insert", b"\x01\x06Q")):
            result.append(dict(name=f"edit-{name}-{op}", line=line, point=b"99", keys=keys))
    for point in (b"bad", b"1+1", b" 2", b"-999", b"2147483648", b"4294967296", b"9223372036854775807", b"999999999999999999999999999999999999"):
        result.append(dict(name="point-syntax-"+point.hex(), line=b"a\xc3\xa9Z", point=point, keys=b"Q"))
    for raw in (False, True):
        result.append(dict(name="continuation-" + ("raw" if raw else "utf8"), continuation=True, raw=raw))
        result.append(dict(name="continuation-bind-" + ("raw" if raw else "utf8"), continuation=True, raw=raw, continuation_bind=True))
        result.append(dict(name="once-" + ("raw" if raw else "utf8"), once=True, raw=raw))
    result.append(dict(name="ps2-unused-counter", once=True, raw=False, ps2_counter=0))
    result.append(dict(name="ps2-continuation-counter", continuation=True, continuation_bind=True, raw=False, ps2_counter=1))
    result.append(dict(name="ps2-multiline-counter", continuation=True, continuation_bind=True, raw=False, ps2_counter=2))
    result.append(dict(name="ps2-raw-counter", continuation=True, continuation_bind=True, raw=True, ps2_counter=1))
    result.append(dict(name="raw-typed", line=b"", point=b"0", keys=b"A\xff\xfe\xc3\xa9", raw=True))
    result.append(dict(name="raw-typed-partial", line=b"", point=b"0", keys=b"", raw=True,
                       input_chunks=[b"A\xff", b"\xfe\xc3", b"\xa9"]))
    result.append(dict(name="bind-lifecycle", lifecycle=True, previous=1))
    result.append(dict(name="bind-lifecycle-zero", lifecycle=True, previous=0))
    result.append(dict(name="bind-prompt-count", lifecycle=True, previous=0, prompt_count=True))
    for variable in (b"READLINE_LINE", b"READLINE_POINT"):
        result.append(dict(name="unset-" + variable.decode("ascii"), line=b"changed", point=b"2",
                           keys=b"", initial=b"original", unset=variable))
    for name, trigger, spec in (("ctrl", b"\x07", br"\C-g"),
                                ("utf8", b"\xc3\xa9", b"\xc3\xa9"),
                                ("multi", b"\x18\xc3\xa9", br"\C-x" + b"\xc3\xa9"),
                                ("hex-key", b"\x07", br"\x07"),
                                ("invalid-key", b"\xff", b"\xff")):
        result.append(dict(name="binding-source-raw-" + name, binding=True, trigger=trigger, spec=spec))
    result.append(dict(name="binding-key-wide-ascii-source", binding=True, trigger="界".encode(),
                       spec="界".encode(), ascii_source=True))
    result.append(dict(name="binding-source-partial-utf8", binding=True, trigger=b"\x18\xe7\x95\x8c",
                       spec=br"\C-x" + "界".encode(), chunks=[b"\x18", b"\xe7", b"\x95", b"\x8c"]))
    result.append(dict(name="binding-source-queued", binding=True, trigger=b"\x07abc\x14", spec=br"\C-g", queued=True))
    result.append(dict(name="binding-source-roundtrip", binding=True, trigger=b"\x07", spec=br"\C-g", roundtrip=True))
    for name, body, line, points in (
        ("utf8", b"\xc3\xa9Z", b"\xc3\xa9Z", (3, 2)),
        ("raw", b"A\xff\xfe\xc3\xa9", b"A\xff\xfe\xc3\xa9", (5, 4)),
        ("unicode-distinct", "\ufffd\ue000".encode(), "\ufffd\ue000".encode(), (6, 2)),
        ("movement", br"ab\C-bQ", b"aQb", (2, 2)),
        ("octal", br"A\377\376", b"A\xff\xfe", (3, 3)),
        ("hex", br"A\xff\xfe", b"A\xff\xfe", (3, 3)),
        ("bound-resume", br"A\C-tZ", b"AZ", (2, 2)),
    ):
        result.append(dict(name="binding-macro-" + name, binding=True, macro=body,
                           expected_line=line, expected_points=points, resume=name == "bound-resume"))
    for name, filename, quoted, basic, function in (
        ("utf8", b"unique-\xc3\xa9", False, False, False),
        ("raw", b"unique-\xff", False, False, False),
        ("raw-basic", b"unique-\xff", False, True, False),
        ("raw-quoted", b"unique-\xff space", True, False, False),
        ("raw-function-once", b"\xff", False, False, True),
    ):
        result.append(dict(name="completion-"+name, completion=True, filename=filename,
                           quoted=quoted, basic=basic, function=function))
    for style in ("default", "fullquote", "noquote", "fullquote-noquote", "only-fullquote", "only-fullquote-noquote"):
        for filename in (b"plain", b"space name", b"dollar$name", b"quote'name", b"dir space/leaf", b"dir plain/", b"dir space/"):
            result.append(dict(name="fullquote-"+style+"-"+filename.hex(), completion=True,
                               filename=filename, quoted=False, basic=False, function=True, quote_style=style))
    result.append(dict(name="completion-raw-function-queued", completion=True, filename=b"\xff",
                       quoted=False, basic=False, function=True, queued=True))
    for policy in ("cmdhist", "lithist", "physical"):
        for name, source, expected in (
            ("compound", b"if true; then\nprintf X >>command.stdout\nfi\n", b"X"),
            ("quoted", b"printf '%s' 'A\nB' >>command.stdout\n", b"A\nB"),
            ("heredoc", b"cat >>command.stdout <<'END'\nX\nEND\n", b"X\n"),
            ("alias", b"n\nprintf X >>command.stdout\nfi\n", b"X"),
            ("blank-comment", b"if true; then\n\n# retained physical comment\nprintf X >>command.stdout\nfi\n", b"X"),
            ("escaped", b"printf X\\\nY >>command.stdout\n", b"XY"),
        ):
            result.append(dict(name=f"history-format-{policy}-{name}", history_format=True,
                               policy=policy, source=source, expected=expected))
    result.append(dict(name="history-utf8", history=True, value=b"\xc3\xa9", escape=br"\303\251"))
    result.append(dict(name="history-raw", history=True, value=b"\xff", escape=br"\377"))
    result.append(dict(name="binding-macro-multiline", binding=True,
                       macro=br"printf A >> command.stdout\C-mprintf B >> command.stdout\C-m",
                       expected_line=b"", expected_points=(0, 0), eval_macro=True))
    for name, script, status in (
        ("normal", b"exit 7\n", 7),
        ("function", b"f(){ exit 7; }\nf\n", 7),
        ("prompt", b"PROMPT_COMMAND='exit 7'\nprintf NEVER\n", 7),
        ("errexit", b"set -e\nfalse\nprintf NEVER\n", 1),
        ("err-trap", b"trap 'exit 7' ERR\nfalse\n", 7),
        ("exit-trap", b"trap 'printf HOOK >&2; exit 9' EXIT\nexit 7\n", 9),
        ("function-exit-trap", b"trap 'printf HOOK >&2; exit 9' EXIT\nf(){ exit 7; }\nf\n", 9),
        ("source", b". ./exit-source\n", 7),
        ("eval", b"eval 'exit 7'\n", 7),
    ):
        result.append(dict(name="exit-announcement-"+name, exit_case=True, source=script, expected_status=status))
    result.append(dict(name="exit-announcement-binding", exit_case=True, tty_bind=True, expected_status=7, source=b""))
    result.append(dict(name="exit-announcement-function-redirect", exit_case=True, expected_status=7,
                       source=b"trap 'printf HOOK >&2' EXIT\nf(){ exit 7; }\nf 2>exit.effects\n",
                       expected_exit_effects=b"exit\nHOOK"))
    for name, script in (
        ("invalid", b"exit nope\nprintf AFTER\nexit 7\n"),
        ("many", b"exit 1 2\nprintf AFTER\nexit 7\n"),
        ("help", b"exit --help >help.effects\nprintf AFTER\nexit 7\n"),
    ):
        result.append(dict(name="exit-announcement-"+name, exit_case=True, source=script,
                           expected_status=7, expected_stdout=b"AFTER"))
    for backend in ("minimal", "basic"):
        for name, script, expected in (
            ("physical", b"set -v\nprintf X\n\n# physical comment\nv=$(printf Y)\nprintf '%s' \"$v\"\nset +v\nprintf Z\nexit 7\n", b"XYZ"),
            ("same-unit", b"set -v; printf X\nset +v; printf Y\nprintf Z\nexit 7\n", b"XYZ"),
            ("quoted", b"set -v\nprintf '%s' 'A\nB'\nset +v\nexit 7\n", b"A\nB"),
        ):
            result.append(dict(name=f"verbose-{backend}-{name}", verbose=True, backend=backend, source=script, expected=expected))
    for mode in ("stdin", "basic-stdin", "interactive-pipe", "pty", "basic-pty", "file", "command"):
        for action in ("close", "redirect"):
            result.append(dict(name=f"reader-{action}-{mode}", reader=True, mode=mode, action=action))
    result.append(dict(name="reader-redirect-eof", reader=True, mode="stdin", action="redirect", no_newline=True))
    return result


def run_binding(binary, brush, case, locale, work):
    env = {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": locale,
           b"TERM": b"xterm-256color", b"HISTFILE": b"/dev/null", b"INPUTRC": b"/dev/null",
           b"PS1": PROMPT, b"PS2": SECONDARY}
    # Raw-byte keyboard corpus explicitly disables Readline's C-locale default
    # high-bit-to-Meta translation. Keep this fixture and setting in evidence;
    # this does not assert Brush implements the separate inputrc feature.
    inputrc = b"set input-meta on\nset convert-meta off\n"
    (work / "inputrc").write_bytes(inputrc)
    env[b"INPUTRC"] = os.fsencode(work / "inputrc")
    args = ([b"--no-config"] if brush else []) + [b"--noprofile", b"--norc"]
    args += ([b"--input-backend", b"reedline"] if brush else []) + [b"-i"]
    setup = b"dump() { printf '%s\\0%s\\0' \"$READLINE_LINE\" \"$READLINE_POINT\" >> observed; }; bind -x '\"\\C-t\":dump'; "
    if "macro" in case:
        setup += b"bind '\"\\C-g\":\"" + case["macro"] + b"\"'; "
        expected_out, expected_err = (b"ABNEXT" if case.get("eval_macro") else b"NEXT"), b"ERR"
        point = case["expected_points"][0 if locale == b"C" else 1]
        expected_observed = case["expected_line"] + b"\0" + str(point).encode() + b"\0"
        if case.get("resume"):
            expected_observed = b"A\0" + b"1\0" + expected_observed
        trigger = b"\x07"
    else:
        # Raw bytes are command SOURCE in the bind operand, not merely values
        # assigned to READLINE_LINE or inherited through an invalid environment.
        command = (b'{ printf "%s" "\xff\xfe"; printf "%s" "\xfe\xff" >&2; } '
                   b'>> command.stdout 2>> command.stderr; '
                   b'READLINE_LINE=""; READLINE_POINT=0')
        if case.get("ascii_source"):
            command = command.replace(b"\xff\xfe", b"BIND").replace(b"\xfe\xff", b"SIDE")
        setup += b"bind -x '\"" + case["spec"] + b'":' + command + b"'; "
        expected_out, expected_err, expected_observed = b"\xff\xfeNEXT", b"\xfe\xffERR", b"\0" + b"0\0"
        if case.get("ascii_source"):
            expected_out, expected_err = b"BINDNEXT", b"SIDEERR"
        trigger = case["trigger"]
        if case.get("queued"):
            expected_observed = b"abc\0" + b"3\0"
        if case.get("roundtrip"):
            setup += b"bind -X > bindings.effects; bind -r '\\C-g'; while IFS= read -r entry; do bind -x \"$entry\"; done < bindings.effects; "
    setup += b"printf READY > ready\n"
    (work / "setup").write_bytes(setup)
    child = Owned(binary, args, work, env, True)
    stdout = stderr = b""
    observed = work / "observed"
    error = None
    try:
        child.wait(lambda: PROMPT in child.raw)
        child.send(b". ./setup\n")
        child.wait(lambda: (work / "ready").exists())
        child.settle()
        if case.get("chunks"):
            for chunk in case["chunks"][:-1]:
                child.send(chunk, "partial-key")
                child.settle()
                if (work / "command.stdout").exists():
                    raise RuntimeError("binding executed before the whole byte key sequence arrived")
            child.send(case["chunks"][-1], "complete-key")
        else:
            child.send(trigger)
        if "macro" in case:
            if case.get("eval_macro"):
                child.wait(lambda: (work / "command.stdout").exists() and (work / "command.stdout").read_bytes() == b"AB")
            if case.get("resume"):
                child.wait(observed.exists)
            child.settle()
            previous_size = observed.stat().st_size if observed.exists() else 0
            child.send(b"\x14")
            child.wait(lambda: observed.exists() and observed.stat().st_size > previous_size)
            child.settle()
            child.send(b"\x05\x15")
        else:
            if not case.get("queued"):
                child.settle()
                child.send(b"\x14")
            child.wait(observed.exists)
            child.settle()
            child.send(b"\x05\x15")
        child.settle()
        child.send(b"printf NEXT >> command.stdout; printf ERR >> command.stderr; exit 7\n")
        child.finish()
        stdout = (work / "command.stdout").read_bytes()
        stderr = (work / "command.stderr").read_bytes()
    except (TimeoutError, subprocess.TimeoutExpired, RuntimeError, OSError) as exc:
        error = str(exc)
        child.timed_out |= isinstance(exc, (TimeoutError, subprocess.TimeoutExpired))
    finally:
        cleanup = child.close()
    observation = observed.read_bytes() if observed.exists() else None
    return {"status": child.child.returncode, "stdout": stdout.hex(), "stderr": stderr.hex(),
            "observed": observation.hex() if observation is not None else None,
            "semantic_valid": stdout == expected_out and stderr == expected_err and observation == expected_observed,
            "pty": bytes(child.raw).hex(), "steps": child.steps, "cleanup": cleanup,
            "timed_out": child.timed_out, "error": error, "argv": [a.hex() for a in args], "setup": setup.hex(), "inputrc": inputrc.hex(),
            "bindings_listing": (work / "bindings.effects").read_bytes().hex() if (work / "bindings.effects").exists() else None,
            "prompt_raw_present": True, "secondary_raw_present": True,
            "renderer_markers_absent": b"\x01" not in child.raw and b"\x02" not in child.raw}


def run_completion(binary, brush, case, locale, work):
    env = {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": locale,
           b"TERM": b"xterm-256color", b"HISTFILE": b"/dev/null", b"INPUTRC": b"/dev/null",
           b"PS1": PROMPT, b"PS2": SECONDARY}
    filename = case["filename"]
    if not case["function"] or "quote_style" in case:
        try:
            fixture_path = work / os.fsdecode(filename)
            fixture_path.parent.mkdir(parents=True, exist_ok=True)
            if filename.endswith(b"/"):
                fixture_path.mkdir(exist_ok=True)
            else:
                fixture_path.write_bytes(b"fixture")
        except OSError as exc:
            # APFS/host-backed filesystems may not admit invalid filename bytes.
            # Record a blocked fixture, NOT an oracle/candidate equivalence pass.
            return {"status": None, "stdout": "", "stderr": "", "observed": None,
                    "semantic_valid": False, "pty": "", "steps": [],
                    "cleanup": {"pid": None, "pids": [], "pgids": [], "remaining": [],
                                "forced": False, "clean": True, "not_started": True},
                    "timed_out": False, "error": f"filename fixture unavailable: errno={exc.errno}; shell not started",
                    "fixture_filename": filename.hex(), "argv": None,
                    "prompt_raw_present": False, "secondary_raw_present": False, "renderer_markers_absent": True}
    args = ([b"--no-config"] if brush else []) + [b"--noprofile", b"--norc"]
    args += ([b"--input-backend", b"reedline"] if brush else []) + [b"-i"]
    setup = b"dump() { printf '%s\\0%s\\0' \"$READLINE_LINE\" \"$READLINE_POINT\" >> observed; }; bind -x '\"\\C-t\":dump'; "
    if case["basic"]:
        env[b"RAW1"] = b"\xff" + PROMPT
        setup += b"PS1=$RAW1; "
    if case["function"]:
        env[b"CANDIDATE"] = filename
        options = b""
        if "quote_style" in case:
            options = b"" if case["quote_style"].startswith("only-") else b" -o filenames"
            if "fullquote" in case["quote_style"]: options += b" -o fullquote"
            if "noquote" in case["quote_style"]: options += b" -o noquote"
        setup += b'cf() { printf X >> calls.effects; COMPREPLY=("$CANDIDATE"); }; complete -F cf' + options + b' printf; '
    setup += b"printf READY > ready\n"
    (work / "setup").write_bytes(setup)
    prefix = b"printf >command.stdout 2>command.stderr '%s' "
    initial = prefix + (b"'" if case["quoted"] else b"") + b"unique-"
    suffix = (b"'" + filename + b"' ") if case["quoted"] else filename + b" "
    if "quote_style" in case and ("noquote" not in case["quote_style"] or "fullquote" in case["quote_style"]):
        suffix = filename.replace(b" ", b"\\ ").replace(b"$", b"\\$").replace(b"'", b"\\'") + b" "
    if "quote_style" in case and filename.endswith(b"/") and not case["quote_style"].startswith("only-"):
        suffix = suffix.removesuffix(b" ")
    expected_line = prefix + suffix + (b"Z" if case.get("queued") else b"")
    expected_point = len(expected_line) - (1 if locale != b"C" and filename == b"unique-\xc3\xa9" else 0)
    expected_observed = expected_line + b"\0" + str(expected_point).encode() + b"\0"
    child = Owned(binary, args, work, env, True)
    observed = work / "observed"
    stdout = stderr = b""
    error = None
    try:
        child.wait(lambda: PROMPT in child.raw)
        child.send(b". ./setup\n")
        child.wait(lambda: (work / "ready").exists())
        child.settle()
        child.send(initial + b"\t" + (b"Z" if case.get("queued") else b""))
        child.settle()
        child.send(b"\x14")
        child.wait(observed.exists)
        child.settle()
        if "quote_style" in case:
            child.send(b"\x05\x15")
        else:
            child.send(b"\n")
            child.wait(lambda: (work / "command.stdout").exists())
        child.settle()
        child.send(b"printf NEXT >> command.stdout; printf ERR >> command.stderr; exit 7\n")
        child.finish()
        stdout = (work / "command.stdout").read_bytes()
        stderr = (work / "command.stderr").read_bytes()
    except (TimeoutError, subprocess.TimeoutExpired, RuntimeError, OSError) as exc:
        error = str(exc)
        child.timed_out |= isinstance(exc, (TimeoutError, subprocess.TimeoutExpired))
    finally:
        cleanup = child.close()
    observation = observed.read_bytes() if observed.exists() else None
    calls = (work / "calls.effects").read_bytes() if (work / "calls.effects").exists() else None
    return {"status": child.child.returncode, "stdout": stdout.hex(), "stderr": stderr.hex(),
            "observed": observation.hex() if observation is not None else None,
            "completion_calls": calls.hex() if calls is not None else None,
            "semantic_valid": stdout == (b"" if "quote_style" in case else filename) + (b"Z" if case.get("queued") else b"") + b"NEXT" and stderr == b"ERR" and observation == expected_observed
                              and calls == (b"X" if case["function"] else None),
            "pty": bytes(child.raw).hex(), "steps": child.steps, "cleanup": cleanup,
            "timed_out": child.timed_out, "error": error, "argv": [a.hex() for a in args], "setup": setup.hex(),
            "prompt_raw_present": not case["basic"] or b"\xff"+PROMPT in child.raw,
            "secondary_raw_present": True, "renderer_markers_absent": b"\x01" not in child.raw and b"\x02" not in child.raw}


def run_history(binary, brush, case, locale, work):
    env = {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": locale,
           b"TERM": b"xterm-256color", b"HISTFILE": b"/dev/null", b"INPUTRC": b"/dev/null",
           b"PS1": PROMPT, b"PS2": SECONDARY}
    args = ([b"--no-config"] if brush else []) + [b"--noprofile", b"--norc"]
    args += ([b"--input-backend", b"reedline"] if brush else []) + [b"-i"]
    command_prefix = b'printf >command.stdout 2>command.stderr "%s" "'
    line = command_prefix + case["value"] + b'"'
    setup = (b"dump() { printf '%s\\0%s\\0' \"$READLINE_LINE\" \"$READLINE_POINT\" >> observed; }; "
             b"bind -x '\"\\C-t\":dump'; history -s $'" + command_prefix + case["escape"] + b'"' + b"'; printf READY > ready\n")
    (work / "setup").write_bytes(setup)
    child = Owned(binary, args, work, env, True)
    observed = work / "observed"
    error = None
    stdout = stderr = b""
    try:
        child.wait(lambda: PROMPT in child.raw)
        child.send(b". ./setup\n")
        child.wait(lambda: (work / "ready").exists())
        child.settle()
        child.send(b"\x1b[A")
        child.settle()
        child.send(b"\x14")
        child.wait(observed.exists)
        child.settle()
        child.send(b"\n")
        child.wait(lambda: (work / "command.stdout").exists())
        child.settle()
        # Exercise an optional history hint matching a raw stored command. It
        # must not replace this newly typed UTF8 input with executable history.
        child.send(b"p")
        child.settle()
        child.send(b"rintf NEXT >> command.stdout; printf ERR >> command.stderr; exit 7\n")
        child.finish()
        stdout = (work / "command.stdout").read_bytes()
        stderr = (work / "command.stderr").read_bytes()
    except (TimeoutError, subprocess.TimeoutExpired, RuntimeError, OSError) as exc:
        error = str(exc)
        child.timed_out |= isinstance(exc, (TimeoutError, subprocess.TimeoutExpired))
    finally:
        cleanup = child.close()
    point = len(line) - (1 if locale != b"C" and case["value"] == b"\xc3\xa9" else 0)
    observation = observed.read_bytes() if observed.exists() else None
    return {"status": child.child.returncode, "stdout": stdout.hex(), "stderr": stderr.hex(),
            "observed": observation.hex() if observation is not None else None,
            "semantic_valid": stdout == case["value"]+b"NEXT" and stderr == b"ERR"
                              and observation == line+b"\0"+str(point).encode()+b"\0",
            "pty": bytes(child.raw).hex(), "steps": child.steps, "cleanup": cleanup,
            "timed_out": child.timed_out, "error": error, "argv": [a.hex() for a in args], "setup": setup.hex(),
            "prompt_raw_present": True, "secondary_raw_present": True,
            "renderer_markers_absent": b"\x01" not in child.raw and b"\x02" not in child.raw}


def run_reader(binary, brush, case, locale, work):
    mode, action = case["mode"], case["action"]
    tty = mode in ("pty", "basic-pty")
    env = {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": locale,
           b"TERM": b"xterm-256color", b"HISTFILE": b"/dev/null", b"INPUTRC": b"/dev/null",
           b"PS1": PROMPT, b"PS2": SECONDARY}
    (work / "fd_probe.py").write_text("import os,json\ntry:\n b=os.read(0,1); r={'byte':b.hex()}\nexcept OSError as e:\n r={'errno':e.errno}\nprint(json.dumps(r,sort_keys=True))\n")
    (work / "replacement").write_bytes(b"XYprintf NEW >> command.stdout\n{ printf ERR >&2; } 2>>command.stderr\nexit 7\n")
    first = b"printf BEFORE >> command.stdout\n"
    if action == "close":
        unit = b"exec 0<&-; /usr/bin/python3 fd_probe.py > fd.effects; printf INUNIT >> command.stdout"
    else:
        unit = b"exec 0<replacement; IFS= read -r -N 1 one; printf '<%s>' \"$one\" >> command.stdout; /usr/bin/python3 fd_probe.py > fd.effects"
    script = first + unit + (b"" if case.get("no_newline") else b"\nprintf OLD >> command.stdout\n{ printf ERR >&2; } 2>>command.stderr\nexit 7\n")
    args = ([b"--no-config"] if brush else []) + [b"--noprofile", b"--norc"]
    if brush:
        backend = b"basic" if mode.startswith("basic") else (b"reedline" if tty else b"minimal")
        args += [b"--input-backend", backend]
    if mode == "file":
        (work / "input.sh").write_bytes(script); args += [b"./input.sh"]
    elif mode == "command":
        args += [b"-c", script]
    else:
        args += ([b"-i"] if tty or mode == "interactive-pipe" else []) + [b"-s"]
    child = Owned(binary, args, work, env, tty)
    error = None
    stream_out = stream_err = b""
    try:
        if tty:
            child.wait(lambda: PROMPT in child.raw)
            child.send(script)
            child.finish()
        else:
            stream_out, stream_err = child.child.communicate(script if mode not in ("file", "command") else b"", timeout=8)
    except (TimeoutError, subprocess.TimeoutExpired, RuntimeError, OSError) as exc:
        error = str(exc); child.timed_out |= isinstance(exc, (TimeoutError, subprocess.TimeoutExpired))
    finally:
        cleanup = child.close()
    file = lambda name: (work / name).read_bytes() if (work / name).exists() else b""
    stdout, stderr, probe = file("command.stdout"), file("command.stderr"), file("fd.effects")
    expected_out = b"BEFORE" + (b"INUNIT" if action == "close" else b"<X>")
    primary_stopped = action == "close" and mode not in ("file", "command")
    expected_out += b"" if primary_stopped else (b"OLD" if mode in ("file", "command") else b"NEW")
    expected_status = 0 if primary_stopped else 7
    expected_probe = {"errno": 9} if action == "close" else {"byte": b"Y".hex()}
    try: actual_probe = json.loads(probe)
    except ValueError: actual_probe = None
    return {"status": child.child.returncode, "expected_status": expected_status,
            "stdout": stdout.hex(), "stderr": stderr.hex(), "observed": None, "fd_probe": actual_probe,
            "semantic_valid": stdout == expected_out and stderr == (b"" if primary_stopped else b"ERR")
                              and child.child.returncode == expected_status and actual_probe == expected_probe,
            "pty": bytes(child.raw).hex(), "process_stdout": stream_out.hex(), "process_stderr": stream_err.hex(),
            "steps": child.steps, "cleanup": cleanup, "timed_out": child.timed_out, "error": error,
            "argv": [a.hex() for a in args], "source": script.hex(),
            "prompt_raw_present": True, "secondary_raw_present": True, "renderer_markers_absent": True}


def run_history_format(binary, brush, case, locale, work):
    env = {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": locale,
           b"TERM": b"xterm-256color", b"HISTFILE": b"/dev/null", b"INPUTRC": b"/dev/null",
           b"PS1": PROMPT, b"PS2": SECONDARY}
    args = ([b"--no-config", b"--input-backend", b"reedline"] if brush else []) + [b"--noprofile", b"--norc", b"-i"]
    policy = {"cmdhist": b"shopt -s cmdhist; shopt -u lithist", "lithist": b"shopt -s cmdhist lithist",
              "physical": b"shopt -u cmdhist lithist"}[case["policy"]]
    setup = policy + b"; alias n='if true; then'; history -c; printf READY > ready\n"
    final = b"history -w saved; { printf ERR >&2; } 2>command.stderr; exit 7\n"
    child = Owned(binary, args, work, env, True)
    error = None
    stdout = stderr = saved = b""
    try:
        child.wait(lambda: PROMPT in child.raw)
        child.send(setup)
        child.wait(lambda: (work / "ready").exists())
        child.settle()
        child.send(case["source"])
        child.wait(lambda: (work / "command.stdout").exists() and (work / "command.stdout").read_bytes() == case["expected"])
        child.settle()
        child.send(final)
        child.finish()
        stdout = (work / "command.stdout").read_bytes()
        stderr = (work / "command.stderr").read_bytes()
        saved = (work / "saved").read_bytes()
    except (TimeoutError, subprocess.TimeoutExpired, RuntimeError, OSError) as exc:
        error = str(exc); child.timed_out |= isinstance(exc, (TimeoutError, subprocess.TimeoutExpired))
    finally:
        cleanup = child.close()
    return {"status": child.child.returncode, "stdout": stdout.hex(), "stderr": stderr.hex(), "observed": saved.hex(),
            "semantic_valid": stdout == case["expected"] and stderr == b"ERR" and bool(saved),
            "pty": bytes(child.raw).hex(), "steps": child.steps, "cleanup": cleanup, "timed_out": child.timed_out,
            "error": error, "argv": [a.hex() for a in args], "source": case["source"].hex(), "setup": setup.hex(),
            "prompt_raw_present": True, "secondary_raw_present": True, "renderer_markers_absent": True}


def run_verbose(binary, brush, case, locale, work):
    args = ([b"--no-config", b"--input-backend", case["backend"].encode()] if brush else []) + [b"--noprofile", b"--norc", b"-s"]
    child = Owned(binary, args, work, {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": locale}, False)
    error = None
    stdout = stderr = b""
    try:
        stdout, stderr = child.child.communicate(case["source"], timeout=8)
    except (TimeoutError, subprocess.TimeoutExpired, RuntimeError, OSError) as exc:
        error = str(exc); child.timed_out |= isinstance(exc, (TimeoutError, subprocess.TimeoutExpired))
    finally:
        cleanup = child.close()
    return {"status": child.child.returncode, "stdout": stdout.hex(), "stderr": stderr.hex(), "observed": None,
            "semantic_valid": stdout == case["expected"] and bool(stderr), "pty": "", "steps": [], "cleanup": cleanup,
            "timed_out": child.timed_out, "error": error, "argv": [a.hex() for a in args], "source": case["source"].hex(),
            "prompt_raw_present": True, "secondary_raw_present": True, "renderer_markers_absent": True}


def run_exit_case(binary, brush, case, locale, work):
    if case.get("tty_bind"):
        (work / "setup").write_bytes(b'bind -x \'"\\C-g":exit 7\'; printf READY >ready\n')
        args = ([b"--no-config", b"--input-backend", b"reedline"] if brush else []) + [b"--noprofile", b"--norc", b"-i"]
        child = Owned(binary, args, work, {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": locale,
                      b"PS1": PROMPT, b"TERM": b"xterm", b"HISTFILE": b"/dev/null", b"INPUTRC": b"/dev/null"}, True)
        error = None
        try:
            child.wait(lambda: PROMPT in child.raw); child.send(b". ./setup\n")
            child.wait(lambda: (work / "ready").exists()); child.settle(); child.send(b"\x07"); child.finish()
        except (TimeoutError, subprocess.TimeoutExpired, RuntimeError, OSError) as exc:
            error = str(exc); child.timed_out |= isinstance(exc, (TimeoutError, subprocess.TimeoutExpired))
        finally:
            cleanup = child.close()
        count = bytes(child.raw).count(b"\r\nexit\r\n")
        return {"status": child.child.returncode, "stdout": "", "stderr": "", "observed": str(count).encode().hex(),
                "semantic_valid": count == 1, "pty": bytes(child.raw).hex(), "steps": child.steps, "cleanup": cleanup,
                "error": error, "timed_out": child.timed_out, "argv": [a.hex() for a in args],
                "prompt_raw_present": True, "secondary_raw_present": True, "renderer_markers_absent": True}
    (work / "exit-source").write_bytes(b"exit 7\n")
    args = ([b"--no-config", b"--input-backend", b"minimal"] if brush else []) + [b"--noprofile", b"--norc", b"-i", b"-s"]
    child = Owned(binary, args, work, {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": locale,
                                      b"PS1": PROMPT, b"PS2": SECONDARY, b"HISTFILE": b"/dev/null", b"TERM": b"xterm"}, False)
    error = None
    stdout = stderr = b""
    try:
        stdout, stderr = child.child.communicate(case["source"], timeout=8)
    except (TimeoutError, subprocess.TimeoutExpired, RuntimeError, OSError) as exc:
        error = str(exc); child.timed_out |= isinstance(exc, (TimeoutError, subprocess.TimeoutExpired))
    finally:
        cleanup = child.close()
    exit_effects = (work / "exit.effects").read_bytes() if (work / "exit.effects").exists() else None
    return {"status": child.child.returncode, "expected_status": case["expected_status"],
            "stdout": stdout.hex(), "stderr": stderr.hex(), "observed": None,
            "exit_effects": exit_effects.hex() if exit_effects is not None else None,
            "semantic_valid": stdout == case.get("expected_stdout", b"") and child.child.returncode == case["expected_status"]
                              and exit_effects == case.get("expected_exit_effects"),
            "pty": "", "steps": [], "cleanup": cleanup, "timed_out": child.timed_out, "error": error,
            "argv": [a.hex() for a in args], "source": case["source"].hex(),
            "help_output": (work / "help.effects").read_bytes().hex() if (work / "help.effects").exists() else None,
            "prompt_raw_present": True, "secondary_raw_present": True, "renderer_markers_absent": True}


def run(binary, brush, case, locale, work):
    if case.get("exit_case"):
        return run_exit_case(binary, brush, case, locale, work)
    if case.get("verbose"):
        return run_verbose(binary, brush, case, locale, work)
    if case.get("history_format"):
        return run_history_format(binary, brush, case, locale, work)
    if case.get("reader"):
        return run_reader(binary, brush, case, locale, work)
    if case.get("history"):
        return run_history(binary, brush, case, locale, work)
    if case.get("completion"):
        return run_completion(binary, brush, case, locale, work)
    if case.get("binding"):
        return run_binding(binary, brush, case, locale, work)
    env = {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": locale,
           b"TERM": b"xterm-256color", b"HISTFILE": b"/dev/null", b"INPUTRC": b"/dev/null",
           b"PS1": PROMPT, b"PS2": SECONDARY}
    args = ([b"--no-config"] if brush else []) + [b"--noprofile", b"--norc"]
    noninteractive = case.get("noninteractive", False)
    if noninteractive:
        script = b"READLINE_LINE=$RAW; READLINE_POINT=71\nprintf '<%s><%s>\\n' \"$READLINE_LINE\" \"$READLINE_POINT\"\nprintf ERR >&2\nexit 7\n"
        env[b"RAW"] = b"ordinary" if case.get("ascii") else b"\xff\xfe\xc3\xa9"
        args += ([b"--input-backend", b"minimal"] if brush else []) + [b"-s"]
    else:
        args += ([b"--input-backend", b"reedline"] if brush else []) + [b"-i"]
        env |= {b"LINE": case.get("line", b""), b"POINT": case.get("point", b"0")}
    observed = work / "observed"
    setup = (b"dump() { printf '%s\\0%s\\0' \"$READLINE_LINE\" \"$READLINE_POINT\" >> observed; }; "
             b"seed() { READLINE_LINE=$LINE; READLINE_POINT=$POINT; }; "
             b"bind -x '\"\\C-g\":seed'; bind -x '\"\\C-t\":dump'; "
             b"printf READY > ready\n")
    if case.get("unset"):
        setup = setup.replace(b"READLINE_POINT=$POINT;", b"READLINE_POINT=$POINT; unset " + case["unset"] + b";")
    if case.get("lifecycle"):
        setup = (b"READLINE_LINE=ordinary; READLINE_POINT=71; "
                 b"dump() { printf 'IN<%s><%s><%s>' \"$READLINE_LINE\" \"$READLINE_POINT\" \"$?\" >> observed; "
                 + (b"true" if case["previous"] else b"false") + b"; }; "
                 b"bind -x '\"\\C-t\":dump'; printf READY > ready; "
                 + (b"false" if case["previous"] else b"true") + b"\n")
    if case.get("prompt_count"):
        setup = b"pc=0; PROMPT_COMMAND='pc=$((pc+1))'; " + setup
        setup = setup.replace(b"IN<%s><%s><%s>", b"IN<%s><%s><%s><%s>")
        setup = setup.replace(b'"$?" >> observed', b'"$?" "$pc" >> observed')
    if case.get("raw"):
        # Change prompts inside seed, so the pending buffer must transfer from
        # the normal editor to Basic on the very next read, not an empty read.
        setup = setup.replace(b"READLINE_POINT=$POINT;", b"READLINE_POINT=$POINT; PS1=$RAW1; PS2=$RAW2;")
        env[b"RAW1"] = b"\x01\xff\x02" + PROMPT
        env[b"RAW2"] = b"\x01\xfe\x02" + SECONDARY
    if "ps2_counter" in case:
        setup = b"n=0; PS2='$((n+=1))__RL_P2__> '; " + setup
        if case.get("raw"):
            env[b"RAW2"] = b"\x01\xfe\x02$((n+=1))" + SECONDARY
    if case.get("once"):
        env[b"LINE"] = b"printf 'ONCE\\n' >> command.stdout; printf E >> command.stderr"
        env[b"POINT"] = b"999"
    if case.get("continuation"):
        env[b"LINE"] = b"printf '%s' 'first"
        env[b"POINT"] = b"999"
    (work / "setup").write_bytes(setup)
    if case.get("input_chunks"):
        (work / "inputrc").write_bytes(b"set input-meta on\nset convert-meta off\n")
        env[b"INPUTRC"] = os.fsencode(work / "inputrc")
    child = Owned(binary, args, work, env, not noninteractive)
    stdout = stderr = b""
    error = None
    try:
        if noninteractive:
            stdout, stderr = child.child.communicate(script, timeout=8)
        else:
            child.wait(lambda: PROMPT in child.raw)
            child.send(b". ./setup\n")
            child.wait(lambda: (work / "ready").exists())
            child.settle()
            if not case.get("lifecycle"):
                child.send(case.get("initial", b"") + b"\x07")
                child.settle()
            if case.get("lifecycle"):
                child.send(b"\x14")
                child.wait(observed.exists)
            elif case.get("continuation"):
                offset = len(child.raw)
                child.send(b"\n")
                child.wait(lambda: SECONDARY in child.raw[offset:])
                if case.get("ps2_counter") == 2:
                    offset = len(child.raw)
                    child.send(b"middle\n")
                    child.wait(lambda: SECONDARY in child.raw[offset:])
                if case.get("continuation_bind"):
                    child.send(b"\x14")
                    child.wait(observed.exists)
                    child.settle()
                child.send(b"second' > command.stdout 2>command.stderr\n")
                child.wait(lambda: (work / "command.stdout").exists())
            elif case.get("once"):
                child.send(b"\n")
                child.wait(lambda: (work / "command.stdout").exists())
            else:
                for chunk in case.get("input_chunks", []):
                    child.send(chunk, "partial-input")
                    child.settle()
                child.send(case["keys"] + b"\x14")
                child.wait(observed.exists)
                child.settle()
                child.send(b"\x05\x15")
            child.settle()
            if case.get("raw") and not (case.get("once") or case.get("continuation")):
                # GNU recomposes PS1 after a submitted command, not immediately
                # after bind -x changes it. Require the new raw prompt then.
                offset = len(child.raw)
                child.send(b":\n")
                child.wait(lambda: b"\xff" + PROMPT in child.raw[offset:])
            if case.get("lifecycle"):
                final = b"printf 'OUT<%s><%s><%s>' \"${READLINE_LINE-unset}\" \"${READLINE_POINT-unset}\" \"$?\" >> command.stdout; printf ERR >> command.stderr; exit 7\n"
                if case.get("prompt_count"):
                    final = final.replace(b"OUT<%s><%s><%s>", b"OUT<%s><%s><%s><%s>")
                    final = final.replace(b'"$?" >> command.stdout', b'"$?" "$pc" >> command.stdout')
                child.send(final)
            else:
                final = b"printf NEXT >> command.stdout; printf ERR >> command.stderr; exit 7\n"
                if "ps2_counter" in case:
                    final = final.replace(b"; exit 7", b"; printf '|N:%s|' \"$n\" >> command.stdout; exit 7")
                child.send(final)
            child.finish()
            stdout = (work / "command.stdout").read_bytes()
            stderr = (work / "command.stderr").read_bytes()
    except (TimeoutError, subprocess.TimeoutExpired, RuntimeError, OSError) as exc:
        error = str(exc)
        child.timed_out |= isinstance(exc, (TimeoutError, subprocess.TimeoutExpired))
    finally:
        cleanup = child.close()
    expected_out = b"NEXT"
    expected_err = b"ERR"
    if noninteractive:
        expected_out = b"<" + env[b"RAW"] + b"><71>\n"
    elif case.get("lifecycle"):
        expected_out = b"OUT<unset><unset><" + str(case["previous"]).encode() + b">"
        if case.get("prompt_count"):
            expected_out += b"<1>"
    elif case.get("once"):
        expected_out = b"ONCE\nNEXT"
        expected_err = b"EERR"
    elif case.get("continuation"):
        expected_out = b"first\n" + (b"middle\n" if case.get("ps2_counter") == 2 else b"") + b"secondNEXT"
    if "ps2_counter" in case:
        expected_out += b"|N:" + str(case["ps2_counter"]).encode() + b"|"
    observation = observed.read_bytes() if observed.exists() else None
    anchor = {
        "point-utf8-2": (b"a\xc3\xa9Z\x002\x00", b"a\xc3\xa9Z\x002\x00"),
        "edit-invalid-left-backspace": (b"a\xffZ\x002\x00", b"aZ\x001\x00"),
        "edit-mixed-left-backspace": (b"a\xc3\xa9Z\x003\x00", b"aZ\x001\x00"),
        "raw-typed": (b"A\xff\xfe\xc3\xa9\x005\x00", b"A\xff\xfe\xc3\xa9\x004\x00"),
        "raw-typed-partial": (b"A\xff\xfe\xc3\xa9\x005\x00", b"A\xff\xfe\xc3\xa9\x004\x00"),
        "unset-READLINE_LINE": (b"original\x002\x00", b"original\x002\x00"),
        "unset-READLINE_POINT": (b"changed\x007\x00", b"changed\x007\x00"),
        "point-syntax-32313437343833363438": (b"Qa\xc3\xa9Z\x001\x00", b"Qa\xc3\xa9Z\x001\x00"),
        "point-syntax-34323934393637323936": (b"Qa\xc3\xa9Z\x001\x00", b"Qa\xc3\xa9Z\x001\x00"),
        "point-syntax-39323233333732303336383534373735383037": (b"Qa\xc3\xa9Z\x001\x00", b"Qa\xc3\xa9Z\x001\x00"),
    }.get(case["name"])
    semantic_valid = stdout == expected_out and stderr == expected_err
    if case.get("continuation_bind"):
        semantic_valid &= observation == b"\0" + b"0\0"
    if case.get("lifecycle"):
        expected_observation = b"IN<><0><" + str(case["previous"]).encode() + b">"
        if case.get("prompt_count"):
            expected_observation += b"<1>"
        semantic_valid &= observation == expected_observation
    elif not noninteractive and not (case.get("once") or case.get("continuation")):
        semantic_valid &= observation is not None and observation.count(b"\0") == 2
    if anchor is not None:
        semantic_valid &= observation == anchor[0 if locale == b"C" else 1]
    return {"status": child.child.returncode, "stdout": stdout.hex(), "stderr": stderr.hex(),
            "semantic_valid": semantic_valid,
            "observed": observed.read_bytes().hex() if observed.exists() else None,
            "pty": bytes(child.raw).hex(), "steps": child.steps, "cleanup": cleanup,
            "timed_out": child.timed_out, "error": error,
            "argv": [arg.hex() for arg in args],
            "inputrc": (work / "inputrc").read_bytes().hex() if (work / "inputrc").exists() else None,
            "prompt_raw_present": not case.get("raw") or b"\xff" + PROMPT in child.raw,
            "secondary_raw_present": not (case.get("raw") and case.get("continuation")) or b"\xfe" + (b"1" if "ps2_counter" in case else b"") + SECONDARY in child.raw,
            "renderer_markers_absent": b"\x01" not in child.raw and b"\x02" not in child.raw}


def signature(result):
    return {key: result.get(key) for key in ("status", "stdout", "stderr", "observed", "bindings_listing", "completion_calls", "fd_probe", "exit_effects")}


def healthy(result):
    return (not result["error"] and not result["timed_out"] and result["cleanup"]["clean"]
            and result["status"] == result.get("expected_status", 7) and result["semantic_valid"] and result["prompt_raw_present"]
            and result["secondary_raw_present"] and result["renderer_markers_absent"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shell", action="append", required=True, metavar="LABEL=PATH")
    parser.add_argument("--brush", action="append", default=[])
    parser.add_argument("--oracle-version", default="5.3.20")
    parser.add_argument("--utf8-locale", default="en_US.UTF-8" if platform.system() == "Darwin" else "C.utf8")
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("--binding-only", action="store_true")
    parser.add_argument("--completion-only", action="store_true")
    parser.add_argument("--reader-only", action="store_true")
    parser.add_argument("--fullquote-only", action="store_true")
    parser.add_argument("--history-format-only", action="store_true")
    parser.add_argument("--verbose-only", action="store_true")
    parser.add_argument("--exit-only", action="store_true")
    parser.add_argument("--require-exact", action="append", default=[])
    parser.add_argument("--evidence", required=True, type=Path)
    args = parser.parse_args()
    binaries = {label: Path(path).resolve(strict=True) for label, path in (s.split("=", 1) for s in args.shell)}
    oracle = next(iter(binaries))
    if not set(args.brush + args.require_exact).issubset(binaries):
        parser.error("unknown label")
    # Reject host executables before invoking anything in a VM.
    if platform.system() != "Darwin":
        for path in binaries.values():
            with path.open("rb") as f:
                if f.read(4) in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xce",
                                 b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"):
                    parser.error("Mach-O binary on non-Darwin host")
    args.evidence.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(prefix="marsh-readline-version-") as tmp:
        version = Owned(binaries[oracle], [b"--version"], tmp,
                        {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(tmp), b"LC_ALL": b"C"}, False)
        try:
            version_out, version_err = version.child.communicate(timeout=3)
        finally:
            version_cleanup = version.close()
    locale_probe = subprocess.run(["/usr/bin/locale", "charmap"], capture_output=True, timeout=3,
                                  env={"LC_ALL": args.utf8_locale}, start_new_session=True)
    identity = {"host": platform.platform(), "oracle_version": args.oracle_version,
                "support_only": args.oracle_version != "5.3.20",
                "oracle_policy": "Linux GNU5.3.20 primary; Mac version recorded explicitly (5.3.9 supporting)", "harness_sha256": sha(__file__),
                "root_lock_sha256": sha(Path(__file__).resolve().parents[2] / "Cargo.lock"),
                "version_stdout": version_out.hex(), "version_stderr": version_err.hex(),
                "version_status": version.child.returncode, "version_cleanup": version_cleanup,
                "utf8_locale_probe": {"name": args.utf8_locale, "stdout": locale_probe.stdout.hex(),
                                      "stderr": locale_probe.stderr.hex(), "status": locale_probe.returncode},
                "binaries": {label: {"path": str(path), "sha256": sha(path)} for label, path in binaries.items()}}
    (args.evidence / "identity.json").write_text(json.dumps(identity, indent=2)+"\n")
    if (b"GNU bash, version " + args.oracle_version.encode() + b"(" not in version_out
            or version.child.returncode != 0 or not version_cleanup["clean"]):
        parser.error("actual GNU oracle version/cleanup mismatch; identity retained")
    if (locale_probe.returncode != 0 or locale_probe.stderr
            or locale_probe.stdout.strip().upper().replace(b"-", b"") != b"UTF8"):
        parser.error("requested UTF8 locale unavailable; identity retained")
    selected = [dict(name="noninteractive-retention", noninteractive=True),
                dict(name="noninteractive-retention-ascii", noninteractive=True, ascii=True), *cases()]
    if args.binding_only:
        selected = [case for case in selected if case.get("binding")]
    if args.completion_only:
        selected = [case for case in selected if case.get("completion")]
    if args.reader_only:
        selected = [case for case in selected if case.get("reader")]
    if args.fullquote_only:
        selected = [case for case in selected if "quote_style" in case]
    if args.history_format_only:
        selected = [case for case in selected if case.get("history_format")]
    if args.verbose_only:
        selected = [case for case in selected if case.get("verbose")]
    if args.exit_only:
        selected = [case for case in selected if case.get("exit_case")]
    if args.case:
        selected = [case for case in selected if case["name"] in args.case]
        if len(selected) != len(set(args.case)):
            parser.error("unknown case filter")
    failures = []
    counts = {label: 0 for label in binaries}
    with (args.evidence / "observations.jsonl").open("w") as log:
        for locale in (b"C", args.utf8_locale.encode()):
            for case in selected:
                results = {}
                for label, binary in binaries.items():
                    with tempfile.TemporaryDirectory(prefix="marsh-readline-") as tmp:
                        results[label] = run(binary, label in args.brush, case, locale, Path(tmp))
                gold = results[oracle]
                exact = {label: healthy(result) and healthy(gold) and signature(result) == signature(gold)
                         for label, result in results.items()}
                for label, match in exact.items():
                    counts[label] += int(match)
                record = {"case": case["name"], "locale": locale.decode("ascii"), "results": results, "exact": exact}
                log.write(json.dumps(record)+"\n")
                log.flush()
                if not healthy(gold) or any(not exact[label] for label in args.require_exact):
                    failures.append([case["name"], locale.decode("ascii")])
    summary = {"observations_per_shell": len(selected)*2, "exact": counts, "failures": failures,
               "support_only": identity["support_only"], "qualified": False}
    (args.evidence / "summary.json").write_text(json.dumps(summary, indent=2)+"\n")
    print(json.dumps(summary))
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
