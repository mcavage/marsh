#!/usr/bin/env python3
"""DOC-01's VM-free half: the services examples in the guides are inventoried, well formed,
cover the compatibility matrix, and the --mutate-doc break is detectable. Running the
examples against a product is the services harness's DOC-01 (c2)."""
import pathlib
import re
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests/acceptance/services/fixtures"))
import doc_inventory as di  # noqa: E402

DOCS = [ROOT / "docs" / name for name in di.DEFAULT_DOCS]
IDS = di.inventory(ROOT / "docs/services-examples.ids")

# One entry per row of the matrix in docs/docker.md ("What works and what is refused"):
# the examples that show its two columns. A new or removed row fails the count below.
MATRIX = [
    ["docker-run-exec-logs", "docker-run-published-port", "refused-privileged"],
    ["compose-up", "refused-host-network", "refused-device"],
    ["named-volume-tmpfs", "refused-outside-bind"],
    ["bind-mount-project", "refused-git-write"],
    ["docker-run-published-port", "refused-bridge-network"],
    ["docker-socket-mount", "refused-device"],
    ["docker-pull-allowed", "refused-registry"],
    ["docker-build", "refused-push", "refused-load", "refused-build-remote"],
    ["testcontainers", "refused-reserved-label"],
    ["resource-limit-default", "resource-limit-over-cap", "refused-reserved-label"],
]
COMMANDS = ["config-services-on", "config-services-off", "services-start", "services-status",
            "services-internet", "services-internet-off", "services-stop", "services-rm",
            "services-hold-release", "reset-include-services"]


def matrix_rows() -> list[str]:
    text = (ROOT / "docs/docker.md").read_text()
    section = text.split("## What works and what is refused", 1)[1].split("\n\n", 3)[2]
    rows = [line for line in section.splitlines() if line.startswith("|")][2:]
    return rows


class ServicesDocs(unittest.TestCase):
    def test_inventory_and_markers_are_exact(self):
        report = di.check(IDS, di.find_markers(DOCS))
        self.assertTrue(report["ok"], report)

    def test_every_marker_is_well_formed(self):
        self.assertEqual(di.problems(di.parse_examples(DOCS)), [])

    def test_matrix_and_commands_are_covered(self):
        self.assertEqual(len(matrix_rows()), len(MATRIX), "the matrix changed: update MATRIX")
        for row in MATRIX:
            for example in row:
                self.assertIn(example, IDS)
        for example in COMMANDS:
            self.assertIn(example, IDS)

    def test_examples_use_only_the_cached_images(self):
        lock = (ROOT / "tests/acceptance/services/fixtures/images.lock").read_text()
        cached = {line.split("@")[0] for line in lock.splitlines() if line and not line.startswith("#")}
        refused_on_purpose = {"quay.io/prometheus/busybox:latest"}
        image = re.compile(r"\b((?:[a-z0-9.-]+/)*[a-z0-9._-]+:[A-Za-z0-9._-]+)\b")
        for example in di.parse_examples(DOCS):
            for line in example.script.splitlines():
                if not re.search(r"docker (run|pull)|^FROM|FROM busybox", line):
                    continue
                for ref in image.findall(line):
                    if ref.split(":")[0] in {"busybox", "alpine", "nginx", "curlimages/curl", "docker", "postgres", "redis"}:
                        self.assertIn(ref, cached | refused_on_purpose, f"{example.id}: {ref} is not in images.lock")

    def test_evaluate_reads_each_expectation(self):
        examples = {e.id: e for e in di.parse_examples(DOCS)}
        blocked = examples["internet-off-blocked"]
        self.assertEqual(di.evaluate(blocked, 6, "000", ""), [])
        self.assertTrue(di.evaluate(blocked, 0, "200", ""))
        refused = examples["refused-privileged"]
        self.assertEqual(di.evaluate(refused, 1, "", "marsh: privileged"), [])
        self.assertTrue(di.evaluate(refused, 1, "", "denied"))

    def test_mutated_doc_is_caught_by_an_unchanged_independent_expectation(self):
        examples = di.parse_examples(DOCS)
        mutated = {e.id: e for e in di.mutate_example(examples)}
        original = {e.id: e for e in examples}
        target = "services-internet"
        self.assertIn("internet off", mutated[target].script)
        self.assertEqual(mutated[target].stdout, original[target].stdout)
        # the flipped command's own output no longer matches its documented text ...
        self.assertTrue(di.evaluate(mutated[target], 0, "Internet is off for this project's Docker: x", ""))
        # ... and a dependent example, observing the state, keeps its expectation and fails
        dependents = [e for e in examples if e.after == target]
        self.assertTrue(dependents, "nothing independent observes the Internet state")
        for dependent in dependents:
            self.assertEqual(mutated[dependent.id].stdout, dependent.stdout)
            self.assertTrue(di.evaluate(dependent, 0, "000", ""))


if __name__ == "__main__":
    unittest.main()
