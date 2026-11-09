#!/usr/bin/env python3
"""DOC-01 support: the inventory and the executable form of the services examples
(TEST-PLAN delta 21).

    doc_inventory.py [--ids docs/services-examples.ids] [--docs docker.md shell.md security.md faq.md]

`docs/services-examples.ids` is a checked-in list of example IDs, one per line
(`#` comments). Examples are fenced blocks in the guides, each marked on the line right
above it by an HTML comment

    <!-- services-example id=ID fixture=NAME run=KIND [after=ID] [status=N|nonzero]
                                [stdout="TEXT"]... [stderr="TEXT"]... -->

The attributes are the example's machine-readable contract (the runner, T-REL's
`doc_examples.py` path, runs the block and compares):

  fixture  a directory of tests/acceptance/services/fixtures staged as the project
           (stage.py), services on for it. `stack-a`, `tc` and `build-ctx` are the
           usual ones.
  run      where the block's commands run, as written:
             host   a Mac shell in the project folder (the marsh CLI itself)
             shell  `marsh -c` in the project (the marsh shell, DOCKER_HOST set)
             job    a job of the `nfx` fixture Kit in the project (its `exec` verb,
                    sh -c, cwd the project: the Docker endpoint and forwards of a job)
             lanes  as `job`, but the block is split by its `# In the project folder`,
                    `# In lane a`, `# In lane b` comment lines: the first part runs
                    in the project, the others as jobs in that lane of one open split
                    made from a checkpoint taken after the first part
  after    an earlier example (by ID) that must have run first in the same project,
           and whose effect this one observes (an independent observation of a state
           another example sets)
  status   the exit status (default 0); `nonzero` means any failure
  stdout / stderr
           text that must appear in that stream (substring; repeat the attribute for
           several; JSON-style escapes such as \\n); at least one of status, stdout or
           stderr must be written down.

This tool does DOC-01 c1: the list is non-empty, every inventoried ID is found exactly
once across the four guides, no marker names an ID missing from the list, and every
marker is well formed (known fixture and run, an expectation, a resolvable `after`).
c2 is the runner's: `evaluate()` is the comparison it makes, and `mutate_example()` is
the `--mutate-doc` break (it flips `internet on|off` in the command of the target
example and leaves every expectation alone). Because the dependent example
`internet-on-reaches-out` observes the Internet state independently of the command's own
output, a flipped command fails the dependent example's unchanged expectation.
Functions for the harness: `inventory(ids_path)`, `find_markers(paths)`, `check(ids,
markers)`, `parse_examples(paths)`, `problems(examples)`, `evaluate(example, status,
stdout, stderr)`, `mutate_example(examples, id)` and `mutate_command(text)`.
"""
import argparse
import json
import pathlib
import re
import sys
from dataclasses import dataclass, field

REPO = pathlib.Path(__file__).resolve().parents[4]
MARKER = re.compile(r"^<!--\s*services-example\s+id=([A-Za-z0-9_.-]+)(.*?)-->\s*$")
DEFAULT_DOCS = ["docker.md", "shell.md", "security.md", "faq.md"]


def inventory(path: pathlib.Path) -> list[str]:
    ids = []
    for line in path.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            ids.append(line)
    return ids


def find_markers(paths: list[pathlib.Path]) -> list[tuple[str, str, int]]:
    """(id, document name, line) for every services-example marker."""
    found = []
    for path in paths:
        if not path.is_file():
            continue
        for number, line in enumerate(path.read_text().splitlines(), 1):
            match = MARKER.match(line.strip())
            if match:
                found.append((match.group(1), path.name, number))
    return found


def check(ids: list[str], markers: list[tuple[str, str, int]]) -> dict:
    seen: dict[str, list[str]] = {}
    for example_id, doc, line in markers:
        seen.setdefault(example_id, []).append(f"{doc}:{line}")
    listed = set(ids)
    return {
        "inventory_size": len(ids),
        "duplicate_ids_in_inventory": sorted({i for i in ids if ids.count(i) > 1}),
        "missing": sorted(listed - set(seen)),
        "duplicated": {i: where for i, where in seen.items() if i in listed and len(where) > 1},
        "extra": {i: where for i, where in seen.items() if i not in listed},
        "ok": bool(ids) and not (listed - set(seen)) and all(len(w) == 1 for w in seen.values())
              and not any(i not in listed for i in seen) and len(ids) == len(listed),
    }


FIXTURES = pathlib.Path(__file__).resolve().parent
RUNS = ("host", "shell", "job", "lanes")
ATTR = re.compile(r'\s+(\w+)=("(?:[^"\\]|\\.)*"|\S+)')
KNOWN_ATTRS = {"fixture", "run", "after", "status", "stdout", "stderr"}


