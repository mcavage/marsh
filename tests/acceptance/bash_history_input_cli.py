#!/usr/bin/env python3
"""Black-box primary-history, pasted-input and input-HUP comparisons.

The probes own every shell session, retain original bytes/PTY transcripts and
fail cleanup problems. GNU is executed independently, never used by marsh as an
implementation. --require-exact fails real candidate differences, not merely
harness failures. Terminal painting is retained rather than ANSI-normalized.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

PROBES = Path(__file__).resolve().with_name("history_input_probes")
SUITES = {
    "context-pipe": ("context.py", ["pipe"]),
    "context-pty": ("context.py", ["pty"]),
    "context-basic": ("context.py", ["basic-pty"]),
    "policy": ("policy.py", []),
    "edit": ("edit.py", []),
    "paste": ("paste.py", []),
    "hup": ("hup.py", []),
    "nul": ("nul.py", []),
    "eof": ("eof.py", []),
}
PROMPT = b"__RL_P1__> "


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def healthy(row: dict) -> bool:
    cleanup = row["cleanup"]
    return (row.get("error") is None and cleanup["clean"]
            and not cleanup["forced"] and not cleanup["remaining"])


def signature(suite: str, row: dict) -> dict:
    fields = ["status", "stdout"]
    if suite in ("context-pipe", "nul"):
        fields += ["stderr"]
    elif suite == "eof":
        fields += ["stderr", "pc", "ps2"]
    elif suite == "policy":
        fields += ["stderr", "history"]
    elif suite == "edit":
        fields += ["before", "buffer", "callbacks", "pc", "after", "final_status"]
    elif suite == "paste":
        fields += ["before", "pc", "ps2"]
    elif suite == "hup":
        fields += ["survived", "early", "effects"]
        if row["mode"] in ("pipe-noedit", "script"):
            fields += ["stderr"]
    result = {field: row[field] for field in fields}
    if suite == "hup" and row["mode"] == "pipe-edit":
        # The setup line is longer than Readline's horizontal display window.
        # Keep the ENTIRE raw stream, but compare the subsequent short physical
        # input/EOF interaction exactly, not its unrelated startup painting.
        # This catches missing partial echo, wrong byte rendering and banners.
        result["input_after_setup"] = [
            part.hex() for part in bytes.fromhex(row["stderr"]).split(PROMPT)[2:]
        ]
    return result


def key(row: dict) -> tuple:
    return row["name"], row.get("posix"), row.get("basic")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shell", action="append", required=True, metavar="LABEL=PATH")
    parser.add_argument("--brush", action="append", default=[])
    parser.add_argument("--require-exact", action="append", default=[])
    parser.add_argument("--suite", action="append", choices=SUITES)
    parser.add_argument("--locale", action="append", help="Actual installed locale; default C")
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    shells = {}
    for value in args.shell:
        label, separator, spelling = value.partition("=")
        if not separator or not re.fullmatch(r"[A-Za-z0-9_-]+", label) or label in shells:
            parser.error("shells require distinct safe LABEL=PATH values")
        shells[label] = Path(spelling).resolve()
    if "gnu" not in shells or "gnu" in args.brush:
        parser.error("an independently executed GNU shell named gnu is required")
    if any(label not in shells or label == "gnu" for label in args.require_exact):
        parser.error("--require-exact must name a distinct candidate")
    if any(label not in shells for label in args.brush):
        parser.error("--brush must name a supplied shell")
    identities = {label: {"path": str(path), "sha256": sha(path)} for label, path in shells.items()}
    for label in args.require_exact:
        if identities[label]["sha256"] == identities["gnu"]["sha256"]:
            parser.error("oracle self-comparison is not a candidate gate")
    args.evidence.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ)
    version = subprocess.check_output([shells["gnu"], "--version"], timeout=3, start_new_session=True)
    if b"version 5.3.20(" not in version:
        parser.error("primary GNU oracle must be Bash 5.3.20")
    identity = {"binaries": identities, "gnu_version": version.hex(),
                "harness_sha256": sha(Path(__file__)),
                "probe_sha256": {name: sha(PROBES / name) for name, _ in SUITES.values()},
                "nul_vectors_sha256": sha(PROBES.parent / "bash_nul_input.py"),
                "scope": "exact command bytes/status/history/cursors/counters/cleanup; raw terminal paint retained"}
    (args.evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    comparisons, failures = [], []
    exact = {label: 0 for label in shells}
    runs = {label: 0 for label in shells}
    clean = {label: 0 for label in shells}
    for locale_index, locale in enumerate(args.locale or ["C"]):
        for suite in args.suite or SUITES:
            filename, extra = SUITES[suite]
            observations = {}
            for label, binary in shells.items():
                directory = args.evidence / f"{locale_index}-{suite}-{label}"
                command = [sys.executable, str(PROBES / filename), str(directory), str(binary),
                           "brush" if label in args.brush else "gnu", *extra]
                probe_env = dict(env, MARSH_TEST_LOCALE=locale)
                with (args.evidence / f"{locale_index}-{suite}-{label}.log").open("wb") as output:
                    # Each finite probe has bounded owned-shell deadlines and
                    # waits/cleans its sessions; do not kill the probe out from
                    # under that ownership protocol with a global timeout.
                    result = subprocess.run(command, env=probe_env, stdout=output,
                                            stderr=subprocess.STDOUT, start_new_session=True)
                receipt = directory / "receipts.json"
                if result.returncode != 0 or not receipt.exists():
                    failures.append({"locale": locale, "suite": suite, "shell": label,
                                     "probe_status": result.returncode, "receipt": str(receipt)})
                if receipt.exists():
                    data = json.loads(receipt.read_text())
                    if data["sha256"] != identities[label]["sha256"]:
                        raise RuntimeError("executable changed during probe")
                    observations[label] = {key(row): row for row in data["receipts"]}
            reference = observations.get("gnu", {})
            if not reference:
                failures.append({"locale": locale, "suite": suite, "error": "missing GNU observations"})
            for label, records in observations.items():
                if records.keys() != reference.keys():
                    failures.append({"locale": locale, "suite": suite, "shell": label,
                                     "error": "different observation sets"})
                for case, row in records.items():
                    runs[label] += 1
                    clean[label] += int(healthy(row))
                    oracle = reference.get(case)
                    equal_source = oracle is not None and all(
                        row.get(field) == oracle.get(field) for field in ("source", "setup", "partial", "tail"))
                    oracle_signature = signature(suite, oracle) if oracle is not None else None
                    actual = signature(suite, row)
                    equal = bool(equal_source and healthy(row) and healthy(oracle)
                                 and actual == oracle_signature)
                    exact[label] += int(equal)
                    comparison = {"locale": locale, "suite": suite, "case": case,
                                  "shell": label, "exact": equal, "healthy": healthy(row),
                                  "source_equal": equal_source, "expected": oracle_signature,
                                  "actual": actual, "receipt": str(args.evidence / f"{locale_index}-{suite}-{label}" / "receipts.json")}
                    comparisons.append(comparison)
                    if not equal:
                        failures.append({name: comparison[name] for name in
                                         ("locale", "suite", "case", "shell", "healthy", "source_equal")})
    summary = {"observations": runs, "exact": exact, "clean": clean,
               "failures": failures, "product_qualified": False}
    (args.evidence / "comparisons.json").write_text(json.dumps(comparisons, indent=2) + "\n")
    (args.evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary))
    required = {"gnu", *args.require_exact}
    return int(any(exact[label] != runs[label] or runs[label] == 0 or clean[label] != runs[label]
                   for label in required)
               or any(failure.get("shell", "gnu") in required
                      and ("probe_status" in failure or "error" in failure)
                      for failure in failures))


if __name__ == "__main__":
    raise SystemExit(main())
