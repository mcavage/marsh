#!/usr/bin/env python3
"""A process a harness launches gets the allowlisted environment, never credentials."""
import os
import subprocess
import unittest

from launch_env import clean_environment, credential_names


class LaunchEnvironment(unittest.TestCase):
    def test_a_launched_shell_sees_no_credential_shaped_variable(self) -> None:
        caller = {**os.environ, "MARSH_TEST_CANARY_API_KEY": "canary-not-a-credential",
                  "FIXTURE_TOKEN": "x", "FIXTURE_PASSWORD": "x", "A_SECRET_VALUE": "x",
                  "LC_CTYPE": "C", "DOCKER_CONTEXT": "fixture"}
        completed = subprocess.run(["/bin/sh", "-c", "env | cut -d= -f1"], env=clean_environment(caller),
                                   stdin=subprocess.DEVNULL, capture_output=True, timeout=30, check=True)
        names = completed.stdout.decode().split()
        self.assertEqual(credential_names(names), [])
        for kept in ("PATH", "LC_CTYPE", "DOCKER_CONTEXT"):
            self.assertIn(kept, names)


if __name__ == "__main__":
    unittest.main()
