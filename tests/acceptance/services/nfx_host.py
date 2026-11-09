"""H-3: what a services E2E run may touch on the host's stock SBX, and the proof
that it put everything back: the sandbox inventory and `sbx policy ls`; the global `sbx settings` value
`sandbox.disk.dockerVolume` is only observed unchanged (revision 3.3), never restored.

Only names this run owns are ever removed: `nfx-<run>-*` sandboxes it creates
itself and `marsh-xnfx01-*` VMs its test daemon makes (DECISIONS 1). Everything
else on the Mac (the user's own marsh VMs and policy rules) is left alone, and
must be unchanged at the end.
"""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
from typing import Any

from provenance import stock_vm_inventory

VM_PREFIX = "marsh-xnfx01-"
DISK_SETTING = "sandbox.disk.dockerVolume"


GENERIC_SANDBOX = re.compile(r"^nfx-[0-9]{12}[0-9a-f]{4}-")


def is_nfx_name(name: str, run: str | None = None) -> bool:
    """A sandbox this harness may have made: `nfx-<run>-*` (RUN's only, or any harness run's)
    or a `marsh-xnfx01-*` VM of the test daemon. Other `nfx-` names belong to someone else."""
    if name.startswith(VM_PREFIX):
        return True
    return name.startswith(f"nfx-{run}-") if run else GENERIC_SANDBOX.match(name) is not None


def sbx_json(sbx: str, *argv: str, env: dict[str, str] | None = None, timeout: float = 60) -> Any:
    completed = subprocess.run([sbx, *argv], env=env, stdin=subprocess.DEVNULL, capture_output=True,
                               timeout=timeout, check=False)
    if completed.returncode != 0:
        raise RuntimeError(f"sbx {' '.join(argv)} failed ({completed.returncode}): "
                           f"{completed.stderr.decode(errors='replace').strip()[-300:]}")
    return json.loads(completed.stdout)


def policy_rules(sbx: str, env: dict[str, str] | None = None) -> dict[str, dict[str, Any]]:
    """`sbx policy ls --format json` rules by id, reduced to what identifies them."""
    document = sbx_json(sbx, "policy", "ls", "--format", "json", env=env)
    rules = {}
    for rule in document.get("rules", []):
        rules[rule["id"]] = {key: rule.get(key) for key in
                             ("id", "name", "scope", "applies_to", "resource_type", "decision", "resources")}
    return rules


def sandbox_of(rule: dict[str, Any]) -> str | None:
    applies = rule.get("applies_to") or ""
    return applies.removeprefix("sandbox:") if applies.startswith("sandbox:") else None


def policy_delta(before: dict[str, dict[str, Any]], after: dict[str, dict[str, Any]],
                 run: str) -> tuple[list[dict[str, Any]], list[str]]:
    """(rules this run must remove or restore, notes about foreign changes).

    Added rules that are global or scoped to one of our sandboxes are ours to
    remove; added rules scoped to someone else's sandbox (the user's own marsh
    working while we run) are reported, not failed. A baseline rule that
    disappeared is always a problem.
    """
    ours: list[dict[str, Any]] = []
    notes: list[str] = []
    for rule_id, rule in after.items():
        if rule_id in before:
            continue
        sandbox = sandbox_of(rule)
        if sandbox is not None and not is_nfx_name(sandbox, run):
            notes.append(f"foreign policy rule appeared for {sandbox}: {rule_id}")
        else:
            ours.append(rule)
    return ours, notes


def policy_errors(before: dict[str, dict[str, Any]], after: dict[str, dict[str, Any]], run: str,
                  tolerate_baseline_changes: bool = False) -> tuple[list[str], list[str]]:
    """(errors, notes): rules we left behind, and baseline rules that changed or vanished. A change to a rule scoped
    to someone else's sandbox is a note only when the caller tolerates baseline churn explicitly; otherwise the
    oracle cannot say the baseline held, which is an error (inconclusive)."""
    ours, notes = policy_delta(before, after, run)
    errors = [f"policy rule left behind: {rule['id']} ({rule.get('applies_to')} {rule.get('resource_type')} "
              f"{rule.get('decision')})" for rule in ours]
    for rule_id, rule in before.items():
        if after.get(rule_id) == rule:
            continue
        sandbox = sandbox_of(rule)
        message = f"baseline policy rule changed or removed: {rule_id}" + (f" (scoped to {sandbox})" if sandbox else "")
        if sandbox is not None and not is_nfx_name(sandbox, run) and tolerate_baseline_changes:
            notes.append(message + " [tolerated]")
        else:
            errors.append(message)
    return errors, notes


