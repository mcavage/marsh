#!/usr/bin/env python3
"""Compare finite raw-NUL shell-input callers with GNU Bash 5.3.20.

Every case also runs with NUL removed to isolate pre-existing behavior. Inputs,
HOME, HISTFILE and BASH_ENV are owned fixtures. Only executable spellings in
stderr are normalized; statuses and every remaining output byte must match.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

cases={
 'nul-second':b'printf FIRST\n\0printf SECOND\n',
 'joined-command':b'# safe\npri\0ntf \'[%s]\\n\' joined\n',
 'quotes-raw-neighbors':b'# safe\nprintf \'[%s]\\n\' a\0b "c\0d" \'e\0f\' $\'g\0h\' \'\xff\0\xfe\'\n',
 'comments-lines':b'# safe\n# comment\0printf BAD\nprintf \'L:%s\\n\' "$LINENO"\n',
 'escaped-newline':b'# safe\nprintf \'[%s]\\n\' a\\\0\nb\n',
 'here-quoted':b"# safe\ncat <<'EOF'\na\0\xffb\0\xfe\nEOF\nprintf 'L:%s\\n' \"$LINENO\"\n",
 'here-expanded':b'# safe\nx=VALUE\ncat <<EOF\na\0$x\0\xff\nEOF\n',
 'here-delimiter':b'# safe\ncat <<EOF\nVALUE\nE\0OF\nprintf END\n',
 'verbose':b"set -v\nprintf '[%s]\\n' 'a\0\xff\xfe'\n",
 'debug-command':b'''# safe\ntrap 'printf "D:%s:%s\\n" "$LINENO" "$BASH_COMMAND"' DEBUG\nprintf '[%s]\\n' 'a\0\xff\xfe'\n''',
 'nul-only-lines':b'# safe\n\0\0\n\0\nprintf \'L:%s\\n\' "$LINENO"\n',
 'eof-nul':b'# safe\nprintf END\0',
 'first-line-nul':b'\0printf FIRST\nprintf SECOND\n',
 'first-line-comment':b'#\0comment\nprintf SECOND\n',
}
for count in (256,257,512,513):
 cases[f'source-sparse-{count}']=b'# safe\nprintf BEGIN\n'+b'#\0\n'*count+b'printf END\n'
 cases[f'source-consecutive-{count}']=b'# safe\nprintf BEGIN\n'+b'\0'*count+b'\nprintf END\n'
cases['source-quote-consecutive']=b"# safe\nprintf '[%s]\\n' 'a\0\0b'\nprintf END\n"
cases['source-status']=b'# safe\nprintf BEGIN\n'+b'#\0\n'*257+b'printf END\n'
for offset in (79, 80, 127, 128):
    cases[f"admission-offset-{offset}"] = b"#" + b"x" * (offset - 1) + b"\0\nprintf AFTER\n"
cases["admission-shebang-second"] = b"#!/not-an-interpreter\n#comment\0\nprintf AFTER\n"
cases["admission-raw-path"] = b"#\0comment\nprintf AFTER\n"
cases["admission-bare-path"] = b"#\0comment\nprintf AFTER\n"
cases["admission-exit-trap"] = b"#\0comment\nprintf AFTER\n"
cases["startup-sparse257"] = b"#safe\nprintf BEGIN\n" + b"#\0\n" * 257 + b"printf END\n"
cases["startup-consecutive513"] = b"#safe\nprintf BEGIN\n" + b"\0" * 513 + b"\nprintf END\n"


def routes_for(name: str) -> tuple[str, ...]:
    if name.startswith("source-"):
        return ("source", "source-alias")
    if name.startswith("startup-"):
        return ("startup",)
    if name.startswith("admission-offset-"):
        return ("script", "external", "exec")
    if name.startswith("admission-"):
        return ("script",)
    if name in ("nul-second", "first-line-nul", "first-line-comment"):
        return ("script", "source", "stdin", "external", "exec")
    return ("script", "source", "stdin")


def observe(shell: Path, root: Path, home: Path, name: str, route: str, data: bytes) -> dict:
    home.mkdir(exist_ok=True)
    raw_name = b"script"
    if name == "admission-raw-path":
        # This Mac test environment rejects creating the invalid UTF-8 leaf.
        # Linux exercises FF/FE; Mac still checks a non-ASCII byte pathname.
        raw_name = b"script-\xc3\xa9" if sys.platform == "darwin" else b"script-\xff\xfe"
    script = root / os.fsdecode(raw_name)
    script.write_bytes(data)
    script.chmod(0o700)
    env = {"PATH": "/usr/bin:/bin", "LC_ALL": "C", "HOME": str(home),
           "HISTFILE": str(home / "history")}
    argv = [str(shell), "--noprofile", "--norc"]
    stdin = b""
    if route == "stdin":
        argv.append("-s")
        stdin = data
    elif route == "startup":
        env["BASH_ENV"] = str(script)
        argv += ["-c", "printf MAIN"]
    elif route == "script":
        if name == "admission-bare-path":
            directory = root / "bin"
            directory.mkdir(exist_ok=True)
            script.rename(directory / "input-script")
            env["PATH"] = str(directory) + ":" + env["PATH"]
            argv.append("input-script")
        else:
            argv.append(str(script))
        if name == "admission-exit-trap":
            startup = root / "startup"
            startup.write_bytes(b"trap 'printf EXIT' EXIT\n")
            env["BASH_ENV"] = str(startup)
    else:
        command = {"source": '. "$1"', "source-alias": 'source "$1"',
                   "external": '"$1"', "exec": 'exec "$1"'}[route]
        if name.startswith("source-"):
            command += '; printf "SOURCE_STATUS:%s\\n" "$?"'
        argv += ["-c", command, "nul-input", str(script)]
    started = time.monotonic()
    proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env, cwd=root, start_new_session=True)
    timed_out = False
    try:
        stdout, stderr = proc.communicate(stdin, timeout=8)
    except subprocess.TimeoutExpired:
        timed_out = True
        proc.kill()  # Only this retained, owned child; no process-group discovery.
        stdout, stderr = proc.communicate(timeout=2)
    return {"pid": proc.pid, "status": proc.returncode, "stdout": stdout.hex(),
            "stderr": stderr.hex(), "elapsed": time.monotonic() - started,
            "timeout": timed_out}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--oracle", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--case", action="append", choices=sorted(cases))
    args = parser.parse_args()
    args.evidence.mkdir(parents=True, exist_ok=True)
    executables = {"gnu": args.oracle.resolve(), "candidate": args.candidate.resolve()}
    rows = []
    with tempfile.TemporaryDirectory(prefix="bash-nul-input-") as directory:
        root = Path(directory)
        version = subprocess.check_output([executables["gnu"], "--version"],
            env={"PATH": "/usr/bin:/bin", "HOME": str(root), "HISTFILE": str(root / "history")},
            start_new_session=True, timeout=3)
        if b"version 5.3.20(" not in version:
            raise SystemExit("GNU oracle must be Bash 5.3.20")

        def normalized(record: dict) -> tuple:
            stderr = bytes.fromhex(record["stderr"])
            for executable in executables.values():
                stderr = stderr.replace(os.fsencode(executable), b"<shell>")
            return record["status"], record["stdout"], stderr.hex(), record["timeout"]

        for name, source in cases.items():
            if args.case and name not in args.case:
                continue
            for route in routes_for(name):
                records = {}
                for variant, data in (("raw", source), ("without-nul", source.replace(b"\0", b""))):
                    for who, shell in executables.items():
                        records[who + "-" + variant] = observe(
                            shell, root, root / (who + "-home"), name, route, data)
                rows.append({"case": name, "route": route, "source_hex": source.hex(),
                    "raw_exact": normalized(records["gnu-raw"]) == normalized(records["candidate-raw"]),
                    "without_nul_exact": normalized(records["gnu-without-nul"]) == normalized(records["candidate-without-nul"]),
                    "runs": records})
    identity = {
        who: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        for who, path in executables.items()
    }
    identity.update({"gnu_version_hex": version.hex(),
        "raw_path_suffix_hex": "c3a9" if sys.platform == "darwin" else "fffe",
        "probe_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "scope": "finite owned sessions and private HOME/HISTFILE/BASH_ENV; no injected signals; executable spelling normalized only in stderr"})
    summary = {"total": len(rows), "raw_exact": sum(row["raw_exact"] for row in rows),
        "without_nul_exact": sum(row["without_nul_exact"] for row in rows),
        "timeouts": sum(record["timeout"] for row in rows for record in row["runs"].values())}
    for name, value in (("identity", identity), ("observations", rows), ("summary", summary)):
        (args.evidence / (name + ".json")).write_text(json.dumps(value, indent=2) + "\n")
    print(json.dumps(summary))
    return int(summary["timeouts"] != 0 or summary["raw_exact"] != len(rows)
               or summary["without_nul_exact"] != len(rows))


if __name__ == "__main__":
    raise SystemExit(main())
