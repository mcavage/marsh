#!/usr/bin/env python3
"""Apply redaction.json to bytes: `redact(data) -> bytes`, or as a filter
(`redact.py < in > out`). Patterns are matched on the latin-1 view so binary
streams keep their length structure outside the matches."""
import json
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
_CONFIG = json.loads((HERE / "redaction.json").read_text())
_PATTERNS = [(re.compile(p["regex"]), p.get("replace")) for p in _CONFIG["patterns"]]


def redact(data: bytes) -> bytes:
    text = data.decode("latin-1")
    for pattern, replace in _PATTERNS:
        text = pattern.sub(replace if replace else _CONFIG["replacement"], text)
    return text.encode("latin-1")


if __name__ == "__main__":
    sys.stdout.buffer.write(redact(sys.stdin.buffer.read()))