def remove_policy_rules(sbx: str, rules: list[dict[str, Any]], env: dict[str, str] | None = None) -> list[str]:
    errors = []
    for rule in rules:
        argv = [sbx, "policy", "rm", "network", "--force", "--id", rule["id"]]
        sandbox = sandbox_of(rule)
        if sandbox:
            argv += ["--sandbox", sandbox]
        completed = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=60,
                                   check=False)
        if completed.returncode != 0:
            errors.append(f"could not remove policy rule {rule['id']}: "
                          f"{completed.stderr.decode(errors='replace').strip()[-200:]}")
    return errors


def read_setting(sbx: str, key: str = DISK_SETTING, env: dict[str, str] | None = None) -> dict[str, Any]:
    document = sbx_json(sbx, "settings", "get", "--json", key, env=env)
    return {"key": key, "value": document["value"], "source": document["source"]}


def setting_errors(sbx: str, baseline: dict[str, Any], env: dict[str, str] | None = None) -> list[str]:
    """Revision 3.3, H-3: the global setting is only OBSERVED. S1 sizes each services VM per sandbox and never touches
    it, and neither does this harness: a change (by anyone) fails the run and is never repaired here."""
    current = read_setting(sbx, baseline["key"], env)
    if current != baseline:
        return [f"global sbx setting {baseline['key']} is {current['value']!r} ({current['source']}), "
                f"baseline was {baseline['value']!r} ({baseline['source']})"]
    return []


def nfx_names(inventory: dict[str, str], run: str | None = None) -> list[str]:
    return sorted(name for name in inventory if is_nfx_name(name, run))


def is_mine(name: str, run: str, owned: set[str] | frozenset[str]) -> bool:
    """A sandbox this run created: `nfx-<run>-*`, or a VM in this run's ownership map."""
    return name.startswith(f"nfx-{run}-") or name in owned


def foreign_names(inventory: dict[str, str], baseline: dict[str, str], run: str,
                  owned: set[str] | frozenset[str]) -> list[str]:
    """Resources with the harness prefixes that this run did not create. They are reported, never touched
    (`make services-e2e-gc` removes them once their run is dead)."""
    return sorted(name for name in inventory if is_nfx_name(name) and not is_mine(name, run, owned))


def inventory_errors(before: dict[str, str], after: dict[str, str], run: str,
                     owned: set[str] | frozenset[str] = frozenset()) -> list[str]:
    """None of this run's sandboxes remain. Foreign nfx/marsh-xnfx01 resources and sandboxes other people's tools
    create or remove while the run is going are not this run's to judge (see inventory_changes)."""
    leaked = sorted(name for name in after if name not in before and is_mine(name, run, owned))
    return [f"this run's sandboxes remain: {leaked}"] if leaked else []


def inventory_changes(before: dict[str, str], after: dict[str, str], removed_by_us: set[str] | frozenset[str]
                      = frozenset(), tolerate: bool = False) -> tuple[list[str], list[str]]:
    """(errors, notes) about sandboxes that existed before the run: baseline equality. The product, the inherited
    cleanup or any other call could remove or replace one without telling this harness, so a change cannot be
    assumed to be someone else's: it is an error (the oracle is inconclusive) unless the caller tolerates baseline
    churn explicitly, and then it is a note recorded as tolerated. A removal this harness issued is always an error."""
    errors, notes = [], []
    gone = sorted(set(before) - set(after))
    replaced = sorted(n for n in before.keys() & after.keys() if before[n] != after[n])
    ours = sorted(n for n in gone + replaced if n in removed_by_us)
    if ours:
        errors.append(f"this harness removed pre-existing sandboxes: {ours}")
    for label, names in (("disappeared", gone), ("were replaced", replaced)):
        names = [n for n in names if n not in ours]
        if names:
            message = f"pre-existing sandboxes {label} during the run, ownership unproven: {names}"
            if tolerate:
                notes.append(message + " [tolerated]")
            else:
                errors.append(message + " (rerun with --tolerate-baseline-changes to accept foreign churn)")
    return errors, notes


def unattributed_prefixed(before: dict[str, str], after: dict[str, str], run: str,
                          owned: set[str] | frozenset[str],
                          other_claims: dict[str, str | None] | None = None) -> list[str]:
    """New `marsh-xnfx01-*` VMs that are neither in this run's ownership map nor a foreign VM whose recorded owner
    and stable ID prove it foreign. OTHER_CLAIMS maps names other runs' records list to the ID they recorded: a new VM
    under such a name counts as foreign only if its live ID equals the recorded one. A same-name VM with another ID, a
    claim without an ID, and a VM nobody claims are all errors (ownership is not provable)."""
    claims = other_claims or {}
    return sorted(n for n in after if n not in before and n.startswith(VM_PREFIX) and n not in owned
                  and not (claims.get(n) is not None and claims[n] == after[n]))


