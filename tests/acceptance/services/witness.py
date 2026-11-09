"""Delta 20: the impostor witness (VM-10), and the controlled-peer helpers (delta 13).

The impostor is a sandbox that must never be used as a services VM. While marsh runs, a process sampler
(`ps -eo pid,lstart,args` every 100 ms) and a `docker events` log run inside it; any `marsh`, `docker load` or
`marsh-svc-engine` activity there fails the check.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import signal
import subprocess
import threading
import time
from typing import Any, Callable

SAMPLER = "while :; do ps -eo pid,lstart,args; echo '--- sample'; sleep 0.1; done"
EVENTS = "docker events --format '{{json .}}'"
# What must never run in an impostor: a marsh binary, an image load, the engine container.
FORBIDDEN_PROCESS = re.compile(r"(?:^|[\s/])marsh(?:d|-[a-z]+)?(?:\s|$)|docker\s+(?:image\s+)?load\b|marsh-svc-engine")
SAMPLER_SELF = re.compile(r"ps -eo pid,lstart,args|while :; do ps|echo '--- sample'")


def scan_process_samples(text: str) -> list[str]:
    """Forbidden processes in a sampler log. The sampler's own command lines are ignored."""
    found: list[str] = []
    for line in text.splitlines():
        if SAMPLER_SELF.search(line) or line.startswith("--- sample"):
            continue
        if FORBIDDEN_PROCESS.search(line) and line.strip() not in found:
            found.append(line.strip())
    return found


def scan_events(lines: list[str]) -> list[str]:
    """Container events that show the engine, a load, or a created container in an impostor."""
    found = []
    for line in lines:
        try:
            event = json.loads(line)
        except ValueError:
            continue
        attributes = (event.get("Actor") or {}).get("Attributes") or {}
        name = attributes.get("name", "")
        action = event.get("Action", "")
        if "marsh-svc-engine" in name or "marsh-svc-engine" in str(attributes.get("image", "")):
            found.append(f"{action} {name}")
        elif event.get("Type") == "image" and action in ("load", "import"):
            found.append(f"image {action}")
        elif event.get("Type") == "container" and action == "create":
            found.append(f"container created: {name or (event.get('Actor') or {}).get('ID', '')[:12]}")
    return found


class ImpostorWitness:
    """Runs the sampler and the event log in IMPOSTOR (a sandbox name) until `stop()`. POPEN starts a long-lived
    helper and returns it (the suite's `popen`, so it is killed with the scenario)."""

    def __init__(self, sbx: str, impostor: str, popen: Callable[[list[str]], "subprocess.Popen[bytes]"]) -> None:
        self.sbx = sbx
        self.impostor = impostor
        self.popen = popen
        self.samples: list[str] = []
        self.events: list[str] = []
        self.processes: list[subprocess.Popen[bytes]] = []
        self.threads: list[threading.Thread] = []

    def start(self) -> "ImpostorWitness":
        for command, sink in ((SAMPLER, self.samples), (f"sudo {EVENTS}", self.events)):
            process = self.popen([self.sbx, "exec", self.impostor, "sh", "-c", command])
            self.processes.append(process)
            thread = threading.Thread(target=self._drain, args=(process, sink), daemon=True)
            thread.start()
            self.threads.append(thread)
        return self

    @staticmethod
    def _drain(process: "subprocess.Popen[bytes]", sink: list[str]) -> None:
        assert process.stdout is not None
        for raw in process.stdout:
            sink.append(raw.decode("utf-8", "replace").rstrip("\n"))

    def stop(self) -> None:
        for process in self.processes:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=10)
        for thread in self.threads:
            thread.join(timeout=10)

    def violations(self) -> list[str]:
        return scan_process_samples("\n".join(self.samples)) + scan_events(self.events)

    def alive(self) -> bool:
        """Both observers still run (a dead sampler would make an empty log look clean)."""
        return all(p.poll() is None for p in self.processes) and bool(self.samples)


# ---- controlled VM peer (delta 13) -------------------------------------------------------------------
PEER_PATH = "/run/nfx/peer.py"


def agent_override_file(home: pathlib.Path) -> pathlib.Path:
    """`$MARSH_HOME/test-holds/agent-override`: names the one project whose agent the daemon replaces with the
    harness's peer while `fault-agent-override` is on."""
    return pathlib.Path(home) / "test-holds" / "agent-override"


def set_agent_override(home: pathlib.Path, project_root: pathlib.Path) -> None:
    path = agent_override_file(home)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(str(project_root) + "\n", encoding="utf-8")


def clear_agent_override(home: pathlib.Path) -> None:
    agent_override_file(home).unlink(missing_ok=True)


def peer_install_command(source: str) -> str:
    """The VM-side command that places the peer script (a setup write through `vx`, never through marsh)."""
    quoted = source.replace("'", "'\\''")
    return f"mkdir -p /run/nfx && printf '%s' '{quoted}' > {PEER_PATH} && chmod 0755 {PEER_PATH}"


def peer_frames(log: str) -> list[Any]:
    """Frames a peer logged (one JSON document per line)."""
    frames = []
    for line in log.splitlines():
        if line.strip():
            try:
                frames.append(json.loads(line))
            except ValueError:
                frames.append(line)
    return frames


def wait_for(condition: Callable[[], bool], timeout: float, what: str) -> None:
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() > deadline:
            raise TimeoutError(f"timed out after {timeout:.0f}s waiting for {what}")
        time.sleep(0.1)
