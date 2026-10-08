#!/usr/bin/env python3
"""Source-order guard for known canonical recipes, not Docker parsing/UAT."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import shlex
from recipe_instructions import recipe_instructions

RECIPES = {
    # One Kit per agent: the native CLI profile and its ACP adapter share
    # the single final scoped gate.
    "kits/marsh-codex/codex.dockerfile": "--profile codex-kit --version \"$CODEX_VERSION\" --adapter codex-acp",
    "kits/marsh-claude/claude.dockerfile": "--profile claude-kit --version \"$CLAUDE_VERSION\" --adapter claude-acp",
    "kits/marsh-pi/pi.dockerfile": "--adapter pi",
    "kits/marsh-shell/shell.dockerfile": None,
    "packaging/shell/Dockerfile": None,
}


def check(text, final_scope):
    lines = recipe_instructions(text)
    froms = [i for i, line in enumerate(lines) if re.match(r"FROM\s", line)]
    if not froms:
        raise ValueError("recipe has no FROM instruction")
    final_from = froms[-1]
    lines = lines[final_from:]
    final = []
    for i, line in enumerate(lines):
        if not line.startswith("RUN ") or "verify.py" not in line:
            continue
        if "--stage base" in line:
            continue
        scoped = any(
            token in line
            for token in [
                "--profile ",
                "--adapter ",
                "--copied-agents",
                "--stage final",
            ]
        )
        if scoped:
            final.append(i)
            suffix = line.split("--receipt", 1)[-1]
            if suffix.strip() or any(
                row.startswith(("RUN ", "COPY ", "ADD ", "ONBUILD ")) for row in lines[i + 1 :]
            ):
                raise ValueError(
                    "payload mutation/execution follows a final-labelled receipt"
                )
    if final_scope is not None:
        if len(final) != 1 or final_scope not in lines[final[0]]:
            raise ValueError("missing/ambiguous final scoped gate")
    elif final:
        raise ValueError("base-only recipe must not claim a full final scope")
    if final_scope and final_scope.startswith("--profile codex-kit"):
        guards = [
            i
            for i, line in enumerate(lines)
            if "command -v codex" in line and "codex-cli ${CODEX_VERSION}" in line
        ]
        paths = [
            i for i, line in enumerate(lines) if line == "ENV PATH=/usr/local/bin:$PATH"
        ]
        if len(guards) != 1 or not paths or not (paths[-1] < guards[0] < final[0]):
            raise ValueError(
                "selected Codex lookup/version guard must follow PATH and precede final receipt"
            )
        if any(
            re.match(r"ENV\s", line) and any(
                token == "PATH" or token.startswith("PATH=")
                for token in shlex.split(line)[1:]
            )
            for line in lines[guards[0] + 1 :]
        ):
            raise ValueError("PATH changes after selected-version guard")
    return {
        "final_gates": final,
        "required_scope": final_scope,
        "scope": "Known logical-instruction order only, not actual image execution or complete Dockerfile semantics",
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--root", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    rows = []
    for name, scope in RECIPES.items():
        data = (a.root / name).read_bytes()
        row = {"path": name, "sha256": hashlib.sha256(data).hexdigest()}
        try:
            row.update(check(data.decode(), scope), passed=True)
        except ValueError as error:
            row.update(passed=False, error=str(error))
        rows.append(row)
    with a.output.open("x") as output:
        output.write(json.dumps(rows, indent=2) + "\n")
    return int(not all(row["passed"] for row in rows))


if __name__ == "__main__":
    raise SystemExit(main())
