#!/usr/bin/env python3
"""scripts/update-models-dev.py on a small api.json, as the models-dev workflow runs it."""
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/update-models-dev.py"
SNAPSHOT = ROOT / "crates/marsh-daemon/data/models-dev.json"


def model(family, output, *, canonical=None, tool_call=True, modality="text", open_weights=False):
    entry = {
        "family": family,
        "tool_call": tool_call,
        "open_weights": open_weights,
        "release_date": "2026-10-01",
        "modalities": {"input": ["text"], "output": [modality]},
        "cost": {"input": 1, "output": output},
    }
    if canonical:
        entry["canonical_model_id"] = canonical
    return entry


class UpdateModelsDev(unittest.TestCase):
    def run_script(self, api):
        with tempfile.TemporaryDirectory() as directory:
            source = pathlib.Path(directory, "api.json")
            output = pathlib.Path(directory, "out.json")
            source.write_text(json.dumps(api))
            result = subprocess.run(
                [sys.executable, SCRIPT, "--input", source, "--output", output],
                capture_output=True,
                text=True,
            )
            return result, (json.loads(output.read_text()) if output.exists() else None)

    def test_trims_to_tool_calling_chat_models_one_row_per_vendor_model(self):
        models = {f"filler-{n}": model("filler", 1) for n in range(120)}
        api = {
            "openai": {"models": {"gpt-6.1-terra": model("gpt-terra", 5), "gpt-image-2": model("gpt-image", 30, modality="image")}},
            "openrouter": {
                "models": {
                    "openai/gpt-6.1-terra": model("gpt-terra", 6, canonical="openai/gpt-6.1-terra"),
                    "qwen/qwen3.8-max": model("qwen", 6, open_weights=True),
                    "no-tools": model("x", 1, tool_call=False),
                }
            },
            "zzz": {"models": models},
        }
        result, snapshot = self.run_script(api)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(snapshot["fields"], ["provider", "id", "family", "maker", "output", "open_weights", "released"])
        rows = {(row[0], row[1]): row for row in snapshot["models"]}
        # The vendor's own listing wins over a host's copy of the same model.
        self.assertIn(("openai", "gpt-6.1-terra"), rows)
        self.assertNotIn(("openrouter", "openai/gpt-6.1-terra"), rows)
        self.assertEqual(rows[("openrouter", "qwen/qwen3.8-max")][3:6], ["qwen", 6.0, True])
        self.assertNotIn(("openai", "gpt-image-2"), rows)
        self.assertNotIn(("openrouter", "no-tools"), rows)

    def test_refuses_a_truncated_catalog(self):
        result, snapshot = self.run_script({"openai": {"models": {"gpt-6.1-terra": model("gpt-terra", 5)}}})
        self.assertEqual(result.returncode, 1)
        self.assertIsNone(snapshot)

    def test_the_committed_snapshot_has_the_documented_shape(self):
        snapshot = json.loads(SNAPSHOT.read_text())
        self.assertEqual(snapshot["source"], "https://models.dev/api.json")
        self.assertGreater(len(snapshot["models"]), 1000)
        self.assertTrue(all(len(row) == 7 for row in snapshot["models"]))


if __name__ == "__main__":
    unittest.main()