@dataclass
class Example:
    id: str
    doc: str
    line: int
    script: str
    fixture: str = ""
    run: str = ""
    after: str = ""
    status: str = ""
    stdout: list[str] = field(default_factory=list)
    stderr: list[str] = field(default_factory=list)
    unknown: list[str] = field(default_factory=list)

    @property
    def wants_nonzero(self) -> bool:
        return self.status == "nonzero"

    @property
    def wanted_status(self) -> int:
        return int(self.status) if self.status.isdigit() else 0


def parse_examples(paths: list[pathlib.Path]) -> list[Example]:
    """Every services-example marker with the fenced block under it, in file order.
    A marker that is not followed (blank lines aside) by a fence is an error."""
    found = []
    for path in paths:
        if not path.is_file():
            continue
        lines = path.read_text().splitlines()
        for number, text in enumerate(lines):
            match = MARKER.match(text.strip())
            if not match:
                continue
            at = number + 1
            while at < len(lines) and not lines[at].strip():
                at += 1
            if at >= len(lines) or not lines[at].startswith("```"):
                raise ValueError(f"{path.name}:{number + 1}: services-example {match.group(1)} is not followed by a code block")
            end = at + 1
            while end < len(lines) and lines[end] != "```":
                end += 1
            example = Example(match.group(1), path.name, number + 1, "\n".join(lines[at + 1:end]) + "\n")
            for key, raw in ATTR.findall(match.group(2)):
                value = json.loads(raw) if raw.startswith(chr(34)) else raw
                if key in ("stdout", "stderr"):
                    getattr(example, key).append(value)
                elif key in KNOWN_ATTRS:
                    setattr(example, key, value)
                else:
                    example.unknown.append(key)
            found.append(example)
    return found


def problems(examples: list[Example]) -> list[str]:
    """What is wrong with the markers themselves (DOC-01 c1's well-formedness)."""
    out = []
    seen: set[str] = set()
    for e in examples:
        where = f"{e.doc}:{e.line} {e.id}"
        if e.unknown:
            out.append(f"{where}: unknown attribute {', '.join(e.unknown)}")
        if not e.fixture or not (FIXTURES / e.fixture).is_dir():
            out.append(f"{where}: fixture {e.fixture!r} is not a directory of tests/acceptance/services/fixtures")
        if e.run not in RUNS:
            out.append(f"{where}: run={e.run!r} is not one of {', '.join(RUNS)}")
        if not (e.status or e.stdout or e.stderr):
            out.append(f"{where}: no expectation (write status=, stdout= or stderr=)")
        if e.status and not (e.status.isdigit() or e.status == "nonzero"):
            out.append(f"{where}: status={e.status!r} is neither a number nor nonzero")
        if e.after and e.after not in seen:
            out.append(f"{where}: after={e.after!r} is not an earlier example")
        if e.run == "lanes" and not all(marker in e.script for marker in ("# In the project folder", "# In lane a", "# In lane b")):
            out.append(f"{where}: a lanes example needs its '# In the project folder', '# In lane a' and '# In lane b' parts")
        seen.add(e.id)
    return out


def evaluate(example: Example, status: int, stdout: str, stderr: str) -> list[str]:
    """c2's comparison of one run with the example's documented expectation."""
    failed = []
    if example.wants_nonzero:
        if status == 0:
            failed.append("exit status 0, documented as a failure")
    elif status != example.wanted_status:
        failed.append(f"exit status {status}, documented {example.wanted_status}")
    failed += [f"stdout lacks {text!r}" for text in example.stdout if text not in stdout]
    failed += [f"stderr lacks {text!r}" for text in example.stderr if text not in stderr]
    return failed


def mutate_example(examples: list[Example], target: str = "services-internet") -> list[Example]:
    """The --mutate-doc break: the target's command is changed, no expectation is."""
    out = []
    hit = False
    for e in examples:
        if e.id == target:
            e = Example(**{**e.__dict__, "script": mutate_command(e.script)})
            hit = True
        out.append(e)
    if not hit:
        raise ValueError(f"no example {target!r} to mutate")
    return out


def mutate_command(text: str) -> str:
    """Flip the first `internet on` / `internet off` (the plan's example mutation)."""
    for old, new in (("internet on", "internet off"), ("internet off", "internet on")):
        if old in text:
            return text.replace(old, new, 1)
    raise ValueError("no `internet on|off` command to mutate; pick another command")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--ids", type=pathlib.Path, default=REPO / "docs/services-examples.ids")
    parser.add_argument("--docs", nargs="*", default=DEFAULT_DOCS)
    args = parser.parse_args()
    if not args.ids.is_file():
        print(json.dumps({"ok": False, "problem": f"{args.ids} is missing"}))
        return 1
    paths = [REPO / "docs" / name for name in args.docs]
    report = check(inventory(args.ids), find_markers(paths))
    try:
        report["malformed"] = problems(parse_examples(paths))
    except ValueError as error:
        report["malformed"] = [str(error)]
    report["ok"] = report["ok"] and not report["malformed"]
    print(json.dumps(report, indent=1, sort_keys=True))
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
