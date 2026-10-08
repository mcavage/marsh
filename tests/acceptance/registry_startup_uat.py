#!/usr/bin/env python3
"""Credential-free real marshd CLI registry preflight. Stock is a poison executable.

Each invocation owns a new session; no VM, Cloud, provider or real stock call.
Rejects invalid overlays before the poison stock boundary, preserves config
bytes, exercises actual packaged registries and explicit ACP replacement.
"""
import argparse
import hashlib
import json
import os
import pathlib
import pwd
import signal
import subprocess
import tempfile
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--marshd", required=True)
    p.add_argument("--source-tree", required=True)
    p.add_argument("--evidence", required=True)
    args = p.parse_args()
    binary = pathlib.Path(args.marshd).resolve(strict=True)
    source = pathlib.Path(args.source_tree).resolve(strict=True)
    evidence = pathlib.Path(args.evidence).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    ref = "example/fixture@sha256:" + "a" * 64
    decl = {"schema_version": 1, "name": "mine", "protocol": "acp_v1", "command": "foo", "workload_digest": ref}
    full = {f"cmd{n}": ref for n in range(251)}
    cases = [
        ("ordinary-foo", {"foo": ref}, None, [], None, True),
        ("override-foo", {"foo": ref, "keep": ref}, {"foo": ref}, [], None, True),
        ("empty-command-overlay-retains", {"foo": ref}, {}, [], None, True),
        ("reserved", {"foo": ref}, {"ps": ref}, [], None, False),
        ("leading-option", {"foo": ref}, {"-option": ref}, [], None, False),
        ("oversize-name", {"foo": ref}, {"x" * 129: ref}, [], None, False),
        ("duplicate-key", {"foo": ref}, '{"bar":"' + ref + '","\\u0062ar":"' + ref + '"}', [], None, False),
        ("malformed-command", {"foo": ref}, "{SECRET_CONFIG_CANARY", [], None, False),
        ("merged-cap", full, {"overflow": ref}, [], None, False),
        ("file-cap", {"foo": ref}, " " * (1048576 + 1) + "{}", [], None, False),
        ("malformed-agents", {"foo": ref}, None, [], "[SECRET_CONFIG_CANARY", False),
        ("duplicate-agents", {"foo": ref}, None, [], [decl, decl], False),
        ("bad-agent-binding", {"foo": ref}, None, [], [{**decl, "workload_digest": "example/other@sha256:" + "b" * 64}], False),
        ("missing-agent-command", {"foo": ref}, None, [], [{**decl, "command": "absent"}], False),
        ("explicit-disable-replaces-invalid-packaged", {"foo": ref}, None, "[INVALID_PACKAGED", [], True),
        ("explicit-replacement", {"foo": ref}, None, [{**decl, "command": "absent"}], [decl], True),
        ("packaged-default", json.loads((source / "packaging/commands.json").read_text()), None, [], None, True),
    ]
    report = {"kind": "real-daemon-cli-poison-stock", "binary": str(binary),
              "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "cases": [], "outcome": "failed"}
    user = pwd.getpwuid(os.getuid())
    for name, packaged, overlay, agents, agent_overlay, expect_stock in cases:
        with tempfile.TemporaryDirectory(prefix="marsh-registry-startup-") as temp:
            root = pathlib.Path(temp).resolve()
            home, control, guest = [root / s for s in ("home", "control", "guest")]
            for path in (home, control, guest):
                path.mkdir(mode=0o700)
            scoped = control / hashlib.sha256(os.fsencode(home)).hexdigest()
            scoped.mkdir(mode=0o700)
            (guest / "kits").symlink_to(source / "kits", target_is_directory=True)
            def write(path, data):
                if data is not None:
                    path.write_text(data if isinstance(data, str) else json.dumps(data))
            write(guest / "commands.json", packaged)
            write(scoped / "commands.json", overlay)
            write(guest / "agents.json", agents)
            write(scoped / "agents.json", agent_overlay)
            marker = root / "stock-called"
            poison = root / "stock-poison"
            poison.write_text("#!/usr/bin/python3\nfrom pathlib import Path\nPath(" + repr(str(marker)) + ").write_text('called')\nraise SystemExit(81)\n")
            poison.chmod(0o700)
            before = {str(path.relative_to(root)): path.read_bytes() for directory in (guest, scoped) for path in directory.glob("*.json")}
            env = {"PATH": "/usr/bin:/bin", "HOME": user.pw_dir, "USER": user.pw_name, "LOGNAME": user.pw_name,
                   "MARSH_HOME": str(home), "MARSH_CONTROL_HOME": str(control), "MARSH_GUEST_ARTIFACTS": str(guest),
                   "MARSH_SBX": str(poison), "MARSH_SHELL_IMAGE": ref}
            started = time.monotonic()
            child = subprocess.Popen([str(binary), "--home", str(home)], env=env, cwd=root,
                                     stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     start_new_session=True)
            case = {"name": name, "pid": child.pid, "pgid": os.getpgid(child.pid), "expected_stock": expect_stock}
            try:
                stdout, stderr = child.communicate(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    stdout, stderr = child.communicate(timeout=2)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    stdout, stderr = child.communicate()
                case["timeout"] = True
            after = {str(path.relative_to(root)): path.read_bytes() for directory in (guest, scoped) for path in directory.glob("*.json")}
            case.update(status=child.returncode, elapsed=time.monotonic() - started, stock_called=marker.exists(),
                        stdout=stdout.decode(errors="replace"), stderr=stderr.decode(errors="replace"),
                        unchanged=before == after)
            case["passed"] = (child.returncode != 0 and marker.exists() == expect_stock and before == after
                              and not case.get("timeout") and b"SECRET_CONFIG_CANARY" not in stderr)
            report["cases"].append(case)
    if all(case["passed"] for case in report["cases"]):
        report["outcome"] = "passed"
    (evidence / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"outcome": report["outcome"], "passed": sum(c["passed"] for c in report["cases"]), "total": len(cases)}))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