def parse_policy_request(argv: tuple[str, ...]) -> dict[str, Any]:
    """What a `sbx policy allow|deny network [--sandbox S] RESOURCES...` call asks for: decision, scope, resources."""
    if len(argv) < 3 or argv[0] not in ("allow", "deny") or argv[1] != "network":
        raise ValueError(f"PolicyRecorder only records `allow|deny network` rules, not {argv[:2]}")
    sandbox, resources, skip = None, [], False
    rest = list(argv[2:])
    for index, word in enumerate(rest):
        if skip:
            skip = False
        elif word == "--sandbox":
            sandbox = rest[index + 1]
            skip = True
        elif word.startswith("--sandbox="):
            sandbox = word.split("=", 1)[1]
        elif word.startswith("--"):
            skip = word in ("--protocol", "--method", "--path", "--query")
        else:
            resources += [r for r in word.split(",") if r]
    return {"decision": argv[0], "applies_to": f"sandbox:{sandbox}" if sandbox else "all",
            "resources": sorted(resources)}


def attribute_policy_rules(request: dict[str, Any], new_rules: dict[str, dict[str, Any]]) -> list[str]:
    """The IDs among NEW_RULES that this request created: same decision, scope and resources. A concurrent rule that
    differs in any of them is not ours. Two indistinguishable candidates are ambiguous and raise."""
    matching = [rule_id for rule_id, rule in new_rules.items()
                if rule.get("decision") == request["decision"] and rule.get("applies_to") == request["applies_to"]
                and sorted(rule.get("resources") or []) == request["resources"]]
    if len(matching) > 1:
        raise ValueError(f"cannot attribute the new rule: {len(matching)} indistinguishable concurrent rules "
                         f"{sorted(matching)}")
    return matching


class PolicyRecorder:
    """The policy rules this run added through the harness, attributed to the operation by exact attributes
    (decision, scope, resources), never by a before/after diff alone: a concurrent unrelated addition is not
    recorded and so is never deleted. Rollback removes exactly the recorded IDs."""

    def __init__(self, sbx: str, env: dict[str, str] | None = None) -> None:
        self.sbx = sbx
        self.env = env
        self.rules: dict[str, dict[str, Any]] = {}

    def add(self, *argv: str, timeout: float = 60) -> subprocess.CompletedProcess[bytes]:
        """`sbx policy allow|deny network ...`; the created rule's ID is recorded."""
        request = parse_policy_request(argv)
        before = policy_rules(self.sbx, self.env)
        done = subprocess.run([self.sbx, "policy", *argv], env=self.env, stdin=subprocess.DEVNULL,
                              capture_output=True, timeout=timeout, check=False)
        if done.returncode == 0:
            after = policy_rules(self.sbx, self.env)
            for rule_id in attribute_policy_rules(request, {i: r for i, r in after.items() if i not in before}):
                self.rules[rule_id] = after[rule_id]
        return done

    def rollback(self) -> list[str]:
        present = policy_rules(self.sbx, self.env)
        errors = remove_policy_rules(self.sbx, [r for i, r in self.rules.items() if i in present], self.env)
        self.rules = {i: r for i, r in self.rules.items() if i in present and errors}
        return errors


REMOVED: set[str] = set()  # every name this process removed, so a baseline change can be blamed on us


def remove_sandbox(sbx: str, name: str, identifier: str | None, baseline: dict[str, str],
                   env: dict[str, str] | None = None) -> None:
    """Remove one nfx sandbox, refusing anything that was in the baseline, anything without a recorded stable ID,
    and anything whose current ID is not the recorded one (an impostor under an owned name)."""
    if not identifier:
        raise ValueError(f"refusing to remove {name}: no recorded stable ID proves it is ours")
    if name in baseline or identifier in baseline.values():
        raise ValueError(f"refusing to remove pre-existing sandbox {name}")
    if not is_nfx_name(name):
        raise ValueError(f"refusing to remove {name}: not an nfx sandbox")
    current = stock_vm_inventory(sbx, env=env)
    if name not in current:
        return
    if current[name] != identifier:
        raise ValueError(f"{name} was replaced (id {current[name]}, recorded {identifier}); refusing removal")
    REMOVED.add(name)
    completed = subprocess.run([sbx, "rm", "--force", name], env=env, stdin=subprocess.DEVNULL,
                               capture_output=True, timeout=120, check=False)
    if completed.returncode != 0 and name in stock_vm_inventory(sbx, env=env):
        raise ValueError(f"sbx rm {name} failed: {completed.stderr.decode(errors='replace').strip()[-200:]}")


