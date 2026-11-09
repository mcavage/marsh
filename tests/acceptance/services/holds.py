"""H-14 hold points: barriers, not breaks.

A component that reaches hold point `<p>` while `MARSH_TEST_BREAK` contains `hold-<p>` creates `<p>.reached` and
waits up to 120 s for `<p>.release`. The daemon and worker use `$MARSH_HOME/test-holds/` on the Mac; the services
agent uses `/run/marsh-test-holds/` inside `marsh-svc-engine`. This is the harness side. The marsh side is a
`break_hold()` helper that a later task adds next to H2's `break_mode()`; it must use exactly these file names.
"""

from __future__ import annotations

import pathlib
import time
from typing import Callable

from catalogue import HOLD_POINTS

AGENT_DIR = "/run/marsh-test-holds"
HOLD_CEILING = 120.0  # what a component waits for `.release` before it carries on by itself


class HoldError(Exception):
    pass


class HoldPoints:
    def __init__(self, home: pathlib.Path, requested: list[str],
                 agent_exec: Callable[[str, bool], tuple[int, str]] | None = None) -> None:
        """HOME: the scope's MARSH_HOME. REQUESTED: the modes the daemon was started with. AGENT_EXEC runs one
        shell command in the engine container (`vx(cmd, engine="prod", write=True)`) and returns (exit, stdout)."""
        self.home = pathlib.Path(home)
        self.requested = set(requested)
        self.agent_exec = agent_exec

    def _check(self, point: str) -> str:
        if point not in HOLD_POINTS:
            raise HoldError(f"unknown hold point {point!r}; registered: {sorted(HOLD_POINTS)}")
        if f"hold-{point}" not in self.requested:
            raise HoldError(f"hold-{point} is not among the modes this daemon was started with: "
                            f"{sorted(self.requested)}")
        return HOLD_POINTS[point]

    def directory(self, point: str) -> str:
        component = self._check(point)
        if component == "daemon":
            return str(self.home / "test-holds")
        if component == "agent":
            return AGENT_DIR
        raise HoldError(f"hold point {point} belongs to {component}, not to this harness")

    def _agent(self, command: str) -> tuple[int, str]:
        if self.agent_exec is None:
            raise HoldError("no engine container to reach: the services VM is not bound")
        return self.agent_exec(command, True)

    def reached(self, point: str) -> bool:
        path = f"{self.directory(point)}/{point}.reached"
        if HOLD_POINTS[point] == "agent":
            return self._agent(f"test -e {path}")[0] == 0
        return pathlib.Path(path).exists()

    def wait_reached(self, point: str, timeout: float = 60.0) -> None:
        """Poll until the component is parked at POINT. A timeout is a harness failure, never a refusal."""
        deadline = time.monotonic() + min(timeout, HOLD_CEILING)
        while not self.reached(point):
            if time.monotonic() > deadline:
                raise HoldError(f"nothing reached hold point {point} within {timeout:.0f}s")
            time.sleep(0.1)

    def aborted(self, point: str) -> bool:
        """The component left POINT by its own abort path (`<point>.aborted`), not because the harness released it."""
        path = f"{self.directory(point)}/{point}.aborted"
        if HOLD_POINTS[point] == "agent":
            return self._agent(f"test -e {path}")[0] == 0
        return pathlib.Path(path).exists()

    def wait_aborted(self, point: str, timeout: float = 60.0) -> None:
        deadline = time.monotonic() + min(timeout, HOLD_CEILING)
        while not self.aborted(point):
            if time.monotonic() > deadline:
                raise HoldError(f"hold point {point} was not aborted within {timeout:.0f}s")
            time.sleep(0.1)

    def release(self, point: str) -> None:
        path = f"{self.directory(point)}/{point}.release"
        if HOLD_POINTS[point] == "agent":
            code, _ = self._agent(f"mkdir -p {AGENT_DIR} && touch {path}")
            if code != 0:
                raise HoldError(f"could not create {path} in the engine container")
        else:
            pathlib.Path(path).parent.mkdir(parents=True, exist_ok=True)
            pathlib.Path(path).touch()

    def clear(self, point: str) -> None:
        """Remove the marker files so the point can be used again."""
        directory = self.directory(point)
        names = " ".join(f"{directory}/{point}.{suffix}" for suffix in ("reached", "release", "aborted"))
        if HOLD_POINTS[point] == "agent":
            self._agent(f"rm -f {names}")
        else:
            for suffix in ("reached", "release", "aborted"):
                pathlib.Path(f"{directory}/{point}.{suffix}").unlink(missing_ok=True)
