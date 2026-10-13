"""The environment a harness gives the product it launches.

A harness never hands marsh (or the shells, Kits and VMs behind it) the
caller's whole login environment, which holds credentials. It starts from this
allowlist: the basics, the stock SBX control-plane selectors the product itself
passes to `sbx` (marsh-runtime's SystemCommandRunner), the Docker CLI
selectors, and the Rust toolchain locations a dev build needs. Each harness
then adds the MARSH_* settings of its own scope. Names only are ever reported.
"""
from __future__ import annotations

import os
import re
from collections.abc import Iterable, Mapping

INHERITED = frozenset({
    "PATH", "HOME", "TERM", "LANG", "TMPDIR", "USER", "LOGNAME", "SHELL",
    "DOCKER_SANDBOXES_API", "DOCKER_SANDBOXES_APP_NAME", "SANDBOXES_STORAGE_ROOT", "XDG_STATE_HOME",
    "DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_CONFIG",
    "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN",
    "MARSH_TEST_RUN",
})

CREDENTIAL = re.compile(r"(_API_KEY|_TOKEN|_PASSWORD)$|SECRET", re.IGNORECASE)

def clean_environment(source: Mapping[str, str] | None = None) -> dict[str, str]:
    """The allowlisted part of `source` (default: this process's environment)."""
    source = os.environ if source is None else source
    environment = {name: value for name, value in source.items() if name in INHERITED or name.startswith("LC_")}
    leaked = credential_names(environment)
    if leaked:
        raise RuntimeError(f"credential-shaped variables would reach the product: {leaked}")
    return environment


def credential_names(names: Iterable[str]) -> list[str]:
    """The credential-shaped names among `names`."""
    return sorted(name for name in names if CREDENTIAL.search(name))
