"""H-9 and H-10: per-scenario evidence, byte-exact command records, timing helpers.

`evidence/<run>/<ID>.json` is validated against evidence.schema.json when it is
written and again for every file at the end of the run. Secrets registered with
`Recorder.add_secret` are replaced before anything is stored; environments are
never recorded.
"""

from __future__ import annotations

import base64
import hashlib
import json
import math
import pathlib
import re
import time
from typing import Any

from run import validate_schema

SCHEMA_PATH = pathlib.Path(__file__).with_name("evidence.schema.json")
SCHEMA_ID = "marsh.services-evidence/v1"
STREAM_LIMIT = 1024 * 1024
REDACTED = "[redacted]"


def load_schema() -> dict[str, Any]:
    return json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))


def validate_evidence(document: Any) -> None:
    """Raises ValueError naming the first offending path."""
    validate_schema(document, load_schema())


def percentile(samples: list[float], q: float) -> float:
    """H-9: nearest rank, `sorted[ceil(q*N)-1]`, over N retained samples (warm-ups are already dropped)."""
    if not samples:
        raise ValueError("no samples")
    if not 0 < q <= 1:
        raise ValueError("q must be in (0, 1]")
    ordered = sorted(samples)
    return ordered[math.ceil(q * len(ordered)) - 1]


class Series:
    """One measured series. N counts retained samples: collect N+3 and drop the first 3 as warm-ups. A cold-start
    series has no warm-ups (a cold start is never one). Failed and gapped attempts are kept, and counted."""

    WARMUPS = 3

    def __init__(self, label: str, n: int, *, cold: bool = False) -> None:
        self.label = label
        self.n = n
        self.cold = cold
        self.warmups = 0 if cold else self.WARMUPS
        self.samples: list[dict[str, Any]] = []

    @property
    def wanted(self) -> int:
        return self.n + self.warmups

    def add(self, ns: int, *, ok: bool = True, gapped: bool = False) -> None:
        self.samples.append({"ns": ns, "ok": ok, "gapped": gapped})

    def retained(self) -> list[dict[str, Any]]:
        return self.samples[self.warmups:]

    def complete(self) -> bool:
        return len(self.samples) >= self.wanted

    def failures(self) -> int:
        """Retained attempts that failed or were gapped; they stay in the reliability accounting."""
        return sum(1 for s in self.retained() if not s["ok"] or s["gapped"])

    def percentile(self, q: float) -> float:
        return percentile([s["ns"] for s in self.retained() if s["ok"] and not s["gapped"]], q)

    def record(self, recorder: "Recorder") -> None:
        for index, sample in enumerate(self.samples):
            recorder.timings.append({"label": f"{self.label}#{index}", "ns": sample["ns"], "ok": sample["ok"],
                                     "gapped": sample["gapped"], "retained": index >= self.warmups,
                                     "cold": self.cold})


# Synthetic secrets any fixture can produce. Fixtures add their own (fixtures/synthetic-secrets.json).
DEFAULT_SECRET_PATTERNS = (r"eyJ[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{8,}",  # JWT (Supabase local keys)
                           r"sb_(?:secret|publishable)_[A-Za-z0-9_-]{8,}", r"NFX_SECRET_[A-Za-z0-9]{4,}")


def load_synthetic_secrets(path: pathlib.Path) -> tuple[list[str], list[tuple[str, str | None]]]:
    """(literals, [(regex, replacement)]) a fixture directory defines; none when the file is absent.

    Two formats: `synthetic-secrets.json` (`{"literals": [...], "patterns": ["regex", ...]}`) and H1b's
    `redaction.json` (`{"patterns": [{"regex": ..., "replace"?: ...}], "replacement": "[redacted:nfx]"}`)."""
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return [], []
    literals = document.get("literals", [])
    default = document.get("replacement")
    patterns: list[tuple[str, str | None]] = []
    for item in document.get("patterns", []):
        regex, replace = (item, None) if isinstance(item, str) else (item.get("regex"), item.get("replace"))
        patterns.append((regex, replace or default))
    if not all(isinstance(v, str) for v in literals) or not all(isinstance(r, str) for r, _ in patterns):
        raise ValueError(f"{path}: literals and patterns must be strings")
    for regex, _ in patterns:
        re.compile(regex)
    return literals, patterns


