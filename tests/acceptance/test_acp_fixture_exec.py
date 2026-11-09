#!/usr/bin/env python3
"""The fake agent's `exec` verb (H-6) against the real agent.mjs under node: no
VM, no marsh. Existing prompts are untouched; this only covers the verb."""
import base64
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

AGENT = pathlib.Path(__file__).with_name("acp-fixture") / "agent.mjs"


def prompt(text, env=None, cwd=None, timeout=60):
    """Updates (the texts) and the final stopReason of one prompt."""
    lines = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize"},
        {"jsonrpc": "2.0", "id": 2, "method": "session/new"},
        {"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": text}]}},
    ]
    process = subprocess.Popen(["node", str(AGENT)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                               env={**os.environ, **(env or {})}, cwd=cwd)
    process.stdin.write("".join(json.dumps(line) + "\n" for line in lines))
    process.stdin.flush()
    texts, stop = [], None
    try:
        for raw in process.stdout:
            message = json.loads(raw)
            update = message.get("params", {}).get("update", {})
            if update.get("sessionUpdate") == "agent_message_chunk":
                texts.append(update["content"]["text"])
            if message.get("id") == 3:
                stop = message["result"]["stopReason"]
                break
    finally:
        process.stdin.close()
        process.stdout.close()
        process.wait(timeout=timeout)
    return texts, stop


def decoded(texts):
    return b"".join(base64.b64decode(t[len("fixture:out:"):]) for t in texts if t.startswith("fixture:out:"))


@unittest.skipUnless(shutil.which("node"), "node is required")
class ExecVerb(unittest.TestCase):
    def test_exit_and_output(self):
        texts, stop = prompt("exec echo nfx-ok; exit 3")
        self.assertEqual(stop, "end_turn")
        self.assertIn("fixture:exit:3", texts)
        self.assertEqual(decoded(texts), b"nfx-ok\n")

    def test_runs_in_the_agent_cwd(self):
        with tempfile.TemporaryDirectory() as directory:
            texts, _ = prompt("exec pwd", cwd=directory)
            self.assertEqual(decoded(texts).decode().strip(), os.path.realpath(directory))

    def test_large_output_is_complete(self):
        texts, stop = prompt("exec head -c 700000 /dev/zero | tr '\\0' x")
        self.assertEqual(stop, "end_turn")
        self.assertEqual(decoded(texts), b"x" * 700000)
        self.assertFalse(any(t.startswith("fixture:truncated") for t in texts))
        self.assertIn("fixture:exit:0", texts)

    def test_signal_status_is_128_plus_signal(self):
        texts, _ = prompt("exec kill -TERM $$")
        self.assertIn("fixture:exit:143", texts)
        texts, _ = prompt("exec kill -KILL $$")
        self.assertIn("fixture:exit:137", texts)

    def test_disabled_verb_replies_only_the_echo(self):
        texts, stop = prompt("exec echo nfx-ok", env={"NFX_FAKE_EXEC": "off"})
        self.assertEqual(texts, ["fixture:exec echo nfx-ok"])
        self.assertEqual(stop, "end_turn")

    def test_other_prompts_are_unchanged(self):
        texts, stop = prompt("hello")
        self.assertEqual((texts, stop), (["fixture:hello"], "end_turn"))


if __name__ == "__main__":
    unittest.main()
