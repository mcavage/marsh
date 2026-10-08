#!/usr/bin/python3
"""Controlled stock-registration boundary for the real ACP host CLI.

Only files beside this executable are changed. This does not launch any VM or
claim stock-SBX qualification. The real CLI owns validation, argv and rollback.
"""
import json
import pathlib
import sys

root = pathlib.Path(__file__).resolve().parent
args = sys.argv[1:]
with (root / "stock-calls.jsonl").open("a") as log:
    log.write(json.dumps(args) + "\n")
path = root / "stock-registry.json"
registry = json.loads(path.read_text()) if path.exists() else {}
if args[:2] == ['inspect', '--json'] and len(args) == 3:
    if args[2] == 'missing':
        sys.stderr.write('fixture sandbox not found\n')
        sys.exit(7)
    print(json.dumps({'name': args[2], 'state': 'running'}))
    sys.exit(0)
if len(args) < 3 or args[0] != "mcp":
    sys.exit(64)
name = args[2]
if args[1] == "inspect":
    if name not in registry:
        sys.stderr.write(f'error: mcp server "{name}" not found: mcp server not found\n  try: sbx mcp ls\n')
        sys.exit(1)
    print(json.dumps(registry[name]))
elif args[1] == "add":
    if name in registry:
        sys.exit("already registered")
    command = args[args.index("--command") + 1]
    server_args = args[args.index("--args") + 1].split(",")
    registry[name] = dict(name=name, type="local", resolved_command=command,
                          command=[command, *server_args])
    path.write_text(json.dumps(registry))
elif args[1] == "load":
    sandbox = args[args.index("--sandbox") + 1]
    assert name in registry
    if sandbox == "missing" or (root / "fail-load").exists():
        sys.stderr.write("fixture load failed after registration\n")
        sys.exit(7)
    with (root / "stock-loads.jsonl").open("a") as log:
        log.write(json.dumps(dict(name=name, sandbox=sandbox)) + "\n")
elif args[1] == "rm":
    if (root / "fail-rm").exists():
        sys.stderr.write("fixture remove failed during rollback\n")
        sys.exit(9)
    name = args[-1]
    if name not in registry:
        sys.exit("registration not found")
    del registry[name]
    path.write_text(json.dumps(registry))
else:
    sys.exit(64)
