#!/usr/bin/env python3
"""Summarize benchmark results (DESIGN.md, "Analysis").

    report.py results/run1 [--baseline a_claude] [--json]

Pass rates on hidden tests with task-clustered bootstrap 95% CIs, paired
differences against the baseline arm (cluster bootstrap and a task-level
sign-flip permutation test; Holm across the three primary comparisons),
gate/hidden agreement, wall time, and cost.
Standard library only; the bootstrap is seeded, so reports are reproducible.
"""
from __future__ import annotations

import argparse
import json
import math
import pathlib
import random
import statistics
import sys
from collections import defaultdict
from typing import Any

ARMS = ("a_claude", "a_codex", "b_retry", "c_race", "d_judge", "e_fallback")
RESAMPLES = 10000
# The three comparisons fixed in DESIGN.md before any run.
PRIMARY = (("c_race", "a_claude"), ("b_retry", "a_claude"), ("d_judge", "c_race"))


def load(directory: pathlib.Path) -> list[dict[str, Any]]:
    return [json.loads(p.read_text()) for p in sorted(directory.glob("*.rep*.json"))]


def by_task(records: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for record in records:
        grouped[record["task"]].append(record)
    return dict(grouped)


def cluster_bootstrap(groups: list[list[float]], seed: int = 1) -> tuple[float, float, float]:
    """Mean over all observations and a percentile 95% CI resampling whole
    clusters (tasks), so repetitions of one task are not counted as independent."""
    flat = [x for g in groups for x in g]
    if not flat:
        return (math.nan, math.nan, math.nan)
    rng = random.Random(seed)
    means = []
    for _ in range(RESAMPLES):
        sample = [x for _ in groups for x in rng.choice(groups)]
        means.append(sum(sample) / len(sample))
    means.sort()
    return (sum(flat) / len(flat), means[int(0.025 * RESAMPLES)], means[int(0.975 * RESAMPLES) - 1])


def mcnemar_exact(b: int, c: int) -> float:
    """Two-sided exact McNemar p-value from discordant counts b and c."""
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    tail = sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n
    return min(1.0, 2 * tail)


def paired(records: list[dict[str, Any]], tasks: dict[str, list[dict[str, Any]]], arm: str,
           base: str) -> dict[str, Any]:
    groups = [[float(r["arms"][arm]["hidden_pass"]) - float(r["arms"][base]["hidden_pass"]) for r in rs]
              for rs in tasks.values()]
    mean, lo, hi = cluster_bootstrap(groups, seed=2)
    b = sum(1 for r in records if r["arms"][arm]["hidden_pass"] and not r["arms"][base]["hidden_pass"])
    c = sum(1 for r in records if r["arms"][base]["hidden_pass"] and not r["arms"][arm]["hidden_pass"])
    task_means = [sum(g) / len(g) for g in groups if g]
    return {"diff": mean, "ci95": [lo, hi], "wins": b, "losses": c, "sign_flip_p": sign_flip_p(task_means),
            "mcnemar_p": mcnemar_exact(b, c),
            "tasks_better": sum(1 for d in task_means if d > 0), "tasks_worse": sum(1 for d in task_means if d < 0)}


def sign_flip_p(diffs: list[float], seed: int = 3) -> float:
    """Two-sided paired permutation p-value on task-level mean differences:
    under no effect each task's difference is equally likely to have either
    sign. Exact up to 20 tasks, else 20,000 seeded random flips. Tasks are
    the unit, so repetitions within a task are not treated as independent."""
    observed = abs(sum(diffs))
    if not diffs or observed == 0:
        return 1.0
    if len(diffs) <= 20:
        total = 1 << len(diffs)
        hits = sum(1 for mask in range(total)
                   if abs(sum(d if mask >> i & 1 else -d for i, d in enumerate(diffs))) >= observed - 1e-12)
        return hits / total
    rng = random.Random(seed)
    hits = sum(1 for _ in range(20000) if abs(sum(d if rng.random() < 0.5 else -d for d in diffs)) >= observed - 1e-12)
    return (hits + 1) / 20001


def holm(pvalues: list[float]) -> list[float]:
    """Holm step-down adjusted p-values, in the input order."""
    order = sorted(range(len(pvalues)), key=lambda i: pvalues[i])
    adjusted, running = [0.0] * len(pvalues), 0.0
    for rank, index in enumerate(order):
        running = max(running, min(1.0, (len(pvalues) - rank) * pvalues[index]))
        adjusted[index] = running
    return adjusted


def summarize(records: list[dict[str, Any]], baseline: str) -> dict[str, Any]:
    tasks = by_task(records)
    out: dict[str, Any] = {"runs": len(records), "tasks": len(tasks),
                           "reps": sorted({r["rep"] for r in records}), "arms": {}, "paired": {}}
    for arm in ARMS:
        groups = [[float(r["arms"][arm]["hidden_pass"]) for r in rs] for rs in tasks.values()]
        mean, lo, hi = cluster_bootstrap(groups)
        walls = [r["arms"][arm]["wall_s"] for r in records if r["arms"][arm]["wall_s"] is not None]
        costs = [r["arms"][arm]["cost_usd"] for r in records]
        passes = sum(r["arms"][arm]["hidden_pass"] for r in records)
        gate = [r["arms"][arm] for r in records]
        gate_pass = [a for a in gate if a["gate_pass"]]
        out["arms"][arm] = {
            "hidden_pass_rate": mean, "ci95": [lo, hi],
            "gate_pass_rate": len(gate_pass) / len(gate) if gate else math.nan,
            "hidden_given_gate": (sum(a["hidden_pass"] for a in gate_pass) / len(gate_pass)) if gate_pass else None,
            "wall_median_s": statistics.median(walls) if walls else None,
            "cost_mean_usd": statistics.fmean(costs) if costs else None,
            "cost_per_hidden_pass_usd": (sum(costs) / passes) if passes else None,
        }
        if arm == "c_race":
            firsts = [r["arms"][arm].get("wall_first_pass_s") for r in records]
            firsts = [f for f in firsts if f is not None]
            out["arms"][arm]["wall_first_pass_median_s"] = statistics.median(firsts) if firsts else None
    for arm in ARMS:
        if arm != baseline:
            out["paired"][arm] = paired(records, tasks, arm, baseline)
    primary = {f"{arm} - {base}": paired(records, tasks, arm, base) for arm, base in PRIMARY}
    for name, adjusted in zip(primary, holm([c["sign_flip_p"] for c in primary.values()])):
        primary[name]["holm_p"] = adjusted
    out["primary"] = primary
    judged = [r for r in records if r["judge"].get("invoked")]
    out["judge"] = {
        "invoked": len(judged),
        "unparseable": sum(1 for r in judged if r["judge"].get("pick") is None),
        # The judge is blind (branches a/b), but may still favour a style.
        "picked_claude": sum(1 for r in judged if r["judge"].get("pick") and r["arms"]["d_judge"]["label"] == "claude"),
        "picked_codex": sum(1 for r in judged if r["judge"].get("pick") and r["arms"]["d_judge"]["label"] == "codex"),
        # Runs where both race patches passed the gate but exactly one passed
        # the hidden tests: the only runs where the pick can matter.
        "decisive": sum(1 for r in judged
                        if r["arms"]["a_claude"]["hidden_pass"] != r["arms"]["a_codex"]["hidden_pass"]),
        "decisive_correct": sum(1 for r in judged
                                if r["arms"]["a_claude"]["hidden_pass"] != r["arms"]["a_codex"]["hidden_pass"]
                                and r["arms"]["d_judge"]["hidden_pass"]),
    }
    out["spend_usd"] = sum(sum(b["cost_usd"] for b in r["branches"].values()) + r["judge"].get("cost_usd", 0)
                           for r in records)
    out["touched_tests"] = sum(1 for r in records for b in r["branches"].values() if b["touched_tests"])
    return out


def fmt(value: Any, spec: str = ".2f") -> str:
    return "—" if value is None or (isinstance(value, float) and math.isnan(value)) else format(value, spec)


def markdown(summary: dict[str, Any], baseline: str) -> str:
    lines = [f"# Benchmark report: {summary['runs']} runs, {summary['tasks']} tasks, reps {summary['reps']}", "",
             "| arm | hidden pass | 95% CI (task bootstrap) | gate pass | P(hidden \\| gate) | median wall s | mean $ | $ / hidden pass |",
             "|---|---|---|---|---|---|---|---|"]
    for arm, s in summary["arms"].items():
        lines.append(f"| {arm} | {fmt(s['hidden_pass_rate'])} | [{fmt(s['ci95'][0])}, {fmt(s['ci95'][1])}] | "
                     f"{fmt(s['gate_pass_rate'])} | {fmt(s['hidden_given_gate'])} | {fmt(s['wall_median_s'], '.0f')} | "
                     f"{fmt(s['cost_mean_usd'])} | {fmt(s['cost_per_hidden_pass_usd'])} |")
    race = summary["arms"]["c_race"]
    lines += ["", f"c_race median time to first passing branch (if the loser were cancelled): "
              f"{fmt(race.get('wall_first_pass_median_s'), '.0f')} s", "",
              f"## Paired against {baseline}", "",
              "| arm | diff | 95% CI | run wins / losses | tasks better / worse | sign-flip p |",
              "|---|---|---|---|---|---|"]
    for arm, p in summary["paired"].items():
        lines.append(f"| {arm} | {fmt(p['diff'], '+.2f')} | [{fmt(p['ci95'][0], '+.2f')}, {fmt(p['ci95'][1], '+.2f')}] "
                     f"| {p['wins']} / {p['losses']} | {p['tasks_better']} / {p['tasks_worse']} "
                     f"| {fmt(p['sign_flip_p'], '.3f')} |")
    lines += ["", "## Primary comparisons (fixed in advance; Holm-adjusted)", "",
              "| comparison | diff | 95% CI | tasks better / worse | sign-flip p | Holm p |", "|---|---|---|---|---|---|"]
    for name, p in summary["primary"].items():
        lines.append(f"| {name} | {fmt(p['diff'], '+.2f')} | [{fmt(p['ci95'][0], '+.2f')}, {fmt(p['ci95'][1], '+.2f')}] "
                     f"| {p['tasks_better']} / {p['tasks_worse']} | {fmt(p['sign_flip_p'], '.3f')} "
                     f"| {fmt(p['holm_p'], '.3f')} |")
    j = summary["judge"]
    lines += ["", "p-values are task-level paired sign-flip tests; with few tasks, read the CIs and the "
              "task counts, not significance.", "",
              f"Judge: invoked {j['invoked']}, unparseable {j['unparseable']}, picked Claude {j['picked_claude']}, "
              f"picked Codex {j['picked_codex']}, decisive {j['decisive']}, "
              f"decisive and correct {j['decisive_correct']}.", "",
              f"Branches that touched test files: {summary['touched_tests']}. Spend: ${summary['spend_usd']:.2f}.", ""]
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("results", type=pathlib.Path)
    parser.add_argument("--baseline", default="a_claude", choices=ARMS)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    records = load(args.results)
    if not records:
        print(f"no results in {args.results}", file=sys.stderr)
        return 1
    summary = summarize(records, args.baseline)
    print(json.dumps(summary, indent=2) if args.json else markdown(summary, args.baseline))
    return 0


if __name__ == "__main__":
    sys.exit(main())