class Recorder:
    """What one scenario did: commands, observations, timings and checks."""

    def __init__(self) -> None:
        self.secrets: set[str] = set()
        self.patterns: list[tuple[re.Pattern[bytes], bytes]] = [
            (re.compile(p.encode()), REDACTED.encode()) for p in DEFAULT_SECRET_PATTERNS]
        self.reset()

    def reset(self) -> None:
        self.commands: list[dict[str, Any]] = []
        self.observations: list[dict[str, Any]] = []
        self.timings: list[dict[str, Any]] = []
        self.checks: list[dict[str, str]] = []

    def add_secret(self, value: str) -> None:
        if value:
            self.secrets.add(value)

    def add_pattern(self, pattern: str, replacement: str | None = None) -> None:
        """A fixture-defined synthetic-secret regex. REPLACEMENT may use group references (`\\1`) to keep context."""
        self.patterns.append((re.compile(pattern.encode()), (replacement or REDACTED).encode()))

    def redact(self, data: bytes) -> bytes:
        """Every evidence byte goes through here: registered literals, then the fixtures' patterns."""
        for secret in sorted(self.secrets, key=len, reverse=True):
            data = data.replace(secret.encode(), REDACTED.encode())
        for pattern, replacement in self.patterns:
            data = pattern.sub(replacement, data)
        return data

    def redact_document(self, value: Any) -> Any:
        """The same redaction over every string of a finished evidence document: values and dictionary keys
        (a key that redacts to an existing one gets a numeric suffix, so nothing is lost or overwritten)."""
        if isinstance(value, str):
            return self.redact(value.encode()).decode()
        if isinstance(value, list):
            return [self.redact_document(item) for item in value]
        if isinstance(value, dict):
            out: dict[Any, Any] = {}
            for key, item in value.items():
                clean = self.redact_document(key) if isinstance(key, str) else key
                candidate, number = clean, 1
                while candidate in out:
                    number += 1
                    candidate = f"{clean}#{number}"
                out[candidate] = self.redact_document(item)
            return out
        return value

    def stream(self, data: bytes | None) -> dict[str, Any]:
        raw = self.redact(data or b"")
        digest = hashlib.sha256(raw).hexdigest()
        kept = raw[:STREAM_LIMIT]
        try:
            return {"encoding": "utf-8", "data": kept.decode("utf-8"), "bytes": len(raw), "sha256": digest,
                    "truncated": len(raw) > STREAM_LIMIT}
        except UnicodeDecodeError:
            return {"encoding": "base64", "data": base64.b64encode(kept).decode(), "bytes": len(raw),
                    "sha256": digest, "truncated": len(raw) > STREAM_LIMIT}

    def command(self, argv: list[str], cwd: pathlib.Path | str | None, exit_code: int | None, started_ns: int,
                elapsed_ns: int, stdout: bytes | None, stderr: bytes | None, **extra: Any) -> dict[str, Any]:
        record = {"argv": [self.redact(a.encode()).decode() for a in argv],
                  "cwd": str(cwd) if cwd is not None else None, "exit": exit_code, "started_ns": started_ns,
                  "elapsed_ns": elapsed_ns, "stdout": self.stream(stdout), "stderr": self.stream(stderr), **extra}
        return record

    def timing(self, label: str, ns: int) -> None:
        self.timings.append({"label": label, "ns": ns})


class Stopwatch:
    """`with Stopwatch() as t:` then `t.ns`: monotonic_ns around one call."""

    def __enter__(self) -> "Stopwatch":
        self.start = time.monotonic_ns()
        self.ns = 0
        return self

    def __exit__(self, *exc: object) -> None:
        self.ns = time.monotonic_ns() - self.start
