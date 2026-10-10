#!/usr/bin/env python3
"""Delete old nightly image tags from Docker Hub.

Release CI tags every nightly build X.Y.Z-nightly.YYYYMMDDHHMMSS.gSHA in each
marsh-* repository (and moves :nightly). This keeps the newest --keep of those
per repository and deletes the rest. Only tags of exactly that shape are ever
touched: never :nightly, :latest, :release, :repaired or a stable X.Y.Z.
Consumers pin images by digest, so this only forgets tags; builds that ship in
a kept GitHub nightly release keep their tags.

Dry run unless --apply. With --apply, DOCKERHUB_USERNAME and DOCKERHUB_TOKEN
(an access token with delete permission) are required. Exits 1 if any listing
or deletion failed, after trying everything else.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

NIGHTLY = re.compile(r"\d+\.\d+\.\d+-nightly\.(\d{14})\.g[0-9a-f]{7}")


class Hub:
    def __init__(self, api: str, token: str | None):
        self.api = api.rstrip("/")
        self.token = token

    def call(self, method: str, path: str, body: dict | None = None, auth: bool = True):
        data = json.dumps(body).encode() if body is not None else None
        for attempt in range(4):
            request = urllib.request.Request(self.api + path, data=data, method=method)
            request.add_header("Accept", "application/json")
            request.add_header("User-Agent", "marsh-prune-nightlies")
            if data is not None:
                request.add_header("Content-Type", "application/json")
            if auth and self.token:
                request.add_header("Authorization", f"Bearer {self.token}")
            try:
                with urllib.request.urlopen(request, timeout=30) as response:
                    raw = response.read()
                    return json.loads(raw) if raw else None
            except urllib.error.HTTPError as error:
                if error.code == 429 and attempt < 3:
                    time.sleep(min(int(error.headers.get("Retry-After", "5")), 60))
                    continue
                raise
        raise AssertionError("unreachable")

    def login(self, username: str, secret: str) -> None:
        reply = self.call("POST", "/v2/users/login", {"username": username, "password": secret}, auth=False)
        self.token = reply["token"]

    def pages(self, path: str):
        sep = "&" if "?" in path else "?"
        url = f"{path}{sep}page_size=100"
        while url:
            reply = self.call("GET", url)
            yield from reply.get("results", [])
            nxt = reply.get("next")
            url = nxt.removeprefix(self.api) if nxt else None


def stale_tags(names: list[str], keep: int) -> list[str]:
    nightlies = sorted((n for n in names if NIGHTLY.fullmatch(n)),
                       key=lambda n: (NIGHTLY.fullmatch(n).group(1), n), reverse=True)
    return nightlies[keep:]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--namespace", required=True)
    parser.add_argument("--prefix", default="marsh-", help="only repositories whose name starts with this")
    parser.add_argument("--keep", type=int, default=10, help="newest nightly tags to keep per repository")
    parser.add_argument("--apply", action="store_true", help="delete (default: only report)")
    parser.add_argument("--api", default="https://hub.docker.com")
    args = parser.parse_args()
    if args.keep < 3:
        parser.error("--keep must be at least 3")

    hub = Hub(args.api, None)
    if args.apply:
        user, secret = os.environ.get("DOCKERHUB_USERNAME"), os.environ.get("DOCKERHUB_TOKEN")
        if not user or not secret:
            parser.error("--apply needs DOCKERHUB_USERNAME and DOCKERHUB_TOKEN")
        hub.login(user, secret)

    failures = deleted = 0
    try:
        repositories = sorted(r["name"] for r in hub.pages(f"/v2/namespaces/{args.namespace}/repositories")
                              if r["name"].startswith(args.prefix))
    except (urllib.error.URLError, KeyError) as error:
        print(f"cannot list repositories: {error}", file=sys.stderr)
        return 1
    for repository in repositories:
        try:
            names = [t["name"] for t in hub.pages(f"/v2/namespaces/{args.namespace}/repositories/{repository}/tags")]
        except (urllib.error.URLError, KeyError) as error:
            print(f"::warning::{repository}: cannot list tags: {error}")
            failures += 1
            continue
        stale = stale_tags(names, args.keep)
        print(f"{repository}: {len(names)} tags, deleting {len(stale)}" if args.apply else
              f"{repository}: {len(names)} tags, would delete {len(stale)}")
        for tag in stale:
            if not args.apply:
                print(f"  would delete {repository}:{tag}")
                continue
            try:
                hub.call("DELETE", f"/v2/repositories/{args.namespace}/{repository}/tags/{tag}/")
                deleted += 1
                print(f"  deleted {repository}:{tag}")
            except urllib.error.URLError as error:
                print(f"::warning::{repository}:{tag}: delete failed: {error}")
                failures += 1
    print(f"{deleted} deleted, {failures} failed" if args.apply else "dry run; pass --apply to delete")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
