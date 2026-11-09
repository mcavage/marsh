#!/usr/bin/env python3
"""Raw-socket HTTP client for the Docker endpoint: sends byte-exact cases and
reports what came back, against per-case expectations. Python 3 standard
library only; runs in a job.

    rawdocker.py [--docker-host URL] [--idle S] [--check] [--calibrate]
                 [--observer-cmd 'ARGV...'] [--no-cleanup]
                 [--case ID ...] [--list] FILE.json ...
    rawdocker.py --record-upstream URL --log FILE      # recording upstream
    rawdocker.py --compare-recording FILE --canonical cases/canonical.json

FILE is a case file (cases/benign.json, encodings.json, framing.json). Each
case runs on a fresh connection to --docker-host (default $DOCKER_HOST;
unix:///path or tcp://host:port). One JSON line per case on stdout:

    {"id", "responses": [{"status", "reason", "headers", "body"}], "closed",
     "bytes_received", "elapsed_ms", "refused", "forbidden_found", "expect_ok",
     "failed": [...why the expectation failed], "cleanup": [...]}

"closed" is true when the peer closed the connection (EOF): after the last
expected response the client keeps reading for --close-wait seconds (default 1)
before it calls the connection open; otherwise within the idle timeout. "refused" is true when no response was 201 Created.

Case format:

    {"id": "...", "description": "...",
     "send": [<segment>, ...],
     "responses": N,                 # optional: stop reading after N complete responses
     "expect": {
        "statuses": [200, [400, 403]],   # response i has this status (a list = any of); null = any
        "marsh_refusals": true,          # every 403 body starts with "marsh: " (a local refusal)
        "closed": true,                  # the connection is closed after the last response
        "close_on_400": true,            # any 400 response must be followed by close
        "forbidden_objects": ["nfxraw-1"]  # containers that must not exist afterwards
     },
     "cleanup": [{"method": "DELETE", "path": "/containers/x?force=1"}]}

Segments: latin-1 text; {"hex": "..."}; {"delay_ms": N}; {"fill": N, "text": "x"}
(N bytes of TEXT repeated, for huge bodies); {"trickle": "text", "bytes": 1,
"delay_ms": 5} (send TEXT BYTES at a time with a pause).

Expectations describe the filtered endpoint (the product). --calibrate reports
them but never fails (use it against a plain engine, or to find the rows the
engine itself would accept). With --check the exit status is 1 when a case's
expectation does not hold. forbidden_objects are looked up after the case on
a fresh connection to --docker-host, or with --observer-cmd (a command line
ending in the docker CLI of a trusted observer, e.g. `sbx exec VM sudo docker`;
`container inspect NAME` is appended; exit 0 = the object exists).

Event witness (--events-cmd): proven before the first case with a probe create that must arrive in the
stream; for each case with forbidden objects the client then waits a bounded settle window
(--event-settle, default 3 s: events delayed beyond it are not caught; the engine emits events as the
operation happens, so the window covers stream and transport lag) and requires a post-case barrier probe
to arrive before it concludes "no create". A stream that cannot deliver the barrier, or that died,
makes the case and the run invalid (exit 2); it is proven again at the end.

Cleanup requests run after each case on a separate, well-formed connection
(skip with --no-cleanup).

--record-upstream URL --log FILE runs a recording upstream instead: a minimal
fake engine on URL (unix:///path or tcp://host:port) that answers plausible
canned responses and appends, per connection, every received chunk (raw base64)
and every parsed request (method, target, headers in order with their original
case and duplicates, body) as JSON lines to FILE. Point the services agent at it
to see exactly what the agent forwards (SEC-23). Stops on SIGTERM/SIGINT.

--compare-recording FILE --canonical CASES --authority JSON [--results RUN.jsonl]
checks what the upstream recorded against each case's exact expected canonical form
(TEST-PLAN SEC-23, revision 3.1). Run the cases first (`rawdocker.py --calibrate
CASES > RUN.jsonl`) against the agent whose upstream is the recorder. Per case, the
recorded requests matching `key` must number `count`; method and target (path +
query) must be exactly as expected; the header NAME set must equal
`header_names_exact` and the listed header values must match (`@body-length` = the
recorded body's length); the body must parse equal to `body` after `${...}` are
filled from --authority (the harness's own scope/attempt/network/limits, never the
product's) and be canonically serialized (`body_compact`); `not_original_bytes`:
neither the client's request bytes nor its body appear upstream; `raw_equal`: the
client's bytes are forwarded unchanged. A case with `refusal_allowed` and no
upstream request passes only when --results shows a verified local refusal (403
with a `marsh: ` body, or 400 with the connection closed). An empty recording
fails every case. Prints one JSON line per case; exit 1 on any failure.
"""
import argparse
import base64
import json
import os
import re
import shlex
import signal
import socket
import socketserver
import subprocess
import sys
import threading
import time
import urllib.parse

