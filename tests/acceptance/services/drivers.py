"""H-8: the two ways a scenario drives the services stack.

`internal` (waves 1-2) reaches the policy only as an unprivileged client inside the engine container, through the
agent's filtered socket, with an authority the harness chooses: `{project, workspace, attempt}`. That is how the
E3 tests run before E2 exists. There is no all-in-VM restore: for S4's internal restore the harness prepares the
target lane directory (a tree copy plus the expected lane files from the U3 fixtures), then calls engine-level
restore into a named scope. `product` (wave 2 or 3 onward) drives the same scenarios through `marsh checkpoint`,
`marsh split`, jobs and lanes. Oracles are identical where they apply; **only `product` passes count for
release**, and every internal gate has a product rerun listed in the test plan's §F.2.

The switch is real and is recorded in every evidence file. The implementations are filled in by the tasks that
bring up the surfaces they call; until then each method raises `Pending`.
"""

from __future__ import annotations

import dataclasses
from typing import Any

from observer import Pending

DRIVERS = ("internal", "product")


@dataclasses.dataclass(frozen=True)
class Authority:
    """The authority an internal client acts with."""
    project: str
    workspace: str = "base"
    attempt: str = "harness"


class Driver:
    name = ""

    def __init__(self, suite: Any) -> None:
        self.suite = suite

    def start_stack(self, project: str, fixture: str, authority: Authority | None = None) -> None:
        raise Pending(f"{self.name} driver: start_stack is filled in by a later task")

    def docker(self, command: str, authority: Authority) -> tuple[int, bytes, bytes]:
        """One docker command as the given authority (internal: the unprivileged client; product: a job)."""
        raise Pending(f"{self.name} driver: docker is filled in by a later task")

    def capture(self, project: str, label: str, scope: str | None = None) -> dict[str, Any]:
        """One capture. Internal capture takes a scope (`--scope a`), so a lane's capture runs in wave 1."""
        raise Pending(f"{self.name} driver: capture is filled in by a later task")

    def restore(self, project: str, checkpoint: str, lane: str) -> dict[str, Any]:
        raise Pending(f"{self.name} driver: restore is filled in by a later task")

    def drop(self, project: str, what: str) -> None:
        raise Pending(f"{self.name} driver: drop is filled in by a later task")


class InternalDriver(Driver):
    name = "internal"


class ProductDriver(Driver):
    name = "product"


def make_driver(name: str, suite: Any) -> Driver:
    return {"internal": InternalDriver, "product": ProductDriver}[name](suite)
