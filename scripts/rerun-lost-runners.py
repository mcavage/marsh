#!/usr/bin/env python3
"""Re-run the failed jobs of a CI or Release run that failed only because no
hosted runner took one of its jobs.

When GitHub has no macOS arm64 capacity, a queued job is cancelled about 15
minutes after it was created, with no runner (runner_name empty) and the
annotation "The job was not acquired by Runner of type hosted even after
multiple attempts". The run then fails (`CI passed` fails on the cancelled
lane, or release.yml's later jobs are skipped) although nothing in it failed.

This re-runs such a run's failed jobs (`gh run rerun --failed`), waits for the
new attempt and checks again, until the run attempt reaches --max-attempts.
It re-runs nothing when any job failed or was cancelled for another reason
(a user's cancel, a timeout), other than the `CI passed` gate, which fails
whenever a lane it needs was cancelled. Dry run unless --apply.

GH_TOKEN needs actions: write (the re-run) and checks: read (annotations).
Exits 1 if an API call failed.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

NOT_ACQUIRED = "was not acquired by Runner of type hosted"
GATES = frozenset({"CI passed", "ci / CI passed"})
FINE = frozenset({"success", "skipped", "neutral"})


class ApiError(Exception):
    pass


class GitHub:
    def __init__(self, api: str, repo: str, token: str | None):
        self.base = f"{api.rstrip('/')}/repos/{repo}"
        self.token = token

    def call(self, method: str, path: str):
        request = urllib.request.Request(self.base + path, method=method)
        request.add_header("Accept", "application/vnd.github+json")
        request.add_header("User-Agent", "marsh-rerun-lost-runners")
        if self.token:
            request.add_header("Authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                raw = response.read()
        except (urllib.error.URLError, TimeoutError) as error:
            raise ApiError(f"{method} {path}: {error}") from error
        return json.loads(raw) if raw else None

    def run(self, run_id: int) -> dict:
        return self.call("GET", f"/actions/runs/{run_id}")

    def jobs(self, run_id: int, attempt: int) -> list[dict]:
        jobs: list[dict] = []
        page = 1
        while True:
            body = self.call("GET", f"/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100&page={page}")
            jobs += body["jobs"]
            if not body["jobs"] or len(jobs) >= body["total_count"]:
                return jobs
            page += 1

    def annotations(self, job_id: int) -> list[str]:
        return [a.get("message") or "" for a in self.call("GET", f"/check-runs/{job_id}/annotations?per_page=100")]

    def rerun_failed(self, run_id: int) -> None:
        self.call("POST", f"/actions/runs/{run_id}/rerun-failed-jobs")


def decide(jobs: list[dict], annotations) -> tuple[bool, str]:
    """Whether a failed attempt's jobs show only lost runners, and why.

    annotations(job_id) lists a job's annotation messages; it is asked only
    about cancelled jobs that never had a runner.
    """
    lost = []
    for job in jobs:
        conclusion = job.get("conclusion")
        if conclusion in FINE:
            continue
        if (conclusion == "cancelled" and not job.get("runner_name")
                and any(NOT_ACQUIRED in message for message in annotations(job["id"]))):
            lost.append(job["name"])
            continue
        if conclusion == "failure" and job["name"] in GATES:
            continue
        return False, f"not re-run: {job['name']} ended {conclusion}"
    if not lost:
        return False, "not re-run: no job lost its runner"
    return True, "no hosted runner took: " + ", ".join(lost)


def watch(hub: GitHub, run_id: int, attempt: int, poll: float) -> dict:
    """The run once its attempt `attempt` (or a later one) has completed."""
    while True:
        run = hub.run(run_id)
        if run["run_attempt"] >= attempt and run["status"] == "completed":
            return run
        time.sleep(poll)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--repo", required=True)
    parser.add_argument("--run", type=int, required=True)
    parser.add_argument("--attempt", type=int, required=True, help="the attempt that completed")
    parser.add_argument("--max-attempts", type=int, default=3)
    parser.add_argument("--poll", type=float, default=60)
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args(argv)
    hub = GitHub(os.environ.get("GITHUB_API_URL", "https://api.github.com"), args.repo, os.environ.get("GH_TOKEN"))
    attempt = args.attempt
    try:
        run = hub.run(args.run)
        while True:
            label = f"run {args.run} attempt {attempt}"
            if run["run_attempt"] != attempt:
                print(f"{label}: attempt {run['run_attempt']} is newer; leaving it")
                return 0
            if run["status"] != "completed" or run["conclusion"] != "failure":
                print(f"{label}: {run['status']} {run.get('conclusion')}; nothing to do")
                return 0
            rerun, reason = decide(hub.jobs(args.run, attempt), hub.annotations)
            print(f"{label}: {reason}")
            if not rerun:
                return 0
            if attempt >= args.max_attempts:
                print(f"::warning::{label}: no hosted runner after {attempt} attempts; re-run it by hand")
                return 0
            if not args.apply:
                print(f"{label}: would re-run its failed jobs (dry run)")
                return 0
            hub.rerun_failed(args.run)
            attempt += 1
            print(f"run {args.run}: re-ran its failed jobs as attempt {attempt}; waiting for it")
            run = watch(hub, args.run, attempt, args.poll)
    except (ApiError, KeyError, ValueError) as error:
        print(f"rerun-lost-runners: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
