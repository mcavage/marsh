#!/usr/bin/env python3
"""result.schema.json and the structural `marsh results` shape run.py validates evidence against."""

from __future__ import annotations

import importlib.util
import json
import pathlib
import unittest


DIRECTORY = pathlib.Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("marsh_acceptance", DIRECTORY / "run.py")
assert SPEC is not None and SPEC.loader is not None
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)
SCHEMA = json.loads((DIRECTORY / "result.schema.json").read_text(encoding="utf-8"))


def valid_result() -> dict[str, object]:
    return {
        "schema": "marsh.acceptance-result/v1",
        "started_at": "2026-09-20T12:00:00Z",
        "finished_at": "2026-09-20T12:00:01Z",
        "outcome": "passed",
        "environment": {
            "host_platform": "macOS-arm64",
            "source_revision": "a" * 40,
            "source_dirty": True,
            "source_tree_sha256": f"sha256:{'e' * 64}",
            "marsh_binary": "/tmp/marsh",
            "marsh_binary_sha256": f"sha256:{'b' * 64}",
            "guest_artifact_sha256": {
                name: f"sha256:{'f' * 64}"
                for name in ("marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64")
            },
            "sbx_binary": "/opt/homebrew/bin/sbx",
            "sbx_version": "0.45.0",
            "fixture_kit_ref": f"example/fixture@sha256:{'c' * 64}",
            "command_mapping_sha256": f"sha256:{'d' * 64}",
            "project": "/tmp/project",
            "marsh_home": "/tmp/home",
            "job_limits": dict(RUN.JOB_LIMITS),
        },
        "checks": [{"name": "example", "outcome": "passed"}],
        "commands": [],
        "snapshots": [],
    }


class ResultSchemaTests(unittest.TestCase):
    def test_current_evidence_shape_validates(self) -> None:
        RUN.validate_schema(valid_result(), SCHEMA)

    def test_missing_limit_and_extra_field_fail_closed(self) -> None:
        missing = valid_result()
        del missing["environment"]["job_limits"]["pids"]  # type: ignore[index]
        with self.assertRaisesRegex(ValueError, "pids"):
            RUN.validate_schema(missing, SCHEMA)

        extra = valid_result()
        extra["unexpected"] = True
        with self.assertRaisesRegex(ValueError, "unexpected"):
            RUN.validate_schema(extra, SCHEMA)

    def test_timing_definition_accepts_only_truthful_phase_names(self) -> None:
        timing = {
            "durations_ms": {name: 1 for name in RUN.PHASES},
            "milestones_unix_ms": {
                "request_received": 1000,
                "worker_progress": 1004,
                "first_output": 1005,
                "process_exit": 1007,
                "output_drained": 1008,
                "completed": 1010,
            },
            "wall_ms": 8,
            "orchestration_ms": 7,
        }
        definition = SCHEMA["$defs"]["timing_report"]
        RUN.validate_schema(timing, definition)

        timing["durations_ms"]["queue"] = 0
        with self.assertRaisesRegex(ValueError, "queue"):
            RUN.validate_schema(timing, definition)

    def test_results_require_newest_first_structural_fields(self) -> None:
        summaries = []
        for cursor in (2, 1):
            summaries.append(
                {
                    "cursor": cursor,
                    "job_id": f"job-{cursor}",
                    "command": "fixture",
                    "placement": "local",
                    "cleanup": "verified",
                    "parent": None,
                    "state": "finished",
                    "exit_code": 0,
                    "wall_ms": 10,
                }
            )
        receipt = {name: None for name in RUN.RESULT_RECEIPT_FIELDS}
        receipt.update({"schema": "marsh.job/v1", "cursor": 2, "job_id": "job-2", "placement": "local"})
        RUN.validate_structural_results(
            {"schema": "marsh.jobs/v1", "jobs": summaries}, receipt
        )

        summaries.reverse()
        with self.assertRaisesRegex(AssertionError, "newest-first"):
            RUN.validate_structural_results(
                {"schema": "marsh.jobs/v1", "jobs": summaries}, receipt
            )

        summaries.reverse()
        receipt["stdout"] = "forbidden"
        with self.assertRaisesRegex(AssertionError, "non-structural"):
            RUN.validate_structural_results(
                {"schema": "marsh.jobs/v1", "jobs": summaries}, receipt
            )


if __name__ == "__main__":
    unittest.main()
