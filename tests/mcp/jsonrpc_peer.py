"""Reusable stdio JSON-RPC test peer; importing it starts no product process.

Callers supply the exact verified command. Standalone protocol suites keep their
binary preflight in the suite, not in this reusable transport helper.
"""
from __future__ import annotations

import json
from pathlib import Path
import queue
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
PROTOCOL_VERSION = "2025-06-18"


class JsonRpcPeer:
    def __init__(self, command: list[str], env: dict[str, str]):
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=ROOT,
            bufsize=0,
            start_new_session=True,
        )
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        assert self.process.stderr is not None
        self._stdin = self.process.stdin
        self._lines: queue.Queue[object] = queue.Queue()
        self._pending: dict[object, dict] = {}
        self._next_id = 1
        self._lock = threading.Lock()
        self._reader = threading.Thread(target=self._read_stdout, daemon=True)
        self._reader.start()
        self._stderr_reader = threading.Thread(
            target=self._read_stderr, args=(self.process.stderr,), daemon=True
        )
        self._stderr_reader.start()

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        for line in iter(self.process.stdout.readline, b""):
            try:
                value = json.loads(line)
            except json.JSONDecodeError as error:
                self._lines.put(
                    AssertionError(f"non-JSON stdout from MCP server: {line!r}: {error}")
                )
                continue
            self._lines.put(value)
        self._lines.put(EOFError("MCP server closed stdout"))

    def _read_stderr(self, stream) -> None:
        self.stderr = stream.read().decode("utf-8", "replace")

    def notify(self, method: str, params: dict | None = None) -> None:
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self._write(message)

    def request(
        self, method: str, params: dict | None = None, timeout: float = 5.0
    ) -> dict:
        with self._lock:
            request_id = self._next_id
            self._next_id += 1
        message = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        self._write(message)
        if request_id in self._pending:
            return self._pending.pop(request_id)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(
                    f"timed out waiting for JSON-RPC response {request_id}"
                )
            try:
                item = self._lines.get(timeout=remaining)
            except queue.Empty:
                raise AssertionError(
                    f"timed out waiting for JSON-RPC response {request_id}"
                )
            if isinstance(item, BaseException):
                raise item
            if item.get("id") == request_id:
                return item
            if "id" in item:
                self._pending[item["id"]] = item

    def _write(self, message: dict) -> None:
        self._stdin.write((json.dumps(message, separators=(",", ":")) + "\n").encode())
        self._stdin.flush()

    def initialize(self) -> dict:
        response = self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "marsh-export-test", "version": "1"},
            },
        )
        self.notify("notifications/initialized", {})
        return response

    def close(self) -> None:
        try:
            self._stdin.close()
        except OSError:
            pass
        self.process.terminate()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        try:
            if self.process.stdout:
                self.process.stdout.close()
            if self.process.stderr:
                self.process.stderr.close()
        except OSError:
            pass
