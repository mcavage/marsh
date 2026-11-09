"""Delta 19: the independent checkpoint manifest `M(C)` and `truth(fork, M, lists)` (test plan §C conventions, r3.2).

At each checkpoint the harness records `M(C)` from the fixture's own declarations plus its own pre-capture
sentinel reads, never from the product's lists:

- **restorable**: the fixture's declared services whose data sits in named volumes, with the harness's pre-capture
  sentinels. An **immutable expected set**: nothing is ever subtracted from it to fit a result;
- **volume_only**: standalone volumes (`notes` used by `keeper`): their data is expected back under a reported name
  and their containers are not expected to restart;
- **missing**: expected volumes the harness observed absent from the snapshot (via `vsnap()`) or removed by its own
  injected fault, recorded separately; they never shrink the expected sets;
- **fresh**: services declared to keep no saved data, each with a pre-capture sentinel; truth needs a *positive* read
  showing it absent (a missing key or a failed read is not absence);
- **unsupported**: blockers, Testcontainers and standalone containers (the containers, not their volumes).

A recorded manifest is deeply immutable (read-only mappings, tuples, frozensets): a scenario cannot adjust the
expected values after recording them.
"""

from __future__ import annotations

import dataclasses
from types import MappingProxyType
from typing import Any, Callable, Iterable, Mapping

Sentinels = Mapping[str, Any]
# A comparator for sentinels that are bounds, not equalities (`LOAD=1`: tokens only grow, files lag by at most one).
Comparator = Callable[[Sentinels, Sentinels], bool]


def freeze(value: Any) -> Any:
    """A deep read-only copy: dict -> read-only mapping, list/set -> tuple/frozenset."""
    if isinstance(value, Mapping):
        return MappingProxyType({key: freeze(item) for key, item in value.items()})
    if isinstance(value, (list, tuple)):
        return tuple(freeze(item) for item in value)
    if isinstance(value, (set, frozenset)):
        return frozenset(freeze(item) for item in value)
    return value


def thaw(value: Any) -> Any:
    if isinstance(value, Mapping):
        return {key: thaw(item) for key, item in value.items()}
    if isinstance(value, (tuple, list)):
        return [thaw(item) for item in value]
    if isinstance(value, (set, frozenset)):
        return sorted(thaw(item) for item in value)
    return value


@dataclasses.dataclass(frozen=True)
class Manifest:
    restorable: Mapping[str, Sentinels]
    fresh: Mapping[str, Sentinels]
    volume_only: Mapping[str, Mapping[str, Any]] = dataclasses.field(default_factory=lambda: MappingProxyType({}))
    unsupported: frozenset[str] = frozenset()
    missing: frozenset[str] = frozenset()

    @classmethod
    def record(cls, declared_restorable: Mapping[str, Sentinels], declared_fresh: Mapping[str, Sentinels],
               unsupported: Iterable[str] = (), observed_missing: Iterable[str] = (),
               volume_only: Mapping[str, Mapping[str, Any]] | None = None) -> "Manifest":
        """M(C). VOLUME_ONLY maps an item to `{"container": name, "reported": name, "sentinels": {...}}`.
        OBSERVED_MISSING names expected services or volume-only items the harness saw absent from the snapshot;
        they are recorded next to, never removed from, the expected sets."""
        return cls(freeze(declared_restorable), freeze(declared_fresh), freeze(volume_only or {}),
                   frozenset(unsupported), frozenset(observed_missing))

    def to_json(self) -> dict[str, Any]:
        return {"restorable": thaw(self.restorable), "fresh": thaw(self.fresh), "volume_only": thaw(self.volume_only),
                "unsupported": sorted(self.unsupported), "missing": sorted(self.missing)}


@dataclasses.dataclass(frozen=True)
class ForkService:
    """What the observer found for one service container in the fork's scope."""
    running: bool
    healthy: bool
    sentinels: Sentinels = dataclasses.field(default_factory=dict)


@dataclasses.dataclass(frozen=True)
class ForkVolume:
    """What the observer found for one volume in the fork's scope: present, and its sentinels."""
    present: bool
    sentinels: Sentinels = dataclasses.field(default_factory=dict)


@dataclasses.dataclass(frozen=True)
class ProductLists:
    """The `restore.<label>` lists the product reported (never trusted, only compared). Always required: truth
    compares them to M (rule 5)."""
    restored: frozenset[str]
    fresh: frozenset[str]
    gaps: frozenset[str]
    reported_names: Mapping[str, str] = dataclasses.field(default_factory=dict)


