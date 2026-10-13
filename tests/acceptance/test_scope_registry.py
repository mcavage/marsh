"""Scope registry and `make check-gc` decisions against a fake `sbx` (no VMs)."""
from __future__ import annotations

import json
import os
import pathlib
import signal
import stat
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import scope_registry  # noqa: E402

FAKE_SBX = """#!{python}
import json, sys
state_path = {state!r}
state = json.load(open(state_path))
args = sys.argv[1:]
state["calls"].append(args)
if args[:2] == ["ls", "--json"]:
    print(json.dumps({{"sandboxes": [{{"name": n, "id": i}} for n, i in state["vms"].items()]}}))
elif args[:2] == ["rm", "--force"] and not state.get("rm_fails"):
    state["vms"].pop(args[2], None)
else:
    json.dump(state, open(state_path, "w"))
    sys.exit(1)
json.dump(state, open(state_path, "w"))
"""

OWNED = "aaaaaaaa-0000-4000-8000-000000000001"
OTHER = "bbbbbbbb-0000-4000-8000-000000000002"
UNRELATED = "cccccccc-0000-4000-8000-000000000003"


class Host:
    """A fake stock SBX plus a fake process table."""

    def __init__(self, root: pathlib.Path, vms: dict[str, str]) -> None:
        self.state = root / "sbx-state.json"
        self.state.write_text(json.dumps({"vms": vms, "calls": []}))
        self.sbx = root / "sbx"
        self.sbx.write_text(FAKE_SBX.format(python=sys.executable, state=str(self.state)))
        self.sbx.chmod(0o700)
        self.processes: dict[int, str] = {}
        self.signals: list[tuple[int, int]] = []
        self.ignore_term: set[int] = set()
        self.unkillable: set[int] = set()

    def vms(self) -> dict[str, str]:
        return json.loads(self.state.read_text())["vms"]

    def calls(self) -> list[list[str]]:
        return json.loads(self.state.read_text())["calls"]

    def mutations(self) -> list[list[str]]:
        return [call for call in self.calls() if call[0] != "ls"]

    def rm_fails(self) -> None:
        state = json.loads(self.state.read_text())
        state["rm_fails"] = True
        self.state.write_text(json.dumps(state))

    def list_processes(self) -> list[tuple[int, int, str]]:
        return [(pid, os.getuid(), command) for pid, command in self.processes.items()]

    def send(self, pid: int, number: int) -> None:
        if pid not in self.processes:
            raise ProcessLookupError(pid)
        self.signals.append((pid, number))
        if pid in self.unkillable or (number == signal.SIGTERM and pid in self.ignore_term):
            return
        del self.processes[pid]

    def collect(self, registry: pathlib.Path, **kwargs) -> list[dict]:
        return scope_registry.collect(registry, sbx_run=scope_registry.sbx_runner(str(self.sbx)),
                                      list_processes=self.list_processes, send=self.send, grace=0.2, **kwargs)


class ScopeRegistryTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name).resolve()
        self.registry = self.root / "state" / "marsh-dev" / "scopes"
        self.prefix = self.root / "prefix"
        (self.prefix / "bin").mkdir(parents=True)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def scope(self, name: str, vms: dict | None = None, grants: dict | None = None) -> tuple[pathlib.Path, pathlib.Path]:
        """A registered scope at <root>/<name>/target/check; returns (home, mirror)."""
        check = self.root / name / "target" / "check"
        home = check / "home"
        home.mkdir(parents=True)
        mirror = scope_registry.register(self.registry, home=home, control_root=check / "control", prefix=self.prefix,
                                         creator="test", checkout=self.root / name)
        if vms is not None:
            document = {"version": 2, "vms": {n: ({"purpose": "kit", "key": n} | ({"uuid": u} if u else {}))
                                              for n, u in vms.items()}}
            if grants:
                document["grants"] = grants
            mirror.write_text(json.dumps(document))
        return home, mirror

    def remove_checkout(self, name: str) -> None:
        import shutil
        shutil.rmtree(self.root / name)

    def daemon(self, home: pathlib.Path, prefix: pathlib.Path | None = None) -> str:
        return f"{prefix or self.prefix}/bin/marshd --home {home}"

    def test_registration_is_private_outside_the_scope_and_seeds_an_existing_map(self) -> None:
        check = self.root / "wt" / "target" / "check"
        (check / "home").mkdir(parents=True)
        seed = self.root / "seed.json"
        seed.write_text(json.dumps({"version": 2, "vms": {"marsh-k-seeded00": {"purpose": "kit", "key": "k",
                                                                              "uuid": OWNED}}}))
        mirror = scope_registry.register(self.registry, home=check / "home", control_root=check / "control",
                                         prefix=self.prefix, creator="test", checkout=self.root / "wt", seed_map=seed)
        entry = mirror.parent
        self.assertEqual(entry.parent, self.registry)
        self.assertFalse(entry.is_relative_to(self.root / "wt"))
        self.assertEqual(stat.S_IMODE(entry.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE((entry / "scope.json").stat().st_mode), 0o600)
        scope = json.loads((entry / "scope.json").read_text())
        self.assertEqual(scope["home"], str((check / "home").resolve()))
        self.assertEqual(scope_registry.recorded_vms(json.loads(mirror.read_text())), {"marsh-k-seeded00": OWNED})
        # A daemon-written mirror is never overwritten by a later seed.
        mirror.write_text(json.dumps({"version": 2, "vms": {}}))
        scope_registry.register(self.registry, home=check / "home", control_root=check / "control",
                                prefix=self.prefix, creator="test", checkout=self.root / "wt", seed_map=seed)
        self.assertEqual(json.loads(mirror.read_text())["vms"], {})
        scope_registry.unregister(self.registry, check / "home")
        self.assertFalse(entry.exists())

    def test_live_scope_is_left_alone(self) -> None:
        home, _ = self.scope("live", {"marsh-k-live0000": OWNED})
        host = Host(self.root, {"marsh-k-live0000": OWNED})
        host.processes[100] = self.daemon(home)
        self.assertEqual(host.collect(self.registry), [])
        self.assertEqual(host.mutations(), [])
        self.assertEqual(host.signals, [])
        self.assertEqual(len(list(self.registry.iterdir())), 2)  # the entry and the lock

    def test_gone_scope_stops_only_its_processes_removes_only_exact_name_and_uuid(self) -> None:
        home, mirror = self.scope("gone", {"marsh-k-owned000": OWNED, "marsh-k-replaced": OWNED.replace("1", "9"),
                                           "marsh-s-intent00": None, "marsh-k-already0": OWNED.replace("1", "8")},
                                  grants={"g": {"names": {"marsh-g-child000": OTHER}}})
        live_home, _ = self.scope("live", {"marsh-k-livevm00": UNRELATED})
        self.remove_checkout("gone")
        host = Host(self.root, {"marsh-k-owned000": OWNED, "marsh-k-replaced": UNRELATED.replace("3", "4"),
                                "marsh-g-child000": OTHER, "marsh-k-livevm00": UNRELATED,
                                "marsh-k-foreign0": OWNED.replace("1", "7")})
        other_prefix = self.root / "other-prefix"
        host.processes.update({
            101: self.daemon(home),
            102: f"{self.prefix}/bin/marsh-mcp export-serve --workspace x --home {home} --marsh m",
            103: self.daemon(live_home),
            104: self.daemon(home, other_prefix),
            105: f"{self.daemon(home)}2",
            106: f"{self.prefix}/bin/marsh-mcp export-serve --home {home}-other --marsh m",
        })
        (report,) = host.collect(self.registry)
        self.assertEqual(sorted(pid for pid, _ in host.signals), [101, 102])
        self.assertEqual(sorted(host.processes), [103, 104, 105, 106])
        self.assertEqual([c for c in host.mutations()], [["rm", "--force", "marsh-g-child000"],
                                                         ["rm", "--force", "marsh-k-owned000"]])
        self.assertEqual(host.vms(), {"marsh-k-replaced": UNRELATED.replace("3", "4"),
                                      "marsh-k-livevm00": UNRELATED, "marsh-k-foreign0": OWNED.replace("1", "7")})
        actions = {vm["name"]: vm["action"] for vm in report["vms"]}
        self.assertEqual(actions, {"marsh-k-owned000": "removed", "marsh-g-child000": "removed",
                                   "marsh-k-replaced": "replaced", "marsh-s-intent00": "gone",
                                   "marsh-k-already0": "gone"})
        self.assertEqual(report["errors"], [])
        self.assertTrue(report["deleted"])
        self.assertFalse(mirror.parent.exists())
        self.assertTrue((self.registry / scope_registry.scope_id(live_home)).is_dir())

    def test_present_intent_cannot_be_proven_and_keeps_the_entry(self) -> None:
        _, mirror = self.scope("gone", {"marsh-k-intent00": None})
        self.remove_checkout("gone")
        host = Host(self.root, {"marsh-k-intent00": OWNED})
        (report,) = host.collect(self.registry)
        self.assertEqual(host.mutations(), [])
        self.assertEqual(report["vms"][0]["action"], "unproven-intent")
        self.assertTrue(report["errors"])
        self.assertTrue(mirror.exists())

    def test_failed_removal_keeps_the_entry_for_a_retry(self) -> None:
        _, mirror = self.scope("gone", {"marsh-k-owned000": OWNED})
        self.remove_checkout("gone")
        host = Host(self.root, {"marsh-k-owned000": OWNED})
        host.rm_fails()
        (report,) = host.collect(self.registry)
        self.assertEqual(report["vms"][0]["action"], "remove-failed")
        self.assertTrue(mirror.exists())
        self.assertEqual(host.vms(), {"marsh-k-owned000": OWNED})

    def test_stubborn_daemon_gets_sigkill_and_a_survivor_keeps_the_entry(self) -> None:
        home, mirror = self.scope("gone", {})
        self.remove_checkout("gone")
        host = Host(self.root, {})
        host.processes[201] = self.daemon(home)
        host.ignore_term.add(201)
        (report,) = host.collect(self.registry)
        self.assertEqual(host.signals, [(201, signal.SIGTERM), (201, signal.SIGKILL)])
        self.assertTrue(report["deleted"])

        home, mirror = self.scope("gone2", {})
        self.remove_checkout("gone2")
        host.processes[202] = self.daemon(home)
        host.unkillable.add(202)
        (report,) = host.collect(self.registry)
        self.assertTrue(report["errors"])
        self.assertTrue(mirror.parent.exists())

    def test_dry_run_changes_nothing(self) -> None:
        home, mirror = self.scope("gone", {"marsh-k-owned000": OWNED})
        self.remove_checkout("gone")
        host = Host(self.root, {"marsh-k-owned000": OWNED})
        host.processes[301] = self.daemon(home)
        (report,) = host.collect(self.registry, dry_run=True)
        self.assertEqual(report["vms"][0]["action"], "remove")
        self.assertEqual((host.signals, host.mutations()), ([], []))
        self.assertTrue(mirror.exists())

    def test_unreadable_or_mismatched_registration_is_left_alone(self) -> None:
        home, mirror = self.scope("gone", {"marsh-k-owned000": OWNED})
        self.remove_checkout("gone")
        scope_file = mirror.parent / "scope.json"
        document = json.loads(scope_file.read_text())
        document["home"] = str(self.root / "elsewhere")  # does not hash to this entry
        scope_file.write_text(json.dumps(document))
        host = Host(self.root, {"marsh-k-owned000": OWNED})
        host.processes[401] = self.daemon(home)
        (report,) = host.collect(self.registry)
        self.assertTrue(report["errors"])
        self.assertEqual((host.signals, host.mutations()), ([], []))
        scope_file.write_text("{")
        (report,) = host.collect(self.registry)
        self.assertTrue(report["errors"])
        self.assertEqual(host.mutations(), [])


if __name__ == "__main__":
    unittest.main()
