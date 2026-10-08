#!/usr/bin/env python3
"""Real ACP host CLI/private transaction checks, with only stock calls controlled.

This peer represents the private parent solely to isolate host validation and
phase reporting. Daemon grant/relay authority is exercised by fixture_caller.rs.
No VM, credentials, or stock qualification is implied.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import uuid


def read_frame(channel):
    def exact(size):
        data = bytearray()
        while len(data) < size:
            part = channel.recv(size - len(data))
            if not part:
                raise AssertionError("host closed without typed completion")
            data.extend(part)
        return data
    size = struct.unpack(">I", exact(4))[0]
    assert size <= 1_048_576
    return json.loads(exact(size))


def send_frame(channel, value):
    data = json.dumps(value).encode()
    channel.sendall(struct.pack(">I", len(data)) + data)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--marsh", type=Path, required=True)
    parser.add_argument("--case", choices=("all", "rollback"), default="all")
    args = parser.parse_args()
    binary = args.marsh.resolve(strict=True)
    stock_source = Path(__file__).with_name("stock.py")
    print(json.dumps({"binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                      "caller_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                      "stock_fixture_sha256": hashlib.sha256(stock_source.read_bytes()).hexdigest()}), flush=True)
    outcomes = {}
    with tempfile.TemporaryDirectory(prefix="acp-typed-host-") as temp:
        root = Path(temp).resolve()
        project, host, home = (root / part for part in ("project", "host", "selected"))
        for directory in (project, host, home, home / "home"):
            directory.mkdir(mode=0o700)
        stock = root / "stock-sbx"
        shutil.copyfile(stock_source, stock)
        stock.chmod(0o700)
        env = dict(PATH="/usr/bin:/bin", USER="fixture", LOGNAME="fixture", HOME=str(host),
                   MARSH_HOME=str(home), MARSH_SBX=str(stock), MARSH_PUBLICATION_CHANNEL="1")

        def run(operation, name, *, kit=None, swap=False, invalid_id=False, remove_registration=False):
            identity = project.stat()
            context = dict(session=dict(session_id=str(uuid.uuid4()), username="fixture", uid=os.getuid(), gid=os.getgid(),
                                        launch_directory=str(project), guest_home=str(host), home_backing=str(home / "home"),
                                        ephemeral_home=False, terminal=False, terminal_size=None),
                           project_identity=[identity.st_dev, identity.st_ino], kind="acp", operation=operation,
                           name=name, kit=kit)
            argv = [str(binary), "acp", "host-" + operation, name]
            if operation == "publish":
                argv += ["invalid" if invalid_id else str(uuid.uuid4()), str(uuid.uuid4())]
            context.update(scope_admitted=False, admission_lock=None, sandbox=None,
                           agent_session_id=argv[4] if operation == 'publish' else None,
                           generation=argv[5] if operation == 'publish' else None)
            parent, child = socket.socketpair()
            parent.settimeout(30)
            process = subprocess.Popen(argv, cwd=project, env=env, stdin=child, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, start_new_session=True)
            child.close()
            events = []
            try:
                send_frame(parent, context)
                while True:
                    event = read_frame(parent)
                    events.append(event)
                    if event["type"] == "prepare_kit":
                        assert kit == "fixture"
                        if swap:
                            project.rename(root / "original-project")
                            project.mkdir(mode=0o700)
                        if remove_registration:
                            (root / "stock-registry.json").write_text("{}")
                        send_frame(parent, {"Ok": "exact-fixture-vm"})
                    elif event["type"] in ("begin_commit", "begin_rollback"):
                        send_frame(parent, {"Ok": None})
                    elif event["type"] == "run_stock":
                        result = subprocess.run([str(stock), *event['data']['arguments']], cwd=project, env=env,
                                                capture_output=True, timeout=65, start_new_session=True)
                        send_frame(parent, {'Ok': dict(exit_code=result.returncode, stdout=list(result.stdout), stderr=list(result.stderr))})
                    elif event["type"] == "complete":
                        break
                    else:
                        raise AssertionError(event)
                stdout, stderr = process.communicate(timeout=30)
                assert not stdout, "host stdout was used as an IPC result"
                return process.returncode, event["data"], events, stderr.decode(errors="replace")
            finally:
                parent.close()
                if process.poll() is None:
                    assert process.pid > 1 and process.pid != os.getpgrp()
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                if swap and (root / "original-project").exists():
                    project.rmdir()
                    (root / "original-project").rename(project)

        code, outcome, events, stderr = run("publish", "invalid.tool", kit="fixture", invalid_id=True)
        assert code != 0 and outcome["status"] == "rejected_before_effect", (outcome, stderr)
        assert [event["type"] for event in events] == ["complete"], events
        assert not (root / "stock-calls.jsonl").exists(), "invalid declaration reached stock inspection"
        outcomes["invalid_declaration"] = outcome

        code, outcome, events, stderr = run("publish", "swap.tool", kit="fixture", swap=True)
        assert code != 0 and outcome["status"] == "uncertain", (outcome, stderr)
        assert [event["type"] for event in events if event["type"] != 'run_stock'] == ["prepare_kit", "complete"], events
        calls = [json.loads(line) for line in (root / "stock-calls.jsonl").read_text().splitlines()]
        assert all(call[0] == 'inspect' or call[1] == 'inspect' for call in calls), calls
        assert not (root / "stock-registry.json").exists()
        outcomes["socket_context_project_recheck"] = outcome

        code, outcome, events, stderr = run("publish", "ok.tool", kit="fixture")
        assert code == 0 and outcome["status"] == "committed", (outcome, stderr)
        assert [event["type"] for event in events if event["type"] != 'run_stock'] == ["prepare_kit", "begin_commit", "complete"], events
        registry = json.loads((root / "stock-registry.json").read_text())
        assert len(registry) == 1
        registration = next(iter(registry.values()))
        command = registration["command"]
        declaration = Path(command[command.index("--declaration") + 1])
        assert outcome["declaration_sha256"] == hashlib.sha256(declaration.read_bytes()).hexdigest()
        outcomes["committed"] = outcome
        if args.case == "all":
            prior_declaration = declaration.read_bytes()
            code, outcome, events, stderr = run("publish", "ok.tool", kit="fixture", remove_registration=True)
            assert code != 0 and outcome["status"] == "uncertain", (outcome, stderr)
            assert [event["type"] for event in events if event["type"] != 'run_stock'] == ["prepare_kit", "complete"], "registration race entered commit: " + repr(events)
            assert "registration changed" in outcome["message"], outcome
            assert declaration.read_bytes() == prior_declaration, "preflight refusal changed the declaration"
            assert json.loads((root / "stock-registry.json").read_text()) == {}
            outcomes["post_prepare_registration_change"] = outcome
        code, outcome, events, stderr = run("unpublish", "ok.tool")
        assert code == 0 and outcome["status"] == "committed", (outcome, stderr)
        assert [event["type"] for event in events if event["type"] != 'run_stock'] == ["begin_commit", "complete"], events
        assert json.loads((root / "stock-registry.json").read_text()) == {}
        assert not declaration.exists()
        outcomes["revoked"] = outcome
        (root / "fail-load").touch()
        (root / "fail-rm").touch()
        code, outcome, events, stderr = run("publish", "rollback.tool", kit="fixture")
        assert code != 0 and outcome["status"] == "uncertain", (outcome, stderr)
        assert "load failed" in outcome["message"] and "rollback failed" in outcome["message"] and "remove failed" in outcome["message"], outcome
        assert json.loads((root / "stock-registry.json").read_text()), "rollback failure was not exercised"
        outcomes["rollback_uncertainty_preserves_both_causes"] = outcome
        (root / "fail-load").unlink()
        (root / "fail-rm").unlink()
        code, outcome, events, stderr = run("unpublish", "rollback.tool")
        assert code == 0 and outcome["status"] == "committed", (outcome, stderr)
        assert json.loads((root / "stock-registry.json").read_text()) == {}
    print(json.dumps({"outcomes": outcomes, "all_assertions_passed": True}), flush=True)


if __name__ == "__main__":
    main()