def running_sandboxes(sbx: str, env: dict[str, str] | None = None) -> int:
    """How many sandboxes are running now (contention that timing evidence must be read against)."""
    try:
        listed = sbx_json(sbx, "ls", "--json", env=env)
    except (RuntimeError, ValueError, OSError):
        return -1
    rows = listed if isinstance(listed, list) else listed.get("sandboxes", [])
    return sum(1 for row in rows if isinstance(row, dict) and row.get("status") == "running")


def host_facts(sbx: str, env: dict[str, str] | None = None) -> dict[str, Any]:
    """A.3: sbx version, macOS version, RAM and free disk, recorded at start and end."""
    def out(argv: list[str]) -> str:
        completed = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=30,
                                   check=False)
        return completed.stdout.decode(errors="replace").strip()

    import shutil
    sbx_version = out([sbx, "version", "--json"]) or out([sbx, "version"])
    try:
        sbx_version = json.loads(sbx_version)
    except ValueError:
        pass
    usage = shutil.disk_usage("/private/tmp")
    import os
    load = os.getloadavg()
    return {"load": {"load1": round(load[0], 2), "load5": round(load[1], 2), "load15": round(load[2], 2),
                     "cpus": os.cpu_count() or 0, "running_sandboxes": running_sandboxes(sbx, env)},
            "sbx_version": sbx_version, "macos": out(["sw_vers", "-productVersion"]),
            "ram_bytes": int(out(["sysctl", "-n", "hw.memsize"]) or 0), "free_disk_bytes": usage.free}


RUN_ID = re.compile(r"^[0-9]{12}[0-9a-f]{4}$")


# ---- controlled egress (H-12, DECISIONS 17) ---------------------------------------------------------------
def egress_settings(environment: dict[str, str] | None = None) -> dict[str, str]:
    """`EGRESS_URL`, `EGRESS_HOST`, `EGRESS_DNS_HOST` and `EGRESS_ECHO_URL` (the header echo), with the DECISIONS 17 defaults."""
    import os
    source = os.environ if environment is None else environment
    return {"url": source.get("EGRESS_URL", "https://example.com/"),
            "host": source.get("EGRESS_HOST", "example.com"),
            "dns_host": source.get("EGRESS_DNS_HOST", "example.com"),
            "echo_url": source.get("EGRESS_ECHO_URL", "https://httpbin.org/headers")}


def egress_preflight(settings: dict[str, str] | None = None, timeout: float = 20) -> list[str]:
    """From the Mac, before a scenario that needs the Internet relies on it: the reachability URL answers, the DNS
    host resolves, and the echo URL answers. Returns the failures (empty when all hold)."""
    import socket
    import urllib.request
    settings = settings or egress_settings()
    failures = []
    try:
        socket.getaddrinfo(settings["dns_host"], 443)
    except OSError as error:
        failures.append(f"{settings['dns_host']} does not resolve: {error}")
    for label in ("url", "echo_url"):
        try:
            with urllib.request.urlopen(settings[label], timeout=timeout) as response:
                if response.status >= 400:
                    failures.append(f"{settings[label]} answered {response.status}")
        except OSError as error:
            failures.append(f"{settings[label]} is unreachable from the Mac: {error}")
    return failures


def baseline_oracle(before_inventory: dict[str, str], after_inventory: dict[str, str], run: str,
                    owned: set[str] | frozenset[str], other_claims: dict[str, str | None] | None,
                    removed_by_us: set[str] | frozenset[str], before_rules: dict[str, dict[str, Any]],
                    after_rules: dict[str, dict[str, Any]], tolerate: bool = False
                    ) -> tuple[list[str], list[str]]:
    """The H-3 verification as one pure function: (errors, notes). No sandbox of this run remains; no new product VM
    is unattributed (a name another run's record lists is foreign only with the ID it recorded); every baseline
    sandbox is unchanged; no policy rule of ours is left and no baseline rule changed. Baseline changes are errors
    (the oracle is inconclusive) unless TOLERATE says to accept foreign churn, and then every note carries
    `[tolerated]`: a result with tolerated changes is non-qualifying, it did not prove baseline equality."""
    errors = inventory_errors(before_inventory, after_inventory, run, owned)
    unattributed = unattributed_prefixed(before_inventory, after_inventory, run, owned, other_claims)
    if unattributed:
        errors.append(f"new product VMs in nobody's ownership map (not provably foreign): {unattributed}")
    changed, notes = inventory_changes(before_inventory, after_inventory, removed_by_us, tolerate)
    policy, policy_notes = policy_errors(before_rules, after_rules, run, tolerate)
    return errors + changed + policy, notes + policy_notes
