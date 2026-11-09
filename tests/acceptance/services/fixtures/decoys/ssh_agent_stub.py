#!/usr/bin/env python3
"""A synthetic ssh-agent that logs every request (CAP-10, SEC-24).

    ssh_agent_stub.py serve SOCKET LOG       # runs until SIGTERM (use & or a service)
    ssh_agent_stub.py count LOG              # number of requests logged

Speaks just enough of the agent protocol (4-byte length + type byte) for
`ssh-add -l` / BuildKit's `--ssh default=SOCKET`: REQUEST_IDENTITIES (11) is
answered with an empty IDENTITIES_ANSWER (12); anything else gets FAILURE (5).
It holds no keys. Each request appends one JSON line {t, type, name} to LOG.
SEC-24 runs one on the Mac (the decoy, pointed to by the harness's SSH_AUTH_SOCK:
it must log 0 requests) and one in the job (the positive control: it must log).
Standard library only.
"""
import json
import os
import signal
import socket
import struct
import sys
import threading
import time

NAMES = {11: "REQUEST_IDENTITIES", 13: "SIGN_REQUEST", 17: "ADD_IDENTITY", 18: "REMOVE_IDENTITY",
         19: "REMOVE_ALL_IDENTITIES", 22: "LOCK", 23: "UNLOCK", 27: "EXTENSION"}


def serve(path: str, log: str) -> None:
    if os.path.lexists(path):
        os.unlink(path)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(path)
    os.chmod(path, 0o600)
    server.listen(16)
    lock = threading.Lock()

    def record(kind: int) -> None:
        with lock, open(log, "a") as handle:
            handle.write(json.dumps({"t": time.time(), "type": kind, "name": NAMES.get(kind, "UNKNOWN")}) + "\n")

    def handle(conn: socket.socket) -> None:
        with conn:
            while True:
                header = conn.recv(4)
                if len(header) < 4:
                    return
                size = struct.unpack(">I", header)[0]
                body = b""
                while len(body) < size:
                    chunk = conn.recv(size - len(body))
                    if not chunk:
                        return
                    body += chunk
                kind = body[0] if body else 0
                record(kind)
                reply = b"\x0c" + struct.pack(">I", 0) if kind == 11 else b"\x05"
                conn.sendall(struct.pack(">I", len(reply)) + reply)

    def stop(*_):
        os._exit(0)

    signal.signal(signal.SIGTERM, stop)
    open(log, "a").close()
    while True:
        conn, _ = server.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "serve":
        serve(sys.argv[2], sys.argv[3])
    elif len(sys.argv) == 3 and sys.argv[1] == "count":
        print(sum(1 for _ in open(sys.argv[2])) if os.path.exists(sys.argv[2]) else 0)
    else:
        raise SystemExit(__doc__)