BODY_LIMIT = 4096


def connect(docker_host: str, timeout: float) -> socket.socket:
    parsed = urllib.parse.urlparse(docker_host)
    if parsed.scheme == "unix":
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(timeout)
        sock.connect(parsed.path)
    elif parsed.scheme == "tcp":
        sock = socket.create_connection((parsed.hostname, parsed.port or 2375), timeout)
    else:
        raise SystemExit(f"rawdocker: unsupported DOCKER_HOST {docker_host!r}")
    return sock


def send_segment(sock: socket.socket, segment) -> bool:
    """Send one segment; False when the peer closed on us."""
    try:
        if isinstance(segment, str):
            sock.sendall(segment.encode("latin-1"))
        elif "hex" in segment:
            sock.sendall(bytes.fromhex(segment["hex"]))
        elif "fill" in segment:
            unit = segment["text"].encode("latin-1")
            block = (unit * (65536 // len(unit) + 1))[:65536]
            remaining = segment["fill"]
            while remaining > 0:
                sock.sendall(block[:min(65536, remaining)])
                remaining -= min(65536, remaining)
        elif "trickle" in segment:
            data = segment["trickle"].encode("latin-1")
            step = segment.get("bytes", 1)
            for start in range(0, len(data), step):
                sock.sendall(data[start:start + step])
                time.sleep(segment.get("delay_ms", 5) / 1000.0)
        elif "delay_ms" in segment:
            time.sleep(segment["delay_ms"] / 1000.0)
        else:
            raise ValueError(f"bad segment: {segment!r}")
    except (BrokenPipeError, ConnectionResetError):
        return False
    return True


def head_fields(head: bytes) -> tuple[list[str], dict]:
    lines = head.decode("latin-1").split("\r\n")
    headers = {}
    for line in lines[1:]:
        name, _, value = line.partition(":")
        headers[name.strip().lower()] = value.strip()
    return lines, headers


def parse_responses(data: bytes) -> list[dict]:
    """Parse as many complete-or-partial HTTP responses as the stream holds."""
    out = []
    while data:
        end = data.find(b"\r\n\r\n")
        if end < 0:
            out.append({"status": None, "reason": "incomplete head", "headers": {},
                        "body": data[:BODY_LIMIT].decode("utf-8", "replace")})
            break
        lines, headers = head_fields(data[:end])
        rest = data[end + 4:]
        parts = lines[0].split(" ", 2)
        status = int(parts[1]) if len(parts) > 1 and parts[1].isdigit() else None
        if status == 101:
            body, data = rest, b""
        elif "chunked" in headers.get("transfer-encoding", "").lower():
            body, data = b"", rest
            while True:
                line_end = data.find(b"\r\n")
                if line_end < 0:
                    data = b""
                    break
                try:
                    size = int(data[:line_end].split(b";")[0], 16)
                except ValueError:
                    data = b""
                    break
                body += data[line_end + 2:line_end + 2 + size]
                data = data[line_end + 2 + size + 2:]
                if size == 0:
                    break
        elif status in (204, 304) or (status is not None and 100 <= status < 200):
            body, data = b"", rest
        elif headers.get("content-length", "").isdigit():
            length = int(headers["content-length"])
            body, data = rest[:length], rest[length:]
        else:
            body, data = rest, b""
        out.append({"status": status, "reason": parts[2] if len(parts) > 2 else "",
                    "headers": headers, "body": body[:BODY_LIMIT].decode("utf-8", "replace")})
    return out


def complete(data: bytes, wanted: int) -> bool:
    """True when the stream holds WANTED responses that are each fully received."""
    seen = 0
    while data and seen < wanted:
        end = data.find(b"\r\n\r\n")
        if end < 0:
            return False
        _, headers = head_fields(data[:end])
        rest = data[end + 4:]
        if "chunked" in headers.get("transfer-encoding", "").lower():
            marker = rest.find(b"\r\n0\r\n\r\n")
            if marker < 0 and not rest.startswith(b"0\r\n\r\n"):
                return False
            data = rest[(marker + 7 if marker >= 0 else 5):]
        else:
            length = int(headers["content-length"]) if headers.get("content-length", "").isdigit() else 0
            if len(rest) < length:
                return False
            data = rest[length:]
        seen += 1
    return seen >= wanted


def run_case(case: dict, docker_host: str, idle: float, close_wait: float = 1.0) -> dict:
    started = time.monotonic()
    sock = connect(docker_host, idle)
    received = b""
    closed = False
    wanted = case.get("responses")
    try:
        for segment in case["send"]:
            if not send_segment(sock, segment):
                closed = True
                break
        deadline = started + 120
        while not closed and time.monotonic() < deadline:
            if wanted is not None and complete(received, wanted):
                # Complete is not closed: look for EOF (and any trailing response)
                # for a bounded time before calling the connection open.
                sock.settimeout(close_wait)
                while True:
                    try:
                        more = sock.recv(65536)
                    except socket.timeout:
                        break
                    except ConnectionResetError:
                        closed = True
                        break
                    if not more:
                        closed = True
                        break
                    received += more
                break
            try:
                chunk = sock.recv(65536)
            except socket.timeout:
                break
            except ConnectionResetError:
                closed = True
                break
            if not chunk:
                closed = True
                break
            received += chunk
    finally:
        sock.close()
    responses = parse_responses(received)
    return {"id": case["id"], "responses": responses, "closed": closed,
            "bytes_received": len(received), "elapsed_ms": int((time.monotonic() - started) * 1000),
            "refused": not any(r["status"] == 201 for r in responses)}


def object_state(name: str, docker_host: str, idle: float, observer: list[str] | None) -> str:
    """'exists', 'absent' (the observer positively said no such object) or
    'error' (it could not tell: permission, transport, a filtered 403)."""
    if observer:
        done = subprocess.run([*observer, "container", "inspect", name], capture_output=True, text=True)
        if done.returncode == 0:
            return "exists"
        return "absent" if re.search(r"no such (container|object)", done.stderr + done.stdout, re.I) else "error"
    wire = f"GET /containers/{name}/json HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n"
    outcome = run_case({"id": "observe", "send": [wire], "responses": 1}, docker_host, idle, 0.2)
    status = outcome["responses"][0]["status"] if outcome["responses"] else None
    return {200: "exists", 404: "absent"}.get(status, "error")


def evaluate(expect: dict | None, result: dict) -> list[str]:
    """Why the expectation does not hold (empty = holds)."""
    failed = []
    if not expect:
        return failed
    responses = result["responses"]
    for index, spec in enumerate(expect.get("statuses", [])):
        if spec is None:
            continue
        allowed = spec if isinstance(spec, list) else [spec]
        if index >= len(responses):
            failed.append(f"response {index}: missing (wanted {allowed})")
        elif responses[index]["status"] not in allowed:
            failed.append(f"response {index}: status {responses[index]['status']} not in {allowed}")
    if expect.get("marsh_refusals"):
        for index, response in enumerate(responses):
            if response["status"] == 403 and not response["body"].startswith("marsh: "):
                failed.append(f"response {index}: 403 body is not a marsh refusal")
    if "closed" in expect and result["closed"] != expect["closed"]:
        failed.append(f"closed is {result['closed']}, wanted {expect['closed']}")
    if expect.get("close_on_400") and any(r["status"] == 400 for r in responses) and not result["closed"]:
        failed.append("a 400 was answered but the connection was not closed")
    if result.get("forbidden_found"):
        failed.append(f"forbidden objects exist: {result['forbidden_found']}")
    if result.get("forbidden_unverified"):
        failed.append(f"could not verify absence (observer error): {result['forbidden_unverified']}")
    if result.get("forbidden_created_events"):
        failed.append(f"create events for forbidden objects: {result['forbidden_created_events']}")
    if result.get("events_unhealthy"):
        failed.append("the event witness died: creates could not be observed (an empty list is not 'nothing created')")
    return failed


def cleanup(case: dict, docker_host: str, idle: float) -> list[dict]:
    done = []
    for request in case.get("cleanup", []):
        wire = (f"{request['method']} {request['path']} HTTP/1.1\r\nHost: d\r\n"
                "Connection: close\r\nContent-Length: 0\r\n\r\n")
        outcome = run_case({"id": "cleanup", "send": [wire]}, docker_host, idle)
        done.append({"path": request["path"],
                     "status": outcome["responses"][0]["status"] if outcome["responses"] else None})
    return done


class EventWitnessError(Exception):
    """The create-event stream cannot be trusted (it never started, never saw the probe,
    ended, or died): an empty list from it would NOT mean "nothing was created"."""


class EventWitness:
    """A live `docker events` subscription (--events-cmd): every container create is
    kept with its arrival time, so a case can be checked for create events of its
    forbidden objects independently of any inspect. The stream is only trusted after
    `prove()` saw a probe create arrive, and only while `healthy()`."""

    def __init__(self, argv: list[str]):
        self.creates: list[tuple[float, str]] = []
        self.ended = False
        self.lock = threading.Lock()
        self.process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        threading.Thread(target=self._read, daemon=True).start()
        time.sleep(0.5)
        if not self.healthy():
            raise EventWitnessError(f"the events command exited at start (status {self.process.poll()})")

    def _read(self) -> None:
        for line in self.process.stdout:
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("Type") == "container" and event.get("Action") == "create":
                with self.lock:
                    self.creates.append((time.monotonic(), event.get("Actor", {}).get("Attributes", {}).get("name", "")))
        self.ended = True

    def healthy(self) -> bool:
        return self.process.poll() is None and not self.ended

    def seen(self, name: str) -> bool:
        with self.lock:
            return any(n == name for _, n in self.creates)

    def prove(self, docker_host: str, idle: float, label: str) -> None:
        """Create a harmless probe container through the same endpoint and require its
        create event to arrive: proof that the subscription is live right now."""
        name = f"nfx-events-probe-{os.getpid()}-{label}"
        body = json.dumps({"Image": PROBE_IMAGE, "Cmd": ["true"]})
        wire = (f"POST /containers/create?name={name} HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\n"
                f"Content-Length: {len(body)}\r\n\r\n{body}")
        outcome = run_case({"id": "probe", "send": [wire], "responses": 1}, docker_host, idle, 0.2)
        status = outcome["responses"][0]["status"] if outcome["responses"] else None
        try:
            if status != 201:
                raise EventWitnessError(f"the probe create got status {status}; the witness cannot be verified")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and self.healthy() and not self.seen(name):
                time.sleep(0.05)
            if not self.seen(name):
                raise EventWitnessError("the probe create never appeared in the event stream"
                                        + ("" if self.healthy() else " (the stream ended)"))
        finally:
            run_case({"id": "probe-rm", "send": [f"DELETE /containers/{name}?force=1 HTTP/1.1\r\nHost: d\r\n"
                                                 "Connection: close\r\nContent-Length: 0\r\n\r\n"]}, docker_host, idle)

    def settle(self, docker_host: str, idle: float, label: str, window: float) -> None:
        """Account for a case's events before judging it: wait the bounded settle WINDOW for
        events still in flight, then require a positive post-case barrier probe to arrive.
        Only after both may an empty create list be read as "nothing was created"."""
        time.sleep(window)
        if not self.healthy():
            raise EventWitnessError("the event stream ended while settling")
        self.prove(docker_host, idle, label)

    def created_since(self, since: float, names: list[str]) -> list[str]:
        with self.lock:
            return sorted({n for t, n in self.creates if t >= since and n in names})

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()


PROBE_IMAGE = "busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"


# --- recording upstream -------------------------------------------------------

CANNED = {
    ("GET", "/_ping"): (200, "text/plain; charset=utf-8", "OK"),
}


class Recorder(socketserver.ThreadingMixIn):
    daemon_threads = True
    log_lock = threading.Lock()
    log_path = ""
    counter = 0


def record(entry: dict) -> None:
    with Recorder.log_lock, open(Recorder.log_path, "a") as handle:
        handle.write(json.dumps(entry, sort_keys=True) + "\n")


class RecordingHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        Recorder.counter += 1
        conn = Recorder.counter
        self.request.settimeout(10)
        buffer = b""
        while True:
            try:
                chunk = self.request.recv(65536)
            except OSError:
                break
            if not chunk:
                break
            record({"conn": conn, "t": time.time(), "raw_b64": base64.b64encode(chunk).decode()})
            buffer += chunk
            while True:
                end = buffer.find(b"\r\n\r\n")
                if end < 0:
                    break
                lines = buffer[:end].decode("latin-1").split("\r\n")
                pairs = [tuple(line.split(":", 1)) if ":" in line else (line, "") for line in lines[1:]]
                headers = {name.strip().lower(): value.strip() for name, value in pairs}
                rest = buffer[end + 4:]
                if "chunked" in headers.get("transfer-encoding", "").lower():
                    marker = rest.find(b"0\r\n\r\n")
                    if marker < 0:
                        break
                    body, buffer = rest[:marker + 5], rest[marker + 5:]
                else:
                    length = int(headers["content-length"]) if headers.get("content-length", "").isdigit() else 0
                    if len(rest) < length:
                        break
                    body, buffer = rest[:length], rest[length:]
                method, _, tail = lines[0].partition(" ")
                target = tail.rsplit(" ", 1)[0]
                record({"conn": conn, "t": time.time(), "request": {
                    "method": method, "target": target, "line": lines[0],
                    "headers": [[name, value.strip()] for name, value in pairs],
                    "body_b64": base64.b64encode(body).decode()}})
                path = urllib.parse.urlparse(target).path
                status, ctype, text = CANNED.get((method, path), (None, None, None))
                if status is None:
                    if method == "POST" and path.endswith("/containers/create"):
                        status, ctype, text = 201, "application/json", json.dumps({"Id": "0" * 64, "Warnings": []})
                    elif method == "POST" and path.endswith("/start"):
                        status, ctype, text = 204, "text/plain", ""
                    elif method == "GET" and path.endswith("/containers/json"):
                        status, ctype, text = 200, "application/json", "[]"
                    else:
                        status, ctype, text = 200, "application/json", "{}"
                reply = (f"HTTP/1.1 {status} {'OK' if status < 300 else 'Error'}\r\nApi-Version: 1.47\r\n"
                         f"Content-Type: {ctype}\r\nContent-Length: {len(text)}\r\n\r\n{text}")
                self.request.sendall(reply.encode())
                if headers.get("connection", "").lower() == "close":
                    return


def serve_recorder(url: str, log: str) -> None:
    Recorder.log_path = log
    open(log, "a").close()
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme == "unix":
        if os.path.exists(parsed.path):
            os.unlink(parsed.path)

        class Server(Recorder, socketserver.UnixStreamServer):
            pass
        server = Server(parsed.path, RecordingHandler)
    else:
        class Server(Recorder, socketserver.TCPServer):
            allow_reuse_address = True
        server = Server((parsed.hostname or "127.0.0.1", parsed.port or 2375), RecordingHandler)
    for name in (signal.SIGTERM, signal.SIGINT):
        signal.signal(name, lambda *_: threading.Thread(target=server.shutdown, daemon=True).start())
    print(f"rawdocker: recording upstream on {url}, log {log}", flush=True)
    server.serve_forever()


def fill(node, authority: dict):
    """Replace "${name}" strings with the harness's authority values (typed when
    the whole string is one placeholder)."""
    if isinstance(node, dict):
        return {k: fill(v, authority) for k, v in node.items()}
    if isinstance(node, list):
        return [fill(v, authority) for v in node]
    if isinstance(node, str):
        whole = re.fullmatch(r"\$\{(\w+)\}", node)
        if whole:
            return authority[whole.group(1)]
        return re.sub(r"\$\{(\w+)\}", lambda m: str(authority[m.group(1)]), node)
    return node


def compare_recording(log: str, canonical_file: str, authority: dict, results_file: str | None) -> int:
    """SEC-23 c1: field-by-field comparison of what the recording upstream saw with
    each case's exact expected canonical form (see --compare-recording above)."""
    entries = [json.loads(line) for line in open(log) if line.strip()]
    requests = [e for e in entries if "request" in e]
    raw_by_conn: dict[int, bytes] = {}
    for e in entries:
        if "raw_b64" in e:
            raw_by_conn[e["conn"]] = raw_by_conn.get(e["conn"], b"") + base64.b64decode(e["raw_b64"])
    results = {}
    if results_file:
        for line in open(results_file):
            if line.strip():
                row = json.loads(line)
                results[row.get("id")] = row
    failed = 0
    for case in json.load(open(canonical_file))["cases"]:
        key = case["key"]
        client = "".join(x for x in case["send"] if isinstance(x, str)).encode("latin-1")
        found = [r for r in requests if key in r["request"]["line"] or key.encode() in base64.b64decode(r["request"]["body_b64"])]
        expect = case["expect_upstream"]
        problems = []
        if not found:
            row = results.get(case["id"])
            statuses = [r["status"] for r in (row or {}).get("responses", [])]
            refused = bool(row) and statuses[:1] and statuses[0] in (400, 403) and (
                statuses[0] != 403 or row["responses"][0]["body"].startswith("marsh: ")) and (
                statuses[0] != 400 or row.get("closed"))
            if not case.get("refusal_allowed"):
                problems.append("the upstream saw no such request")
            elif not refused:
                problems.append("the upstream saw nothing and there is no verified local refusal (403 `marsh: ` or "
                                "400 + close) for this case in --results")
        elif len(found) != expect.get("count", 1):
            problems.append(f"{len(found)} requests reached the upstream, wanted {expect.get('count', 1)}")
        else:
            entry = found[0]
            request = entry["request"]
            if request["method"] != expect["method"]:
                problems.append(f"method {request['method']!r}, wanted {expect['method']!r}")
            if request["target"] != expect["target"]:
                problems.append(f"target {request['target']!r}, wanted {expect['target']!r}")
            names = [name for name, _ in request["headers"]]
            if "header_names_exact" in expect and sorted(names) != sorted(expect["header_names_exact"]):
                problems.append(f"header set {sorted(names)}, wanted exactly {sorted(expect['header_names_exact'])}")
            body = base64.b64decode(request["body_b64"])
            for name, want in expect.get("headers", {}).items():
                got = [v for n, v in request["headers"] if n == name]
                want = str(len(body)) if want == "@body-length" else want
                if got != [want]:
                    problems.append(f"header {name}: {got}, wanted [{want!r}]")
            if "body" in expect:
                unparsed = object()  # JSON null is a value (None); only a parse failure is `unparsed`
                try:
                    parsed = json.loads(body)
                except ValueError:
                    parsed = unparsed
                    problems.append("body is not JSON")
                wanted = fill(expect["body"], authority)
                if parsed is not unparsed and parsed != wanted:
                    problems.append(f"body {json.dumps(parsed, sort_keys=True)} != expected {json.dumps(wanted, sort_keys=True)}")
                if expect.get("body_compact") and parsed is not unparsed and \
                        json.dumps(parsed, separators=(",", ":"), ensure_ascii=False) != body.decode("utf-8", "replace"):
                    problems.append("body is not canonically serialized (whitespace outside strings)")
            conn_raw = raw_by_conn.get(entry["conn"], b"")
            if expect.get("not_original_bytes") and (client in conn_raw or body == client.split(b"\r\n\r\n", 1)[-1]):
                problems.append("the client's original bytes reached the upstream")
            if expect.get("raw_equal") and (client not in conn_raw or [list(h) for h in request["headers"]] != [["Host", "d"]]):
                problems.append("the request was not forwarded byte-for-byte unchanged")
        failed += bool(problems)
        print(json.dumps({"id": case["id"], "ok": not problems, "problems": problems, "seen": len(found)}))
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("files", nargs="*")
    parser.add_argument("--docker-host", default=os.environ.get("DOCKER_HOST", "unix:///var/run/docker.sock"))
    parser.add_argument("--idle", type=float, default=2.0, help="seconds without data before giving up on a read")
    parser.add_argument("--case", action="append", default=[], help="run only this case id (repeatable)")
    parser.add_argument("--check", action="store_true", help="exit 1 when a case's expectation does not hold")
    parser.add_argument("--calibrate", action="store_true", help="report expectations, never fail")
    parser.add_argument("--observer-cmd", help="trusted docker CLI prefix for forbidden_objects lookups")
    parser.add_argument("--events-cmd", help="a `docker events ...` command line: live create-event witness")
    parser.add_argument("--event-settle", type=float, default=3.0,
                        help="seconds the event witness waits after each case that has forbidden objects, before the "
                             "post-case barrier probe (default 3). A create event delayed beyond this window is NOT caught")
    parser.add_argument("--close-wait", type=float, default=1.0, help="seconds to look for EOF after the last response")
    parser.add_argument("--no-cleanup", action="store_true")
    parser.add_argument("--list", action="store_true", help="print case ids and exit")
    parser.add_argument("--record-upstream", metavar="URL")
    parser.add_argument("--log", help="recording upstream log (JSON lines)")
    parser.add_argument("--compare-recording", metavar="FILE")
    parser.add_argument("--canonical", help="canonical case file for --compare-recording")
    parser.add_argument("--authority", default="{}", help="JSON of the harness-computed enrichment inputs: "
                        "scope, attempt, network, memory, nano_cpus, pids_limit")
    parser.add_argument("--results", help="the --calibrate output of the same cases (refusal evidence)")
    args = parser.parse_args()

    if args.compare_recording:
        if not args.canonical:
            parser.error("--compare-recording needs --canonical")
        return compare_recording(args.compare_recording, args.canonical, json.loads(args.authority), args.results)
    if args.record_upstream:
        if not args.log:
            parser.error("--record-upstream needs --log")
        serve_recorder(args.record_upstream, args.log)
        return 0
    if not args.files:
        parser.error("case files required")

    cases = []
    for path in args.files:
        with open(path, encoding="utf-8") as handle:
            cases.extend(json.load(handle)["cases"])
    if args.case:
        unknown = set(args.case) - {case["id"] for case in cases}
        if unknown:
            raise SystemExit(f"rawdocker: unknown case id(s): {sorted(unknown)}")
        cases = [case for case in cases if case["id"] in args.case]
    if args.list:
        for case in cases:
            print(case["id"], "-", case.get("description", ""))
        return 0

    observer = shlex.split(args.observer_cmd) if args.observer_cmd else None
    witness = None
    if args.events_cmd:
        argv = shlex.split(args.events_cmd)
        if argv[-1] == "events":  # a docker CLI: ask only for container creates, as JSON
            argv += ["--format", "{{json .}}", "--filter", "type=container", "--filter", "event=create"]
        try:
            witness = EventWitness(argv)
            witness.prove(args.docker_host, args.idle, "start")
        except (EventWitnessError, OSError) as error:
            print(json.dumps({"events_witness": "invalid", "why": str(error)}), flush=True)
            if witness:
                witness.stop()
            return 2
    failed = 0
    witness_broken = False
    for index, case in enumerate(cases):
        try:
            began = time.monotonic()
            result = run_case(case, args.docker_host, args.idle, args.close_wait)
            names = case.get("expect", {}).get("forbidden_objects", [])
            states = {n: object_state(n, args.docker_host, args.idle, observer) for n in names}
            result["forbidden_found"] = [n for n, st in states.items() if st == "exists"]
            result["forbidden_unverified"] = [n for n, st in states.items() if st == "error"]
            if witness is not None and names:
                try:
                    witness.settle(args.docker_host, args.idle, f"c{index}", args.event_settle)
                    result["forbidden_created_events"] = witness.created_since(began, names)
                except (EventWitnessError, OSError) as error:
                    result["events_unhealthy"] = True
                    result["events_why"] = str(error)
                    witness_broken = True
            elif witness is not None and not witness.healthy():
                result["events_unhealthy"] = True
                result["events_why"] = "the event stream ended"
                witness_broken = True
            result["failed"] = evaluate(case.get("expect"), result)
            result["expect_ok"] = None if not case.get("expect") else not result["failed"]
            if result.get("events_unhealthy"):
                result["expect_ok"] = False
                if not any("event witness" in why for why in result["failed"]):
                    result["failed"].append("the event witness died: creates could not be observed")
        except OSError as error:
            result = {"id": case["id"], "error": f"{type(error).__name__}: {error}", "expect_ok": False,
                      "failed": [f"{type(error).__name__}: {error}"]}
        if not args.no_cleanup and case.get("cleanup"):
            try:
                result["cleanup"] = cleanup(case, args.docker_host, args.idle)
            except OSError as error:
                result["cleanup"] = [{"error": str(error)}]
        print(json.dumps(result, sort_keys=True), flush=True)
        if result.get("expect_ok") is False:
            failed += 1
    if witness is not None:
        if witness_broken:
            print(json.dumps({"events_witness": "invalid", "why": "a case could not be settled against the event stream"}), flush=True)
            witness.stop()
            return 2
        try:
            witness.prove(args.docker_host, args.idle, "end")
        except (EventWitnessError, OSError) as error:
            print(json.dumps({"events_witness": "invalid", "why": f"at the end of the run: {error}"}), flush=True)
            witness.stop()
            return 2
        witness.stop()
    return 1 if args.check and not args.calibrate and failed else 0


if __name__ == "__main__":
    sys.exit(main())
