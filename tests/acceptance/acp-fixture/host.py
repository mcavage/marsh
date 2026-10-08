#!/usr/bin/python3
"""Controlled ACP host fixture: real typed private IPC, no stock resources.

A .host-cli marker switches positive journeys to the exact real CLI; only the
stock executable and Kit launcher remain fixtures. Fault modes below test lost
or malformed results without making stdout an authoritative reply.
"""
import hashlib
import json
import os
import pathlib
import socket
import struct
import sys
import time

project = pathlib.Path.cwd()
real = project / ".host-cli"
if real.exists():
    # Keep the daemon's actual host control root. The caller runner supplies
    # an isolated HOME before starting the daemon; changing it here would make
    # the host declaration use a different scope lock than the daemon retains.
    env = dict(os.environ)
    binary = real.read_text().strip()
    os.execve(binary, [binary, *sys.argv[1:]], env)

assert os.environ.get("MARSH_PUBLICATION_CHANNEL") == "1"
channel = socket.socket(fileno=0)
channel.settimeout(430)


def receive(length):
    data = bytearray()
    while len(data) < length:
        chunk = channel.recv(length - len(data))
        if not chunk:
            raise RuntimeError("publication channel closed")
        data.extend(chunk)
    return data


def read():
    length = struct.unpack(">I", receive(4))[0]
    assert length <= 1_048_576
    return json.loads(receive(length))


def send(value):
    data = json.dumps(value).encode()
    channel.sendall(struct.pack(">I", len(data)) + data)


context = read()
operation = {"host-publish": "publish", "host-unpublish": "unpublish"}[sys.argv[2]]
assert context["kind"] == "acp" and context["operation"] == operation
assert context["name"] == sys.argv[3]
assert context["session"]["session_id"]
if pathlib.Path(".host-timeout").exists():
    time.sleep(330)  # Actual host budget must expire; no synthetic clock/knob.
    sys.exit(0)


def event(value):
    with pathlib.Path(".host-events.jsonl").open("a") as log:
        log.write(json.dumps(value) + "\n")
    send(value)


def complete(status, message, digest=None):
    result = dict(status=status, message=message)
    if digest is not None:
        result["declaration_sha256"] = digest
    event(dict(type="complete", data=result))
    sys.exit(0 if status == "committed" else 1)


def exchange(kind):
    event(dict(type=kind))
    result = read()
    if "Err" in result:
        complete("uncertain", result["Err"])
    return result["Ok"]


if operation == "publish":
    name, agent_id, generation = sys.argv[3:6]
    if pathlib.Path(".host-name-collision").exists():
        complete("rejected_before_effect", "fixture host registration name conflict")
    if context.get("kit"):
        assert exchange("prepare_kit") == "exact-fixture-vm"
    assert exchange("begin_commit") is None
    declaration = dict(schema_version="marsh.published_acp/v1", tool_name=name,
                       agent_session_id=agent_id, generation=generation,
                       canonical_workspace=os.getcwd())
    encoded = json.dumps(declaration).encode()
    pathlib.Path("publication.json").write_bytes(encoded)
    os.chmod("publication.json", 0o600)
    if pathlib.Path(".host-lost-result").exists():
        print("Committed: fixture registration boundary")
        sys.exit(0)  # Never an authoritative outcome, despite exit 0/stdout.
    if pathlib.Path(".host-partial-result").exists():
        channel.sendall(struct.pack(">I", 128) + b'{"typ')
        sys.exit(0)  # Truncated typed frame is not an authoritative rejection.
    if pathlib.Path(".host-malformed-result").exists():
        data = b"not JSON"
        channel.sendall(struct.pack(">I", len(data)) + data)
        sys.exit(0)
    if pathlib.Path(".host-false-rejection").exists():
        complete("rejected_before_effect", "incorrect host rejection after mutation")
    complete("committed", "fixture registration boundary", hashlib.sha256(encoded).hexdigest())
else:
    if pathlib.Path(".host-reject-unpublish").exists():
        complete("rejected_before_effect", "fixture host removal preflight rejected")
    assert exchange("begin_commit") is None
    pathlib.Path("publication.json").unlink(missing_ok=True)
    complete("committed", "fixture registration removed")
