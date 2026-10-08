#!/usr/bin/env python3
"""Extract the runnable examples of a user guide (default docs/split.md).

A fenced block runs once per `<!-- doc-test: KIND ATTR... -->` marker in the
lines right above it. KIND is `host` (plain /bin/bash on the Mac with the
installed `marsh` on PATH) or `shell` (`marsh -c` in the shell VM). ATTRs:

  status=N           expected exit status (default 0)
  stdout="TEXT"      stdout must contain TEXT
  stderr="TEXT"      stderr must contain TEXT
  dir=plain          run in a plain directory instead of a Git repository
  prep=other         first let the *other* side refresh the Git index
  control="TEXT"     also run with TEXT removed; that run must fail
  replace=["A","B"]  run with A replaced by B (a placeholder command)

`workspaces_uat.py` (W29-doc-examples) runs them in its isolated scope;
`python3 doc_examples.py` lists them.
"""

from __future__ import annotations

import json
import pathlib
import re
import shlex
import sys
from dataclasses import dataclass, field

DOC = pathlib.Path(__file__).resolve().parents[2] / "docs" / "split.md"
MARKER = re.compile(r"^<!-- doc-test: (host|shell)(.*?) -->$")
ATTR = re.compile(r'\s+(\w+)=("(?:[^"\\]|\\.)*"|\[[^\]]*\]|\S+)')


@dataclass
class Example:
    line: int
    kind: str
    script: str
    doc: str = DOC.name
    status: int = 0
    stdout: str = ""
    stderr: str = ""
    plain: bool = False
    prep: bool = False
    control: str = ""
    replace: list[str] = field(default_factory=list)

    def name(self) -> str:
        return f"{self.doc}:{self.line} ({self.kind})"


def examples(path: pathlib.Path = DOC) -> list[Example]:
    lines = path.read_text(encoding="utf-8").splitlines()
    found: list[Example] = []
    pending: list[tuple[int, str, str]] = []
    index = 0
    while index < len(lines):
        line = lines[index]
        marker = MARKER.match(line)
        if marker:
            pending.append((index + 1, marker.group(1), marker.group(2)))
        elif line.startswith("```") and pending:
            end = lines.index("```", index + 1)
            script = "\n".join(lines[index + 1:end]) + "\n"
            for number, kind, attrs in pending:
                example = Example(number, kind, script, path.name)
                for key, raw in ATTR.findall(attrs):
                    value = json.loads(raw) if raw[0] in '"[' else raw
                    if key == "status":
                        example.status = int(value)
                    elif key in ("stdout", "stderr", "control"):
                        setattr(example, key, value)
                    elif key == "dir":
                        example.plain = value == "plain"
                    elif key == "prep":
                        example.prep = value == "other"
                    elif key == "replace":
                        example.replace = value
                    else:
                        raise ValueError(f"{path}:{number}: unknown doc-test attribute {key}")
                if example.replace:
                    old, new = example.replace
                    if old not in script:
                        raise ValueError(f"{path}:{number}: replace target not in the example")
                    example.script = script.replace(old, new)
                found.append(example)
            pending = []
            index = end
        elif line.strip() and pending and not line.startswith("<!--"):
            raise ValueError(f"{path}:{pending[0][0]}: doc-test marker not followed by a code block")
        index += 1
    return found


if __name__ == "__main__":
    for example in examples(pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else DOC):
        print(f"{example.name()}: {shlex.quote(example.script)[:120]}")
