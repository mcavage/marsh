#!/usr/bin/env python3
"""Pin ACP agent declarations to the release's Kit references.

    pin-agents.py COMMANDS_JSON AGENTS_JSON OUT_JSON

An immutable (OCI) Kit's ACP declaration must carry that Kit's exact identity.
For a registry reference the daemon's identity is the reference string itself
(`repository@sha256:DIGEST`, as in the pinned commands.json), so each
declaration's `workload_digest` is its command's pinned reference. marshd
refuses to start an immutable Kit whose declaration lacks it.
"""
import json
import re
import sys

PINNED = re.compile(r"[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}")


def main(commands_path, agents_path, out_path):
    commands = json.load(open(commands_path))
    agents = json.load(open(agents_path))
    for agent in agents:
        ref = commands.get(agent["command"])
        if not ref:
            sys.exit(f"agent {agent['name']} names command {agent['command']}, which commands.json does not register")
        if not PINNED.fullmatch(ref):
            sys.exit(f"command {agent['command']} is not pinned by digest: {ref}")
        agent["workload_digest"] = ref
    with open(out_path, "w") as out:
        json.dump(agents, out, indent=2)
        out.write("\n")


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    main(*sys.argv[1:])