@dataclasses.dataclass(frozen=True)
class Truth:
    ok: bool
    reasons: tuple[str, ...]


def normalize(value: Any) -> Any:
    """A canonical form for comparison: read-only mappings and dicts compare equal, tuples and lists compare equal,
    sets compare as sorted lists (a frozen tuple in M must equal the list an observer parsed from JSON)."""
    if isinstance(value, Mapping):
        return {key: normalize(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [normalize(item) for item in value]
    if isinstance(value, (set, frozenset)):
        return sorted((normalize(item) for item in value), key=repr)
    return value


def equal(actual: Sentinels, expected: Sentinels) -> bool:
    return normalize(actual) == normalize(expected)


MISSING = object()


def is_error(observed: Any) -> bool:
    """An observation that failed (a read that errored) rather than reported a value."""
    return isinstance(observed, Mapping) and ("error" in observed and len(observed) <= 2)


def truth(fork: Mapping[str, ForkService], manifest: Manifest, lists: ProductLists, *,
          volumes: Mapping[str, ForkVolume] | None = None, started_with_data: Iterable[str] = (),
          compare: Mapping[str, Comparator] | None = None) -> Truth:
    """True iff all six rules hold. FORK is the observed containers by service, VOLUMES the observed fork volumes
    by reported name, STARTED_WITH_DATA the unsupported items the observer saw started with data."""
    reasons: list[str] = []
    compare, volumes = compare or {}, volumes or {}
    for name, expected in manifest.restorable.items():                                                    # 1
        found = fork.get(name)
        if found is None:
            reasons.append(f"restorable {name} is absent from the fork")
        elif not (found.running and found.healthy):
            reasons.append(f"restorable {name} is not running and healthy")
        elif not compare.get(name, equal)(found.sentinels, expected):
            reasons.append(f"restorable {name} has sentinels {thaw(found.sentinels)}, expected {thaw(expected)}")
    for item, spec in manifest.volume_only.items():                                                       # 2
        reported = lists.reported_names.get(item) or spec.get("reported")
        volume = volumes.get(reported)
        if volume is None or not volume.present:
            reasons.append(f"volume-only {item}: its data is not present under the reported name {reported!r}")
        elif not equal(volume.sentinels, spec.get("sentinels", {})):
            reasons.append(f"volume-only {item}: sentinels {thaw(volume.sentinels)}, expected "
                           f"{thaw(spec.get('sentinels', {}))}")
        container = fork.get(spec.get("container", ""))
        if container is not None and container.running:
            reasons.append(f"volume-only {item}: its container {spec.get('container')} was started")
    for name, before in manifest.fresh.items():                                                           # 3
        found = fork.get(name)
        if found is None or not found.running:
            reasons.append(f"fresh {name} is not running")
            continue
        for key, value in before.items():
            observed = found.sentinels.get(key, MISSING)
            if observed is MISSING or is_error(observed):
                reasons.append(f"fresh {name}: no positive observation that sentinel {key!r} is absent "
                               f"({'not read' if observed is MISSING else 'the read failed'})")
            elif normalize(observed) == normalize(value):
                reasons.append(f"fresh {name} still holds its pre-capture sentinel {key}={normalize(value)!r}")
    for name in sorted(set(started_with_data) & manifest.unsupported):                                    # 4
        reasons.append(f"unsupported {name} was started with data")
    if set(lists.restored) != set(manifest.restorable):                                                   # 5
        reasons.append(f"restored {sorted(lists.restored)} != M.restorable {sorted(manifest.restorable)}")
    if not set(manifest.fresh) <= set(lists.fresh):
        reasons.append(f"fresh {sorted(lists.fresh)} lacks {sorted(set(manifest.fresh) - set(lists.fresh))}")
    unreported = (manifest.unsupported | manifest.missing) - set(lists.gaps)
    if unreported:
        reasons.append(f"gaps {sorted(lists.gaps)} lack {sorted(unreported)}")
    if manifest.missing:                                                                                  # 6
        reasons.append(f"observed missing captures: {sorted(manifest.missing)}")
    return Truth(not reasons, tuple(reasons))


def ok_agrees(product_ok: object, verdict: Truth) -> str | None:
    """The `ok` contract: the product's `ok` must equal `truth`. Returns the disagreement, if any."""
    if product_ok is verdict.ok:
        return None
    return (f"the product reported ok={product_ok!r} but truth is {verdict.ok}"
            + (f" ({'; '.join(verdict.reasons)})" if verdict.reasons else ""))
