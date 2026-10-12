"""Per-case verdicts that every bash_*.py suite writes for bash_suites.py.

A suite compares each case's candidate observation with GNU Bash's and writes
EVIDENCE/cases.json: {"cases": {CASE_ID: VERDICT}, "errors": [...]}.

  match     the candidate's observation equals GNU Bash's
  mismatch  it differs (the runner requires a classified divergence)
  oracle    GNU Bash itself failed the case's own expectation or timed out:
            the case is not a valid reference and always fails the run

errors lists harness failures (a probe that crashed, a fixture that could not be
created); any error fails the run. Case ids are stable and unique per suite.
"""
from __future__ import annotations

import json
from pathlib import Path

MATCH, MISMATCH, ORACLE = "match", "mismatch", "oracle"


def write(evidence: Path, verdicts: dict[str, str], errors: list[str] | tuple = ()) -> None:
    bad = {case: verdict for case, verdict in verdicts.items() if verdict not in (MATCH, MISMATCH, ORACLE)}
    if bad:
        raise ValueError(f"unknown verdicts: {bad}")
    evidence.mkdir(parents=True, exist_ok=True)
    (evidence / "cases.json").write_text(json.dumps({"cases": verdicts, "errors": list(errors)},
                                                    indent=1, sort_keys=True) + "\n")


def verdict(oracle_valid: bool, equal: bool) -> str:
    return ORACLE if not oracle_valid else MATCH if equal else MISMATCH
